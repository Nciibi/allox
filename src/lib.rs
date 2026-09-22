//! `allox` — a pure-Rust, thread-cached general-purpose memory allocator.
//!
//! Zero dependencies, zero build scripts, no C toolchain: compiles anywhere
//! `rustc` does. Design overview in `DESIGN.md` at the repository root.
//!
//! # As a global allocator
//!
//! ```
//! use allox::Allox;
//!
//! #[global_allocator]
//! static GLOBAL: Allox = Allox;
//!
//! let v: Vec<u32> = (0..1000).collect();
//! assert_eq!(v.len(), 1000);
//! ```
//!
//! # Direct use
//!
//! ```
//! use std::alloc::{Layout, LayoutError};
//!
//! unsafe {
//!     let p = allox::malloc(64);
//!     assert!(!p.is_null());
//!     allox::free(p);
//! }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::missing_safety_doc)]

mod cache;
mod classes;
mod ffi;
mod heap;
mod page;
mod sys;
mod thread_exit;

use crate::classes::{
    class_for_size, medium_class_for_size, MAX_MEDIUM_BLOCK, MAX_SMALL_SIZE, MIN_ALIGN,
};
use crate::page::{
    align_up, LargeHeader, SpanMaster, LARGE_HEADER_SIZE, LARGE_MAGIC, PAGE_MASK,
};
use core::alloc::GlobalAlloc;
use core::ptr;
use heap::{HEAP, MEDIUM_HEAP};

/// The allocator handle. Implementor of [`GlobalAlloc`]; also usable through
/// the free functions [`malloc`], [`calloc`], [`realloc`], [`free`] and
/// [`aligned_alloc`].
pub struct Allox;

impl Allox {
    /// Create a handle. The allocator is process-global; handles are
    /// interchangeable.
    pub const fn new() -> Self {
        Allox
    }
}

impl Default for Allox {
    fn default() -> Self {
        Allox
    }
}

/// Per-thread cache access.
///
/// std: const-initialized TLS, no destructor (DESIGN.md §4.5), UnsafeCell on
/// the fast path. no_std: a single global cache behind the allocator's own
/// spin mutex — embedded targets are single-threaded, and the allocator never
/// re-enters this lock, so it stays deadlock-free.
mod tls {
    #[cfg(feature = "std")]
    pub(crate) mod imp {
        use super::super::cache::ThreadCache;
        use core::cell::UnsafeCell;

        thread_local! {
            // UnsafeCell, not RefCell: the borrow-flag check costs measurable
            // time on the fast path. Aliasing is impossible because the
            // allocator never invokes user code while the cache is borrowed,
            // so reentrant allocation cannot observe two `&mut`s.
            static CACHE: UnsafeCell<ThreadCache> =
                const { UnsafeCell::new(ThreadCache::new()) };
        }

        pub(crate) fn with<R>(
            f: impl FnOnce(&mut ThreadCache) -> R,
            fallback: impl FnOnce() -> R,
        ) -> R {
            let result = CACHE.try_with(|c| {
                // Safety: see the CACHE declaration; no reentrancy possible.
                f(unsafe { &mut *c.get() })
            });
            match result {
                Ok(r) => r,
                Err(_) => fallback(),
            }
        }

        pub(crate) fn flush() {
            with(|c| unsafe { c.flush_all() }, || {});
        }
    }

    #[cfg(not(feature = "std"))]
    pub(crate) mod imp {
        use super::super::cache::ThreadCache;
        use crate::sys::{Mutex, MutexGuard};
        use core::cell::UnsafeCell;

        struct GlobalCache(UnsafeCell<ThreadCache>);
        unsafe impl Send for GlobalCache {}

        static CACHE: Mutex<GlobalCache> =
            Mutex::new(GlobalCache(UnsafeCell::new(ThreadCache::new())));

        fn locked() -> MutexGuard<'static, GlobalCache> {
            CACHE.lock()
        }

        pub(crate) fn with<R>(
            f: impl FnOnce(&mut ThreadCache) -> R,
            fallback: impl FnOnce() -> R,
        ) -> R {
            let _ = fallback; // the global cache is always available
            let guard = locked();
            f(unsafe { &mut *guard.0.get() })
        }

        pub(crate) fn flush() {
            with(|c| unsafe { c.flush_all() }, || {});
        }
    }

    pub(crate) use imp::flush;
    pub(crate) use imp::with;
}

/// Full flush of the calling thread's cache (may block on heap locks).
/// Used by the OS thread-exit hook, where blocking is safe (no allocator
/// locks are ever held at thread exit, and heap locks never cycle with OS
/// teardown locks). Panic-free by construction.
#[cfg(all(feature = "std", any(unix, windows)))]
pub(crate) unsafe fn tls_flush_full() {
    tls::flush();
}

