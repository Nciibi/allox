//! Per-thread cache of detached free-block chains, one bin per size class.
//!
//! Fast path: pop/push on a bin's intrusive list — no locks, no atomics.
//! Slow paths: refill from the global heap (batched), and flush back when a
//! bin grows past [`FLUSH_LIMIT`] (grouped by owning page so each page needs
//! only one lock acquisition).

use crate::classes::CLASSES;
use crate::classes::NUM_CLASSES;
use crate::heap::REFILL_BATCH;
use crate::page::{pop_block, push_block, PageHeader, HEADER_SIZE, PAGE_MAGIC, PAGE_MASK};
use core::ptr;

/// Start flushing when a bin exceeds this many cached blocks...
const FLUSH_LIMIT: u32 = 2 * REFILL_BATCH + 1;
/// ...and shrink it back down to this level (retain half: fewer future
/// refills and fewer future flushes than drain-to-empty).
const FLUSH_TARGET: u32 = REFILL_BATCH;
/// Distinct pages touched by one flush; bounded because a flushed bin holds
/// at most `FLUSH_LIMIT + 1` blocks.
const MAX_FLUSH_GROUPS: usize = (FLUSH_LIMIT as usize) + 8;

#[derive(Clone, Copy)]
struct Group {
    page: *mut PageHeader,
    head: *mut u8,
    tail: *mut u8,
    n: u16,
}

impl Group {
    const EMPTY: Group = Group {
        page: ptr::null_mut(),
        head: ptr::null_mut(),
        tail: ptr::null_mut(),
        n: 0,
    };
}

#[derive(Clone, Copy)]
struct Bin {
    head: *mut u8,
    len: u32,
}

pub(crate) struct ThreadCache {
    bins: [Bin; NUM_CLASSES],
}

impl ThreadCache {
    pub(crate) const fn new() -> Self {
        ThreadCache {
            bins: [Bin {
                head: ptr::null_mut(),
                len: 0,
            }; NUM_CLASSES],
        }
    }

    /// Fast-path allocation. Returns null only when the heap is out of memory.
    pub(crate) unsafe fn alloc(&mut self, class: usize) -> *mut u8 {
        let bin = &mut self.bins[class];
        if let Some(p) = pop_block(&mut bin.head) {
            bin.len -= 1;
            return p;
        }
        self.refill(class)
    }

    /// Slow path: pull a batch of blocks from the global heap.
    unsafe fn refill(&mut self, class: usize) -> *mut u8 {
        let (chain, count) = crate::heap::HEAP.take_blocks(class);
        if chain.is_null() {
            return ptr::null_mut();
        }
        // Split one block off to return; the rest stay in the bin.
        let first = chain;
        let rest = *first.cast::<*mut u8>();
        let bin = &mut self.bins[class];
        bin.head = rest;
        bin.len += count - 1;
        first
    }

    pub(crate) unsafe fn dealloc(&mut self, p: *mut u8) {
        #[cfg(debug_assertions)]
        debug_validate_free(p);

        let page = PageHeader::of(p);
        let class = (*page).class as usize;
        {
            let bin = &mut self.bins[class];
            push_block(&mut bin.head, p);
            bin.len += 1;
        }
        if self.bins[class].len > FLUSH_LIMIT {
            self.flush_bin(class);
        }
    }

    /// Return cached blocks of `class` down to [`FLUSH_TARGET`], grouped by
    /// owning page so each page needs only one lock acquisition.
    unsafe fn flush_bin(&mut self, class: usize) {
        let mut groups = [Group::EMPTY; MAX_FLUSH_GROUPS];
        let mut ng = 0usize;
        let bin = &mut self.bins[class];

        while bin.len > FLUSH_TARGET {
            let b = match pop_block(&mut bin.head) {
                Some(b) => b,
                None => break,
            };
            bin.len -= 1;
            let page = PageHeader::of(b);
            *b.cast::<*mut u8>() = ptr::null_mut();
            let mut slot = None;
            for g in groups.iter_mut().take(ng) {
                if g.page == page {
                    slot = Some(g);
                    break;
                }
            }
            match slot {
                Some(g) => {
                    *g.tail.cast::<*mut u8>() = b;
                    g.tail = b;
                    g.n += 1;
                }
                None => {
                    groups[ng] = Group {
                        page,
                        head: b,
                        tail: b,
                        n: 1,
                    };
                    ng += 1;
                }
            }
        }

        for g in groups.iter_mut().take(ng) {
            crate::heap::HEAP.release_blocks(g.page, g.head, g.n);
        }
    }

    /// Return all cached blocks (used at explicit shutdown/flush requests).
    pub(crate) unsafe fn flush_all(&mut self) {
        for class in 0..NUM_CLASSES {
            if !self.bins[class].head.is_null() {
                self.flush_bin(class);
            }
        }
    }
}

/// Debug-build validation that `p` is a live-looking block of its page:
/// correct magic, inside the block area, class-aligned, and not already on
/// the page free list (double-free detection). Runs under the heap lock so
/// it never races with list mutation.
#[cfg(debug_assertions)]
unsafe fn debug_validate_free(p: *mut u8) {
    if p.is_null() {
        return;
    }
    let base = p as usize & !PAGE_MASK;
    let page = base as *mut PageHeader;
    if (*page).magic != PAGE_MAGIC {
        invalid("allox: dealloc of pointer outside allocator pages");
    }
    let class = (*page).class as usize;
    let block_size = CLASSES[class];
    let offset = p as usize - base;
    if offset < HEADER_SIZE || (offset - HEADER_SIZE) % block_size != 0 {
        invalid("allox: dealloc of misaligned interior pointer");
    }
    let _guard = crate::heap::HEAP.debug_lock(class);
    let mut cur = (*page).free_head;
    let mut steps = (*page).free_count;
    while steps > 0 {
        if cur == p {
            invalid("allox: double free detected");
        }
        cur = *cur.cast::<*mut u8>();
        steps -= 1;
    }
}

#[cfg(debug_assertions)]
fn invalid(msg: &'static str) -> ! {
    eprintln!("{}", msg);
    std::process::abort()
}
