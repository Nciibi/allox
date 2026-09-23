//! Global page heap: per-size-class partial-page lists, each under its own
//! mutex (lock sharding: contention spreads across 64 independent locks).
//!
//! All mutation of a page's `free_head` happens while holding that page
//! class' mutex, so thread caches only ever own *detached* block chains.
//! A page is unmapped exactly when its `used` count drops to zero, which by
//! construction cannot happen while any thread still caches one of its
//! blocks. No code path ever holds two class locks at once.

use crate::classes::{span_pages_for, NUM_CLASSES, NUM_MEDIUM};
#[cfg(all(unix, feature = "std"))]
use crate::classes::{big_span_pages_for, NUM_BIG};
#[cfg(feature = "telemetry")]
use crate::classes::TOTAL_CLASSES;
use crate::page::{
    pop_block, PageHeader, SpanMaster, FLAG_IN_PARTIAL, FLAG_VIRGIN, PAGE_SIZE,
};
#[cfg(all(unix, feature = "std"))]
use crate::page::BigMaster;
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

/// Cold-page array slots per small class. Values (page bases), never
/// intrusive links: discard zeroes page contents including any link fields
/// (same lesson as medium spans). 256 × 64 KiB = 16 MiB, matching the byte
/// cap so either bound can bite first.
const MAX_COLD_PAGE_SLOTS: usize = 256;
/// Cold (discarded, virtually retained) bytes per small class. Deep on
/// 64-bit where virtual is free; shallow on 32-bit address spaces.
#[cfg(target_pointer_width = "64")]
const MAX_COLD_PAGE_BYTES_PER_CLASS: usize = 16 * 1024 * 1024;
#[cfg(not(target_pointer_width = "64"))]
const MAX_COLD_PAGE_BYTES_PER_CLASS: usize = 2 * 1024 * 1024;

pub(crate) static MAPPED_PAGES: AtomicU64 = AtomicU64::new(0);
pub(crate) static MAP_CALLS: AtomicU64 = AtomicU64::new(0);
pub(crate) static UNMAP_CALLS: AtomicU64 = AtomicU64::new(0);
/// Diagnostic split of MAP_CALLS by path (spans vs directly-mapped large).
/// Read via `allox::__debug_map_split` (hidden; for tuning, see ROADMAP P2).
pub(crate) static SPAN_MAP_CALLS: AtomicU64 = AtomicU64::new(0);
pub(crate) static SPAN_UNMAP_CALLS: AtomicU64 = AtomicU64::new(0);
pub(crate) static SMALL_MAP_CALLS: AtomicU64 = AtomicU64::new(0);
pub(crate) static SMALL_UNMAP_CALLS: AtomicU64 = AtomicU64::new(0);
/// Diagnostic split for big-span takes/disposals (see `__debug_map_split`
/// extension). Arena targets only (big spans don't exist elsewhere).
#[cfg(all(unix, feature = "std"))]
pub(crate) static BIG_MAP_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(all(unix, feature = "std"))]
pub(crate) static BIG_UNMAP_CALLS: AtomicU64 = AtomicU64::new(0);

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
    /// Small classes at 0..NUM_CLASSES, medium classes after.
    pub(crate) per_class: [AtomicU64; TOTAL_CLASSES],
}

#[cfg(feature = "telemetry")]
pub(crate) static TELEMETRY: TelemetryCounters = TelemetryCounters {
    total_allocs: AtomicU64::new(0),
    total_frees: AtomicU64::new(0),
    bytes_in: AtomicU64::new(0),
    bytes_out: AtomicU64::new(0),
    peak_live_bytes: AtomicU64::new(0),
    large_allocs: AtomicU64::new(0),
    per_class: [const { AtomicU64::new(0) }; TOTAL_CLASSES],
};

pub(crate) struct ListHead {
    /// Partial pages (spare free blocks), doubly linked via prev/next.
    head: *mut PageHeader,
    /// Fully free pages held for reuse instead of unmapping; singly linked
    /// via `next`. All carry a full free list and `used == 0`.
    empty: *mut PageHeader,
    empty_count: u32,
    /// Cold pages: discarded (madvise) but virtually retained, stored as
    /// base values. Re-carved on reuse; no syscalls in steady churn.
    cold: [*mut PageHeader; MAX_COLD_PAGE_SLOTS],
    cold_len: u32,
    cold_bytes: usize,
}

// Raw pointers are only manipulated while holding the enclosing Mutex.
unsafe impl Send for ListHead {}

