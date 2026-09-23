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

use crate::classes::{MEDIUM_CLASSES, NUM_MEDIUM};
#[cfg(all(unix, feature = "std"))]
use crate::classes::{BIG_CLASSES, NUM_BIG};
#[cfg(feature = "telemetry")]
use crate::classes::TOTAL_CLASSES;
use crate::classes::{CLASSES, NUM_CLASSES};
use crate::heap::{MEDIUM_HEAP, REFILL_BATCH, ReleaseChunk};
#[cfg(all(unix, feature = "std"))]
use crate::heap::BIG_HEAP;
use crate::page::{pop_block, push_block, PageHeader, SpanMaster};
#[cfg(all(unix, feature = "std"))]
use crate::page::BigMaster;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

/// Total bytes one thread's cache may retain before trimming starts.
/// Worst-case overhead is this many bytes per thread.
/// Overridable at startup via `allox::set_thread_cache_budget` (atomic read;
/// never from the environment, since environment access can allocate).
const DEFAULT_THREAD_CACHE_BUDGET: usize = 32 * 1024 * 1024;

/// Remote-free drift cap (ROADMAP P1 step 4): once a thread's cache exceeds
/// this fraction of the budget, frees of blocks whose page/span was claimed
/// by another thread are *counted* (`foreign_bytes`) so they can be shed in
/// batch via `trim` — never one `release_blocks` per free (that serialized
/// producer-consumer on the class lock and thrashed the arena). Same-thread
/// frees below the gate stay header-free (the Phase-0 free-path win).
const DRIFT_GATE_DIV: usize = 2;

/// Once this many foreign bytes are held under the drift gate, shed via
/// `trim` (chunked, page-grouped: one class lock per FLUSH_CHUNK blocks).
/// Budget-scaled so `set_thread_cache_budget` moves both gates together.
const FOREIGN_SHED_DIV: usize = 8;

/// Monotonic thread ids for ownership heuristics (1-based; 0 = unassigned).
static NEXT_TID: AtomicU32 = AtomicU32::new(1);

#[cfg(feature = "std")]
static CACHE_BUDGET: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(DEFAULT_THREAD_CACHE_BUDGET);

#[inline]
fn thread_cache_budget() -> usize {
    #[cfg(feature = "std")]
    {
        CACHE_BUDGET.load(core::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(feature = "std"))]
    DEFAULT_THREAD_CACHE_BUDGET
}

#[cfg(feature = "std")]
pub(crate) fn set_budget(bytes: usize) {
    CACHE_BUDGET.store(
        bytes.max(REFILL_BATCH as usize * 16),
        core::sync::atomic::Ordering::Relaxed,
    );
}

/// Slots / bytes one thread may keep as whole large regions without touching
/// the global cache. Large allocs are rare vs small ones, but each miss costs
/// syscalls, so even a small stash removes the global lock from the hot
/// same-thread reuse path (the common benchmark shape).
pub(crate) const LARGE_STASH_SLOTS: usize = 8;
pub(crate) const LARGE_STASH_CAP_BYTES: usize = 8 * 1024 * 1024;

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

/// One span's share of a medium flush chunk (mirrors [`Group`]).
#[derive(Clone, Copy)]
struct MGroup {
    master: *mut SpanMaster,
    head: *mut u8,
    tail: *mut u8,
    n: u32,
}

impl MGroup {
    const EMPTY: MGroup = MGroup {
        master: ptr::null_mut(),
        head: ptr::null_mut(),
        tail: ptr::null_mut(),
        n: 0,
    };
}

/// One span's share of a big flush chunk (mirrors [`MGroup`]).
#[cfg(all(unix, feature = "std"))]
#[derive(Clone, Copy)]
struct BGroup {
    master: *mut BigMaster,
    head: *mut u8,
    tail: *mut u8,
    n: u32,
}