#[inline]
fn with_cache<R>(f: impl FnOnce(&mut cache::ThreadCache) -> R, fallback: impl FnOnce() -> R) -> R {
    tls::with(f, fallback)
}

unsafe fn alloc_small(class: usize) -> *mut u8 {
    with_cache(
        |c| c.alloc(class),
        || {
            let (chain, _, _) = HEAP.take_blocks(class);
            chain
        },
    )
}

unsafe fn dealloc_small(p: *mut u8) {
    let page = page::PageHeader::of(p);
    with_cache(
        |c| c.dealloc(p),
        || {
            HEAP.release_blocks(page, p, 1);
        },
    );
}

unsafe fn alloc_medium(mclass: usize) -> *mut u8 {
    with_cache(
        |c| c.alloc_medium(mclass),
        || {
            let (chain, _, _) = MEDIUM_HEAP.take_blocks(mclass);
            chain
        },
    )
}

unsafe fn dealloc_medium(p: *mut u8, span: *mut SpanMaster) {
    with_cache(
        |c| c.dealloc_medium(p, span),
        || {
            MEDIUM_HEAP.release_blocks(span, p, 1);
        },
    );
}

/// Cache of recently freed large regions, recycled on the next matching
/// large allocation instead of paying unmap+map syscalls.
///
/// Three tiers:
/// - per-thread stash in `ThreadCache` (lock-free): absorbs same-thread
///   reuse without any synchronization.
/// - sharded hot cache (independent mutexes): spreads cross-thread
///   contention; best-fit over a bounded entry list.
/// - sharded cold cache: physical dropped via `sys::discard`, virtual
///   retained for reuse. Absorbs churn bursts without the unmap/remap storm
///   (same trick as medium-span cold retention in `heap.rs`).
///
/// Worst-case retention is the hot+cold caps plus each live thread's stash.
/// Cold bytes are virtual-only after discard; RSS impact is bounded by live
/// demand, not by the caps.
const NUM_LARGE_SHARDS: usize = 8;
/// Slots per shard: generous, because the byte cap (not the slot count)
/// bounds retention — 64 slots × 16 B = 1 KiB of static storage per shard.
/// Single-threaded same-size traffic lands on one shard and must not
/// slot-starve there (the old single 64-slot cache never did).
const LARGE_SHARD_SLOTS: usize = 64;
const LARGE_SHARD_CAP_BYTES: usize = 8 * 1024 * 1024; // 8 x 8 MiB = 64 MiB total
/// Cold (discarded, virtually retained) bytes per shard. Deep on 64-bit
/// where virtual is free; shallow on 32-bit address spaces.
#[cfg(target_pointer_width = "64")]
const LARGE_COLD_CAP_BYTES: usize = 64 * 1024 * 1024; // 8 x 64 MiB virtual
#[cfg(not(target_pointer_width = "64"))]
const LARGE_COLD_CAP_BYTES: usize = 8 * 1024 * 1024;
const LARGE_COLD_SLOTS: usize = 64;

struct LargeRegionCache {
    len: usize,
    bytes: usize,
    entries: [(*mut u8, u32); LARGE_SHARD_SLOTS], // (base, mapped_pages)
    cold_len: usize,
    cold_bytes: usize,
    cold: [(*mut u8, u32); LARGE_COLD_SLOTS],
}

// Raw pointers are only touched while holding the enclosing mutex.
unsafe impl Send for LargeRegionCache {}

impl LargeRegionCache {
    const fn new() -> Self {
        LargeRegionCache {
            len: 0,
            bytes: 0,
            entries: [(ptr::null_mut(), 0); LARGE_SHARD_SLOTS],
            cold_len: 0,
            cold_bytes: 0,
            cold: [(ptr::null_mut(), 0); LARGE_COLD_SLOTS],
        }
    }

