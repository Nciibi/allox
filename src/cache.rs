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
use crate::heap::{MEDIUM_HEAP, PageReleaseChunk, REFILL_BATCH, ReleaseChunk};
#[cfg(all(unix, feature = "std"))]
use crate::heap::BIG_HEAP;
use crate::page::{pop_block, push_block, PageHeader, SpanMaster};
#[cfg(all(unix, feature = "std"))]
use crate::page::BigMaster;
use core::mem::MaybeUninit;
use core::ptr;
#[cfg(all(feature = "std", any(unix, windows)))]
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
#[cfg(all(feature = "std", any(unix, windows)))]
use core::sync::atomic::{AtomicU8, AtomicUsize};

/// Base budget for small and other cached bytes before trimming starts.
/// Medium and big tiers receive a separate 2x allowance; the aggregate trim
/// target is bounded accordingly.
/// Overridable at startup via `allox::set_thread_cache_budget` (atomic read;
/// never from the environment, since environment access can allocate).
const DEFAULT_THREAD_CACHE_BUDGET: usize = 32 * 1024 * 1024;

/// Remote-free drift cap (ROADMAP P1 step 4): once a thread's cache exceeds
/// this fraction of the budget, frees of blocks whose page/span was claimed
/// by another thread are *counted* (`foreign_bytes`) so they can be shed in
/// batch via `trim` — never one `release_blocks` per free (that serialized
/// producer-consumer on the class lock and thrashed the arena). Same-thread
/// frees below the gate stay header-free (the Phase-0 free-path win).
///
/// `DIV = 1` (gate at full budget) was measured and **rejected**: mixed-all
/// stayed ~0.71× mimalloc (PMU's "72% of dealloc_medium" did not translate
/// to wall-clock) and prodcons regressed 32.7M → ~26.8M ops/s because
/// `foreign_bytes` never reached `budget / 8` before `cached_bytes > budget`
/// already forced `should_shed`, killing the early foreign shed.
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