#[cfg(all(unix, feature = "std"))]
impl BGroup {
    const EMPTY: BGroup = BGroup {
        master: ptr::null_mut(),
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
    /// This thread's ownership id (0 = not yet assigned). Used only for the
    /// remote-free drift-cap heuristic; never for correctness.
    tid: u32,
    /// Bytes accepted from foreign-owned pages while the drift gate is open.
    /// Cleared on every `trim`/`flush_all`; heuristic only (never read for
    /// correctness). Shed batch via `trim` once ≥ budget/[`FOREIGN_SHED_DIV`].
    foreign_bytes: usize,
    /// Per class: number of guaranteed-OS-zero blocks currently at the
    /// *bottom* of the bin (from refills of virgin pages). A pop is zeroed
    /// iff the remaining length drops below this count.
    virgin: [u32; NUM_CLASSES],
    /// Medium bins (multi-page spans), same discipline as small bins.
    mbins: [Bin; NUM_MEDIUM],
    mvirgin: [u32; NUM_MEDIUM],
    /// Big bins (whole spans past the chunk cap), same discipline. Arena
    /// targets only (big spans don't exist elsewhere).
    #[cfg(all(unix, feature = "std"))]
    bigbins: [Bin; NUM_BIG],
    #[cfg(all(unix, feature = "std"))]
    bvirgin: [u32; NUM_BIG],
    /// Whether this thread armed the OS thread-exit flush. Set once on the
    /// first slow path; fast paths never touch it (nor the hook machinery).
    exit_armed: bool,
    /// Per-thread stash of freed large regions: (base, mapped_pages).
    /// Touched only by the owning thread (or the global-cache lock holder in
    /// no_std, which is still mutually exclusive), so no synchronization.
    large: [(*mut u8, u32); LARGE_STASH_SLOTS],
    large_len: u32,
    large_bytes: usize,
    /// Telemetry accumulators, published to the global atomics in batches.
    #[cfg(feature = "telemetry")]
    pending: Pending,
}

/// Thread-local telemetry deltas, flushed every [`FLUSH_OPS`] operations.
#[cfg(feature = "telemetry")]
const FLUSH_OPS: u32 = 8192;

/// Thread-local telemetry accumulators, published to the global atomics
/// in batches.
#[cfg(feature = "telemetry")]
pub(crate) struct Pending {
    ops: u32,
    allocs: u64,
    frees: u64,
    bytes_in: u64,
    bytes_out: u64,
    /// Small classes at 0..NUM_CLASSES, medium classes after.
    per_class: [u64; TOTAL_CLASSES],
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
            per_class: [0; TOTAL_CLASSES],
        }
    }
}

impl ThreadCache {
    pub(crate) const fn new() -> Self {        ThreadCache {
            bins: [Bin {
                head: ptr::null_mut(),
                len: 0,
            }; NUM_CLASSES],
            cached_bytes: 0,
            tid: 0,
            foreign_bytes: 0,
            virgin: [0; NUM_CLASSES],
            mbins: [Bin {
                head: ptr::null_mut(),
                len: 0,
            }; NUM_MEDIUM],
            mvirgin: [0; NUM_MEDIUM],
            #[cfg(all(unix, feature = "std"))]
            bigbins: [Bin {
                head: ptr::null_mut(),
                len: 0,
            }; NUM_BIG],
            #[cfg(all(unix, feature = "std"))]
            bvirgin: [0; NUM_BIG],
            exit_armed: false,
            large: [(ptr::null_mut(), 0); LARGE_STASH_SLOTS],
            large_len: 0,
            large_bytes: 0,
            #[cfg(feature = "telemetry")]
            pending: Pending::new(),
        }
    }

