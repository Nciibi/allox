//! Global page heap: per-class partial-page lists guarded by one mutex.
//!
//! All mutation of a page's `free_head` happens while holding the heap mutex,
//! so thread caches only ever own *detached* block chains. A page is unmapped
//! exactly when its `used` count drops to zero, which by construction cannot
//! happen while any thread still caches one of its blocks.

use crate::classes::NUM_CLASSES;
use crate::page::{pop_block, FLAG_IN_PARTIAL, PAGE_SIZE, PageHeader};
use crate::sys::{self, Mutex, MutexGuard};
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

/// Blocks moved from a page into a thread cache in one batch.
pub(crate) const REFILL_BATCH: u32 = 32;

pub(crate) static MAPPED_PAGES: AtomicU64 = AtomicU64::new(0);
pub(crate) static MAP_CALLS: AtomicU64 = AtomicU64::new(0);
pub(crate) static UNMAP_CALLS: AtomicU64 = AtomicU64::new(0);

struct Inner {
    partial: [*mut PageHeader; NUM_CLASSES],
}

pub(crate) struct GlobalHeap {
    inner: Mutex<Inner>,
}

unsafe fn link_partial(list: &mut *mut PageHeader, p: *mut PageHeader) {
    (*p).prev = ptr::null_mut();
    (*p).next = *list;
    if !(*list).is_null() {
        (**list).prev = p;
    }
    *list = p;
    (*p).flags |= FLAG_IN_PARTIAL;
}

/// Returns true if the page was linked and has been removed.
unsafe fn unlink_partial(list: &mut *mut PageHeader, p: *mut PageHeader) -> bool {
    if (*p).flags & FLAG_IN_PARTIAL == 0 {
        return false;
    }
    let prev = (*p).prev;
    let next = (*p).next;
    if !prev.is_null() {
        (*prev).next = next;
    } else {
        *list = next;
    }
    if !next.is_null() {
        (*next).prev = prev;
    }
    (*p).prev = ptr::null_mut();
    (*p).next = ptr::null_mut();
    (*p).flags &= !FLAG_IN_PARTIAL;
    true
}

/// Pop up to REFILL_BATCH blocks from the pages of one partial list,
/// building a detached chain. Caller must hold the heap lock.
unsafe fn fill_from_list(list: &mut *mut PageHeader, chain: &mut *mut u8, count: &mut u32) {
    while *count < REFILL_BATCH {
        let page = *list;
        if page.is_null() {
            break;
        }
        match pop_block(&mut (*page).free_head) {
            Some(b) => {
                *b.cast::<*mut u8>() = *chain;
                *chain = b;
                *count += 1;
                (*page).free_count -= 1;
                (*page).used += 1;
                if (*page).free_count == 0 {
                    unlink_partial(list, page);
                }
            }
            None => {
                // Empty page must never be on the partial list; recover anyway.
                unlink_partial(list, page);
            }
        }
    }
}

impl GlobalHeap {
    pub(crate) const fn new() -> Self {
        GlobalHeap {
            inner: Mutex::new(Inner {
                partial: [ptr::null_mut(); NUM_CLASSES],
            }),
        }
    }

    /// Acquire up to REFILL_BATCH free blocks of `class` as an intrusive chain.
    /// Returns `(null, 0)` only when the OS refuses us memory.
    pub(crate) unsafe fn take_blocks(&self, class: usize) -> (*mut u8, u32) {
        let mut chain: *mut u8 = ptr::null_mut();
        let mut count: u32 = 0;

        {
            let mut inner = self.inner.lock();
            fill_from_list(&mut inner.partial[class], &mut chain, &mut count);
        }

        if count == 0 {
            let raw = sys::map(PAGE_SIZE);
            if !raw.is_null() {
                let page = raw.cast::<PageHeader>();
                (*page).init(class);
                MAPPED_PAGES.fetch_add(1, Ordering::Relaxed);
                MAP_CALLS.fetch_add(1, Ordering::Relaxed);
                let mut inner = self.inner.lock();
                link_partial(&mut inner.partial[class], page);
                fill_from_list(&mut inner.partial[class], &mut chain, &mut count);
            }
        }

        (chain, count)
    }

    /// Return a chain of `n` blocks, all belonging to `page`, to that page.
    pub(crate) unsafe fn release_blocks(
        &self,
        page: *mut PageHeader,
        chain: *mut u8,
        n: u16,
    ) {
        let unmap_now = {
            let mut inner = self.inner.lock();
            let mut tail = chain;
            while !(*tail.cast::<*mut u8>()).is_null() {
                tail = *tail.cast::<*mut u8>();
            }
            *tail.cast::<*mut u8>() = (*page).free_head;
            (*page).free_head = chain;
            (*page).free_count += n;
            (*page).used -= n;
            if (*page).used == 0 {
                unlink_partial(&mut inner.partial[(*page).class as usize], page);
                true
            } else {
                if (*page).flags & FLAG_IN_PARTIAL == 0 {
                    link_partial(&mut inner.partial[(*page).class as usize], page);
                }
                false
            }
        };
        if unmap_now {
            sys::unmap(page.cast::<u8>(), PAGE_SIZE);
            MAPPED_PAGES.fetch_sub(1, Ordering::Relaxed);
            UNMAP_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Lock access for external validation (debug double-free detection).
    pub(crate) fn lock_for_debug(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock()
    }
}

pub(crate) static HEAP: GlobalHeap = GlobalHeap::new();
