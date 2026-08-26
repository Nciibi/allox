//! Per-thread cache of detached free-block chains, one bin per size class.
//!
//! Fast path: pop/push on a bin's intrusive list — no locks, no atomics.
//! Slow paths: refill from the global heap (batched), and trimming when the
//! thread's total cached bytes exceed [`THREAD_CACHE_BUDGET`].
//!
//! Design note: freed blocks almost always come back to the same thread.
//! Round-tripping them through the global heap (lock + list surgery +
//! re-carve) is the dominant slow-path cost under mixed workloads, so bins
//! grow freely and trimming happens only against the aggregate byte budget,
//! biggest classes first.

use crate::classes::CLASSES;
use crate::classes::NUM_CLASSES;
use crate::heap::REFILL_BATCH;
use crate::page::{pop_block, push_block, PageHeader};
use core::ptr;

/// Total bytes one thread's cache may retain before trimming starts.
/// Worst-case overhead is this many bytes per thread.
const THREAD_CACHE_BUDGET: usize = 64 * 1024 * 1024;

/// Blocks released to the global heap per grouping pass. Bounds the stack
/// buffer used to group blocks by owning page.
const FLUSH_CHUNK: u32 = 2048;
const MAX_FLUSH_GROUPS: usize = FLUSH_CHUNK as usize + 8;

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
    /// Per class: number of guaranteed-OS-zero blocks currently at the
    /// *bottom* of the bin (from refills of virgin pages). A pop is zeroed
    /// iff the remaining length drops below this count.
    virgin: [u32; NUM_CLASSES],
    /// Telemetry accumulators, published to the global atomics in batches.
    #[cfg(feature = "telemetry")]
    pending: Pending,
}

/// Thread-local telemetry deltas, flushed every [`FLUSH_OPS`] operations.
#[cfg(feature = "telemetry")]
pub(crate) struct Pending {
    ops: u32,
    allocs: u64,
    frees: u64,
    bytes_in: u64,
    bytes_out: u64,
    per_class: [u64; NUM_CLASSES],
}

#[cfg(feature = "telemetry")]
impl Pending {
    const fn new() -> Self {
        Pending {
            ops: 0,
            allocs: 0,
            frees: 0,
            bytes_in: 0,
            bytes_out: 0,
            per_class: [0; NUM_CLASSES],
        }
    }
}

impl ThreadCache {
    pub(crate) const fn new() -> Self {
        ThreadCache {
            bins: [Bin {
                head: ptr::null_mut(),
                len: 0,
            }; NUM_CLASSES],
            cached_bytes: 0,
            virgin: [0; NUM_CLASSES],
            #[cfg(feature = "telemetry")]
            pending: Pending {
                ops: 0,
                allocs: 0,
                frees: 0,
                bytes_in: 0,
                bytes_out: 0,
                per_class: [0; NUM_CLASSES],
            },
        }
    }

    /// Publish accumulated telemetry deltas to the global atomics.
    #[cfg(feature = "telemetry")]
    fn publish(&mut self) {
        use core::sync::atomic::Ordering::Relaxed;
        let t = &crate::heap::TELEMETRY;
        if self.pending.allocs != 0 {
            t.TOTAL_ALLOCS.fetch_add(self.pending.allocs, Relaxed);
        }
        if self.pending.frees != 0 {
            t.TOTAL_FREES.fetch_add(self.pending.frees, Relaxed);
        }
        if self.pending.bytes_in != 0 {
            t.bytes_in.fetch_add(self.pending.bytes_in, Relaxed);
        }
        if self.pending.bytes_out != 0 {
            t.bytes_out.fetch_add(self.pending.bytes_out, Relaxed);
        }
        for (class, n) in self.pending.per_class.iter().enumerate() {
            if *n != 0 {
                t.per_class[class].fetch_add(*n, Relaxed);
                *n = 0;
            }
        }
        // Sampled peak: exact between flush points by design.
        let live = t
            .bytes_in
            .load(Relaxed)
            .saturating_sub(t.bytes_out.load(Relaxed));
        t.peak_live_bytes.fetch_max(live, Relaxed);
        self.pending.allocs = 0;
        self.pending.frees = 0;
        self.pending.bytes_in = 0;
        self.pending.bytes_out = 0;
        self.pending.ops = 0;
    }

    #[inline]
    #[cfg(feature = "telemetry")]
    fn note_alloc(&mut self, class: usize) {
        self.pending.ops += 1;
        self.pending.allocs += 1;
        self.pending.bytes_in += CLASSES[class] as u64;
        self.pending.per_class[class] += 1;
        if self.pending.ops >= FLUSH_OPS {
            self.publish();
        }
    }

    #[inline]
    #[cfg(feature = "telemetry")]
    fn note_free(&mut self, class: usize) {
        self.pending.ops += 1;
        self.pending.frees += 1;
        self.pending.bytes_out += CLASSES[class] as u64;
        self.pending.per_class[class] += 1;
        if self.pending.ops >= FLUSH_OPS {
            self.publish();
        }
    }