    /// Publish accumulated telemetry deltas to the global atomics.
    #[cfg(feature = "telemetry")]
    fn publish(&mut self) {
        use core::sync::atomic::Ordering::Relaxed;
        let t = &crate::heap::TELEMETRY;
        if self.pending.allocs != 0 {
            t.total_allocs.fetch_add(self.pending.allocs, Relaxed);
        }
        if self.pending.frees != 0 {
            t.total_frees.fetch_add(self.pending.frees, Relaxed);
        }
        if self.pending.bytes_in != 0 {
            t.bytes_in.fetch_add(self.pending.bytes_in, Relaxed);
        }
        if self.pending.bytes_out != 0 {
            t.bytes_out.fetch_add(self.pending.bytes_out, Relaxed);
        }
        for (class, n) in self.pending.per_class.iter_mut().enumerate() {
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

    #[inline]
    #[cfg(feature = "telemetry")]
    fn note_alloc_medium(&mut self, mclass: usize) {
        self.pending.ops += 1;
        self.pending.allocs += 1;
        self.pending.bytes_in += MEDIUM_CLASSES[mclass] as u64;
        self.pending.per_class[NUM_CLASSES + mclass] += 1;
        if self.pending.ops >= FLUSH_OPS {
            self.publish();
        }
    }

    #[inline]
    #[cfg(feature = "telemetry")]
    fn note_free_medium(&mut self, mclass: usize) {
        self.pending.ops += 1;
        self.pending.frees += 1;
        self.pending.bytes_out += MEDIUM_CLASSES[mclass] as u64;
        self.pending.per_class[NUM_CLASSES + mclass] += 1;
        if self.pending.ops >= FLUSH_OPS {
            self.publish();
        }
    }

    #[inline]
    #[cfg(all(feature = "telemetry", unix, feature = "std"))]
    fn note_alloc_big(&mut self, bclass: usize) {
        self.pending.ops += 1;
        self.pending.allocs += 1;
        self.pending.bytes_in += BIG_CLASSES[bclass] as u64;
        self.pending.per_class[NUM_CLASSES + NUM_MEDIUM + bclass] += 1;
        if self.pending.ops >= FLUSH_OPS {
            self.publish();
        }
    }

    #[inline]
    #[cfg(all(feature = "telemetry", unix, feature = "std"))]
    fn note_free_big(&mut self, bclass: usize) {
        self.pending.ops += 1;
        self.pending.frees += 1;
        self.pending.bytes_out += BIG_CLASSES[bclass] as u64;
        self.pending.per_class[NUM_CLASSES + NUM_MEDIUM + bclass] += 1;
        if self.pending.ops >= FLUSH_OPS {
            self.publish();
        }
    }

    /// Arm the OS thread-exit flush once per thread. Called on slow paths
    /// only (refill/trim/large) — fast paths stay untouched. Threads whose
    /// caches never leave the fast path hold nothing worth reclaiming.
    #[inline]
    pub(crate) fn arm_exit_hook(&mut self) {
        if !self.exit_armed {
            self.exit_armed = true;
            crate::thread_exit::ensure_hook();
        }
    }

    /// This thread's ownership id, assigned on first use (slow paths only).
    #[inline]
    fn tid(&mut self) -> u32 {
        if self.tid == 0 {
            // Never hand out 0 (means "unassigned"); wrap is vanishingly rare
            // and 0 is re-skipped below.
            let mut id = NEXT_TID.fetch_add(1, Ordering::Relaxed);
            if id == 0 {
                id = NEXT_TID.fetch_add(1, Ordering::Relaxed);
            }
            self.tid = id;
        }
        self.tid
    }

    /// Claim `page`/`span` for this thread on refill (owner heuristic for the
    /// drift cap). Benign race: last writer wins; correctness never reads
    /// `owner` except under the drift gate below.
    #[inline]
    fn claim_page(&mut self, page: *mut PageHeader) {
        let id = self.tid();
        // SAFETY: caller holds a live page pointer from a refill chain.
        unsafe {
            (*page).owner = id;
        }
    }

    #[inline]
    fn claim_span(&mut self, span: *mut SpanMaster) {
        let id = self.tid();
        unsafe {
            (*span).owner = id;
        }
    }

    /// True when this cache is under enough pressure that foreign frees
    /// should be counted for a batched shed (see [`DRIFT_GATE_DIV`]).
    #[inline]
    fn drift_gate_open(&self) -> bool {
        self.cached_bytes > thread_cache_budget() / DRIFT_GATE_DIV
    }

    /// True when either the total budget or the foreign-byte shed limit is
    /// exceeded — caller should `trim` (batched, page-grouped) and clear
    /// `foreign_bytes`.
    #[inline]
    fn should_shed(&self) -> bool {
        self.cached_bytes > thread_cache_budget()
            || self.foreign_bytes >= thread_cache_budget() / FOREIGN_SHED_DIV
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
        self.arm_exit_hook();
        // Under aggregate pressure, shed some cache before asking for more.
        if self.cached_bytes > thread_cache_budget() / 2 {
            self.trim();
        }
        let (chain, count, virgin) = crate::heap::HEAP.take_blocks(class);
        if chain.is_null() {
            return (ptr::null_mut(), false);
        }
        // Claim every page in the batch for this thread (drift-cap owner).
        {
            let mut b = chain;
            for _ in 0..count {
                if b.is_null() {
                    break;
                }
                self.claim_page(PageHeader::of(b));
                b = *b.cast::<*mut u8>();
            }
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

    /// Free into the thread cache with a class already known from the
    /// pointer's header (`free()` path) or from layout size (fast path).
    /// Class is a parameter so layout-routed frees never load a header.
    pub(crate) unsafe fn dealloc(&mut self, p: *mut u8, class: usize) {
        #[cfg(debug_assertions)]
        debug_validate_free(p);

        // Drift cap (ROADMAP P1 step 4): under cache pressure, a free of a
        // block whose page/span was claimed by another thread is counted as
        // foreign. Sheds run through `trim` (chunked + page-grouped) once the
        // foreign or total budget is hit — never a lock-per-free
        // `release_blocks`, which serialized prodcons and thrashed the arena.
        // The ownership load only runs when the gate is already open.
        let mut foreign = false;
        if self.drift_gate_open() {
            let page = PageHeader::of(p);
            let owner = (*page).owner;
            foreign = owner != 0 && owner != self.tid();
        }

        let bin = &mut self.bins[class];
        push_block(&mut bin.head, p);
        bin.len += 1;
        self.cached_bytes += CLASSES[class];
        if foreign {
            self.foreign_bytes += CLASSES[class];
        }
        #[cfg(feature = "telemetry")]
        self.note_free(class);
        if self.should_shed() {
            self.trim();
            self.foreign_bytes = 0;
        }
    }

    /// Medium fast-path allocation. Returns null only on OS exhaustion.
    pub(crate) unsafe fn alloc_medium(&mut self, mclass: usize) -> *mut u8 {
        let bin = &mut self.mbins[mclass];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= MEDIUM_CLASSES[mclass];
            if below < self.mvirgin[mclass] {
                self.mvirgin[mclass] -= 1;
            }
            #[cfg(feature = "telemetry")]
            self.note_alloc_medium(mclass);
            return p;
        }
        let (p, _) = self.mrefill(mclass);
        #[cfg(feature = "telemetry")]
        if !p.is_null() {
            self.note_alloc_medium(mclass);
        }
        p
    }

    /// Medium allocation reporting OS-zero status for `alloc_zeroed`.
    pub(crate) unsafe fn alloc_medium_zeroed(&mut self, mclass: usize) -> (*mut u8, bool) {
        let bin = &mut self.mbins[mclass];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= MEDIUM_CLASSES[mclass];
            let zeroed = below < self.mvirgin[mclass];
            if zeroed {
                self.mvirgin[mclass] -= 1;
            }
            #[cfg(feature = "telemetry")]
            self.note_alloc_medium(mclass);
            return (p, zeroed);
        }
        let r = self.mrefill(mclass);
        #[cfg(feature = "telemetry")]
        if !r.0.is_null() {
            self.note_alloc_medium(mclass);
        }
        r
    }

    /// Medium slow path: pull one span's worth of blocks from the heap.
    #[inline]
    unsafe fn mrefill(&mut self, mclass: usize) -> (*mut u8, bool) {
        self.arm_exit_hook();
        if self.cached_bytes > thread_cache_budget() / 2 {
            self.trim();
        }
        let (chain, count, virgin) = MEDIUM_HEAP.take_blocks(mclass);
        if chain.is_null() {
            return (ptr::null_mut(), false);
        }
        // Claim every span in the batch for this thread (drift-cap owner).
        {
            let mut b = chain;
            for _ in 0..count {
                if b.is_null() {
                    break;
                }
                let span = SpanMaster::of(b);
                if !span.is_null() {
                    self.claim_span(span);
                }
                b = *b.cast::<*mut u8>();
            }
        }
        let first = chain;
        let rest = *first.cast::<*mut u8>();
        let bin = &mut self.mbins[mclass];
        bin.head = rest;
        bin.len += count - 1;
        self.cached_bytes += MEDIUM_CLASSES[mclass] * (count - 1) as usize;
        self.mvirgin[mclass] = if virgin { count - 1 } else { 0 };
        (first, virgin)
    }

    /// Medium free with `mclass` already known (layout-routed fast path;
    /// `free()` callers pass `(*span).mclass` after their own header load).
    /// No span load here — the span is only needed on the cold no-TLS
    /// fallback, which re-derives it from the pointer.
    pub(crate) unsafe fn dealloc_medium(&mut self, p: *mut u8, mclass: usize) {
        #[cfg(debug_assertions)]
        {
            let span = SpanMaster::of(p);
            debug_assert!(!span.is_null() && (*span).contains(p));
            debug_assert_eq!((*span).mclass as usize, mclass);
            debug_validate_free_medium(p, span);
        }

        // Drift cap: same pressure + batched-shed discipline as small frees
        // (see `dealloc`).
        let mut foreign = false;
        if self.drift_gate_open() {
            let span = SpanMaster::of(p);
            if !span.is_null() {
                let owner = (*span).owner;
                foreign = owner != 0 && owner != self.tid();
            }
        }

        let bin = &mut self.mbins[mclass];
        push_block(&mut bin.head, p);
        bin.len += 1;
        self.cached_bytes += MEDIUM_CLASSES[mclass];
        if foreign {
            self.foreign_bytes += MEDIUM_CLASSES[mclass];
        }
        #[cfg(feature = "telemetry")]
        self.note_free_medium(mclass);
        if self.should_shed() {
            self.trim();
            self.foreign_bytes = 0;
        }
    }

    /// Big fast-path allocation. Returns null when the arena is unavailable
    /// (callers fall back to the large path) or on OS exhaustion.
    #[cfg(all(unix, feature = "std"))]
    pub(crate) unsafe fn alloc_big(&mut self, bclass: usize) -> *mut u8 {
        let bin = &mut self.bigbins[bclass];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= BIG_CLASSES[bclass];
            if below < self.bvirgin[bclass] {
                self.bvirgin[bclass] -= 1;
            }
            #[cfg(all(feature = "telemetry", unix, feature = "std"))]
            self.note_alloc_big(bclass);
            return p;
        }
        let (p, _) = self.bigrefill(bclass);
        #[cfg(all(feature = "telemetry", unix, feature = "std"))]
        if !p.is_null() {
            self.note_alloc_big(bclass);
        }
        p
    }

    /// Big allocation reporting OS-zero status for `alloc_zeroed`.
    #[cfg(all(unix, feature = "std"))]
    pub(crate) unsafe fn alloc_big_zeroed(&mut self, bclass: usize) -> (*mut u8, bool) {
        let bin = &mut self.bigbins[bclass];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= BIG_CLASSES[bclass];
            let zeroed = below < self.bvirgin[bclass];
            if zeroed {
                self.bvirgin[bclass] -= 1;
            }
            #[cfg(all(feature = "telemetry", unix, feature = "std"))]
            self.note_alloc_big(bclass);
            return (p, zeroed);
        }
        let r = self.bigrefill(bclass);
        #[cfg(all(feature = "telemetry", unix, feature = "std"))]
        if !r.0.is_null() {
            self.note_alloc_big(bclass);
        }
        r
    }