#[inline]
fn tier_cache_budget() -> usize {
    thread_cache_budget().saturating_mul(2)
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

/// One span's share of a medium flush chunk (mirrors [`PageReleaseChunk`]).
#[derive(Clone, Copy)]
struct MGroup {
    master: *mut SpanMaster,
    head: *mut u8,
    tail: *mut u8,
    n: u32,
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

#[cfg(all(feature = "std", any(unix, windows)))]
const RETIRED_SLOT_COUNT: usize = 8;
#[cfg(all(feature = "std", any(unix, windows)))]
const RETIRED_SLOT_BYTES: usize = 8 * 1024 * 1024;
#[cfg(all(feature = "std", any(unix, windows)))]
const RETIRED_EMPTY: u8 = 0;
#[cfg(all(feature = "std", any(unix, windows)))]
const RETIRED_WRITING: u8 = 1;
#[cfg(all(feature = "std", any(unix, windows)))]
const RETIRED_READY: u8 = 2;
#[cfg(all(feature = "std", any(unix, windows)))]
const RETIRED_TAKING: u8 = 3;

#[cfg(all(feature = "std", any(unix, windows)))]
struct RetiredSlot {
    state: AtomicU8,
    cache: UnsafeCell<MaybeUninit<ThreadCache>>,
}

#[cfg(all(feature = "std", any(unix, windows)))]
unsafe impl Sync for RetiredSlot {}

#[cfg(all(feature = "std", any(unix, windows)))]
impl RetiredSlot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(RETIRED_EMPTY),
            cache: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

#[cfg(all(feature = "std", any(unix, windows)))]
static RETIRED_SLOTS: [RetiredSlot; RETIRED_SLOT_COUNT] =
    [const { RetiredSlot::new() }; RETIRED_SLOT_COUNT];

#[cfg(all(feature = "std", any(unix, windows)))]
static RETIRED_READY_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct Bin {
    head: *mut u8,
    len: u32,
}

#[derive(Clone, Copy)]
struct ActiveMedium {
    span: *mut SpanMaster,
    base: usize,
    end: usize,
    head: *mut u8,
    len: u32,
    virgin: u32,
}

impl ActiveMedium {
    const fn empty() -> Self {
        Self {
            span: ptr::null_mut(),
            base: 0,
            end: 0,
            head: ptr::null_mut(),
            len: 0,
            virgin: 0,
        }
    }
}

#[cfg(all(unix, feature = "std"))]
#[derive(Clone, Copy)]
struct ActiveBig {
    span: *mut BigMaster,
    base: usize,
    end: usize,
    head: *mut u8,
    len: u32,
    virgin: u32,
}

#[cfg(all(unix, feature = "std"))]
impl ActiveBig {
    const fn empty() -> Self {
        Self {
            span: ptr::null_mut(),
            base: 0,
            end: 0,
            head: ptr::null_mut(),
            len: 0,
            virgin: 0,
        }
    }
}

pub(crate) struct ThreadCache {
    bins: [Bin; NUM_CLASSES],
    cached_bytes: usize,
    tier_cached_bytes: usize,
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
    mactive: [ActiveMedium; NUM_MEDIUM],
    /// Big bins (whole spans past the chunk cap), same discipline. Arena
    /// targets only (big spans don't exist elsewhere).
    #[cfg(all(unix, feature = "std"))]
    bigbins: [Bin; NUM_BIG],
    #[cfg(all(unix, feature = "std"))]
    bvirgin: [u32; NUM_BIG],
    #[cfg(all(unix, feature = "std"))]
    bactive: [ActiveBig; NUM_BIG],
    /// Whether this thread armed the OS thread-exit flush. Set once after
    /// the hook is installed; fast paths only check the flag.
    exit_armed: bool,
    retired_reclaimed: bool,
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
            tier_cached_bytes: 0,
            tid: 0,
            foreign_bytes: 0,
            virgin: [0; NUM_CLASSES],
            mbins: [Bin {
                head: ptr::null_mut(),
                len: 0,
            }; NUM_MEDIUM],
            mvirgin: [0; NUM_MEDIUM],
            mactive: [ActiveMedium::empty(); NUM_MEDIUM],
            #[cfg(all(unix, feature = "std"))]
            bigbins: [Bin {
                head: ptr::null_mut(),
                len: 0,
            }; NUM_BIG],
            #[cfg(all(unix, feature = "std"))]
            bvirgin: [0; NUM_BIG],
            #[cfg(all(unix, feature = "std"))]
            bactive: [ActiveBig::empty(); NUM_BIG],
            exit_armed: false,
            retired_reclaimed: false,
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
        if self.pending.ops >= FLUSH_OPS {
            self.publish();
        }
    }

    /// Arm the OS thread-exit flush once per thread. Called on slow paths and
    /// when a block enters the cache.
    #[inline]
    pub(crate) fn arm_exit_hook(&mut self) {
        if !self.exit_armed && crate::thread_exit::ensure_hook() {
            self.exit_armed = true;
        }
    }

    #[cfg(debug_assertions)]
    fn validate_cache_chains(&self) {
        for bin in self.bins.iter() {
            let mut cursor = bin.head;
            let mut steps = bin.len;
            while steps > 0 {
                assert!(!cursor.is_null());
                cursor = unsafe { *cursor.cast::<*mut u8>() };
                steps -= 1;
            }
            assert!(cursor.is_null());
        }
    }

    #[inline]
    fn reclaim_retired(&mut self) {
        #[cfg(all(feature = "std", any(unix, windows)))]
        {
            if self.retired_reclaimed {
                return;
            }
            if self.cached_bytes == 0 && self.large_len == 0 {
                if let Some(adopted) = take_one() {
                    let current_armed = self.exit_armed;
                    let current_tid = self.tid;
                    let old = core::mem::replace(self, adopted);
                    #[cfg(feature = "telemetry")]
                    let mut old = old;
                    #[cfg(feature = "telemetry")]
                    old.publish();
                    #[cfg(not(feature = "telemetry"))]
                    let _ = old;
                    #[cfg(debug_assertions)]
                    self.validate_cache_chains();
                    self.exit_armed = current_armed;
                    self.tid = current_tid;
                    self.retired_reclaimed = true;
                    return;
                }
            }
            if reclaim_one() {
                self.retired_reclaimed = true;
            }
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
            (*page).owner.store(id, Ordering::Relaxed);
        }
    }

    #[inline]
    fn claim_span(&mut self, span: *mut SpanMaster) {
        let id = self.tid();
        unsafe {
            (*span).owner.store(id, Ordering::Relaxed);
        }
    }

    #[cfg(debug_assertions)]
    fn actual_tier_bytes(&self) -> usize {
        let mut medium = 0usize;
        for (mclass, size) in MEDIUM_CLASSES.iter().enumerate() {
            medium += self.mactive[mclass].len as usize * size;
            medium += self.mbins[mclass].len as usize * size;
        }
        #[cfg(all(unix, feature = "std"))]
        let mut big = 0usize;
        #[cfg(all(unix, feature = "std"))]
        for (bclass, size) in BIG_CLASSES.iter().enumerate() {
            big += self.bactive[bclass].len as usize * size;
            big += self.bigbins[bclass].len as usize * size;
        }
        #[cfg(not(all(unix, feature = "std")))]
        let big = 0usize;
        medium + big
    }

    #[inline]
    fn small_cached_bytes(&self) -> usize {
        #[cfg(debug_assertions)]
        debug_assert_eq!(self.tier_cached_bytes, self.actual_tier_bytes());
        self.cached_bytes.saturating_sub(self.tier_cached_bytes)
    }

    /// True when this cache is under enough pressure that foreign frees
    /// should be counted for a batched shed (see [`DRIFT_GATE_DIV`]).
    #[inline]
    fn drift_gate_open(&self) -> bool {
        if self.tier_cached_bytes == 0 {
            return self.cached_bytes > thread_cache_budget() / DRIFT_GATE_DIV;
        }
        self.small_cached_bytes() > thread_cache_budget() / DRIFT_GATE_DIV
            || self.tier_cached_bytes > tier_cache_budget() / DRIFT_GATE_DIV
    }

    #[inline]
    fn drift_gate_open_small(&self) -> bool {
        self.cached_bytes > thread_cache_budget() / DRIFT_GATE_DIV
    }

    #[inline]
    fn should_shed_small(&self) -> bool {
        self.cached_bytes > thread_cache_budget()
            || self.foreign_bytes >= thread_cache_budget() / FOREIGN_SHED_DIV
    }

    /// True when either the total budget or the foreign-byte shed limit is
    /// exceeded — caller should `trim` (batched, page-grouped) and clear
    /// `foreign_bytes`.
    #[inline]
    fn should_shed(&self) -> bool {
        if self.tier_cached_bytes == 0 {
            return self.cached_bytes > thread_cache_budget()
                || self.foreign_bytes >= thread_cache_budget() / FOREIGN_SHED_DIV;
        }
        self.small_cached_bytes() > thread_cache_budget()
            || self.tier_cached_bytes > tier_cache_budget()
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
        self.reclaim_retired();
        // Under aggregate pressure, shed some cache before asking for more.
        if self.small_cached_bytes() > thread_cache_budget() / 2 {
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
        self.arm_exit_hook();
        #[cfg(debug_assertions)]
        debug_validate_free(p);

        // Drift cap (ROADMAP P1 step 4): under cache pressure, a free of a
        // block whose page/span was claimed by another thread is counted as
        // foreign. Sheds run through `trim` (chunked + page-grouped) once the
        // foreign or total budget is hit — never a lock-per-free
        // `release_blocks`, which serialized prodcons and thrashed the arena.
        // The ownership load only runs when the gate is already open.
        let mut foreign = false;
        if self.drift_gate_open_small() {
            let page = PageHeader::of(p);
            let owner = (*page).owner.load(Ordering::Relaxed);
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
        if self.should_shed_small() {
            self.shed();
        }
    }

    #[inline]
    unsafe fn active_medium_alloc(&mut self, mclass: usize) -> (*mut u8, bool) {
        let block_size = MEDIUM_CLASSES[mclass];
        let (p, zeroed) = {
            let active = &mut self.mactive[mclass];
            let p = match pop_block(&mut active.head) {
                Some(p) => p,
                None => return (ptr::null_mut(), false),
            };
            let below = active.len - 1;
            active.len = below;
            let zeroed = below < active.virgin;
            if zeroed {
                active.virgin -= 1;
            }
            (p, zeroed)
        };
        self.cached_bytes -= block_size;
        self.tier_cached_bytes = self.tier_cached_bytes.saturating_sub(block_size);
        (p, zeroed)
    }

    #[inline]
    fn active_medium_contains(&self, p: *mut u8, mclass: usize) -> bool {
        let active = &self.mactive[mclass];
        !active.span.is_null()
            && (p as usize) >= active.base
            && (p as usize) < active.end
    }

    #[inline]
    unsafe fn active_medium_dealloc(&mut self, p: *mut u8, mclass: usize) {
        let active = &mut self.mactive[mclass];
        push_block(&mut active.head, p);
        active.len += 1;
    }

    /// Medium fast-path allocation. Returns null only on OS exhaustion.
    pub(crate) unsafe fn alloc_medium(&mut self, mclass: usize) -> *mut u8 {
        let (p, _) = self.active_medium_alloc(mclass);
        if !p.is_null() {
            #[cfg(feature = "telemetry")]
            self.note_alloc_medium(mclass);
            return p;
        }
        let bin = &mut self.mbins[mclass];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= MEDIUM_CLASSES[mclass];
            self.tier_cached_bytes = self
                .tier_cached_bytes
                .saturating_sub(MEDIUM_CLASSES[mclass]);
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
        let (p, zeroed) = self.active_medium_alloc(mclass);
        if !p.is_null() {
            #[cfg(feature = "telemetry")]
            self.note_alloc_medium(mclass);
            return (p, zeroed);
        }
        let bin = &mut self.mbins[mclass];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= MEDIUM_CLASSES[mclass];
            self.tier_cached_bytes = self
                .tier_cached_bytes
                .saturating_sub(MEDIUM_CLASSES[mclass]);
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
        self.reclaim_retired();
        if self.small_cached_bytes() > thread_cache_budget() / 2 {
            self.trim();
        }
        let (chain, count, virgin) = MEDIUM_HEAP.take_blocks(mclass);
        if chain.is_null() {
            return (ptr::null_mut(), false);
        }
        let mut source: *mut SpanMaster = ptr::null_mut();
        let mut single_span = true;
        {
            let mut b = chain;
            let mut claimed = ptr::null_mut();
            for _ in 0..count {
                if b.is_null() {
                    break;
                }
                let span = SpanMaster::of(b);
                if span.is_null() {
                    single_span = false;
                } else {
                    if source.is_null() {
                        source = span;
                    } else if source != span {
                        single_span = false;
                    }
                    if span != claimed {
                        self.claim_span(span);
                        claimed = span;
                    }
                }
                b = *b.cast::<*mut u8>();
            }
        }
        let first = chain;
        let rest = *first.cast::<*mut u8>();
        *first.cast::<*mut u8>() = ptr::null_mut();
        if !single_span {
            let bin = &mut self.mbins[mclass];
            debug_assert!(bin.head.is_null());
            bin.head = rest;
            bin.len = count - 1;
            self.cached_bytes += MEDIUM_CLASSES[mclass] * (count - 1) as usize;
            self.tier_cached_bytes += MEDIUM_CLASSES[mclass] * (count - 1) as usize;
            self.mvirgin[mclass] = if virgin { count - 1 } else { 0 };
            return (first, virgin);
        }
        let span = source;
        debug_assert!(!span.is_null());
        let active = &mut self.mactive[mclass];
        debug_assert!(active.head.is_null());
        active.span = span;
        active.base = span as usize;
        active.end = active.base + (*span).mapped_bytes();
        active.head = rest;
        active.len = count - 1;
        active.virgin = if virgin { count - 1 } else { 0 };
        self.cached_bytes += MEDIUM_CLASSES[mclass] * (count - 1) as usize;
        self.tier_cached_bytes += MEDIUM_CLASSES[mclass] * (count - 1) as usize;
        self.mvirgin[mclass] = 0;
        (first, virgin)
    }

    /// Medium free with `mclass` already known (layout-routed fast path;
    /// `free()` callers pass `(*span).mclass` after their own header load).
    /// No span load here — the span is only needed on the cold no-TLS
    /// fallback, which re-derives it from the pointer.
    pub(crate) unsafe fn dealloc_medium(&mut self, p: *mut u8, mclass: usize) {
        self.arm_exit_hook();
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
                let owner = (*span).owner.load(Ordering::Relaxed);
                foreign = owner != 0 && owner != self.tid();
            }
        }

        if self.active_medium_contains(p, mclass) {
            self.active_medium_dealloc(p, mclass);
        } else {
            let bin = &mut self.mbins[mclass];
            push_block(&mut bin.head, p);
            bin.len += 1;
        }
        self.cached_bytes += MEDIUM_CLASSES[mclass];
        self.tier_cached_bytes += MEDIUM_CLASSES[mclass];
        if foreign {
            self.foreign_bytes += MEDIUM_CLASSES[mclass];
        }
        #[cfg(feature = "telemetry")]
        self.note_free_medium(mclass);
        if self.should_shed() {
            self.shed();
        }
    }

    #[cfg(all(unix, feature = "std"))]
    #[inline]
    unsafe fn active_big_alloc(&mut self, bclass: usize) -> (*mut u8, bool) {
        let block_size = BIG_CLASSES[bclass];
        let (p, zeroed) = {
            let active = &mut self.bactive[bclass];
            let p = match pop_block(&mut active.head) {
                Some(p) => p,
                None => {
                    *active = ActiveBig::empty();
                    return (ptr::null_mut(), false);
                }
            };
            let below = active.len - 1;
            active.len = below;
            let zeroed = below < active.virgin;
            if zeroed {
                active.virgin -= 1;
            }
            (p, zeroed)
        };
        self.cached_bytes -= block_size;
        self.tier_cached_bytes = self.tier_cached_bytes.saturating_sub(block_size);
        (p, zeroed)
    }

    #[cfg(all(unix, feature = "std"))]
    #[inline]
    fn active_big_contains(&self, p: *mut u8, bclass: usize, span: *mut BigMaster) -> bool {
        let active = &self.bactive[bclass];
        active.span == span
            && !active.span.is_null()
            && (p as usize) >= active.base
            && (p as usize) < active.end
    }

    #[cfg(all(unix, feature = "std"))]
    #[inline]
    unsafe fn active_big_dealloc(&mut self, p: *mut u8, bclass: usize) {
        let active = &mut self.bactive[bclass];
        push_block(&mut active.head, p);
        active.len += 1;
    }

    /// Big fast-path allocation. Returns null when the arena is unavailable
    /// (callers fall back to the large path) or on OS exhaustion.
    #[cfg(all(unix, feature = "std"))]
    pub(crate) unsafe fn alloc_big(&mut self, bclass: usize) -> *mut u8 {
        let (p, _) = self.active_big_alloc(bclass);
        if !p.is_null() {
            #[cfg(all(feature = "telemetry", unix, feature = "std"))]
            self.note_alloc_big(bclass);
            return p;
        }
        let bin = &mut self.bigbins[bclass];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= BIG_CLASSES[bclass];
            self.tier_cached_bytes = self.tier_cached_bytes.saturating_sub(BIG_CLASSES[bclass]);
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
        let (p, zeroed) = self.active_big_alloc(bclass);
        if !p.is_null() {
            #[cfg(all(feature = "telemetry", unix, feature = "std"))]
            self.note_alloc_big(bclass);
            return (p, zeroed);
        }
        let bin = &mut self.bigbins[bclass];
        if let Some(p) = pop_block(&mut bin.head) {
            let below = bin.len - 1;
            bin.len = below;
            self.cached_bytes -= BIG_CLASSES[bclass];
            self.tier_cached_bytes = self.tier_cached_bytes.saturating_sub(BIG_CLASSES[bclass]);
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
        self.reclaim_retired();
        if self.small_cached_bytes() > thread_cache_budget() / 2 {
            self.trim();
        }
        if self.tier_cached_bytes > tier_cache_budget() {
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
                    (*span).owner.store(id, Ordering::Relaxed);
                }
                b = *b.cast::<*mut u8>();
            }
        }
        let first = chain;
        let source = crate::arena::big_table_get(first);
        let mut single = !source.is_null();
        let mut cursor = *first.cast::<*mut u8>();
        for _ in 1..count {
            if cursor.is_null() || crate::arena::big_table_get(cursor) != source {
                single = false;
                break;
            }
            cursor = *cursor.cast::<*mut u8>();
        }
        let rest = *first.cast::<*mut u8>();
        *first.cast::<*mut u8>() = ptr::null_mut();
        if single {
            let active = &mut self.bactive[bclass];
            debug_assert!(active.head.is_null());
            active.span = source;
            active.base = source as usize;
            active.end = source as usize + (*source).mapped_bytes();
            active.head = rest;
            active.len = count - 1;
            active.virgin = if virgin { count - 1 } else { 0 };
            self.cached_bytes += BIG_CLASSES[bclass] * (count - 1) as usize;
            self.tier_cached_bytes += BIG_CLASSES[bclass] * (count - 1) as usize;
            self.bvirgin[bclass] = 0;
        } else {
            let bin = &mut self.bigbins[bclass];
            bin.head = rest;
            bin.len += count - 1;
            self.cached_bytes += BIG_CLASSES[bclass] * (count - 1) as usize;
            self.tier_cached_bytes += BIG_CLASSES[bclass] * (count - 1) as usize;
            self.bvirgin[bclass] = if virgin { count - 1 } else { 0 };
        }
        (first, virgin)
    }

    #[cfg(all(unix, feature = "std"))]
    pub(crate) unsafe fn dealloc_big(&mut self, p: *mut u8, span: *mut BigMaster) {
        self.arm_exit_hook();
        #[cfg(debug_assertions)]
        debug_validate_free_big(p, span);

        let bclass = (*span).bclass as usize;

        // Drift cap: same pressure + batched-shed discipline as small frees
        // (see `dealloc`).
        let mut foreign = false;
        if self.drift_gate_open() {
            let owner = (*span).owner.load(Ordering::Relaxed);
            foreign = owner != 0 && owner != self.tid();
        }

        if self.active_big_contains(p, bclass, span) {
            self.active_big_dealloc(p, bclass);
            self.cached_bytes += BIG_CLASSES[bclass];
            self.tier_cached_bytes += BIG_CLASSES[bclass];
            if foreign {
                self.foreign_bytes += BIG_CLASSES[bclass];
            }
            #[cfg(all(feature = "telemetry", unix, feature = "std"))]
            self.note_free_big(bclass);
            if self.should_shed() {
                self.shed();
            }
            return;
        }

        let bin = &mut self.bigbins[bclass];
        push_block(&mut bin.head, p);
        bin.len += 1;
        self.cached_bytes += BIG_CLASSES[bclass];
        self.tier_cached_bytes += BIG_CLASSES[bclass];
        if foreign {
            self.foreign_bytes += BIG_CLASSES[bclass];
        }
        #[cfg(all(feature = "telemetry", unix, feature = "std"))]
        self.note_free_big(bclass);
        if self.should_shed() {
            self.shed();
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

    unsafe fn flush_active_medium(&mut self, mclass: usize) {
        let block_size = MEDIUM_CLASSES[mclass];
        let (span, head, len) = {
            let active = &self.mactive[mclass];
            (active.span, active.head, active.len)
        };
        if head.is_null() || len == 0 {
            return;
        }
        let mut tail = head;
        while !(*tail.cast::<*mut u8>()).is_null() {
            tail = *tail.cast::<*mut u8>();
        }
        self.cached_bytes = self.cached_bytes.saturating_sub(block_size * len as usize);
        self.tier_cached_bytes = self
            .tier_cached_bytes
            .saturating_sub(block_size * len as usize);
        {
            let active = &mut self.mactive[mclass];
            active.span = ptr::null_mut();
            active.base = 0;
            active.end = 0;
            active.head = ptr::null_mut();
            active.len = 0;
            active.virgin = 0;
        }
        crate::heap::MEDIUM_HEAP.release_blocks(mclass, span, head, tail, len);
    }

    unsafe fn shed(&mut self) {
        self.trim();
        self.foreign_bytes = 0;
    }

    /// Bring cached bytes under the tier-adjusted target by repeatedly halving
    /// the largest bin. Fixed-size passes over small, medium, and big bins; no allocation.
    unsafe fn trim(&mut self) {
        self.arm_exit_hook();
        let tier_allowance = self.tier_cached_bytes.min(tier_cache_budget());
        let target = thread_cache_budget() / 2 + tier_allowance;
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
            let mut abest = usize::MAX;
            let mut abest_bytes = 0usize;
            for (mclass, size) in MEDIUM_CLASSES.iter().enumerate() {
                let bin_bytes = self.mactive[mclass].len as usize * size;
                if self.mactive[mclass].len > 0 && bin_bytes > abest_bytes {
                    abest_bytes = bin_bytes;
                    abest = mclass;
                }
            }
            if abest != usize::MAX {
                self.flush_active_medium(abest);
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
            #[cfg(all(unix, feature = "std"))]
            let mut active_bbest = usize::MAX;
            #[cfg(all(unix, feature = "std"))]
            let mut active_bbest_bytes = 0usize;
            #[cfg(all(unix, feature = "std"))]
            for (bclass, size) in BIG_CLASSES.iter().enumerate() {
                let active_bytes = self.bactive[bclass].len as usize * size;
                if self.bactive[bclass].len > 0 && active_bytes > active_bbest_bytes {
                    active_bbest_bytes = active_bytes;
                    active_bbest = bclass;
                }
            }
            #[cfg(all(unix, feature = "std"))]
            if active_bbest != usize::MAX {
                self.flush_active_big(active_bbest);
                continue;
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

    unsafe fn flush_bin_exit(&mut self, class: usize) {
        let len = self.bins[class].len;
        if len == 0 {
            return;
        }
        if len > FLUSH_CHUNK {
            self.flush_bin(class, 0);
            return;
        }
        let block_size = CLASSES[class];
        let chain = self.bins[class].head;
        let mut groups: [MaybeUninit<PageReleaseChunk>; MAX_FLUSH_GROUPS] =
            unsafe { MaybeUninit::uninit().assume_init() };
        let mut ng = 0usize;
        let mut cursor = chain;
        for _ in 0..len {
            let next = *cursor.cast::<*mut u8>();
            let page = PageHeader::of(cursor);
            let mut slot = None;
            for i in 0..ng {
                let g = unsafe { groups[i].assume_init_ref() };
                if g.page == page {
                    slot = Some(i);
                    break;
                }
            }
            match slot {
                Some(i) => {
                    let g = unsafe { groups[i].assume_init_mut() };
                    g.n += 1;
                }
                None => {
                    groups[ng] = MaybeUninit::new(PageReleaseChunk {
                        page,
                        head: ptr::null_mut(),
                        tail: ptr::null_mut(),
                        n: 1,
                    });
                    ng += 1;
                }
            }
            cursor = next;
        }
        debug_assert!(cursor.is_null());
        self.cached_bytes = self
            .cached_bytes
            .saturating_sub(block_size * len as usize);
        self.bins[class].head = ptr::null_mut();
        self.bins[class].len = 0;
        self.virgin[class] = 0;
        let chunks = unsafe {
            core::slice::from_raw_parts_mut(
                groups.as_mut_ptr() as *mut PageReleaseChunk,
                ng,
            )
        };
        crate::heap::HEAP.release_bin_chunks(class, chain, len, chunks);
    }

    /// Shrink `class`'s bin down to `floor_blocks` blocks, returning removed
    /// blocks to their owning pages in chunked, grouped batches so each page
    /// needs only one lock acquisition per chunk.
    unsafe fn flush_bin(&mut self, class: usize, floor_blocks: u32) {
        let block_size = CLASSES[class];
        let bin = &mut self.bins[class];

        while bin.len > floor_blocks {
            // Only slots 0..ng are ever written; a full [PageReleaseChunk; N]
            // zeroed ~65 KiB of stack per call (PMU: ~35% of flush_bin).
            let mut groups: [MaybeUninit<PageReleaseChunk>; MAX_FLUSH_GROUPS] =
                unsafe { MaybeUninit::uninit().assume_init() };
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
                for i in 0..ng {
                    let g = unsafe { groups[i].assume_init_ref() };
                    if g.page == page {
                        slot = Some(i);
                        break;
                    }
                }
                match slot {
                    Some(i) => {
                        let g = unsafe { groups[i].assume_init_mut() };
                        *g.tail.cast::<*mut u8>() = b;
                        g.tail = b;
                        g.n += 1;
                    }
                    None => {
                        groups[ng] = MaybeUninit::new(PageReleaseChunk {
                            page,
                            head: b,
                            tail: b,
                            n: 1,
                        });
                        ng += 1;
                    }
                }
            }

            let chunks = unsafe {
                core::slice::from_raw_parts(groups.as_ptr() as *const PageReleaseChunk, ng)
            };
            crate::heap::HEAP.release_many(class, chunks);
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
            // Uninit: only 0..ng written (the old EMPTY array zeroed ~8 KiB
            // of stack per call; same pattern as flush_bin).
            let mut groups: [MaybeUninit<MGroup>; MAX_MFLUSH_GROUPS] =
                unsafe { MaybeUninit::uninit().assume_init() };
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
                self.tier_cached_bytes = self.tier_cached_bytes.saturating_sub(block_size);

                let master = SpanMaster::of(b);
                debug_assert!(!master.is_null());
                *b.cast::<*mut u8>() = ptr::null_mut();
                let mut slot = None;
                for i in 0..ng {
                    let g = unsafe { groups[i].assume_init_ref() };
                    if g.master == master {
                        slot = Some(i);
                        break;
                    }
                }
                match slot {
                    Some(i) => {
                        let g = unsafe { groups[i].assume_init_mut() };
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
                        groups[ng] = MaybeUninit::new(MGroup {
                            master,
                            head: b,
                            tail: b,
                            n: 1,
                        });
                        ng += 1;
                    }
                }
            }

            // One class lock for the whole chunk (not one per group); tails
            // are already tracked so release never re-walks the chains.
            let mut chunks: [MaybeUninit<ReleaseChunk>; MAX_MFLUSH_GROUPS] =
                unsafe { MaybeUninit::uninit().assume_init() };
            let nch = ng.min(MAX_MFLUSH_GROUPS);
            for i in 0..nch {
                let g = unsafe { groups[i].assume_init() };
                chunks[i] = MaybeUninit::new(ReleaseChunk {
                    span: g.master,
                    head: g.head,
                    tail: g.tail,
                    n: g.n,
                });
            }
            let chunks_slice = unsafe {
                core::slice::from_raw_parts(chunks.as_ptr() as *const ReleaseChunk, nch)
            };
            crate::heap::MEDIUM_HEAP.release_many(mclass, chunks_slice);
            if popped == 0 {
                break;
            }
        }
    }

    #[cfg(all(unix, feature = "std"))]
    unsafe fn flush_active_big(&mut self, bclass: usize) {
        let block_size = BIG_CLASSES[bclass];
        let (span, head, len) = {
            let active = &self.bactive[bclass];
            (active.span, active.head, active.len)
        };
        if span.is_null() || head.is_null() || len == 0 {
            self.bactive[bclass] = ActiveBig::empty();
            return;
        }
        let mut tail = head;
        while !(*tail.cast::<*mut u8>()).is_null() {
            tail = *tail.cast::<*mut u8>();
        }
        self.cached_bytes = self.cached_bytes.saturating_sub(block_size * len as usize);
        self.tier_cached_bytes = self
            .tier_cached_bytes
            .saturating_sub(block_size * len as usize);
        self.bactive[bclass] = ActiveBig::empty();
        crate::heap::BIG_HEAP.release_blocks(span, head, len);
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
            let mut groups: [MaybeUninit<BGroup>; MAX_BFLUSH_GROUPS] =
                unsafe { MaybeUninit::uninit().assume_init() };
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
                self.tier_cached_bytes = self.tier_cached_bytes.saturating_sub(block_size);

                let master = crate::arena::big_table_get(b);
                debug_assert!(!master.is_null() && (*master).contains(b));
                *b.cast::<*mut u8>() = ptr::null_mut();
                let mut slot = None;
                for i in 0..ng {
                    let g = unsafe { groups[i].assume_init_ref() };
                    if g.master == master {
                        slot = Some(i);
                        break;
                    }
                }
                match slot {
                    Some(i) => {
                        let g = unsafe { groups[i].assume_init_mut() };
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
                        groups[ng] = MaybeUninit::new(BGroup {
                            master,
                            head: b,
                            tail: b,
                            n: 1,
                        });
                        ng += 1;
                    }
                }
            }

            for i in 0..ng {
                let g = unsafe { groups[i].assume_init() };
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
                self.flush_bin_exit(class);
            }
        }
        for mclass in 0..NUM_MEDIUM {
            if self.mactive[mclass].head.is_null() {
                continue;
            }
            self.flush_active_medium(mclass);
        }
        for mclass in 0..NUM_MEDIUM {
            if !self.mbins[mclass].head.is_null() {
                self.flush_mbin(mclass, 0);
            }
        }
        #[cfg(all(unix, feature = "std"))]
        for bclass in 0..NUM_BIG {
            if !self.bactive[bclass].head.is_null() {
                self.flush_active_big(bclass);
            }
            if !self.bigbins[bclass].head.is_null() {
                self.flush_bbin(bclass, 0);
            }
        }
        self.cached_bytes = 0;
        self.tier_cached_bytes = 0;
        self.foreign_bytes = 0;
        self.retired_reclaimed = false;
        // Keep `tid` stable across flushes: it identifies this OS thread for
        // the drift-cap owner heuristic, not a cache generation.
        self.virgin = [0; NUM_CLASSES];
        self.mvirgin = [0; NUM_MEDIUM];
        self.mactive = [ActiveMedium::empty(); NUM_MEDIUM];
        #[cfg(all(unix, feature = "std"))]
        {
            self.bvirgin = [0; NUM_BIG];
            self.bactive = [ActiveBig::empty(); NUM_BIG];
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

#[cfg(all(feature = "std", any(unix, windows)))]
pub(crate) fn retire(mut cache: ThreadCache) {
    let bytes = cache.cached_bytes.saturating_add(cache.large_bytes);
    if bytes == 0 {
        return;
    }
    if bytes > RETIRED_SLOT_BYTES {
        unsafe { cache.flush_all() };
        return;
    }
    for slot in &RETIRED_SLOTS {
        if slot
            .state
            .compare_exchange(
                RETIRED_EMPTY,
                RETIRED_WRITING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            unsafe { (*slot.cache.get()).write(cache) };
            slot.state.store(RETIRED_READY, Ordering::Release);
            RETIRED_READY_COUNT.fetch_add(1, Ordering::AcqRel);
            return;
        }
    }
    unsafe { cache.flush_all() };
}

#[cfg(all(feature = "std", any(unix, windows)))]
fn take_one() -> Option<ThreadCache> {
    if RETIRED_READY_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    for slot in &RETIRED_SLOTS {
        if slot
            .state
            .compare_exchange(
                RETIRED_READY,
                RETIRED_TAKING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            let cache = unsafe { (*slot.cache.get()).assume_init_read() };
            slot.state.store(RETIRED_EMPTY, Ordering::Release);
            RETIRED_READY_COUNT.fetch_sub(1, Ordering::AcqRel);
            return Some(cache);
        }
    }
    None
}

#[cfg(all(feature = "std", any(unix, windows)))]
pub(crate) fn reclaim_one() -> bool {
    if let Some(mut cache) = take_one() {
        unsafe { cache.flush_all() };
        true
    } else {
        false
    }
}

#[cfg(all(feature = "std", any(unix, windows)))]
pub(crate) fn reclaim_all() {
    while reclaim_one() {}
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
    #[cfg(all(unix, feature = "std"))]
    let arena_owned = crate::arena::contains(
        base as *mut u8,
        npages * PAGE_SIZE,
    );
    #[cfg(not(all(unix, feature = "std")))]
    let arena_owned = false;
    if arena_owned {
        let start = base + SPAN_MASTER_SIZE;
        let end = base + npages * PAGE_SIZE;
        if (p as usize) < start || (p as usize) + block_size > end {
            invalid("allox: medium dealloc of misaligned interior pointer");
        }
        if (p as usize - start) % block_size != 0 {
            invalid("allox: medium dealloc of misaligned interior pointer");
        }
    } else {
        let rel = (p as usize - base) / PAGE_SIZE;
        let chunk = base + rel * PAGE_SIZE;
        let (start, end) = if rel == 0 {
            (chunk + SPAN_MASTER_SIZE, chunk + PAGE_SIZE)
        } else {
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
    }
    let _ = PAGE_MASK;
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