    /// Best-fit entry with at least `mapped` bytes: hot first, then cold.
    /// Removes and returns `(base, mapped_pages)`. Caller holds the lock.
    /// Both tiers need only a header rewrite (fresh=false); cold contents
    /// were discarded, hot contents are dirty — calloc memsets either way.
    ///
    /// Exact-size matches win over merely-fitting ones: under size variance,
    /// best-fit eats big regions for small requests, fragmenting the cache
    /// so future big requests miss. Exact-first preserves each size class's
    /// own reuse pool (temporal locality in churn is per-size).
    fn take_fit(&mut self, mapped: usize) -> Option<(*mut u8, u32)> {
        let wanted = (mapped / page::PAGE_SIZE) as u32;
        for i in 0..self.len {
            if self.entries[i].1 == wanted {
                let last = self.len - 1;
                let entry = self.entries[i];
                self.entries[i] = self.entries[last];
                self.entries[last] = (ptr::null_mut(), 0);
                self.len = last;
                self.bytes -= entry.1 as usize * page::PAGE_SIZE;
                return Some(entry);
            }
        }
        for i in 0..self.cold_len {
            if self.cold[i].1 == wanted {
                let last = self.cold_len - 1;
                let entry = self.cold[i];
                self.cold[i] = self.cold[last];
                self.cold[last] = (ptr::null_mut(), 0);
                self.cold_len = last;
                self.cold_bytes -= entry.1 as usize * page::PAGE_SIZE;
                return Some(entry);
            }
        }
        let mut best: Option<usize> = None;
        for i in 0..self.len {
            let (_, pages) = self.entries[i];
            if (pages as usize) * page::PAGE_SIZE >= mapped
                && best.map_or(true, |b| self.entries[i].1 < self.entries[b].1)
            {
                best = Some(i);
            }
        }
        if let Some(i) = best {
            let last = self.len - 1;
            let entry = self.entries[i];
            self.entries[i] = self.entries[last];
            self.entries[last] = (ptr::null_mut(), 0);
            self.len = last;
            self.bytes -= entry.1 as usize * page::PAGE_SIZE;
            return Some(entry);
        }
        let mut cbest: Option<usize> = None;
        for i in 0..self.cold_len {
            let (_, pages) = self.cold[i];
            if (pages as usize) * page::PAGE_SIZE >= mapped
                && cbest.map_or(true, |b| self.cold[i].1 < self.cold[b].1)
            {
                cbest = Some(i);
            }
        }
        cbest.map(|i| {
            let last = self.cold_len - 1;
            let entry = self.cold[i];
            self.cold[i] = self.cold[last];
            self.cold[last] = (ptr::null_mut(), 0);
            self.cold_len = last;
            self.cold_bytes -= entry.1 as usize * page::PAGE_SIZE;
            entry
        })
    }
}

static LARGE_SHARDS: [sys::Mutex<LargeRegionCache>; NUM_LARGE_SHARDS] =
    [const { sys::Mutex::new(LargeRegionCache::new()) }; NUM_LARGE_SHARDS];

/// Pick a shard from the region size salted by the calling thread, so equal
/// sizes from different threads spread while similar sizes on one thread
/// still meet for reuse. `salt` is the thread-cache address (0 off-thread).
#[inline]
fn large_shard(mapped_pages: usize, salt: usize) -> usize {
    (mapped_pages ^ (salt >> 6)) % NUM_LARGE_SHARDS
}

unsafe fn alloc_large(size: usize, align: usize) -> *mut u8 {
    alloc_large_ex(size, align).0
}