    /// Big slow path: pull one span's worth of blocks from the heap.
    /// Null means arena-unavailable or exhausted (caller routes large).
    #[cfg(all(unix, feature = "std"))]
    #[inline]
    unsafe fn bigrefill(&mut self, bclass: usize) -> (*mut u8, bool) {
        self.arm_exit_hook();
        if self.cached_bytes > thread_cache_budget() / 2 {
            self.trim();
        }
        let (chain, count, virgin) = BIG_HEAP.take_blocks(bclass);
        if chain.is_null() {
            return (ptr::null_mut(), false);
        }
        // Claim every span in the batch for this thread (drift-cap owner).
        {
            let mut b = chain;
            for _ in 0..count {
                if b.is_null() {
                    break;
                }
                let span = crate::arena::big_table_get(b);
                if !span.is_null() {
                    let id = self.tid();
                    (*span).owner = id;
                }
                b = *b.cast::<*mut u8>();
            }
        }
        let first = chain;
        let rest = *first.cast::<*mut u8>();
        let bin = &mut self.bigbins[bclass];
        bin.head = rest;
        bin.len += count - 1;
        self.cached_bytes += BIG_CLASSES[bclass] * (count - 1) as usize;
        self.bvirgin[bclass] = if virgin { count - 1 } else { 0 };
        (first, virgin)
    }