impl ListHead {
    pub(crate) const fn new() -> Self {
        ListHead {
            head: ptr::null_mut(),
            empty: ptr::null_mut(),
            empty_count: 0,
            cold: [ptr::null_mut(); MAX_COLD_PAGE_SLOTS],
            cold_len: 0,
            cold_bytes: 0,
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

/// Post-lock fate of a released page: kept (nothing to do), parked cold
/// (caller discards outside the lock), or over caps (caller unmaps).
enum PageFate {
    Keep,
    Cold,
    Unmap,
}

/// Splice `chain` (n blocks of `page`) back onto the page. Lock-only core;
/// syscalls happen in the caller, outside the lock.
unsafe fn release_inner(list: &mut ListHead, page: *mut PageHeader, chain: *mut u8, n: u16) -> PageFate {
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
            PageFate::Keep
        } else if (list.cold_len as usize) < MAX_COLD_PAGE_SLOTS
            && list.cold_bytes + PAGE_SIZE <= MAX_COLD_PAGE_BYTES_PER_CLASS
        {
            // Cold: drop physical, keep virtual. Array-stored base so the
            // discard can't destroy the linkage. Re-carved on reuse.
            //
            // The discard runs UNDER the lock: the page is exclusively
            // ours (used==0 observed above, unlinked from every list, and
            // the cold entry is not visible until unlock), so no concurrent
            // pop can hand out blocks mid-discard and lose user writes.
            // Discarding after unlock raced exactly so: a taker could pop,
            // re-carve, and hand out the page before our discard landed,
            // zeroing live blocks and headers (use-after-discard leading to
            // short free lists, magic mismatches, and wild splices).
            // Same discipline as medium spans and large regions.
            let idx = list.cold_len as usize;
            list.cold[idx] = page;
            list.cold_len += 1;
            list.cold_bytes += PAGE_SIZE;
            sys::discard(page.cast::<u8>(), PAGE_SIZE);
            PageFate::Cold
        } else {
            PageFate::Unmap
        }
    } else {
        if (*page).flags & FLAG_IN_PARTIAL == 0 {
            link_partial(&mut list.head, page);
        }
        PageFate::Keep
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

            if count == 0 && list.cold_len > 0 {
                // Cold page: virtual survived, contents didn't (discard
                // zeroes the header and free list too). Re-carve and treat
                // as non-virgin so calloc always memsets — sound even where
                // discard is a no-op (wasm).
                list.cold_len -= 1;
                let cidx = list.cold_len as usize;
                let page = list.cold[cidx];
                list.cold[cidx] = ptr::null_mut();
                list.cold_bytes -= PAGE_SIZE;
                (*page).init(class);
                (*page).flags &= !FLAG_VIRGIN;
                virgin = false;
                link_partial(&mut list.head, page);
                fill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin);
            }
        }