    /// Fast-path allocation. Returns null only when the heap is out of memory.
    pub(crate) unsafe fn alloc(&mut self, class: usize) -> *mut u8 {
        let bin = &mut self.bins[class];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= CLASSES[class];
            if below < self.virgin[class] {
                self.virgin[class] -= 1;
            }
            #[cfg(feature = "telemetry")]
            self.note_alloc(class);
            return p;
        }
        let (p, _) = self.refill(class);
        #[cfg(feature = "telemetry")]
        if !p.is_null() {
            self.note_alloc(class);
        }
        p
    }

    /// Allocation that also reports whether the block is still OS-zero,
    /// letting `alloc_zeroed` skip the memset.
    pub(crate) unsafe fn alloc_zeroed(&mut self, class: usize) -> (*mut u8, bool) {
        let bin = &mut self.bins[class];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= CLASSES[class];
            let zeroed = below < self.virgin[class];
            if zeroed {
                self.virgin[class] -= 1;
            }
            #[cfg(feature = "telemetry")]
            self.note_alloc(class);
            return (p, zeroed);
        }
        let r = self.refill(class);
        #[cfg(feature = "telemetry")]
        if !r.0.is_null() {
            self.note_alloc(class);
        }
        r
    }

    /// Slow path: pull a batch of blocks from the global heap.
    #[inline]
    unsafe fn refill(&mut self, class: usize) -> (*mut u8, bool) {
        // Under aggregate pressure, shed some cache before asking for more.
        if self.cached_bytes > THREAD_CACHE_BUDGET / 2 {
            self.trim();
        }
        let (chain, count, virgin) = crate::heap::HEAP.take_blocks(class);
        if chain.is_null() {
            return (ptr::null_mut(), false);
        }
        // Split one block off to return; the rest stay in the bin.
        let first = chain;
        let rest = *first.cast::<*mut u8>();
        let bin = &mut self.bins[class];
        bin.head = rest;
        bin.len += count - 1;
        self.cached_bytes += CLASSES[class] * (count - 1) as usize;
        // Refill only happens on an empty bin, so the whole batch sits at the
        // bottom; the block we returned was part of it.
        self.virgin[class] = if virgin { count - 1 } else { 0 };
        (first, virgin)
    }

    pub(crate) unsafe fn dealloc(&mut self, p: *mut u8) {
        #[cfg(debug_assertions)]
        debug_validate_free(p);

        let page = PageHeader::of(p);
        let class = (*page).class as usize;
        let bin = &mut self.bins[class];
        push_block(&mut bin.head, p);
        bin.len += 1;
        self.cached_bytes += CLASSES[class];
        #[cfg(feature = "telemetry")]
        self.note_free(class);
        if self.cached_bytes > THREAD_CACHE_BUDGET {
            self.trim();
        }
    }

    /// Bring total cached bytes under half the budget by repeatedly halving
    /// the largest bin. Fixed-size passes over a 64-entry array; no allocation.
    unsafe fn trim(&mut self) {
        let target = THREAD_CACHE_BUDGET / 2;
        while self.cached_bytes > target {
            let mut best = usize::MAX;
            let mut best_bytes = 0usize;
            for (class, size) in CLASSES.iter().enumerate() {
                let bin_bytes = self.bins[class].len as usize * size;
                if self.bins[class].len > REFILL_BATCH && bin_bytes > best_bytes {
                    best_bytes = bin_bytes;
                    best = class;
                }
            }
            if best == usize::MAX {
                self.cached_bytes = target; // nothing trimmable left; stop
                break;
            }
            let len = self.bins[best].len;
            self.flush_bin(best, len / 2);
        }
    }

    /// Shrink `class`'s bin down to `floor_blocks` blocks, returning removed
    /// blocks to their owning pages in chunked, grouped batches so each page
    /// needs only one lock acquisition per chunk.
    unsafe fn flush_bin(&mut self, class: usize, floor_blocks: u32) {
        let block_size = CLASSES[class];
        let bin = &mut self.bins[class];

        while bin.len > floor_blocks {
            let mut groups = [Group::EMPTY; MAX_FLUSH_GROUPS];
            let mut ng = 0usize;
            let mut popped = 0u32;

            while bin.len > floor_blocks && popped < FLUSH_CHUNK {
                let b = match pop_block(&mut bin.head) {
                    Some(b) => b,
                    None => break,
                };
                let below = bin.len - 1;
                bin.len = below;
                if below < self.virgin[class] {
                    self.virgin[class] -= 1;
                }
                popped += 1;
                self.cached_bytes = self.cached_bytes.saturating_sub(block_size);

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
            if popped == 0 {
                break;
            }
        }
    }

    /// Return all cached blocks (used at explicit shutdown/flush requests).
    pub(crate) unsafe fn flush_all(&mut self) {
        for class in 0..NUM_CLASSES {
            if !self.bins[class].head.is_null() {
                self.flush_bin(class, 0);
            }
        }
        self.cached_bytes = 0;
        self.virgin = [0; NUM_CLASSES];
        #[cfg(feature = "telemetry")]
        self.publish();
    }
}

/// Debug-build validation that `p` is a live-looking block of its page:
/// correct magic, inside the block area, class-aligned, and not already on
/// the page free list (double-free detection). Runs under the heap lock so
/// it never races with list mutation.
#[cfg(debug_assertions)]
unsafe fn debug_validate_free(p: *mut u8) {
    use crate::page::{HEADER_SIZE, PAGE_MAGIC, PAGE_MASK};
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