/// Returns `(ptr, fresh)` where `fresh` means the memory is guaranteed
/// OS-zero (a brand-new mapping rather than a recycled one).
unsafe fn alloc_large_ex(size: usize, align: usize) -> (*mut u8, bool) {
    let total = match size
        .checked_add(align)
        .and_then(|v| v.checked_add(LARGE_HEADER_SIZE))
    {
        Some(t) => t,
        None => return (ptr::null_mut(), false),
    };
    let mapped = align_up(total.max(LARGE_HEADER_SIZE), page::PAGE_SIZE);
    let mapped_pages = (mapped / page::PAGE_SIZE) as u32;

    // Tier 1: per-thread stash, no locks. Also yields a salt (the cache
    // address) that spreads the tier-2 shard choice across threads.
    let (stashed, salt): (Option<(*mut u8, u32)>, usize) = with_cache(
        |c| {
            c.arm_exit_hook();
            let salt = c as *mut _ as usize;
            (c.take_large_stash(mapped_pages), salt)
        },
        || (None, 0),
    );
    if let Some((base, pages)) = stashed {
        let region_size = pages as usize * page::PAGE_SIZE;
        let ret = align_up(base as usize + LARGE_HEADER_SIZE, align);
        if ret + size <= base as usize + region_size {
            let hdr = (ret - LARGE_HEADER_SIZE) as *mut LargeHeader;
            (*hdr).magic = LARGE_MAGIC;
            (*hdr).mapped_size = region_size;
            (*hdr).base = base;
            return (ret as *mut u8, false);
        }
        // Alignment made the cached region unusable; drop it.
        sys::unmap(base, region_size);
        heap::MAPPED_PAGES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        heap::UNMAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    // Tier 2+3: best-fit region from the sharded recycle cache (hot, then
    // cold). Cold hits need no syscall — virtual survived the discard.
    {
        let taken = {
            let mut c = LARGE_SHARDS[large_shard(mapped_pages as usize, salt)].lock();
            c.take_fit(mapped)
        };
        if let Some((base, pages)) = taken {
            let region_size = pages as usize * page::PAGE_SIZE;
            let ret = align_up(base as usize + LARGE_HEADER_SIZE, align);
            if ret + size <= base as usize + region_size {
                let hdr = (ret - LARGE_HEADER_SIZE) as *mut LargeHeader;
                (*hdr).magic = LARGE_MAGIC;
                (*hdr).mapped_size = region_size;
                (*hdr).base = base;
                return (ret as *mut u8, false);
            }
            // Alignment made the cached region unusable; drop it.
            sys::unmap(base, region_size);
            heap::MAPPED_PAGES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
            heap::UNMAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }

    // Large regions are located by offset header, never by address masking,
    // so kernel page alignment suffices — no over-map/trim tax (unix), and
    // elsewhere map_any is already the optimal primitive.
    let base = sys::map_any(mapped);
    if base.is_null() {
        return (ptr::null_mut(), false);
    }
    // Header lives directly before the user pointer: high alignment can push
    // the user pointer past the first 64 KiB boundary of the region, so the
    // region base is not a reliable place to find it.
    let ret = align_up(base as usize + LARGE_HEADER_SIZE, align);
    if ret + size > base as usize + mapped {
        sys::unmap(base, mapped);
        return (ptr::null_mut(), false);
    }
    let hdr = (ret - LARGE_HEADER_SIZE) as *mut LargeHeader;
    (*hdr).magic = LARGE_MAGIC;
    (*hdr).mapped_size = mapped;
    (*hdr).base = base;
    heap::MAPPED_PAGES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    heap::MAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    #[cfg(feature = "telemetry")]
    {
        use core::sync::atomic::Ordering::Relaxed;
        heap::TELEMETRY.large_allocs.fetch_add(1, Relaxed);
        heap::TELEMETRY.total_allocs.fetch_add(1, Relaxed);
        heap::TELEMETRY.bytes_in.fetch_add(size as u64, Relaxed);
        let live = heap::TELEMETRY
            .bytes_in
            .load(Relaxed)
            .saturating_sub(heap::TELEMETRY.bytes_out.load(Relaxed));
        heap::TELEMETRY.peak_live_bytes.fetch_max(live, Relaxed);
    }
    (ret as *mut u8, true)
}

unsafe fn free_large(p: *mut u8) {
    let hdr = (p as usize - LARGE_HEADER_SIZE) as *mut LargeHeader;
    let mapped = (*hdr).mapped_size;
    let base = (*hdr).base;
    let pages = (mapped / page::PAGE_SIZE) as u32;

    // Tier 1: per-thread stash — the freeing thread usually reallocates next.
    // Single TLS visit: stash the region and report our shard salt together.
    let (stashed, salt): (bool, usize) = with_cache(
        |c| {
            c.arm_exit_hook();
            (c.push_large_stash(base, pages), c as *mut _ as usize)
        },
        || (false, p as usize),
    );
    if stashed {
        return;
    }

    // Tier 2+3: park the region on its shard (hot), else cold with physical
    // dropped, else unmap. Discard runs outside the lock: it never touches
    // allocator state.
    enum Fate {
        Kept,
        Cold,
        Unmap,
    }
    let fate = {
        let mut c = LARGE_SHARDS[large_shard(pages as usize, salt)].lock();
        if c.len < LARGE_SHARD_SLOTS && c.bytes + mapped <= LARGE_SHARD_CAP_BYTES {
            let idx = c.len;
            c.entries[idx] = (base, pages);
            c.len = idx + 1;
            c.bytes += mapped;
            Fate::Kept
        } else if c.cold_len < LARGE_COLD_SLOTS
            && c.cold_bytes + mapped <= LARGE_COLD_CAP_BYTES
        {
            let idx = c.cold_len;
            c.cold[idx] = (base, pages);
            c.cold_len = idx + 1;
            c.cold_bytes += mapped;
            Fate::Cold
        } else {
            Fate::Unmap
        }
    };
    match fate {
        Fate::Kept => {}
        Fate::Cold => {
            sys::discard(base, mapped);
        }
        Fate::Unmap => {
            sys::unmap(base, mapped);
            heap::MAPPED_PAGES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
            heap::UNMAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
    #[cfg(feature = "telemetry")]
    {
        use core::sync::atomic::Ordering::Relaxed;
        heap::TELEMETRY.total_frees.fetch_add(1, Relaxed);
        let user = p as usize - base as usize;
        heap::TELEMETRY
            .bytes_out
            .fetch_add(mapped.saturating_sub(user) as u64, Relaxed);
    }
}

#[cold]
fn corrupt_pointer() -> ! {
    #[cfg(feature = "std")]
    {
        eprintln!("allox: free/realloc of pointer not owned by this allocator");
        std::process::abort();
    }
    #[cfg(not(feature = "std"))]
    panic!("allox: free/realloc of pointer not owned by this allocator")
}

/// Core dispatch used by every public entry point.
unsafe fn alloc_impl(size: usize, align: usize) -> *mut u8 {
    debug_assert!(align.is_power_of_two());
    if size == 0 {
        return align.max(1) as *mut u8;
    }
    if align > MIN_ALIGN || size > MAX_MEDIUM_BLOCK {
        return alloc_large(size, align);
    }
    if size > MAX_SMALL_SIZE {
        return alloc_medium(medium_class_for_size(size));
    }
    alloc_small(class_for_size(size))
}

/// Like `alloc_impl` but zeroes the allocation. Virgin small/medium blocks
/// only need their freelist-link word cleared; recycled large regions are
/// memset.
unsafe fn alloc_zeroed_impl(size: usize, align: usize) -> *mut u8 {
    debug_assert!(align.is_power_of_two());
    if size == 0 {
        return align.max(1) as *mut u8;
    }
    if align > MIN_ALIGN || size > MAX_MEDIUM_BLOCK {
        let (p, fresh) = alloc_large_ex(size, align);
        if !p.is_null() && !fresh {
            // Recycled region: dirtied by its previous life.
            ptr::write_bytes(p, 0, size);
        }
        return p;
    }
    if size > MAX_SMALL_SIZE {
        let mclass = medium_class_for_size(size);
        let (p, virgin) = with_cache(
            |c| c.alloc_medium_zeroed(mclass),
            || {
                let (chain, _, virgin) = MEDIUM_HEAP.take_blocks(mclass);
                (chain, virgin)
            },
        );
        if !p.is_null() {
            if virgin {
                p.cast::<u64>().write(0);
            } else {
                ptr::write_bytes(p, 0, size);
            }
        }
        return p;
    }
    let class = class_for_size(size);
    let (p, virgin) = with_cache(
        |c| c.alloc_zeroed(class),
        || {
            let (chain, _, virgin) = HEAP.take_blocks(class);
            (chain, virgin)
        },
    );
    if !p.is_null() {
        if virgin {
            // Only the freelist link word is dirty.
            p.cast::<u64>().write(0);
        } else {
            ptr::write_bytes(p, 0, size);
        }
    }
    p
}

/// Validate a large-region header for `p`: magic matches AND the range is
/// self-consistent (nonzero 64 KiB-multiple size, header strictly below `p`,
/// `p` inside `[base, base+mapped)`). Returns `(base, mapped_size)`.
///
/// The range check turns a coincidental 8-byte magic match in adjacent user
/// data (2⁻⁶⁴ on its own) into a ~2⁻¹⁰⁰ non-event, which matters now that
/// this probe runs before the exact small-page check.
#[inline]
unsafe fn large_header_of(p: *mut u8) -> Option<(*mut u8, usize)> {
    let hdr = (p as usize - LARGE_HEADER_SIZE) as *const LargeHeader;
    if (*hdr).magic != LARGE_MAGIC {
        return None;
    }
    let mapped = (*hdr).mapped_size;
    let base = (*hdr).base as usize;
    if mapped == 0 || mapped & PAGE_MASK != 0 {
        return None;
    }
    let off = (p as usize).wrapping_sub(base);
    if off == 0 || off >= mapped {
        return None;
    }
    Some((base as *mut u8, mapped))
}

unsafe fn dealloc_impl(p: *mut u8) {
    if p.is_null() {
        return;
    }
    // Large-offset check FIRST: it is in-bounds for every live pointer
    // (headers sit 32 B below any large user pointer by construction), while
    // masked reads can round down *outside* an unaligned large region into
    // unmapped memory and fault. Small pages stay hot path second (one extra
    // predictable branch); spans last.
    let hdr = (p as usize - LARGE_HEADER_SIZE) as *const LargeHeader;
    if (*hdr).magic == LARGE_MAGIC {
        free_large(p);
        return;
    }
    let masked_magic = *((p as usize & !PAGE_MASK) as *const u64);
    if masked_magic == page::PAGE_MAGIC {
        dealloc_small(p);
        return;
    }
    let span = SpanMaster::of(p);
    if !span.is_null() && (*span).contains(p) {
        dealloc_medium(p, span);
        return;
    }
    corrupt_pointer()
}

/// Layout-routed free for `GlobalAlloc` callers, who contractually pass the
/// layout the pointer was allocated with. Routes with zero probing reads —
/// no masked loads at all — so unaligned large bases cannot fault dispatch,
/// and the hot paths shed branches. Debug builds verify the layout against
/// the pointer and abort on mismatch (contract violation).
unsafe fn dealloc_with_layout(p: *mut u8, size: usize, align: usize) {
    debug_assert!(align.is_power_of_two());
    if align > MIN_ALIGN || size > MAX_MEDIUM_BLOCK {
        #[cfg(debug_assertions)]
        {
            let hdr = (p as usize - LARGE_HEADER_SIZE) as *const LargeHeader;
            if (*hdr).magic != LARGE_MAGIC {
                corrupt_pointer();
            }
        }
        free_large(p);
        return;
    }
    if size > MAX_SMALL_SIZE {
        // Span pages stay 64 KiB-aligned, so this masking is fault-safe.
        let span = SpanMaster::of(p);
        #[cfg(debug_assertions)]
        if span.is_null() || !(*span).contains(p) {
            corrupt_pointer();
        }
        dealloc_medium(p, span);
        return;
    }
    #[cfg(debug_assertions)]
    {
        let page = page::PageHeader::of(p);
        if (*page).magic != page::PAGE_MAGIC {
            corrupt_pointer();
        }
    }
    dealloc_small(p);
}

unsafe impl GlobalAlloc for Allox {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        alloc_impl(layout.size(), layout.align())
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: core::alloc::Layout) {
        if layout.size() == 0 {
            return;
        }
        dealloc_with_layout(p, layout.size(), layout.align())
    }

    unsafe fn realloc(&self, p: *mut u8, layout: core::alloc::Layout, new_size: usize) -> *mut u8 {
        if new_size == 0 {
            self.dealloc(p, layout);
            return layout.align().max(1) as *mut u8;
        }
        if !p.is_null()
            && layout.align() <= MIN_ALIGN
            && layout.size() <= MAX_SMALL_SIZE
            && new_size <= MAX_SMALL_SIZE
            && class_for_size(layout.size()) == class_for_size(new_size)
        {
            return p;
        }
        // Medium same-class resize is identity too (spans never move).
        if !p.is_null()
            && layout.align() <= MIN_ALIGN
            && layout.size() > MAX_SMALL_SIZE
            && layout.size() <= MAX_MEDIUM_BLOCK
            && new_size > MAX_SMALL_SIZE
            && new_size <= MAX_MEDIUM_BLOCK
            && medium_class_for_size(layout.size()) == medium_class_for_size(new_size)
        {
            return p;
        }
        let new_p = self.alloc(core::alloc::Layout::from_size_align_unchecked(
            new_size,
            layout.align(),
        ));
        if new_p.is_null() {
            return ptr::null_mut();
        }
        let copy = layout.size().min(new_size);
        ptr::copy_nonoverlapping(p, new_p, copy);
        self.dealloc(p, layout);
        new_p
    }

    unsafe fn alloc_zeroed(&self, layout: core::alloc::Layout) -> *mut u8 {
        alloc_zeroed_impl(layout.size(), layout.align())
    }
}

// ---------------------------------------------------------------------------
// Free-function API
// ---------------------------------------------------------------------------

/// Allocate `size` bytes with alignment 16. Returns null on failure or when
/// `size` exceeds [`isize::MAX`].
///
/// # Safety
/// Returned pointer must be freed with [`free`]/[`realloc`], never used after.
pub unsafe fn malloc(size: usize) -> *mut u8 {
    if size > isize::MAX as usize {
        return ptr::null_mut();
    }
    alloc_impl(size, 1)
}

/// Allocate `nmemb * size` zero-initialized bytes. Returns null on overflow
/// or exhaustion.
///
/// # Safety
/// Same ownership rules as [`malloc`].
pub unsafe fn calloc(nmemb: usize, size: usize) -> *mut u8 {
    let total = match nmemb.checked_mul(size) {
        Some(t) if t <= isize::MAX as usize => t,
        _ => return ptr::null_mut(),
    };
    alloc_zeroed_impl(total, 1)
}

/// Resize an allocation from [`malloc`]/[`calloc`]/[`realloc`].
/// Returns null (leaving the original intact) on failure.
///
/// # Safety
/// `p` must be null or a live allocation of this allocator.
pub unsafe fn realloc(p: *mut u8, size: usize) -> *mut u8 {
    if size > isize::MAX as usize {
        return ptr::null_mut();
    }
    if p.is_null() {
        return malloc(size);
    }
    // Large-offset check first (fault-safe for every live pointer; masked
    // reads can dangle outside unaligned large regions — see dealloc_impl).
    // Large resizes always go alloc-copy-free below via usable_size.
    let old_large_ok = {
        let hdr = (p as usize - LARGE_HEADER_SIZE) as *const LargeHeader;
        (*hdr).magic == LARGE_MAGIC
    };
    if !old_large_ok {
        let old_class_ok = {
            let magic = *((p as usize & !PAGE_MASK) as *const u64);
            magic == page::PAGE_MAGIC
        };
        if old_class_ok && size != 0 {
            let page = page::PageHeader::of(p);
            let old_class = (*page).class as usize;
            if size <= classes::MAX_SMALL_SIZE && class_for_size(size) == old_class {
                return p;
            }
        }
    }
    let old_span_ok = {
        let span = SpanMaster::of(p);
        !span.is_null() && (*span).contains(p)
    };
    if old_span_ok && size != 0 {
        let span = SpanMaster::of(p);
        let old_mclass = (*span).mclass as usize;
        if size > classes::MAX_SMALL_SIZE
            && size <= classes::MAX_MEDIUM_BLOCK
            && medium_class_for_size(size) == old_mclass
        {
            return p;
        }
    }
    let new_p = malloc(size);
    if !new_p.is_null() && size != 0 {
        let old_size = usable_size(p);
        ptr::copy_nonoverlapping(p, new_p, old_size.min(size));
    }
    if !new_p.is_null() {
        free(p);
    }
    new_p
}

/// Free an allocation. Null is ignored.
///
/// # Safety
/// `p` must be null or a live allocation of this allocator, and must not be
/// used afterwards.
pub unsafe fn free(p: *mut u8) {
    dealloc_impl(p)
}

/// Allocate `size` bytes with at least `align` alignment (power of two).
///
/// # Safety
/// Same ownership rules as [`malloc`].
pub unsafe fn aligned_alloc(align: usize, size: usize) -> *mut u8 {
    if align == 0 || !align.is_power_of_two() || size > isize::MAX as usize {
        return ptr::null_mut();
    }
    alloc_impl(size, align)
}

/// Number of bytes actually backing a live allocation (>= requested size).
///
/// # Safety
/// `p` must be a live allocation of this allocator or null.
pub unsafe fn usable_size(p: *mut u8) -> usize {
    if p.is_null() {
        return 0;
    }
    // Large-offset check first: fault-safe for every live pointer (masked
    // reads can dangle outside unaligned large regions — see dealloc_impl).
    // Cold path, so the extra load on small/medium is irrelevant.
    if (*(p.wrapping_sub(LARGE_HEADER_SIZE) as *const LargeHeader)).magic == LARGE_MAGIC {
        let hdr = (p as usize - LARGE_HEADER_SIZE) as *const LargeHeader;
        return (*hdr).mapped_size - (p as usize - (*hdr).base as usize);
    }
    let base = p as usize & !PAGE_MASK;
    if *(base as *const u64) == page::PAGE_MAGIC {
        let page = base as *mut page::PageHeader;
        classes::CLASSES[(*page).class as usize]
    } else {
        // Span before abort: a medium block's header-ward bytes must never be
        // mistaken for a large header (checked with contains, not just magic).
        let span = SpanMaster::of(p);
        if !span.is_null() && (*span).contains(p) {
            classes::MEDIUM_CLASSES[(*span).mclass as usize]
        } else {
            0
        }
    }
}

/// Snapshot current statistics.
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    /// Pages (64 KiB units) currently mapped from the OS, including large
    /// allocations' regions.
    pub mapped_pages: u64,
    /// Total successful OS mappings so far.
    pub map_calls: u64,
    /// Total OS unmaps so far.
    pub unmap_calls: u64,
}

/// Snapshot current statistics.
pub fn stats() -> Stats {
    use core::sync::atomic::Ordering::Relaxed;
    Stats {
        mapped_pages: heap::MAPPED_PAGES.load(Relaxed),
        map_calls: heap::MAP_CALLS.load(Relaxed),
        unmap_calls: heap::UNMAP_CALLS.load(Relaxed),
    }
}

/// Span-vs-large map split for tuning (see ROADMAP P2). Returns
/// `(span_maps, span_unmaps, small_maps, small_unmaps)`; large maps are
/// `map_calls - span_maps - small_maps`.
/// Hidden: not semver-covered, may change or vanish.
#[doc(hidden)]
pub fn __debug_map_split() -> (u64, u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (
        heap::SPAN_MAP_CALLS.load(Relaxed),
        heap::SPAN_UNMAP_CALLS.load(Relaxed),
        heap::SMALL_MAP_CALLS.load(Relaxed),
        heap::SMALL_UNMAP_CALLS.load(Relaxed),
    )
}

/// Built-in allocation telemetry.
///
/// Enable with the `telemetry` feature (zero cost when disabled). Counters
/// are accumulated per thread without atomics and published in batches, so
/// `snapshot()` values may lag by up to a few thousand operations per active
/// thread; call [`flush_current_thread()`] first for an exact view of one
/// thread's activity.
#[cfg(feature = "telemetry")]
pub mod telemetry {
    use core::sync::atomic::Ordering::Relaxed;

    /// A point-in-time view of allocator-wide activity.
    #[derive(Clone, Copy, Debug)]
    pub struct Telemetry {
        /// Allocation calls observed (any size).
        pub total_allocs: u64,
        /// Deallocation calls observed.
        pub total_frees: u64,
        /// `total_allocs - total_frees`.
        pub live_allocs: u64,
        /// Bytes handed out (rounded up to size class / mapped region).
        pub allocated_bytes: u64,
        /// Bytes released through `free`.
        pub freed_bytes: u64,
        /// `allocated_bytes - freed_bytes`.
        pub live_bytes: u64,
        /// High-water mark of `live_bytes`, sampled at counter flushes
        /// (a few thousand ops apart per thread).
        pub peak_live_bytes: u64,
        /// Allocations served by direct OS mappings (large/over-aligned).
        pub large_allocs: u64,
        /// Current OS mappings (pages of 64 KiB), including large regions.
        pub mapped_pages: u64,
        /// Total OS map calls.
        pub map_calls: u64,
        /// Total OS unmap calls.
        pub unmap_calls: u64,
        /// Allocation count per size class; indices `0..NUM_CLASSES` cover
        /// small classes, the rest cover medium classes. Block sizes are
        /// internal but stable for a given build.
        pub per_class_allocs: [u64; crate::classes::TOTAL_CLASSES],
    }

    /// Read the current telemetry snapshot.
    pub fn snapshot() -> Telemetry {
        let t = &crate::heap::TELEMETRY;
        let total_allocs = t.total_allocs.load(Relaxed);
        let total_frees = t.total_frees.load(Relaxed);
        let allocated_bytes = t.bytes_in.load(Relaxed);
        let freed_bytes = t.bytes_out.load(Relaxed);
        let mut per_class = [0u64; crate::classes::TOTAL_CLASSES];
        for (i, c) in t.per_class.iter().enumerate() {
            per_class[i] = c.load(Relaxed);
        }
        Telemetry {
            total_allocs,
            total_frees,
            live_allocs: total_allocs.saturating_sub(total_frees),
            allocated_bytes,
            freed_bytes,
            live_bytes: allocated_bytes.saturating_sub(freed_bytes),
            peak_live_bytes: t.peak_live_bytes.load(Relaxed),
            large_allocs: t.large_allocs.load(Relaxed),
            mapped_pages: crate::heap::MAPPED_PAGES.load(Relaxed),
            map_calls: crate::heap::MAP_CALLS.load(Relaxed),
            unmap_calls: crate::heap::UNMAP_CALLS.load(Relaxed),
            per_class_allocs: per_class,
        }
    }

    impl Default for Telemetry {
        fn default() -> Self {
            Telemetry {
                total_allocs: 0,
                total_frees: 0,
                live_allocs: 0,
                allocated_bytes: 0,
                freed_bytes: 0,
                live_bytes: 0,
                peak_live_bytes: 0,
                large_allocs: 0,
                mapped_pages: 0,
                map_calls: 0,
                unmap_calls: 0,
                per_class_allocs: [0; crate::classes::TOTAL_CLASSES],
            }
        }
    }
}

/// Set the per-thread cache retention budget in bytes (default 32 MiB).
///
/// Threads may each retain up to this many freed bytes before trimming
/// starts. Lower it to trade some allocation speed for resident memory on
/// many-threaded servers. Must be called before spawning worker threads;
/// reads are atomic so it is safe at any time, but mid-flight threads pick
/// the new value up lazily.
pub fn set_thread_cache_budget(bytes: usize) {
    #[cfg(feature = "std")]
    {
        cache::set_budget(bytes);
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = bytes; // fixed budget in no_std builds
    }
}

/// Return this thread's cached free blocks to their pages.
///
/// Useful for thread-pool workers between tasks; otherwise blocks stay cached
/// until the pages naturally die. Deliberately *not* run in a TLS destructor:
/// see DESIGN.md §4.5 for why. With `std` disabled this flushes the single
/// global cache.
pub fn flush_current_thread() {
    tls::flush();
}