        if count == 0 {
            let (raw, fresh) = map_heap_pages(1);
            if !raw.is_null() {
                let page = raw.cast::<PageHeader>();
                (*page).init(class);
                if fresh {
                    MAPPED_PAGES.fetch_add(1, Ordering::Relaxed);
                }
                MAP_CALLS.fetch_add(1, Ordering::Relaxed);
                SMALL_MAP_CALLS.fetch_add(1, Ordering::Relaxed);
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
        let fate = {
            let mut list = self.classes[class].lock();
            release_inner(&mut list, page, chain, n)
        };
        match fate {
            PageFate::Keep => {}
            // Cold pages were already discarded under the class lock in
            // `release_inner` (discarding here, after unlock, would race a
            // concurrent cold reuse and zero live blocks). Nothing to do.
            PageFate::Cold => {}
            PageFate::Unmap => {
                // Arena-owned pages park in holes (no syscall, counters stay
                // balanced); legacy mappings need the true unmap +
                // decrements. Same shape as the span fate arm below and
                // large's `crate::unmap_or_return`.
                #[cfg(all(unix, feature = "std"))]
                {
                    if crate::arena::contains(page.cast::<u8>(), PAGE_SIZE) {
                        crate::arena::release(page.cast::<u8>(), 1);
                        return;
                    }
                }
                sys::unmap(page.cast::<u8>(), PAGE_SIZE);
                MAPPED_PAGES.fetch_sub(1, Ordering::Relaxed);
                UNMAP_CALLS.fetch_add(1, Ordering::Relaxed);
                SMALL_UNMAP_CALLS.fetch_add(1, Ordering::Relaxed);
            }
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
/// are large (up to ~16 pages); the cap is byte-scaled in release_blocks
/// (see MAX_EMPTY_SPAN_BYTES) so small-medium classes keep several spans
/// while 60 KiB-class spans don't blow RSS.
const EMPTY_SPAN_CACHE_PER_CLASS: u32 = 8;
/// Cap on retained empty-span bytes per medium class.
const MAX_EMPTY_SPAN_BYTES_PER_CLASS: usize = 2 * 1024 * 1024;
/// Cap on cold (discarded-physical, retained-virtual) span bytes per class.
/// Sized to swallow harness drain bursts (~96 MiB live freed at once across
/// ~13 classes) so steady-state churn re-carves instead of mmap/munmap.
#[cfg(target_pointer_width = "64")]
const MAX_COLD_SPAN_BYTES_PER_CLASS: usize = 256 * 1024 * 1024;
/// 32-bit address space can't afford deep virtual retention; keep cold as a
/// small burst buffer and unmap the rest (physical is what matters there).
#[cfg(not(target_pointer_width = "64"))]
const MAX_COLD_SPAN_BYTES_PER_CLASS: usize = 16 * 1024 * 1024;

/// Cold-span array slots per medium class. The byte cap binds first for big
/// spans; slots bound small-span counts (256 × 192 KiB ≈ 49 MiB).
const MAX_COLD_SPAN_SLOTS: usize = 256;

pub(crate) struct MSpanList {
    /// Partial spans (spare free blocks), doubly linked via prev/next.
    head: *mut SpanMaster,
    /// Fully free spans held for reuse; singly linked via `next`.
    /// Never discarded, so intrusive links stay valid.
    empty: *mut SpanMaster,
    empty_count: u32,
    empty_bytes: usize,
    /// Cold spans: virtual reservation retained, physical dropped via
    /// discard (madvise). Stored as (base, npages) VALUES — never intrusive
    /// links, because discard zeroes everything inside the span including
    /// any link fields. Re-carved on reuse.
    cold: [(*mut u8, u32); MAX_COLD_SPAN_SLOTS],
    cold_len: u32,
    cold_bytes: usize,
}

// Raw pointers are only manipulated while holding the enclosing Mutex.
unsafe impl Send for MSpanList {}

impl MSpanList {
    pub(crate) const fn new() -> Self {
        MSpanList {
            head: ptr::null_mut(),
            empty: ptr::null_mut(),
            empty_count: 0,
            empty_bytes: 0,
            cold: [(ptr::null_mut(), 0); MAX_COLD_SPAN_SLOTS],
            cold_len: 0,
            cold_bytes: 0,
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

/// Post-lock fate of a released span: kept (nothing more to do) or over
/// caps (caller unmaps). Cold parking discards inline (see below).
enum SpanFate {
    Keep,
    Unmap(usize),
}

/// Splice `chain` (n blocks of `span`) back onto the span. Shared core for
/// the blocking and try paths; syscalls happen in the caller, outside locks.
unsafe fn mrelease_inner(list: &mut MSpanList, span: *mut SpanMaster, chain: *mut u8, n: u32) -> SpanFate {
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
        let span_bytes = (*span).mapped_bytes();
        // Byte-scaled retention: count cap AND byte cap. Keeps several
        // spans for small-medium classes, at most ~2 MiB per class hot.
        if list.empty_count < EMPTY_SPAN_CACHE_PER_CLASS
            && list.empty_bytes + span_bytes <= MAX_EMPTY_SPAN_BYTES_PER_CLASS
        {
            // Delayed reclamation: keep the span mapped for reuse.
            (*span).next = list.empty;
            list.empty = span;
            list.empty_count += 1;
            list.empty_bytes += span_bytes;
            SpanFate::Keep
                } else if (list.cold_len as usize) < MAX_COLD_SPAN_SLOTS
                    && list.cold_bytes + span_bytes <= MAX_COLD_SPAN_BYTES_PER_CLASS
                {
                    // Cold: drop physical, keep virtual. Array-stored (base,
                    // npages) so the discard can't destroy the linkage.
                    // Discard runs UNDER the lock: the span is exclusively
                    // ours until unlock (used==0 observed above), so no
                    // concurrent pop can hand out blocks mid-discard and lose
                    // user writes. Discarding after unlock raced exactly so.
                    let idx = list.cold_len as usize;
                    list.cold[idx] = (span.cast::<u8>(), (*span).npages);
                    list.cold_len += 1;
                    list.cold_bytes += span_bytes;
                    sys::discard(span.cast::<u8>(), span_bytes);
                    SpanFate::Keep
                } else {
            SpanFate::Unmap(span_bytes)
        }
    } else {
        if (*span).flags & FLAG_IN_PARTIAL == 0 {
            mlink_partial(&mut list.head, span);
        }
        SpanFate::Keep
    }
}

unsafe fn mact_fate(base: *mut u8, fate: SpanFate) {
    match fate {
        SpanFate::Keep => {}
        SpanFate::Unmap(bytes) => {
            // Arena-owned slices park in holes (no syscall, counters stay
            // balanced); legacy mappings need the true unmap + decrements.
            // Same shape as `crate::unmap_or_return` for large regions, but
            // with the span counter split.
            #[cfg(all(unix, feature = "std"))]
            {
                if crate::arena::contains(base, bytes) {
                    crate::arena::release(base, bytes / PAGE_SIZE);
                    return;
                }
            }
            sys::unmap(base, bytes);
            MAPPED_PAGES.fetch_sub(1, Ordering::Relaxed);
            UNMAP_CALLS.fetch_add(1, Ordering::Relaxed);
            SPAN_UNMAP_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Map fresh heap pages (small pages or medium spans): arena commit first
/// (1 VMA op, 64 KiB-aligned by construction), legacy over-map-trim
/// fallback when the arena is unavailable. Fresh zeros either way, so the
/// caller keeps `FLAG_VIRGIN` set exactly as for legacy maps (carving
/// dirties only freelist-link words — the virgin invariant).
///
/// Returns `(base, fresh)`: `fresh` is true for genuinely new virtual
/// (arena bump or legacy map) and false for recommitted arena holes. The
/// caller counts one mapping op (`MAP_CALLS` + class split) per non-null
/// return but live virtual (`MAPPED_PAGES`) only when `fresh` is set,
/// mirroring `map_large_region`.
#[inline]
unsafe fn map_heap_pages(pages: usize) -> (*mut u8, bool) {
    #[cfg(all(unix, feature = "std"))]
    {
        debug_assert_eq!(PAGE_SIZE, crate::arena::ARENA_ALIGN);
        let (base, fresh) = crate::arena::commit(pages);
        if !base.is_null() {
            debug_assert_eq!(base as usize & (PAGE_SIZE - 1), 0);
            return (base, fresh);
        }
    }
    let base = sys::map(pages * PAGE_SIZE);
    (base, !base.is_null())
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
                list.empty_bytes -= (*span).mapped_bytes();
                if (*span).flags & FLAG_VIRGIN == 0 {
                    virgin = false;
                }
                mlink_partial(&mut list.head, span);
                mfill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin, MEDIUM_REFILL_BATCH);
            }

            if count == 0 && list.cold_len > 0 {
                // Cold span: virtual reservation survived, contents didn't
                // (discard zeroes headers too — npages comes from the array,
                // never from inside the span). Re-carve (no syscalls) and
                // treat as non-virgin so calloc always memsets — safe even
                // if the discard was a no-op.
                list.cold_len -= 1;
                let cidx = list.cold_len as usize;
                let (base, pages) = list.cold[cidx];
                list.cold[cidx] = (ptr::null_mut(), 0);
                list.cold_bytes -= pages as usize * PAGE_SIZE;
                let span = base.cast::<SpanMaster>();
                (*span).init(mclass, pages);
                (*span).flags &= !FLAG_VIRGIN;
                virgin = false;
                mlink_partial(&mut list.head, span);
                mfill_from_list(&mut list.head, &mut chain, &mut count, &mut virgin, MEDIUM_REFILL_BATCH);
            }
        }

        if count == 0 {
            let pages = span_pages_for(crate::classes::MEDIUM_CLASSES[mclass]);
            let (raw, fresh) = map_heap_pages(pages);
            if !raw.is_null() {
                let span = raw.cast::<SpanMaster>();
                (*span).init(mclass, pages as u32);
                if fresh {
                    MAPPED_PAGES.fetch_add(1, Ordering::Relaxed);
                }
                MAP_CALLS.fetch_add(1, Ordering::Relaxed);
                SPAN_MAP_CALLS.fetch_add(1, Ordering::Relaxed);
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
        let base = span.cast::<u8>();
        let fate = {
            let mut list = self.classes[mclass].lock();
            mrelease_inner(&mut list, span, chain, n)
        };
        mact_fate(base, fate);
    }

    /// Lock access to a class' partial list for external validation
    /// (debug double-free detection).
    #[cfg(debug_assertions)]
    pub(crate) fn debug_lock_medium(&self, mclass: usize) -> MutexGuard<'_, MSpanList> {
        self.classes[mclass].lock()
    }
}

pub(crate) static MEDIUM_HEAP: MediumHeap = MediumHeap::new();
