//! Per-thread cache of detached free-block chains, one bin per size class.
//!
//! Fast path: pop/push on a bin's intrusive list — no locks, no atomics.
//! Slow paths: refill from the global heap (batched), and trimming when the
//! thread's total cached bytes exceed [`THREAD_CACHE_BUDGET`] (grouped by
//! owning page so each page needs only one lock acquisition).

use crate::classes::CLASSES;
use crate::classes::NUM_CLASSES;
use crate::heap::REFILL_BATCH;
use crate::page::{pop_block, push_block, PageHeader, HEADER_SIZE, PAGE_MAGIC, PAGE_MASK};
use core::ptr;

/// Total bytes one thread's cache may retain before trimming starts.
///
/// Blocks freed by a thread are very likely to be re-allocated by that same
/// thread; round-tripping them through the global heap (lock + list surgery +
/// re-carve) is by far the most expensive slow path. So caches grow freely
/// and we trim only when the aggregate budget is exceeded. Worst-case overhead
/// is `THREAD_CACHE_BUDGET` bytes per thread.
const THREAD_CACHE_BUDGET: usize = 32 * 1024 * 1024;

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
    cached_bytes: usize,
}

impl ThreadCache {
    pub(crate) const fn new() -> Self {
        ThreadCache {
            bins: [Bin {
                head: ptr::null_mut(),
                len: 0,
            }; NUM_CLASSES],
            cached_bytes: 0,
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
        let limit = flush_limit(class);
        if self.bins[class].len > limit {
            self.flush_bin(class, false, limit / 2);
        }
    }

    /// Return cached blocks of `class`, grouped by owning page so each page
    /// needs only one lock acquisition. With `full`, drain the bin entirely;
    /// otherwise shrink it to `floor` (retaining half reduces future refills
    /// and future flushes).
    unsafe fn flush_bin(&mut self, class: usize, full: bool, floor: u32) {
        let mut groups = [Group::EMPTY; MAX_FLUSH_GROUPS];
        let mut ng = 0usize;
        let bin = &mut self.bins[class];

        while bin.len > floor {
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
                self.flush_bin(class, true, 0);
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