    #[cfg(all(unix, feature = "std"))]
    pub(crate) unsafe fn dealloc_big(&mut self, p: *mut u8, span: *mut BigMaster) {
        #[cfg(debug_assertions)]
        debug_validate_free_big(p, span);

        let bclass = (*span).bclass as usize;

        // Drift cap: same pressure + batched-shed discipline as small frees
        // (see `dealloc`).
        let mut foreign = false;
        if self.drift_gate_open() {
            let owner = (*span).owner;
            foreign = owner != 0 && owner != self.tid();
        }

        let bin = &mut self.bigbins[bclass];
        push_block(&mut bin.head, p);
        bin.len += 1;
        self.cached_bytes += BIG_CLASSES[bclass];
        if foreign {
            self.foreign_bytes += BIG_CLASSES[bclass];
        }
        #[cfg(all(feature = "telemetry", unix, feature = "std"))]
        self.note_free_big(bclass);
        if self.should_shed() {
            self.trim();
            self.foreign_bytes = 0;
        }
    }

    /// Take a stashed large region with at least `pages_needed` pages.
    /// Exact-size matches win over merely-fitting ones (see
    /// `LargeRegionCache::take_fit` for why); best-fit otherwise. Lock-free:
    /// owning thread only.
    pub(crate) fn take_large_stash(&mut self, pages_needed: u32) -> Option<(*mut u8, u32)> {
        for i in 0..self.large_len as usize {
            if self.large[i].1 == pages_needed {
                let last = self.large_len as usize - 1;
                let entry = self.large[i];
                self.large[i] = self.large[last];
                self.large[last] = (ptr::null_mut(), 0);
                self.large_len = last as u32;
                self.large_bytes -= entry.1 as usize * crate::page::PAGE_SIZE;
                return Some(entry);
            }
        }
        let mut best: Option<usize> = None;
        for i in 0..self.large_len as usize {
            let (_, pages) = self.large[i];
            if pages >= pages_needed && best.map_or(true, |b| pages < self.large[b].1) {
                best = Some(i);
            }
        }
        best.map(|i| {
            let last = self.large_len as usize - 1;
            let entry = self.large[i];
            self.large[i] = self.large[last];
            self.large[last] = (ptr::null_mut(), 0);
            self.large_len = last as u32;
            self.large_bytes -= entry.1 as usize * crate::page::PAGE_SIZE;
            entry
        })
    }

