//! Global page heap: per-size-class partial-page lists, each under its own
//! mutex (lock sharding: contention spreads across 64 independent locks).
//!
//! All mutation of a page's `free_head` happens while holding that page
//! class' mutex, so thread caches only ever own *detached* block chains.
//! A page is unmapped exactly when its `used` count drops to zero, which by
//! construction cannot happen while any thread still caches one of its
//! blocks. No code path ever holds two class locks at once.

use crate::classes::{span_pages_for, NUM_CLASSES, NUM_MEDIUM};
use crate::page::{
    pop_block, PageHeader, SpanMaster, FLAG_IN_PARTIAL, FLAG_VIRGIN, PAGE_SIZE,
};
use crate::sys::{self, Mutex};
#[cfg(debug_assertions)]
use crate::sys::MutexGuard;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

/// Blocks moved from pages into a thread cache in one batch.
pub(crate) const REFILL_BATCH: u32 = 64;

/// Fully-freed pages kept mapped per size class before unmapping.
/// Avoids map/unmap syscalls on alloc/free churn; worst-case retention is
/// `64 classes x CAP x 64 KiB` (~16 MiB).
const EMPTY_PAGE_CACHE_PER_CLASS: u32 = 4;

pub(crate) static MAPPED_PAGES: AtomicU64 = AtomicU64::new(0);
pub(crate) static MAP_CALLS: AtomicU64 = AtomicU64::new(0);
pub(crate) static UNMAP_CALLS: AtomicU64 = AtomicU64::new(0);

/// Global telemetry counters, written in batches from thread-local
/// accumulators (see `cache.rs`) so the hot path stays contention-free.
#[cfg(feature = "telemetry")]
pub(crate) struct TelemetryCounters {
    pub(crate) total_allocs: AtomicU64,
    pub(crate) total_frees: AtomicU64,
    pub(crate) bytes_in: AtomicU64,
    pub(crate) bytes_out: AtomicU64,
    pub(crate) peak_live_bytes: AtomicU64,
    pub(crate) large_allocs: AtomicU64,
    pub(crate) per_class: [AtomicU64; NUM_CLASSES],
}

#[cfg(feature = "telemetry")]
pub(crate) static TELEMETRY: TelemetryCounters = TelemetryCounters {
    total_allocs: AtomicU64::new(0),
    total_frees: AtomicU64::new(0),
    bytes_in: AtomicU64::new(0),
    bytes_out: AtomicU64::new(0),
    peak_live_bytes: AtomicU64::new(0),
    large_allocs: AtomicU64::new(0),
    per_class: [const { AtomicU64::new(0) }; NUM_CLASSES],
};

pub(crate) struct ListHead {
    /// Partial pages (spare free blocks), doubly linked via prev/next.
    head: *mut PageHeader,
    /// Fully free pages held for reuse instead of unmapping; singly linked
    /// via `next`. All carry a full free list and `used == 0`.
    empty: *mut PageHeader,
    empty_count: u32,
}

// Raw pointers are only manipulated while holding the enclosing Mutex.
unsafe impl Send for ListHead {}

impl ListHead {
    pub(crate) const fn new() -> Self {
        ListHead {
            head: ptr::null_mut(),
            empty: ptr::null_mut(),
            empty_count: 0,
        }
    }
}

pub(crate) struct GlobalHeap {
    classes: [Mutex<ListHead>; NUM_CLASSES],
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
/// building a detached chain. Caller must hold the class' lock.
/// `*virgin` stays true only if every contributing page is still OS-zero.
unsafe fn fill_from_list(
    list: &mut *mut PageHeader,
    chain: &mut *mut u8,
    count: &mut u32,
    virgin: &mut bool,
) {
    while *count < REFILL_BATCH {
        let page = *list;
        if page.is_null() {
            break;
        }
        if (*page).flags & FLAG_VIRGIN == 0 {
            *virgin = false;
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
            classes: [const { Mutex::new(ListHead::new()) }; NUM_CLASSES],
        }
    }

    /// Acquire up to REFILL_BATCH free blocks of `class` as an intrusive chain.
    /// The `virgin` flag is true when every returned block is guaranteed to
    /// still be OS-zero (never allocated since its page was carved).
    /// Returns `(null, 0, _)` only when the OS refuses us memory.
    pub(crate) unsafe fn take_blocks(&self, class: usize) -> (*mut u8, u32, bool) {
        let mut chain: *mut u8 = ptr::null_mut();
        let mut count: u32 = 0;
        let mut virgin = true;

        {
            let mut list = self.classes[class].lock();
            fill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin);

            if count == 0 && !list.empty.is_null() {
                // Recycle a cached empty page of the same class instead of
                // asking the OS. Its free list is already full and intact.
                let page = list.empty;
                list.empty = (*page).next;
                (*page).next = ptr::null_mut();
                list.empty_count -= 1;
                if (*page).flags & FLAG_VIRGIN == 0 {
                    virgin = false;
                }
                link_partial(&mut list.head, page);
                fill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin);
            }
        }