    /// Stash a freed large region for lock-free reuse. Returns false when the
    /// stash is full or over budget (caller falls back to the global shards).
    pub(crate) fn push_large_stash(&mut self, base: *mut u8, pages: u32) -> bool {
        let bytes = pages as usize * crate::page::PAGE_SIZE;
        if self.large_len as usize >= LARGE_STASH_SLOTS
            || self.large_bytes + bytes > LARGE_STASH_CAP_BYTES
        {
            return false;
        }
        let idx = self.large_len as usize;
        self.large[idx] = (base, pages);
        self.large_len += 1;
        self.large_bytes += bytes;
        true
    }

    /// Bring total cached bytes under half the budget by repeatedly halving
    /// the largest bin. Fixed-size passes over small + medium bins; no allocation.
    unsafe fn trim(&mut self) {
        self.arm_exit_hook();
        let target = thread_cache_budget() / 2;
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
            if best != usize::MAX {
                let len = self.bins[best].len;
                self.flush_bin(best, len / 2);
                continue;
            }
            // Small bins have nothing worth trimming; shed the largest
            // medium bin instead (medium blocks are huge, so any non-empty
            // medium bin outranks the small-bin threshold logic).
            let mut mbest = usize::MAX;
            let mut mbest_bytes = 0usize;
            for (mclass, size) in MEDIUM_CLASSES.iter().enumerate() {
                let bin_bytes = self.mbins[mclass].len as usize * size;
                if self.mbins[mclass].len > 0 && bin_bytes > mbest_bytes {
                    mbest_bytes = bin_bytes;
                    mbest = mclass;
                }
            }
            // Big bins outrank medium the same way (arena targets only).
            #[cfg(all(unix, feature = "std"))]
            let mut bbest = usize::MAX;
            #[cfg(all(unix, feature = "std"))]
            let mut bbest_bytes = 0usize;
            #[cfg(all(unix, feature = "std"))]
            for (bclass, size) in BIG_CLASSES.iter().enumerate() {
                let bin_bytes = self.bigbins[bclass].len as usize * size;
                if self.bigbins[bclass].len > 0 && bin_bytes > bbest_bytes {
                    bbest_bytes = bin_bytes;
                    bbest = bclass;
                }
            }
            #[cfg(all(unix, feature = "std"))]
            if bbest != usize::MAX && bbest_bytes > mbest_bytes {
                let len = self.bigbins[bbest].len;
                self.flush_bbin(bbest, len / 2);
                continue;
            }
            if mbest == usize::MAX {
                // Nothing trimmable left: stop WITHOUT touching cached_bytes.
                // It must stay exactly equal to retained bytes (every push,
                // pop, refill, and flush adjusts it symmetrically); fudging
                // it down here used to lag actual retention until a later
                // pop drove it below zero (debug underflow panic, release
                // wrap into an over-trim storm). A futile rescan per
                // over-budget dealloc is the honest price — O(classes), no
                // syscalls, and it stops the moment bins become trimmable.
                break;
            }
            let len = self.mbins[mbest].len;
            self.flush_mbin(mbest, len / 2);
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

    /// Shrink medium `mclass`'s bin down to `floor_blocks`, returning removed
    /// blocks to their owning spans grouped by master (one heap lock per span
    /// per chunk). Chunks are smaller than for small bins because medium
    /// blocks are huge and bins hold few of them.
    unsafe fn flush_mbin(&mut self, mclass: usize, floor_blocks: u32) {
        const MFLUSH_CHUNK: u32 = 256;
        const MAX_MFLUSH_GROUPS: usize = MFLUSH_CHUNK as usize + 4;
        let block_size = MEDIUM_CLASSES[mclass];
        let bin = &mut self.mbins[mclass];

        while bin.len > floor_blocks {
            let mut groups = [MGroup::EMPTY; MAX_MFLUSH_GROUPS];
            let mut ng = 0usize;
            let mut popped = 0u32;

            while bin.len > floor_blocks && popped < MFLUSH_CHUNK {
                let b = match pop_block(&mut bin.head) {
                    Some(b) => b,
                    None => break,
                };
                let below = bin.len - 1;
                bin.len = below;
                if below < self.mvirgin[mclass] {
                    self.mvirgin[mclass] -= 1;
                }
                popped += 1;
                self.cached_bytes = self.cached_bytes.saturating_sub(block_size);

                let master = SpanMaster::of(b);
                debug_assert!(!master.is_null());
                *b.cast::<*mut u8>() = ptr::null_mut();
                let mut slot = None;
                for g in groups.iter_mut().take(ng) {
                    if g.master == master {
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
                        // Groups buffer always has room: at most MFLUSH_CHUNK
                        // blocks popped per chunk, one group each worst case.
                        debug_assert!(ng < MAX_MFLUSH_GROUPS);
                        if ng >= MAX_MFLUSH_GROUPS {
                            // No room to group: release solo (tail == head).
                            crate::heap::MEDIUM_HEAP.release_blocks(mclass, master, b, b, 1);
                            continue;
                        }
                        groups[ng] = MGroup {
                            master,
                            head: b,
                            tail: b,
                            n: 1,
                        };
                        ng += 1;
                    }
                }
            }

            // One class lock for the whole chunk (not one per group); tails
            // are already tracked so release never re-walks the chains.
            let mut chunks = [ReleaseChunk {
                span: ptr::null_mut(),
                head: ptr::null_mut(),
                tail: ptr::null_mut(),
                n: 0,
            }; MAX_MFLUSH_GROUPS];
            let nch = ng.min(MAX_MFLUSH_GROUPS);
            for (i, g) in groups.iter().take(nch).enumerate() {
                chunks[i] = ReleaseChunk {
                    span: g.master,
                    head: g.head,
                    tail: g.tail,
                    n: g.n,
                };
            }
            crate::heap::MEDIUM_HEAP.release_many(mclass, &chunks[..nch]);
            if popped == 0 {
                break;
            }
        }
    }

    /// Shrink big `bclass`'s bin down to `floor_blocks`, returning removed
    /// blocks to their owning spans grouped by master (one heap lock per span
    /// per chunk). Owning spans come from the arena side table (data chunks
    /// carry no headers to mask).
    #[cfg(all(unix, feature = "std"))]
    unsafe fn flush_bbin(&mut self, bclass: usize, floor_blocks: u32) {
        const BFLUSH_CHUNK: u32 = 64;
        const MAX_BFLUSH_GROUPS: usize = BFLUSH_CHUNK as usize + 4;
        let block_size = BIG_CLASSES[bclass];
        let bin = &mut self.bigbins[bclass];

        while bin.len > floor_blocks {
            let mut groups = [BGroup::EMPTY; MAX_BFLUSH_GROUPS];
            let mut ng = 0usize;
            let mut popped = 0u32;

            while bin.len > floor_blocks && popped < BFLUSH_CHUNK {
                let b = match pop_block(&mut bin.head) {
                    Some(b) => b,
                    None => break,
                };
                let below = bin.len - 1;
                bin.len = below;
                if below < self.bvirgin[bclass] {
                    self.bvirgin[bclass] -= 1;
                }
                popped += 1;
                self.cached_bytes = self.cached_bytes.saturating_sub(block_size);

                let master = crate::arena::big_table_get(b);
                debug_assert!(!master.is_null() && (*master).contains(b));
                *b.cast::<*mut u8>() = ptr::null_mut();
                let mut slot = None;
                for g in groups.iter_mut().take(ng) {
                    if g.master == master {
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
                        // Groups buffer always has room: at most BFLUSH_CHUNK
                        // blocks popped per chunk, one group each worst case.
                        debug_assert!(ng < MAX_BFLUSH_GROUPS);
                        if ng >= MAX_BFLUSH_GROUPS {
                            // No room to group: release solo.
                            crate::heap::BIG_HEAP.release_blocks(master, b, 1);
                            continue;
                        }
                        groups[ng] = BGroup {
                            master,
                            head: b,
                            tail: b,
                            n: 1,
                        };
                        ng += 1;
                    }
                }
            }

            for g in groups.iter_mut().take(ng) {
                crate::heap::BIG_HEAP.release_blocks(g.master, g.head, g.n);
            }
            if popped == 0 {
                break;
            }
        }
    }

    /// Return all cached blocks (used at explicit shutdown/flush requests,
    /// and by the OS thread-exit hook).
    pub(crate) unsafe fn flush_all(&mut self) {
        for class in 0..NUM_CLASSES {
            if !self.bins[class].head.is_null() {
                self.flush_bin(class, 0);
            }
        }
        for mclass in 0..NUM_MEDIUM {
            if !self.mbins[mclass].head.is_null() {
                self.flush_mbin(mclass, 0);
            }
        }
        #[cfg(all(unix, feature = "std"))]
        for bclass in 0..NUM_BIG {
            if !self.bigbins[bclass].head.is_null() {
                self.flush_bbin(bclass, 0);
            }
        }
        self.cached_bytes = 0;
        self.foreign_bytes = 0;
        // Keep `tid` stable across flushes: it identifies this OS thread for
        // the drift-cap owner heuristic, not a cache generation.
        self.virgin = [0; NUM_CLASSES];
        self.mvirgin = [0; NUM_MEDIUM];
        #[cfg(all(unix, feature = "std"))]
        {
            self.bvirgin = [0; NUM_BIG];
        }
        // Stashed large regions are released directly (no global lock held
        // here beyond the caller's cache ownership) so an explicit flush
        // actually returns memory instead of shuffling it to shared shards.
        // Arena-owned slices park in arena holes; legacy ones truly unmap.
        for i in 0..self.large_len as usize {
            let (base, pages) = self.large[i];
            if !base.is_null() {
                let size = pages as usize * crate::page::PAGE_SIZE;
                crate::unmap_or_return(base, size);
                self.large[i] = (ptr::null_mut(), 0);
            }
        }
        self.large_len = 0;
        self.large_bytes = 0;
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

/// Debug-build validation for medium frees: `p` must sit block-aligned
/// inside a carved chunk of `span` (never on a header) and must not already
/// be on the span free list. Runs under the medium-heap lock.
#[cfg(debug_assertions)]
unsafe fn debug_validate_free_medium(p: *mut u8, span: *mut SpanMaster) {
    use crate::page::{PAGE_MASK, PAGE_SIZE, SPAN_MASTER_SIZE, SPAN_SUB_SIZE};
    if p.is_null() || span.is_null() {
        invalid("allox: medium dealloc of null");
    }
    let mclass = (*span).mclass as usize;
    let block_size = MEDIUM_CLASSES[mclass];
    let base = span as usize;
    let npages = (*span).npages as usize;
    if p as usize <= base || p as usize >= base + npages * PAGE_SIZE {
        invalid("allox: medium dealloc outside owning span");
    }
    // Block must lie inside its page-chunk, clear of headers.
    let rel = (p as usize - base) / PAGE_SIZE;
    let chunk = base + rel * PAGE_SIZE;
    let (start, end) = if rel == 0 {
        (chunk + SPAN_MASTER_SIZE, chunk + PAGE_SIZE)
    } else {
        // Must really be a sub-page of this span.
        if *(chunk as *const u64) != crate::page::SPAN_SUBMAGIC {
            invalid("allox: medium dealloc through corrupt sub-page");
        }
        (chunk + SPAN_SUB_SIZE, chunk + PAGE_SIZE)
    };
    if (p as usize) < start || (p as usize) + block_size > end {
        invalid("allox: medium dealloc of misaligned interior pointer");
    }
    if (p as usize - start) % block_size != 0 {
        invalid("allox: medium dealloc of misaligned interior pointer");
    }
    let _ = PAGE_MASK; // masking already done by SpanMaster::of
    let _guard = crate::heap::MEDIUM_HEAP.debug_lock_medium(mclass);
    let mut cur = (*span).free_head;
    let mut steps = (*span).free_count;
    while steps > 0 {
        if cur == p {
            invalid("allox: double free detected");
        }
        if cur.is_null() {
            break;
        }
        cur = *cur.cast::<*mut u8>();
        steps -= 1;
    }
}

/// Debug-build validation for big frees: `p` must sit block-aligned inside
/// the span's single contiguous carved area (past the 64 B master — data
/// chunks reserve nothing, so unlike medium spans there are no per-chunk
/// bounds, only the whole-span extent) and must not already be on the span
/// free list. Runs under the big-heap lock.
#[cfg(all(debug_assertions, unix, feature = "std"))]
unsafe fn debug_validate_free_big(p: *mut u8, span: *mut BigMaster) {
    use crate::page::BIG_MASTER_SIZE;
    if p.is_null() || span.is_null() {
        invalid("allox: big dealloc of null");
    }
    let bclass = (*span).bclass as usize;
    let block_size = BIG_CLASSES[bclass];
    let base = span as usize;
    let npages = (*span).npages as usize;
    if p as usize <= base || p as usize >= base + npages * crate::page::PAGE_SIZE {
        invalid("allox: big dealloc outside owning span");
    }
    // Contiguous carve: everything past the master header is blocks.
    let start = base + BIG_MASTER_SIZE;
    let end = base + npages * crate::page::PAGE_SIZE;
    if (p as usize) < start || (p as usize) + block_size > end {
        invalid("allox: big dealloc of misaligned interior pointer");
    }
    if (p as usize - start) % block_size != 0 {
        invalid("allox: big dealloc of misaligned interior pointer");
    }
    let _guard = crate::heap::BIG_HEAP.debug_lock_big(bclass);
    let mut cur = (*span).free_head;
    let mut steps = (*span).free_count;
    while steps > 0 {
        if cur == p {
            invalid("allox: double free detected");
        }
        if cur.is_null() {
            break;
        }
        cur = *cur.cast::<*mut u8>();
        steps -= 1;
    }
}

#[cfg(all(debug_assertions, feature = "std"))]
fn invalid(msg: &'static str) -> ! {
    eprintln!("{}", msg);
    std::process::abort()
}

#[cfg(all(debug_assertions, not(feature = "std")))]
fn invalid(msg: &'static str) -> ! {
    panic!("{}", msg)
}