        if count == 0 {
            let raw = sys::map(PAGE_SIZE);
            if !raw.is_null() {
                let page = raw.cast::<PageHeader>();
                (*page).init(class);
                MAPPED_PAGES.fetch_add(1, Ordering::Relaxed);
                MAP_CALLS.fetch_add(1, Ordering::Relaxed);
                let mut list = self.classes[class].lock();
                link_partial(&mut list.head, page);
                fill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin);
            } else {
                virgin = false;
            }
        }

        (chain, count, virgin)
    }

    /// Return a chain of `n` blocks, all belonging to `page`, to that page.
    pub(crate) unsafe fn release_blocks(&self, page: *mut PageHeader, chain: *mut u8, n: u16) {
        let class = (*page).class as usize;
        let unmap_now = {
            let mut list = self.classes[class].lock();
            // Freed blocks are dirty by definition.
            (*page).flags &= !FLAG_VIRGIN;
            let mut tail = chain;
            while !(*tail.cast::<*mut u8>()).is_null() {
                tail = *tail.cast::<*mut u8>();
            }
            *tail.cast::<*mut u8>() = (*page).free_head;
            (*page).free_head = chain;
            (*page).free_count += n;
            (*page).used -= n;
            if (*page).used == 0 {
                unlink_partial(&mut list.head, page);
                if list.empty_count < EMPTY_PAGE_CACHE_PER_CLASS {
                    // Delayed reclamation: keep the page mapped for reuse.
                    (*page).next = list.empty;
                    list.empty = page;
                    list.empty_count += 1;
                    false
                } else {
                    true
                }
            } else {
                if (*page).flags & FLAG_IN_PARTIAL == 0 {
                    link_partial(&mut list.head, page);
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

    /// Lock access to a class' partial list for external validation
    /// (debug double-free detection).
    #[cfg(debug_assertions)]
    pub(crate) fn debug_lock(&self, class: usize) -> MutexGuard<'_, ListHead> {
        self.classes[class].lock()
    }
}

pub(crate) static HEAP: GlobalHeap = GlobalHeap::new();

// ---------------------------------------------------------------------------
// Medium-span heap: one partial-span list per medium class, same sharding
// discipline as the small heap (no path ever holds two class locks). Spans
// are multi-page runs carved into medium blocks; batches are span-sized
// (MEDIUM_REFILL_BATCH) rather than 64, since one span already holds 8+.
 // ---------------------------------------------------------------------------

/// Blocks moved from spans into a thread cache per slow-path take.
pub(crate) const MEDIUM_REFILL_BATCH: u32 = 16;

/// Fully-freed spans kept mapped per medium class before unmapping. Spans
/// are large (up to ~16 pages), so the cap is lower than for small pages.
const EMPTY_SPAN_CACHE_PER_CLASS: u32 = 2;

pub(crate) struct MSpanList {
    /// Partial spans (spare free blocks), doubly linked via prev/next.
    head: *mut SpanMaster,
    /// Fully free spans held for reuse; singly linked via `next`.
    empty: *mut SpanMaster,
    empty_count: u32,
}

// Raw pointers are only manipulated while holding the enclosing Mutex.
unsafe impl Send for MSpanList {}

impl MSpanList {
    pub(crate) const fn new() -> Self {
        MSpanList {
            head: ptr::null_mut(),
            empty: ptr::null_mut(),
            empty_count: 0,
        }
    }
}

pub(crate) struct MediumHeap {
    classes: [Mutex<MSpanList>; NUM_MEDIUM],
}

unsafe fn mlink_partial(list: &mut *mut SpanMaster, s: *mut SpanMaster) {
    (*s).prev = ptr::null_mut();
    (*s).next = *list;
    if !(*list).is_null() {
        (**list).prev = s;
    }
    *list = s;
    (*s).flags |= FLAG_IN_PARTIAL;
}

/// Returns true if the span was linked and has been removed.
unsafe fn munlink_partial(list: &mut *mut SpanMaster, s: *mut SpanMaster) -> bool {
    if (*s).flags & FLAG_IN_PARTIAL == 0 {
        return false;
    }
    let prev = (*s).prev;
    let next = (*s).next;
    if !prev.is_null() {
        (*prev).next = next;
    } else {
        *list = next;
    }
    if !next.is_null() {
        (*next).prev = prev;
    }
    (*s).prev = ptr::null_mut();
    (*s).next = ptr::null_mut();
    (*s).flags &= !FLAG_IN_PARTIAL;
    true
}

/// Pop up to `cap` blocks from the spans of one partial list. Caller must
/// hold the class' lock. `*virgin` stays true only if every contributing
/// span is still OS-zero.
unsafe fn mfill_from_list(
    list: &mut *mut SpanMaster,
    chain: &mut *mut u8,
    count: &mut u32,
    virgin: &mut bool,
    cap: u32,
) {
    while *count < cap {
        let span = *list;
        if span.is_null() {
            break;
        }
        if (*span).flags & FLAG_VIRGIN == 0 {
            *virgin = false;
        }
        match pop_block(&mut (*span).free_head) {
            Some(b) => {
                *b.cast::<*mut u8>() = *chain;
                *chain = b;
                *count += 1;
                (*span).free_count -= 1;
                (*span).used += 1;
                if (*span).free_count == 0 {
                    munlink_partial(list, span);
                }
            }
            None => {
                // Empty span must never be on the partial list; recover anyway.
                munlink_partial(list, span);
            }
        }
    }
}

impl MediumHeap {
    pub(crate) const fn new() -> Self {
        MediumHeap {
            classes: [const { Mutex::new(MSpanList::new()) }; NUM_MEDIUM],
        }
    }

    /// Acquire up to MEDIUM_REFILL_BATCH free blocks of `mclass` as an
    /// intrusive chain. Returns `(null, 0, _)` only on OS exhaustion.
    pub(crate) unsafe fn take_blocks(&self, mclass: usize) -> (*mut u8, u32, bool) {
        let mut chain: *mut u8 = ptr::null_mut();
        let mut count: u32 = 0;
        let mut virgin = true;

        {
            let mut list = self.classes[mclass].lock();
            mfill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin, MEDIUM_REFILL_BATCH);

            if count == 0 && !list.empty.is_null() {
                let span = list.empty;
                list.empty = (*span).next;
                (*span).next = ptr::null_mut();
                list.empty_count -= 1;
                if (*span).flags & FLAG_VIRGIN == 0 {
                    virgin = false;
                }
                mlink_partial(&mut list.head, span);
                mfill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin, MEDIUM_REFILL_BATCH);
            }
        }

        if count == 0 {
            let pages = span_pages_for(crate::classes::MEDIUM_CLASSES[mclass]);
            let raw = sys::map(pages * PAGE_SIZE);
            if !raw.is_null() {
                let span = raw.cast::<SpanMaster>();
                (*span).init(mclass, pages as u32);
                MAPPED_PAGES.fetch_add(1, Ordering::Relaxed);
                MAP_CALLS.fetch_add(1, Ordering::Relaxed);
                let mut list = self.classes[mclass].lock();
                mlink_partial(&mut list.head, span);
                mfill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin, MEDIUM_REFILL_BATCH);
            } else {
                virgin = false;
            }
        }

        (chain, count, virgin)
    }

    /// Return a chain of `n` blocks, all belonging to `span`, to that span.
    pub(crate) unsafe fn release_blocks(&self, span: *mut SpanMaster, chain: *mut u8, n: u32) {
        let mclass = (*span).mclass as usize;
        let unmap = {
            let mut list = self.classes[mclass].lock();
            // Freed blocks are dirty by definition.
            (*span).flags &= !FLAG_VIRGIN;
            let mut tail = chain;
            while !(*tail.cast::<*mut u8>()).is_null() {
                tail = *tail.cast::<*mut u8>();
            }
            *tail.cast::<*mut u8>() = (*span).free_head;
            (*span).free_head = chain;
            (*span).free_count += n;
            (*span).used -= n;
            if (*span).used == 0 {
                munlink_partial(&mut list.head, span);
                if list.empty_count < EMPTY_SPAN_CACHE_PER_CLASS {
                    // Delayed reclamation: keep the span mapped for reuse.
                    (*span).next = list.empty;
                    list.empty = span;
                    list.empty_count += 1;
                    None
                } else {
                    Some((*span).mapped_bytes())
                }
            } else {
                if (*span).flags & FLAG_IN_PARTIAL == 0 {
                    mlink_partial(&mut list.head, span);
                }
                None
            }
        };
        if let Some(bytes) = unmap {
            sys::unmap(span.cast::<u8>(), bytes);
            MAPPED_PAGES.fetch_sub(1, Ordering::Relaxed);
            UNMAP_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Lock access to a class' partial list for external validation
    /// (debug double-free detection).
    #[cfg(debug_assertions)]
    pub(crate) fn debug_lock_medium(&self, mclass: usize) -> MutexGuard<'_, MSpanList> {
        self.classes[mclass].lock()
    }
}

pub(crate) static MEDIUM_HEAP: MediumHeap = MediumHeap::new();
