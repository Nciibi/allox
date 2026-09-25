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
mod counters;
mod ffi;
mod heap;
mod page;
mod sys;
mod thread_exit;
/// Virtual-memory arena (unix + `std` only; legacy paths elsewhere).
#[cfg(all(unix, feature = "std"))]
mod arena;

use crate::classes::{
    class_for_size, medium_class_for_size, MAX_MEDIUM_BLOCK, MAX_SMALL_SIZE, MIN_ALIGN,
};
#[cfg(all(unix, feature = "std"))]
use crate::classes::{big_class_for_size, MAX_BIG_BLOCK};
use crate::page::{
    align_up, LargeHeader, SpanMaster, LARGE_HEADER_SIZE, LARGE_MAGIC, PAGE_MASK,
};
#[cfg(all(unix, feature = "std"))]
use crate::page::BigMaster;
use core::alloc::GlobalAlloc;
use core::ptr;
use heap::{HEAP, MEDIUM_HEAP};
#[cfg(all(unix, feature = "std"))]
use heap::BIG_HEAP;

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
            crate::cache::reclaim_all();
            with(|c| unsafe { c.flush_all() }, || {});
        }

        pub(crate) unsafe fn retire() {
            let _ = CACHE.try_with(|c| {
                let cache = core::ptr::read(c.get());
                crate::cache::retire(cache);
            });
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
    #[cfg(all(feature = "std", any(unix, windows)))]
    pub(crate) use imp::retire;
    pub(crate) use imp::with;
}

/// Retire the calling thread's cache at OS thread exit.
#[cfg(all(feature = "std", any(unix, windows)))]
pub(crate) unsafe fn tls_retire() {
    tls::retire();
}

#[inline]
fn with_cache<R>(f: impl FnOnce(&mut cache::ThreadCache) -> R, fallback: impl FnOnce() -> R) -> R {
    tls::with(f, fallback)
}

unsafe fn take_one_small(class: usize) -> (*mut u8, bool) {
    let (chain, count, virgin) = HEAP.take_blocks(class);
    if chain.is_null() {
        return (ptr::null_mut(), false);
    }
    let first = chain;
    let mut tail = *first.cast::<*mut u8>();
    *first.cast::<*mut u8>() = ptr::null_mut();
    for _ in 1..count {
        if tail.is_null() {
            break;
        }
        let next = *tail.cast::<*mut u8>();
        *tail.cast::<*mut u8>() = ptr::null_mut();
        HEAP.release_blocks(page::PageHeader::of(tail), tail, 1);
        tail = next;
    }
    (first, virgin)
}

unsafe fn take_one_medium(mclass: usize) -> (*mut u8, bool) {
    let (chain, count, virgin) = MEDIUM_HEAP.take_blocks(mclass);
    if chain.is_null() {
        return (ptr::null_mut(), false);
    }
    let first = chain;
    let mut tail = *first.cast::<*mut u8>();
    *first.cast::<*mut u8>() = ptr::null_mut();
    for _ in 1..count {
        if tail.is_null() {
            break;
        }
        let next = *tail.cast::<*mut u8>();
        *tail.cast::<*mut u8>() = ptr::null_mut();
        let span = SpanMaster::of(tail);
        MEDIUM_HEAP.release_blocks(mclass, span, tail, tail, 1);
        tail = next;
    }
    (first, virgin)
}

#[cfg(all(unix, feature = "std"))]
unsafe fn take_one_big(bclass: usize) -> (*mut u8, bool) {
    let (chain, count, virgin) = BIG_HEAP.take_blocks(bclass);
    if chain.is_null() {
        return (ptr::null_mut(), false);
    }
    let first = chain;
    let mut tail = *first.cast::<*mut u8>();
    *first.cast::<*mut u8>() = ptr::null_mut();
    for _ in 1..count {
        if tail.is_null() {
            break;
        }
        let next = *tail.cast::<*mut u8>();
        *tail.cast::<*mut u8>() = ptr::null_mut();
        let span = crate::arena::big_table_get(tail);
        BIG_HEAP.release_blocks(span, tail, 1);
        tail = next;
    }
    (first, virgin)
}

unsafe fn alloc_small(class: usize) -> *mut u8 {
    with_cache(
        |c| c.alloc(class),
        || take_one_small(class).0,
    )
}

unsafe fn dealloc_small(p: *mut u8) {
    // free() has no layout: load class from the page header once, then
    // hand it to the cache (which no longer re-derives it).
    let page = page::PageHeader::of(p);
    let class = (*page).class as usize;
    with_cache(
        |c| c.dealloc(p, class),
        || {
            *p.cast::<*mut u8>() = ptr::null_mut();
            HEAP.release_blocks(page, p, 1);
        },
    );
}

unsafe fn alloc_medium(mclass: usize) -> *mut u8 {
    with_cache(
        |c| c.alloc_medium(mclass),
        || take_one_medium(mclass).0,
    )
}

unsafe fn dealloc_medium(p: *mut u8, span: *mut SpanMaster) {
    let mclass = (*span).mclass as usize;
    with_cache(
        |c| c.dealloc_medium(p, mclass),
        || {
            *p.cast::<*mut u8>() = ptr::null_mut();
            MEDIUM_HEAP.release_blocks(mclass, span, p, p, 1);
        },
    );
}

/// Big-span allocate: arena side-table spans past the chunk cap. Null means
/// the arena is unavailable — callers fall back to the large path (big
/// spans exist only in the arena, so there is no legacy-mapped form).
#[cfg(all(unix, feature = "std"))]
unsafe fn alloc_big(bclass: usize) -> *mut u8 {
    with_cache(
        |c| c.alloc_big(bclass),
        || take_one_big(bclass).0,
    )
}

#[cfg(all(unix, feature = "std"))]
unsafe fn dealloc_big(p: *mut u8, span: *mut BigMaster) {
    with_cache(
        |c| c.dealloc_big(p, span),
        || {
            *p.cast::<*mut u8>() = ptr::null_mut();
            BIG_HEAP.release_blocks(span, p, 1);
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
const LARGE_SHARD_CAP_BYTES: usize = 32 * 1024 * 1024;
/// Cold (discarded, virtually retained) bytes per shard. Deep on 64-bit
/// where virtual is free; shallow on 32-bit address spaces.
#[cfg(target_pointer_width = "64")]
const LARGE_COLD_CAP_BYTES: usize = 64 * 1024 * 1024; // 8 x 64 MiB virtual
#[cfg(not(target_pointer_width = "64"))]
const LARGE_COLD_CAP_BYTES: usize = 8 * 1024 * 1024;
/// Cold (discarded, virtually retained) slots per shard. Deep (512) because
/// cold is virtual-only after discard — RSS stays bounded by live demand,
/// not the cap — while 64 slots x ~150 KiB capped retention far below the
/// 64 MiB/shard byte cap, starving exact reuse and flooding the arena holes
/// under large-only variance (measured §4: 64 -> ~74k abandonments/run;
/// 512 -> unmaps 30-50k/s collapse toward 0, +28% median, variance
/// collapse). 8 shards x 512 x 16 B = 64 KiB static. Hot stays shallow
/// (hot retention is mapped RSS, not virtual).
const LARGE_COLD_SLOTS: usize = 512;
const LARGE_EXACT_MAX_PAGES: usize = LARGE_COLD_CAP_BYTES / page::PAGE_SIZE;
const EMPTY_LARGE_INDEX: u16 = u16::MAX;

struct LargeRegionCache {
    len: usize,
    bytes: usize,
    entries: [(*mut u8, u32); LARGE_SHARD_SLOTS], // (base, mapped_pages)
    hot_zeroed: [bool; LARGE_SHARD_SLOTS],
    hot_exact: [u16; LARGE_EXACT_MAX_PAGES + 1],
    cold_len: usize,
    cold_bytes: usize,
    cold: [(*mut u8, u32); LARGE_COLD_SLOTS],
    cold_zeroed: [bool; LARGE_COLD_SLOTS],
    cold_exact: [u16; LARGE_EXACT_MAX_PAGES + 1],
}

// Raw pointers are only touched while holding the enclosing mutex.
unsafe impl Send for LargeRegionCache {}

fn refresh_large_exact(
    entries: &[(*mut u8, u32)],
    len: usize,
    pages: usize,
    exact: &mut [u16; LARGE_EXACT_MAX_PAGES + 1],
) {
    if pages > LARGE_EXACT_MAX_PAGES {
        return;
    }
    let mut found = EMPTY_LARGE_INDEX;
    let mut i = 0;
    while i < len {
        if entries[i].1 as usize == pages {
            found = i as u16;
            break;
        }
        i += 1;
    }
    exact[pages] = found;
}

impl LargeRegionCache {
    const fn new() -> Self {
        LargeRegionCache {
            len: 0,
            bytes: 0,
            entries: [(ptr::null_mut(), 0); LARGE_SHARD_SLOTS],
            hot_zeroed: [false; LARGE_SHARD_SLOTS],
            hot_exact: [EMPTY_LARGE_INDEX; LARGE_EXACT_MAX_PAGES + 1],
            cold_len: 0,
            cold_bytes: 0,
            cold: [(ptr::null_mut(), 0); LARGE_COLD_SLOTS],
            cold_zeroed: [false; LARGE_COLD_SLOTS],
            cold_exact: [EMPTY_LARGE_INDEX; LARGE_EXACT_MAX_PAGES + 1],
        }
    }

    fn index_hot(&mut self, index: usize, pages: usize) {
        if pages > LARGE_EXACT_MAX_PAGES {
            return;
        }
        let current = self.hot_exact[pages] as usize;
        if current == EMPTY_LARGE_INDEX as usize {
            self.hot_exact[pages] = index as u16;
        } else if current >= self.len || self.entries[current].1 as usize != pages {
            refresh_large_exact(
                &self.entries,
                self.len,
                pages,
                &mut self.hot_exact,
            );
        }
    }

    fn index_cold(&mut self, index: usize, pages: usize) {
        if pages > LARGE_EXACT_MAX_PAGES {
            return;
        }
        let current = self.cold_exact[pages] as usize;
        if current == EMPTY_LARGE_INDEX as usize {
            self.cold_exact[pages] = index as u16;
        } else if current >= self.cold_len || self.cold[current].1 as usize != pages {
            refresh_large_exact(
                &self.cold,
                self.cold_len,
                pages,
                &mut self.cold_exact,
            );
        }
    }

    fn exact_hot(&self, wanted: u32) -> Option<usize> {
        let pages = wanted as usize;
        if pages > LARGE_EXACT_MAX_PAGES {
            return None;
        }
        let index = self.hot_exact[pages] as usize;
        if index < self.len && self.entries[index].1 == wanted {
            Some(index)
        } else {
            None
        }
    }

    fn exact_cold(&self, wanted: u32) -> Option<usize> {
        let pages = wanted as usize;
        if pages > LARGE_EXACT_MAX_PAGES {
            return None;
        }
        let index = self.cold_exact[pages] as usize;
        if index < self.cold_len && self.cold[index].1 == wanted {
            Some(index)
        } else {
            None
        }
    }

    fn remove_hot(&mut self, index: usize) -> (*mut u8, u32, bool) {
        let last = self.len - 1;
        let entry = self.entries[index];
        let zeroed = self.hot_zeroed[index];
        self.entries[index] = self.entries[last];
        self.entries[last] = (ptr::null_mut(), 0);
        self.hot_zeroed[index] = self.hot_zeroed[last];
        self.hot_zeroed[last] = false;
        self.len = last;
        self.bytes -= entry.1 as usize * page::PAGE_SIZE;
        if index < self.len {
            let moved_pages = self.entries[index].1 as usize;
            if moved_pages <= LARGE_EXACT_MAX_PAGES
                && self.hot_exact[moved_pages] == last as u16
            {
                self.hot_exact[moved_pages] = index as u16;
            }
        }
        let removed_pages = entry.1 as usize;
        if removed_pages <= LARGE_EXACT_MAX_PAGES {
            let bucket = self.hot_exact[removed_pages] as usize;
            if bucket == index
                || bucket >= self.len
                || self.entries[bucket].1 as usize != removed_pages
            {
                refresh_large_exact(
                    &self.entries,
                    self.len,
                    removed_pages,
                    &mut self.hot_exact,
                );
            }
        }
        (entry.0, entry.1, zeroed)
    }

    fn remove_cold(&mut self, index: usize) -> (*mut u8, u32, bool) {
        let last = self.cold_len - 1;
        let entry = self.cold[index];
        let zeroed = self.cold_zeroed[index];
        self.cold[index] = self.cold[last];
        self.cold[last] = (ptr::null_mut(), 0);
        self.cold_zeroed[index] = self.cold_zeroed[last];
        self.cold_zeroed[last] = false;
        self.cold_len = last;
        self.cold_bytes -= entry.1 as usize * page::PAGE_SIZE;
        if index < self.cold_len {
            let moved_pages = self.cold[index].1 as usize;
            if moved_pages <= LARGE_EXACT_MAX_PAGES
                && self.cold_exact[moved_pages] == last as u16
            {
                self.cold_exact[moved_pages] = index as u16;
            }
        }
        let removed_pages = entry.1 as usize;
        if removed_pages <= LARGE_EXACT_MAX_PAGES {
            let bucket = self.cold_exact[removed_pages] as usize;
            if bucket == index
                || bucket >= self.cold_len
                || self.cold[bucket].1 as usize != removed_pages
            {
                refresh_large_exact(
                    &self.cold,
                    self.cold_len,
                    removed_pages,
                    &mut self.cold_exact,
                );
            }
        }
        (entry.0, entry.1, zeroed)
    }

    /// Best-fit entry with at least `mapped` bytes: hot first, then cold.
    /// Removes and returns `(base, mapped_pages, known_zeroed)`. Caller holds the lock.
    /// Both tiers need only a header rewrite (fresh=false); the returned
    /// zero flag tells calloc whether the recycled bytes are known zero.
    ///
    /// Exact-size matches win over merely-fitting ones: under size variance,
    /// best-fit eats big regions for small requests, fragmenting the cache
    /// so future big requests miss. Exact-first preserves each size class's
    /// own reuse pool (temporal locality in churn is per-size).
    fn take_fit(&mut self, mapped: usize) -> Option<(*mut u8, u32, bool)> {
        let wanted = (mapped / page::PAGE_SIZE) as u32;
        if let Some(i) = self.exact_hot(wanted) {
            return Some(self.remove_hot(i));
        }
        if let Some(i) = self.exact_cold(wanted) {
            return Some(self.remove_cold(i));
        }
        for i in 0..self.len {
            if self.entries[i].1 == wanted {
                return Some(self.remove_hot(i));
            }
        }
        for i in 0..self.cold_len {
            if self.cold[i].1 == wanted {
                return Some(self.remove_cold(i));
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
            return Some(self.remove_hot(i));
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
        cbest.map(|i| self.remove_cold(i))
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
    alloc_large_ex(size, align, false, 0).0
}

#[inline]
pub(crate) unsafe fn map_large_region(mapped: usize) -> (*mut u8, bool) {
    #[cfg(all(unix, feature = "std"))]
    {
        let (base, fresh) =
            crate::arena::commit((mapped / page::PAGE_SIZE) as usize);
        if !base.is_null() {
            return (base, fresh);
        }
        // Arena unavailable or reservation exhausted: this large region
        // costs a real mmap. Counted so the fallback rate is visible in
        // `__diagnostics::volume().arena_fallbacks` rather than inferred
        // from a missing arena commit.
        counters::bump(&counters::VOLUME.arena_fallbacks, 1);
    }
    let base = sys::map_any(mapped);
    (base, !base.is_null())
}

/// Release a large region: back to arena holes when arena-owned (no
/// syscall), true `munmap` otherwise. Arena slices are never unmapped (that
/// would punch holes third parties could claim); legacy mappings need the
/// real unmap plus counter updates.
#[inline]
pub(crate) unsafe fn unmap_or_return(base: *mut u8, mapped: usize) {
    #[cfg(all(unix, feature = "std"))]
    {
        if crate::arena::contains(base, mapped) {
            crate::arena::release(base, (mapped / page::PAGE_SIZE) as usize);
            counters::bump(&counters::VOLUME.arena_parks, 1);
            return;
        }
    }
    if sys::unmap(base, mapped) {
        heap::MAPPED_PAGES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        heap::UNMAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(feature = "telemetry")]
fn note_large_alloc(size: usize) {
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

#[cfg(feature = "telemetry")]
fn note_large_free(requested: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    heap::TELEMETRY.total_frees.fetch_add(1, Relaxed);
    heap::TELEMETRY
        .bytes_out
        .fetch_add(requested as u64, Relaxed);
}

#[inline]
unsafe fn init_large_header(
    hdr: *mut LargeHeader,
    base: *mut u8,
    mapped: usize,
    requested: usize,
) {
    (*hdr).magic = LARGE_MAGIC;
    (*hdr).mapped_size = mapped;
    (*hdr).base = base;
    (*hdr).requested_size = requested;
    (*hdr).registry_next = ptr::null_mut();
}

static LARGE_REGISTRY: sys::Mutex<usize> = sys::Mutex::new(0);

unsafe fn register_legacy_large(hdr: *mut LargeHeader) {
    let mut head = LARGE_REGISTRY.lock();
    (*hdr).registry_next = *head as *mut LargeHeader;
    *head = hdr as usize;
}

unsafe fn unregister_legacy_large(hdr: *mut LargeHeader) {
    let mut head = LARGE_REGISTRY.lock();
    let mut previous: *mut LargeHeader = ptr::null_mut();
    let mut current = *head as *mut LargeHeader;
    while !current.is_null() {
        if current == hdr {
            let next = (*current).registry_next;
            if previous.is_null() {
                *head = next as usize;
            } else {
                (*previous).registry_next = next;
            }
            return;
        }
        previous = current;
        current = (*current).registry_next;
    }
}

unsafe fn legacy_large_contains(p: *mut u8) -> bool {
    let expected = match (p as usize).checked_sub(LARGE_HEADER_SIZE) {
        Some(address) if address & (MIN_ALIGN - 1) == 0 => address as *mut LargeHeader,
        _ => return false,
    };
    let head = LARGE_REGISTRY.lock();
    let mut current = *head as *mut LargeHeader;
    while !current.is_null() {
        if current == expected {
            return true;
        }
        current = (*current).registry_next;
    }
    false
}

unsafe fn large_region_known(p: *mut u8) -> bool {
    #[cfg(all(unix, feature = "std"))]
    if crate::arena::contains(p, 1) {
        return !crate::arena::large_table_get(p).is_null();
    }
    legacy_large_contains(p) && large_header_of(p).is_some()
}

unsafe fn register_large_region(_base: *mut u8, _mapped: usize, hdr: *mut LargeHeader) {
    #[cfg(all(unix, feature = "std"))]
    if crate::arena::contains(_base, _mapped) {
        crate::arena::large_table_set(
            _base,
            (_mapped / page::PAGE_SIZE) as u32,
            hdr,
        );
        return;
    }
    register_legacy_large(hdr);
}

unsafe fn unregister_large_region(_base: *mut u8, _mapped: usize, hdr: *mut LargeHeader) {
    #[cfg(all(unix, feature = "std"))]
    if crate::arena::contains(_base, _mapped) {
        crate::arena::large_table_clear(
            _base,
            (_mapped / page::PAGE_SIZE) as u32,
        );
        return;
    }
    unregister_legacy_large(hdr);
}

/// Returns `(ptr, fresh, known_zeroed)` where `fresh` distinguishes new
/// virtual memory and `known_zeroed` permits calloc to skip its memset.
///
/// `slack` reserves extra address space past the requested `size` (the
/// header still records `size` as the request, so `usable_size` and the
/// in-place growth path both see the reserve). Slack pages are untouched
/// anonymous pages: reserved virtual only, faulted on first write.
unsafe fn alloc_large_ex(
    size: usize,
    align: usize,
    zeroed_requested: bool,
    slack: usize,
) -> (*mut u8, bool, bool) {
    let total = match size
        .checked_add(align)
        .and_then(|v| v.checked_add(LARGE_HEADER_SIZE))
        .and_then(|v| v.checked_add(slack))
    {
        Some(t) => t,
        None => return (ptr::null_mut(), false, false),
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
            let known_zeroed = zeroed_requested && sys::discard(base, region_size);
            let hdr = (ret - LARGE_HEADER_SIZE) as *mut LargeHeader;
            init_large_header(hdr, base, region_size, size);
            register_large_region(base, region_size, hdr);
            #[cfg(feature = "telemetry")]
            note_large_alloc(size);
            return (ret as *mut u8, false, known_zeroed);
        }
        // Alignment made the cached region unusable; drop it (arena-owned
        // slices park in holes, legacy ones truly unmap — counters follow).
        unmap_or_return(base, region_size);
    }

    // Tier 2+3: best-fit region from the sharded recycle cache (hot, then
    // cold). Cold hits need no syscall — virtual survived the discard.
    {
        let taken = {
            let mut c = LARGE_SHARDS[large_shard(mapped_pages as usize, salt)].lock();
            c.take_fit(mapped)
        };
        if let Some((base, pages, zeroed)) = taken {
            let region_size = pages as usize * page::PAGE_SIZE;
            let ret = align_up(base as usize + LARGE_HEADER_SIZE, align);
            if ret + size <= base as usize + region_size {
                let known_zeroed = if zeroed_requested && !zeroed {
                    sys::discard(base, region_size)
                } else {
                    zeroed
                };
                let hdr = (ret - LARGE_HEADER_SIZE) as *mut LargeHeader;
                init_large_header(hdr, base, region_size, size);
                register_large_region(base, region_size, hdr);
                #[cfg(feature = "telemetry")]
                note_large_alloc(size);
                return (ret as *mut u8, false, known_zeroed);
            }
            // Alignment made the cached region unusable; drop it.
            unmap_or_return(base, region_size);
        }
    }

    // Large regions are located by offset header, never by address masking,
    // so kernel page alignment suffices — no over-map/trim tax (unix), and
    // elsewhere map_any is already the optimal primitive.
    let (base, fresh) = map_large_region(mapped);
    if base.is_null() {
        return (ptr::null_mut(), false, false);
    }
    // Count the fresh mapping exactly once here: every success is one
    // kernel mapping op (`MAP_CALLS`); live virtual (`MAPPED_PAGES`) only
    // grows for genuinely new address space — arena hole reuses recommit
    // virtual that is already counted (their release never decremented it).
    // All disposals below balance it via unmap_or_return (arena parks keep
    // it counted; legacy unmaps decrement).
    if fresh {
        heap::MAPPED_PAGES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    heap::MAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // Header lives directly before the user pointer: high alignment can push
    // the user pointer past the first 64 KiB boundary of the region, so the
    // region base is not a reliable place to find it.
    let ret = align_up(base as usize + LARGE_HEADER_SIZE, align);
    if ret + size > base as usize + mapped {
        unmap_or_return(base, mapped);
        return (ptr::null_mut(), false, false);
    }
    let hdr = (ret - LARGE_HEADER_SIZE) as *mut LargeHeader;
    init_large_header(hdr, base, mapped, size);
    register_large_region(base, mapped, hdr);
    #[cfg(feature = "telemetry")]
    note_large_alloc(size);
    (ret as *mut u8, true, true)
}

#[cfg(all(unix, feature = "std", feature = "telemetry"))]
fn note_large_growth(old_size: usize, new_size: usize) {
    let delta = new_size.saturating_sub(old_size) as u64;
    if delta == 0 {
        return;
    }
    use core::sync::atomic::Ordering::Relaxed;
    heap::TELEMETRY.bytes_in.fetch_add(delta, Relaxed);
    let live = heap::TELEMETRY
        .bytes_in
        .load(Relaxed)
        .saturating_sub(heap::TELEMETRY.bytes_out.load(Relaxed));
    heap::TELEMETRY.peak_live_bytes.fetch_max(live, Relaxed);
}

#[cfg(all(unix, feature = "std"))]
#[inline]
unsafe fn try_grow_large_frontier(p: *mut u8, size: usize) -> bool {
    #[cfg(all(unix, feature = "std"))]
    {
        if !crate::arena::contains(p, 1) || !large_region_known(p) {
            return false;
        }
        let hdr = (p as usize - LARGE_HEADER_SIZE) as *mut LargeHeader;
        let old_mapped = (*hdr).mapped_size;
        let (_, validated_mapped) = match large_header_of(p) {
            Some(validated) => validated,
            None => return false,
        };
        if validated_mapped != old_mapped {
            return false;
        }
        let old_requested = (*hdr).requested_size;
        let old_base = (*hdr).base;
        let capacity = match old_mapped.checked_sub(p as usize - old_base as usize) {
            Some(capacity) => capacity,
            None => return false,
        };
        if size <= old_requested {
            return false;
        }
        if size <= capacity {
            (*hdr).requested_size = size;
            #[cfg(feature = "telemetry")]
            note_large_growth(old_requested, size);
            return true;
        }
        let align = pointer_alignment(p);
        let total = match size
            .checked_add(align)
            .and_then(|value| value.checked_add(LARGE_HEADER_SIZE))
        {
            Some(total) => total,
            None => return false,
        };
        let new_mapped = align_up(total.max(LARGE_HEADER_SIZE), page::PAGE_SIZE);
        if new_mapped <= old_mapped {
            return false;
        }
        let old_pages = old_mapped / page::PAGE_SIZE;
        let new_pages = new_mapped / page::PAGE_SIZE;
        let added_pages = match new_pages.checked_sub(old_pages) {
            Some(added) if added <= u32::MAX as usize => added,
            _ => return false,
        };
        if !crate::arena::grow_frontier(old_base, old_pages, new_pages) {
            return false;
        }
        (*hdr).mapped_size = new_mapped;
        (*hdr).requested_size = size;
        let added_base = (old_base as usize + old_mapped) as *mut u8;
        crate::arena::large_table_set(added_base, added_pages as u32, hdr);
        heap::MAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "telemetry")]
        note_large_growth(old_requested, size);
        true
    }
    #[cfg(not(all(unix, feature = "std")))]
    {
        let _ = (p, size);
        false
    }
}

/// Record a relocating `realloc` in the calling thread's batched counters.
///
/// This is deliberately *not* a shared atomic: at four threads, one
/// `fetch_add` per realloc cost 20% of process-global application
/// throughput (233k -> 194k docs/s) because every thread ping-ponged the
/// same cache line tens of millions of times per second. One thread-local
/// add here, one global atomic per 8192 operations in `ThreadCache::publish`.
#[inline]
fn note_realloc(copied: usize, promoted: bool) {
    with_cache(
        |cache| cache.note_realloc(copied, promoted),
        || {
            // No TLS (no_std, or a thread that never allocated): fall back
            // to the global counter directly. This path is cold.
            counters::bump(&counters::VOLUME.realloc_relocations, 1);
            counters::bump(&counters::VOLUME.realloc_copy_bytes, copied as u64);
            if promoted {
                counters::bump(&counters::VOLUME.realloc_promotions, 1);
            }
        },
    );
}

/// Upper bound on the reserve a promoted big block may claim, as a multiple
/// of the size it is being grown to. The big chain tops out at
/// `MAX_BIG_BLOCK`, so any first growth into the upper half of the big range
/// reserves the whole cap and never needs a second relocation.
#[cfg(all(unix, feature = "std"))]
const BIG_GROW_SLACK_FACTOR: usize = 8;

/// Growable-extent promotion for packed big blocks (arena targets).
///
/// Big blocks are carved contiguously out of a shared span, so a cross-class
/// `realloc` growth cannot extend in place: the span's neighbours are live
/// data. The generic path therefore allocates a fresh block and copies. On the
/// *first* cross-class growth we instead move the block once into a large
/// region that reserves slack up to the big cap. Every later big growth then
/// fits the existing mapping and is served in place by
/// `try_grow_large_frontier` — no copy, no syscall, no span traffic.
///
/// The reserve is virtual-only (untouched anonymous pages, faulted on first
/// write), so RSS still tracks live demand. It is bounded to
/// `BIG_GROW_SLACK_FACTOR` x the new size and never exceeds the big cap, and
/// any failure falls back to the ordinary alloc-copy-free path.
#[cfg(all(unix, feature = "std"))]
#[inline]
unsafe fn try_promote_big_grow(p: *mut u8, new_size: usize, align: usize) -> Option<*mut u8> {
    if !crate::arena::contains(p, 1) {
        return None;
    }
    // A live large region (e.g. a previous promotion) is never a big block,
    // even if a stale big-table entry covers its pages: the large table wins.
    if !crate::arena::large_table_get(p).is_null() {
        return None;
    }
    let big = crate::arena::big_table_get(p);
    if big.is_null() {
        return None;
    }
    let big = big.cast::<BigMaster>();
    let bclass = (*big).bclass as usize;
    if bclass >= classes::NUM_BIG {
        return None;
    }
    let old_usable = classes::BIG_CLASSES[bclass];
    // Only a real growth: same-class (and shrink) requests are handled by the
    // identity fast path, never by moving the block.
    if new_size <= old_usable {
        return None;
    }
    let reserve = new_size
        .saturating_mul(BIG_GROW_SLACK_FACTOR)
        .min(classes::MAX_BIG_BLOCK);
    if reserve <= new_size {
        return None;
    }
    let (np, _fresh, _known_zeroed) = alloc_large_ex(new_size, align, false, reserve - new_size);
    if np.is_null() {
        return None;
    }
    let copy = old_usable.min(new_size);
    ptr::copy_nonoverlapping(p, np, copy);
    note_realloc(copy, true);
    dealloc_impl(p);
    Some(np)
}

unsafe fn free_large(p: *mut u8) {
    let hdr = (p as usize - LARGE_HEADER_SIZE) as *mut LargeHeader;
    let mapped = (*hdr).mapped_size;
    let base = (*hdr).base;
    #[cfg(feature = "telemetry")]
    let requested = (*hdr).requested_size;
    let pages = (mapped / page::PAGE_SIZE) as u32;
    unregister_large_region(base, mapped, hdr);

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
        #[cfg(feature = "telemetry")]
        note_large_free(requested);
        return;
    }

    // Tier 2+3: park the region on its shard (hot), else cold with physical
    // dropped, else unmap. The cold discard runs UNDER the shard lock: the
    // region is exclusively ours until unlock, so no concurrent take can
    // hand it out mid-discard and lose user writes (the race that
    // discard-after-unlock had).
    enum Fate {
        Kept,
        Unmap,
    }
    let fate = {
        let mut c = LARGE_SHARDS[large_shard(pages as usize, salt)].lock();
        if c.len < LARGE_SHARD_SLOTS && c.bytes + mapped <= LARGE_SHARD_CAP_BYTES {
            let idx = c.len;
            c.entries[idx] = (base, pages);
            c.hot_zeroed[idx] = false;
            c.len = idx + 1;
            c.bytes += mapped;
            c.index_hot(idx, pages as usize);
            Fate::Kept
        } else if c.cold_len < LARGE_COLD_SLOTS
            && c.cold_bytes + mapped <= LARGE_COLD_CAP_BYTES
        {
            let idx = c.cold_len;
            c.cold[idx] = (base, pages);
            c.cold_len = idx + 1;
            c.cold_bytes += mapped;
            c.index_cold(idx, pages as usize);
            c.cold_zeroed[idx] = sys::discard(base, mapped);
            Fate::Kept
        } else {
            Fate::Unmap
        }
    };
    match fate {
        Fate::Kept => {}
        Fate::Unmap => {
            unmap_or_return(base, mapped);
        }
    }
    #[cfg(feature = "telemetry")]
    note_large_free(requested);
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
    // Small tier first, and as one test: it is the overwhelmingly common
    // case, and every comparison it saves is on the hot path (this
    // function measured 7.6% of a process-global application profile, the
    // single largest allocator entry point). The tier tests below are then
    // all off the hot path.
    if size <= MAX_SMALL_SIZE && align <= MIN_ALIGN {
        return alloc_small(class_for_size(size));
    }
    // Big spans exist only in the arena (unix + std): elsewhere sizes past
    // the medium cap route straight to large, and the big machinery below
    // doesn't exist (gated out, so no dead code either).
    #[cfg(all(unix, feature = "std"))]
    {
        if align > MIN_ALIGN || size > MAX_BIG_BLOCK {
            return alloc_large(size, align);
        }
        if size > MAX_MEDIUM_BLOCK {
            let p = alloc_big(big_class_for_size(size));
            if p.is_null() {
                // Arena unavailable (exhaustion/init failure): legacy large
                // path, which may still hit its caches or map directly.
                return alloc_large(size, align);
            }
            return p;
        }
    }
    #[cfg(not(all(unix, feature = "std")))]
    {
        if align > MIN_ALIGN || size > MAX_MEDIUM_BLOCK {
            return alloc_large(size, align);
        }
    }
    if size > MAX_SMALL_SIZE {
        return alloc_medium(medium_class_for_size(size));
    }
    alloc_small(class_for_size(size))
}

#[inline]
unsafe fn zero_large_allocation(p: *mut u8, size: usize, known_zeroed: bool) {
    if !p.is_null() && !known_zeroed {
        ptr::write_bytes(p, 0, size);
        counters::bump(&counters::VOLUME.zeroed_calls, 1);
        counters::bump(&counters::VOLUME.zeroed_bytes, size as u64);
    }
}

/// Software zeroing of a recycled block: one memset plus the counters that
/// make "how much zeroing did we actually do" measurable (the virgin
/// fast paths above skip both).
#[inline]
unsafe fn note_zeroed_fill(p: *mut u8, size: usize) {
    ptr::write_bytes(p, 0, size);
    counters::bump(&counters::VOLUME.zeroed_calls, 1);
    counters::bump(&counters::VOLUME.zeroed_bytes, size as u64);
}

/// Like `alloc_impl` but zeroes the allocation. Virgin small/medium/big
/// blocks only need their freelist-link word cleared; recycled large regions
/// are memset unless their cache metadata proves they were discarded.
unsafe fn alloc_zeroed_impl(size: usize, align: usize) -> *mut u8 {
    debug_assert!(align.is_power_of_two());
    if size == 0 {
        return align.max(1) as *mut u8;
    }
    if align > MIN_ALIGN || size > MAX_MEDIUM_BLOCK {
        // Big-span range on arena targets falls through to the big branch
        // below; everywhere else (and for true large sizes/alignments)
        // this is the large path.
        #[cfg(not(all(unix, feature = "std")))]
        {
            let (p, _fresh, known_zeroed) = alloc_large_ex(size, align, true, 0);
            zero_large_allocation(p, size, known_zeroed);
            return p;
        }
        #[cfg(all(unix, feature = "std"))]
        if align > MIN_ALIGN || size > MAX_BIG_BLOCK {
            let (p, _fresh, known_zeroed) = alloc_large_ex(size, align, true, 0);
            zero_large_allocation(p, size, known_zeroed);
            return p;
        }
    }
    #[cfg(all(unix, feature = "std"))]
    if size > MAX_MEDIUM_BLOCK {
        let bclass = big_class_for_size(size);
        let (p, virgin) = with_cache(
            |c| c.alloc_big_zeroed(bclass),
            || take_one_big(bclass),
        );
        if p.is_null() {
            let (lp, _fresh, known_zeroed) = alloc_large_ex(size, align, true, 0);
            zero_large_allocation(lp, size, known_zeroed);
            return lp;
        }
        if virgin {
            p.cast::<u64>().write(0);
        } else {
            note_zeroed_fill(p, size);
        }
        return p;
    }
    if size > MAX_SMALL_SIZE {
        let mclass = medium_class_for_size(size);
        let (p, virgin) = with_cache(
            |c| c.alloc_medium_zeroed(mclass),
            || take_one_medium(mclass),
        );
        if !p.is_null() {
            if virgin {
                p.cast::<u64>().write(0);
            } else {
                note_zeroed_fill(p, size);
            }
        }
        return p;
    }
    let class = class_for_size(size);
    let (p, virgin) = with_cache(
        |c| c.alloc_zeroed(class),
        || take_one_small(class),
    );
    if !p.is_null() {
        if virgin {
            // Only the freelist link word is dirty.
            p.cast::<u64>().write(0);
        } else {
            note_zeroed_fill(p, size);
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
    let address = p as usize;
    let header_address = address.checked_sub(LARGE_HEADER_SIZE)?;
    if header_address & (MIN_ALIGN - 1) != 0 {
        return None;
    }
    let hdr = header_address as *const LargeHeader;
    if (*hdr).magic != LARGE_MAGIC {
        return None;
    }
    let mapped = (*hdr).mapped_size;
    let base = (*hdr).base as usize;
    if mapped < LARGE_HEADER_SIZE || mapped & PAGE_MASK != 0 {
        return None;
    }
    let off = (p as usize).wrapping_sub(base);
    if off == 0 || off >= mapped {
        return None;
    }
    Some((base as *mut u8, mapped))
}

#[inline]
fn pointer_alignment(p: *mut u8) -> usize {
    let address = p as usize;
    if address == 0 {
        1
    } else {
        address & address.wrapping_neg()
    }
}

unsafe fn dealloc_impl(p: *mut u8) {
    if p.is_null() {
        return;
    }
    #[cfg(all(unix, feature = "std"))]
    if crate::arena::contains(p, 1) {
        let large = crate::arena::large_table_get(p);
        if !large.is_null() {
            free_large(p);
            return;
        }
        let medium = crate::arena::medium_table_get(p);
        if !medium.is_null() {
            if (*medium).contains(p) {
                dealloc_medium(p, medium);
                return;
            }
            corrupt_pointer();
        }
        let big = crate::arena::big_table_get(p);
        if !big.is_null() {
            if (*big).contains(p) {
                dealloc_big(p, big);
                return;
            }
            corrupt_pointer();
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
        corrupt_pointer();
    }
    if large_region_known(p) {
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
        // Big-span range on arena targets routes below; everywhere else
        // (and for true large sizes/alignments) this is the large path.
        #[cfg(not(all(unix, feature = "std")))]
        {
            #[cfg(debug_assertions)]
            if !large_region_known(p) {
                corrupt_pointer();
            }
            free_large(p);
            return;
        }
        #[cfg(all(unix, feature = "std"))]
        if align > MIN_ALIGN || size > MAX_BIG_BLOCK {
            #[cfg(debug_assertions)]
            if !large_region_known(p) {
                corrupt_pointer();
            }
            free_large(p);
            return;
        }
    }
    #[cfg(all(unix, feature = "std"))]
    if size > MAX_MEDIUM_BLOCK {
        if !crate::arena::contains(p, 1) {
            #[cfg(debug_assertions)]
            if !large_region_known(p) {
                corrupt_pointer();
            }
            free_large(p);
            return;
        }
        let large = crate::arena::large_table_get(p);
        if !large.is_null() {
            free_large(p);
            return;
        }
        let big = crate::arena::big_table_get(p);
        if big.is_null() {
            corrupt_pointer();
        }
        #[cfg(debug_assertions)]
        if !(*big).contains(p) {
            corrupt_pointer();
        }
        dealloc_big(p, big);
        return;
    }
    if size > MAX_SMALL_SIZE {
        // Layout-routed: class comes from size (LUT), never from a header
        // load — the SPAN_MAGIC load + sub-header follow was ~48% of free
        // cycles on mixed-all (perf annotate 2026-09-23). The span is only
        // needed on the cold no-TLS fallback.
        let mclass = medium_class_for_size(size);
        #[cfg(debug_assertions)]
        {
            let span = SpanMaster::of(p);
            if span.is_null() || !(*span).contains(p) || (*span).mclass as usize != mclass {
                corrupt_pointer();
            }
        }
        with_cache(
            |c| c.dealloc_medium(p, mclass),
            || {
                let span = SpanMaster::of(p);
                *p.cast::<*mut u8>() = ptr::null_mut();
                MEDIUM_HEAP.release_blocks(mclass, span, p, p, 1);
            },
        );
        return;
    }
    // Small: class from size; page header only on the cold fallback path.
    let class = class_for_size(size);
    #[cfg(debug_assertions)]
    {
        let page = page::PageHeader::of(p);
        if (*page).magic != page::PAGE_MAGIC || (*page).class as usize != class {
            corrupt_pointer();
        }
    }
    with_cache(
        |c| c.dealloc(p, class),
        || {
            let page = page::PageHeader::of(p);
            *p.cast::<*mut u8>() = ptr::null_mut();
            HEAP.release_blocks(page, p, 1);
        },
    );
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
        // Same-class identity fires only for real (nonzero) allocations: a
        // zero-size layout's pointer is dangling by Rust convention, and
        // handing it back for a nonzero size would alias address ~align as
        // live memory. Zero sizes fall through to fresh alloc below (the
        // copy is skipped and the dealloc is a no-op for them).
        if !p.is_null()
            && layout.size() != 0
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
        // Big same-class resize is identity too (big spans never move;
        // arena targets only — elsewhere these sizes are large-routed and
        // never reach here as big).
        #[cfg(all(unix, feature = "std"))]
        if !p.is_null()
            && layout.align() <= MIN_ALIGN
            && layout.size() > MAX_MEDIUM_BLOCK
            && layout.size() <= MAX_BIG_BLOCK
            && new_size > MAX_MEDIUM_BLOCK
            && new_size <= MAX_BIG_BLOCK
            && big_class_for_size(layout.size()) == big_class_for_size(new_size)
        {
            return p;
        }
        // Big/large growth. Two cases share this size range:
        //  - a packed big block crossing classes: promote once into a slack
        //    reserve so the rest of the doubling chain runs in place;
        //  - an already-promoted (large) block whose requested size still sits
        //    in the big range: grow it in place inside its reserve.
        // Arena targets only; elsewhere these sizes are large-routed and the
        // frontier path below handles them.
        #[cfg(all(unix, feature = "std"))]
        if !p.is_null()
            && layout.align() <= MIN_ALIGN
            && layout.size() > MAX_MEDIUM_BLOCK
            && new_size > MAX_MEDIUM_BLOCK
        {
            if new_size <= MAX_BIG_BLOCK && layout.size() <= MAX_BIG_BLOCK {
                if let Some(np) = try_promote_big_grow(p, new_size, layout.align()) {
                    return np;
                }
            }
            if try_grow_large_frontier(p, new_size) {
                return p;
            }
        }
        #[cfg(all(unix, feature = "std"))]
        if !p.is_null()
            && (layout.align() > MIN_ALIGN || layout.size() > MAX_BIG_BLOCK)
            && try_grow_large_frontier(p, new_size)
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
        if copy > 0 {
            ptr::copy_nonoverlapping(p, new_p, copy);
            note_realloc(copy, false);
        }
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

/// Allocate `size` bytes with alignment 16. Returns null on failure, when
/// `size` exceeds [`isize::MAX`], or when `size` is zero (C allows either
/// null or a unique freeable pointer for zero size; null keeps `free` /
/// `realloc` / `usable_size` on their null contracts instead of probing
/// headers of a dangling pointer).
///
/// # Safety
/// Returned pointer must be freed with [`free`]/[`realloc`], never used after.
pub unsafe fn malloc(size: usize) -> *mut u8 {
    if size == 0 || size > isize::MAX as usize {
        return ptr::null_mut();
    }
    alloc_impl(size, 1)
}

/// Allocate `nmemb * size` zero-initialized bytes. Returns null on overflow,
/// exhaustion, or zero total (same zero-size rule as [`malloc`]).
///
/// # Safety
/// Same ownership rules as [`malloc`].
pub unsafe fn calloc(nmemb: usize, size: usize) -> *mut u8 {
    let total = match nmemb.checked_mul(size) {
        Some(t) if t > 0 && t <= isize::MAX as usize => t,
        _ => return ptr::null_mut(),
    };
    alloc_zeroed_impl(total, 1)
}

/// Resize an allocation from [`malloc`]/[`calloc`]/[`realloc`].
/// Returns null (leaving the original intact) on failure. Resizing to zero
/// frees the original and returns null (C semantics).
///
/// # Safety
/// `p` must be null or a live allocation of this allocator.
pub unsafe fn realloc(p: *mut u8, size: usize) -> *mut u8 {
    if size > isize::MAX as usize {
        return ptr::null_mut();
    }
    if size == 0 {
        free(p);
        return ptr::null_mut();
    }
    if p.is_null() {
        return malloc(size);
    }
    if (p as usize) < LARGE_HEADER_SIZE {
        return malloc(size);
    }
    // Large-offset check first (fault-safe for every live pointer; masked
    // reads can dangle outside unaligned large regions — see dealloc_impl).
    // Large resizes always go alloc-copy-free below via usable_size.
    #[cfg(all(unix, feature = "std"))]
    let old_arena = crate::arena::contains(p, 1);
    #[cfg(all(unix, feature = "std"))]
    let old_large_ok = if old_arena {
        !crate::arena::large_table_get(p).is_null()
    } else {
        legacy_large_contains(p)
    };
    #[cfg(not(all(unix, feature = "std")))]
    let old_large_ok = legacy_large_contains(p);
    #[cfg(all(unix, feature = "std"))]
    let old_big: Option<*mut u8> = if old_arena {
        let big = crate::arena::big_table_get(p);
        if !big.is_null() {
            #[cfg(debug_assertions)]
            if !(*big).contains(p) {
                corrupt_pointer();
            }
            Some(big.cast())
        } else {
            None
        }
    } else {
        None
    };
    #[cfg(not(all(unix, feature = "std")))]
    let old_big: Option<*mut u8> = None;
    #[cfg(all(unix, feature = "std"))]
    let old_medium: Option<*mut u8> = if old_arena {
        let medium = crate::arena::medium_table_get(p);
        if !medium.is_null() {
            #[cfg(debug_assertions)]
            if !(*medium).contains(p) {
                corrupt_pointer();
            }
            Some(medium.cast())
        } else {
            None
        }
    } else {
        None
    };
    #[cfg(not(all(unix, feature = "std")))]
    let old_medium: Option<*mut u8> = None;

    #[cfg(all(unix, feature = "std"))]
    if old_large_ok && old_arena && try_grow_large_frontier(p, size) {
        return p;
    }

    if old_big.is_none() && old_medium.is_none() && !old_large_ok {
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
        let span = SpanMaster::of(p);
        let old_span_ok = !span.is_null() && (*span).contains(p);
        if old_span_ok && size != 0 {
            let old_mclass = (*span).mclass as usize;
            if size > classes::MAX_SMALL_SIZE
                && size <= classes::MAX_MEDIUM_BLOCK
                && medium_class_for_size(size) == old_mclass
            {
                return p;
            }
        }
    }
    #[cfg(all(unix, feature = "std"))]
    if let Some(big) = old_big {
        if size != 0 {
            let big = big.cast::<BigMaster>();
            let old_bclass = (*big).bclass as usize;
            if size > classes::MAX_MEDIUM_BLOCK
                && size <= classes::MAX_BIG_BLOCK
                && big_class_for_size(size) == old_bclass
            {
                return p;
            }
            // First cross-class growth: relocate once into a slack reserve so
            // the rest of the doubling chain runs entirely in place.
            if size > classes::MAX_MEDIUM_BLOCK && size <= classes::MAX_BIG_BLOCK {
                if let Some(np) = try_promote_big_grow(p, size, pointer_alignment(p)) {
                    return np;
                }
            }
        }
    }
    #[cfg(all(unix, feature = "std"))]
    if let Some(medium) = old_medium {
        if size != 0 {
            let medium = medium.cast::<SpanMaster>();
            let old_mclass = (*medium).mclass as usize;
            if size > classes::MAX_SMALL_SIZE
                && size <= classes::MAX_MEDIUM_BLOCK
                && medium_class_for_size(size) == old_mclass
            {
                return p;
            }
        }
    }
    let new_p = if old_large_ok {
        alloc_impl(size, pointer_alignment(p))
    } else {
        malloc(size)
    };
    if !new_p.is_null() && size != 0 {
        let old_size = usable_size(p);
        let copy = old_size.min(size);
        ptr::copy_nonoverlapping(p, new_p, copy);
        note_realloc(copy, false);
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
/// Returns null on invalid alignment, overflow, or zero size (same
/// zero-size rule as [`malloc`]).
///
/// # Safety
/// Same ownership rules as [`malloc`].
pub unsafe fn aligned_alloc(align: usize, size: usize) -> *mut u8 {
    if align == 0 || !align.is_power_of_two() || size == 0 || size > isize::MAX as usize {
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
    if (p as usize) < LARGE_HEADER_SIZE {
        return 0;
    }
    #[cfg(all(unix, feature = "std"))]
    if crate::arena::contains(p, 1) {
        let large = crate::arena::large_table_get(p);
        if !large.is_null() {
            return (*large).mapped_size.saturating_sub(p as usize - (*large).base as usize);
        }
        let medium = crate::arena::medium_table_get(p);
        if !medium.is_null() {
            if (*medium).contains(p) {
                return classes::MEDIUM_CLASSES[(*medium).mclass as usize];
            }
            return 0;
        }
        let big = crate::arena::big_table_get(p);
        if !big.is_null() {
            if (*big).contains(p) {
                return classes::BIG_CLASSES[(*big).bclass as usize];
            }
            return 0;
        }
        let base = p as usize & !PAGE_MASK;
        if *(base as *const u64) == page::PAGE_MAGIC {
            let page = base as *mut page::PageHeader;
            return classes::CLASSES[(*page).class as usize];
        }
        let span = SpanMaster::of(p);
        if !span.is_null() && (*span).contains(p) {
            return classes::MEDIUM_CLASSES[(*span).mclass as usize];
        }
        return 0;
    }
    if legacy_large_contains(p) {
        let hdr = (p as usize - LARGE_HEADER_SIZE) as *const LargeHeader;
        return (*hdr).mapped_size - (p as usize - (*hdr).base as usize);
    }
    let base = p as usize & !PAGE_MASK;
    if *(base as *const u64) == page::PAGE_MAGIC {
        let page = base as *mut page::PageHeader;
        classes::CLASSES[(*page).class as usize]
    } else {
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
    /// Live virtual mappings from the OS: one per fresh take, regardless of
    /// mapping size (a 16-page span counts the same as a 1-page small page),
    /// including large allocations' regions. Arena hole reuses recommit
    /// already-counted virtual and don't move it; only genuinely new address
    /// space increments, true unmaps decrement.
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

#[doc(hidden)]
pub fn __debug_exit_flush_count() -> u64 {
    thread_exit::flush_count()
}

/// Span-vs-large map split for tuning (see ROADMAP P2). Returns
/// `(span_maps, span_unmaps, small_maps, small_unmaps, arena_commits,
/// arena_reuses, big_maps, big_unmaps)`; large maps are `map_calls -
/// span_maps - small_maps - big_maps`. Big counters read zero where big
/// spans don't exist (non-arena targets).
/// Hidden: not semver-covered, may change or vanish.
#[doc(hidden)]
pub fn __debug_map_split() -> (u64, u64, u64, u64, u64, u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    #[cfg(all(unix, feature = "std"))]
    let (acommits, areuses, _) = crate::arena::stats();
    #[cfg(not(all(unix, feature = "std")))]
    let (acommits, areuses) = (0, 0);
    #[cfg(all(unix, feature = "std"))]
    let (big_maps, big_unmaps) = (
        heap::BIG_MAP_CALLS.load(Relaxed),
        heap::BIG_UNMAP_CALLS.load(Relaxed),
    );
    #[cfg(not(all(unix, feature = "std")))]
    let (big_maps, big_unmaps) = (0, 0);
    (
        heap::SPAN_MAP_CALLS.load(Relaxed),
        heap::SPAN_UNMAP_CALLS.load(Relaxed),
        heap::SMALL_MAP_CALLS.load(Relaxed),
        heap::SMALL_UNMAP_CALLS.load(Relaxed),
        acommits,
        areuses,
        big_maps,
        big_unmaps,
    )
}

/// Arena internals for tuning (see REMAINING_PLAN §3). Returns
/// `(abandoned, bump_high_water_bytes)`: cumulative hole-store overflow
/// parks (virtual retained, never reused) and the monotonic reservation
/// frontier (the reservation-sizing validation input).
/// Hidden: not semver-covered, may change or vanish.
#[doc(hidden)]
pub fn __debug_arena_detail() -> (u64, u64) {
    #[cfg(all(unix, feature = "std"))]
    {
        let (_, _, abandoned) = crate::arena::stats();
        (abandoned, crate::arena::high_water())
    }
    #[cfg(not(all(unix, feature = "std")))]
    {
        (0, 0)
    }
}

#[doc(hidden)]
pub fn __debug_arena_hole_stats() -> (u64, u64, u64, u64) {
    #[cfg(all(unix, feature = "std"))]
    {
        crate::arena::hole_stats()
    }
    #[cfg(not(all(unix, feature = "std")))]
    {
        (0, 0, 0, 0)
    }
}

/// Diagnostic counters for tuning and benchmark instrumentation.
///
/// `volume()` counts work (refills, flushes, copied bytes, purges, arena
/// fallbacks, ...). Those counters live on slow paths, so they are always
/// compiled and always updated — a production build without the
/// `telemetry` feature still reports them. Nanosecond *timings* need a
/// clock read and therefore require the feature; see
/// [`crate::telemetry::timing`].
///
/// Hidden: not semver-covered, may change or vanish. Field names come
/// from [`crate::counters::VOLUME_FIELDS`] so a consumer can label
/// [`volume_raw`] columns.
#[doc(hidden)]
pub mod __diagnostics {
    pub use crate::counters::{volume, volume_raw, Volume, VOLUME_FIELDS};
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
        /// Live virtual mappings from the OS (one per fresh take, any size;
        /// same counting rule as `Stats::mapped_pages`).
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

    /// Cumulative wall time spent inside instrumented allocator regions.
    ///
    /// Unlike [`Telemetry`], these need a clock read per event, so they
    /// exist only in `telemetry` builds (the feature this module lives
    /// behind). Divide by the matching call count in
    /// [`crate::__diagnostics::volume`] for a mean; e.g.
    /// `lock_wait_ns / small_refills` is the average time a small refill
    /// spent waiting for its class lock.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Timing {
        /// Wall time inside heap-mutex acquisitions. Divide by
        /// `Volume::heap_lock_acquisitions` for a mean.
        pub lock_wait_ns: u64,
        /// Wall time inside `discard` (madvise / `VirtualAlloc` release).
        pub purge_ns: u64,
        /// Wall time inside thread-exit cache flushes.
        pub exit_flush_ns: u64,
    }

    /// Read the cumulative timing counters. All zero in `no_std` builds,
    /// which have no clock to read.
    pub fn timing() -> Timing {
        let t = crate::counters::timing_snapshot();
        Timing {
            lock_wait_ns: t.lock_wait_ns,
            purge_ns: t.purge_ns,
            exit_flush_ns: t.exit_flush_ns,
        }
    }
}

/// Set the base per-thread cache retention budget in bytes (default 32 MiB).
///
/// Medium and big tiers receive a separate allowance that scales with this
/// value. Lower it to trade allocation speed for resident memory on
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
/// until the pages naturally die or a later slow path reclaims them. The
/// bounded retirement queue is drained before this call. With `std` disabled
/// this flushes the single global cache.
pub fn flush_current_thread() {
    tls::flush();
}

/// White-box coverage for the counter wiring that integration tests cannot
/// provoke cheaply: the retention policy needs megabytes of churn before it
/// discards anything, but the instrumentation itself sits on two lines.
#[cfg(all(test, feature = "std", any(unix, windows)))]
mod counter_wiring_tests {
    use super::*;
    use crate::counters::{volume, VOLUME};

    #[test]
    fn discard_counts_calls_and_bytes() {
        let before = volume();
        let pages = 4 * page::PAGE_SIZE;
        // SAFETY: fresh mapping, discarded before it is released.
        unsafe {
            let p = sys::map(pages);
            assert!(!p.is_null());
            sys::discard(p, pages);
            assert!(sys::unmap(p, pages));
        }
        let after = volume();
        assert!(after.purge_calls > before.purge_calls, "discard uncounted");
        assert!(
            after.purge_bytes - before.purge_bytes >= pages as u64,
            "discard bytes uncounted: {} < {pages}",
            after.purge_bytes - before.purge_bytes
        );
    }

    #[test]
    fn mutex_lock_counts_acquisitions() {
        let before = crate::counters::get(&VOLUME.heap_lock_acquisitions);
        static M: crate::sys::Mutex<u32> = crate::sys::Mutex::new(0);
        {
            let mut g = M.lock();
            *g += 1;
        }
        let after = crate::counters::get(&VOLUME.heap_lock_acquisitions);
        assert_eq!(after, before + 1, "lock acquisition uncounted");
    }

    #[cfg(all(unix, feature = "std"))]
    #[test]
    fn arena_region_round_trip_counts_park_not_fallback() {
        let before = volume();
        let bytes = 4 * page::PAGE_SIZE;
        // SAFETY: region is released through the same helper below.
        unsafe {
            let (base, _fresh) = map_large_region(bytes);
            assert!(!base.is_null(), "arena commit failed");
            unmap_or_return(base, bytes);
        }
        let after = volume();
        #[cfg(all(unix, feature = "std"))]
        {
            assert!(
                after.arena_parks > before.arena_parks,
                "arena release not counted as a park"
            );
            assert_eq!(
                after.arena_fallbacks, before.arena_fallbacks,
                "arena commit succeeded, so there must be no legacy fallback"
            );
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod large_cache_tests {
    use super::*;
    use crate::page::PAGE_SIZE;

    fn add_hot(cache: &mut LargeRegionCache, pages: u32) -> *mut u8 {
        let index = cache.len;
        let base = (0x1000 + index * PAGE_SIZE) as *mut u8;
        cache.entries[index] = (base, pages);
        cache.hot_zeroed[index] = false;
        cache.len += 1;
        cache.bytes += pages as usize * PAGE_SIZE;
        cache.index_hot(index, pages as usize);
        base
    }

    fn add_cold(cache: &mut LargeRegionCache, pages: u32) -> *mut u8 {
        let index = cache.cold_len;
        let base = (0x200000 + index * PAGE_SIZE) as *mut u8;
        cache.cold[index] = (base, pages);
        cache.cold_zeroed[index] = true;
        cache.cold_len += 1;
        cache.cold_bytes += pages as usize * PAGE_SIZE;
        cache.index_cold(index, pages as usize);
        base
    }

    #[test]
    fn zeroing_decision_honors_known_zero_state() {
        let mut bytes = [0xA5; 64];
        unsafe {
            zero_large_allocation(bytes.as_mut_ptr(), bytes.len(), false);
        }
        assert!(bytes.iter().all(|&byte| byte == 0));

        bytes.fill(0xA5);
        unsafe {
            zero_large_allocation(bytes.as_mut_ptr(), bytes.len(), true);
        }
        assert!(bytes.iter().all(|&byte| byte == 0xA5));
    }

    #[test]
    fn exact_precedence_and_accounting() {
        let mut cache = LargeRegionCache::new();
        let hot_four = add_hot(&mut cache, 4);
        let hot_eight = add_hot(&mut cache, 8);
        let cold_four = add_cold(&mut cache, 4);
        let cold_six = add_cold(&mut cache, 6);

        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((hot_four, 4, false)));
        let replacement = add_hot(&mut cache, 4);
        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((replacement, 4, false)));
        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((cold_four, 4, true)));
        assert_eq!(cache.take_fit(6 * PAGE_SIZE), Some((cold_six, 6, true)));
        assert_eq!(cache.take_fit(6 * PAGE_SIZE), Some((hot_eight, 8, false)));
        assert_eq!(cache.len, 0);
        assert_eq!(cache.bytes, 0);
        assert_eq!(cache.cold_len, 0);
        assert_eq!(cache.cold_bytes, 0);
    }

    #[test]
    fn swap_removal_repairs_exact_index() {
        let mut cache = LargeRegionCache::new();
        let first = add_hot(&mut cache, 4);
        let second = add_hot(&mut cache, 4);
        let third = add_hot(&mut cache, 4);
        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((first, 4, false)));
        assert_eq!(cache.hot_exact[4], 0);
        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((third, 4, false)));
        assert_eq!(cache.hot_exact[4], 0);
        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((second, 4, false)));
        assert_eq!(cache.hot_exact[4], EMPTY_LARGE_INDEX);

        let cold_first = add_cold(&mut cache, 4);
        let cold_second = add_cold(&mut cache, 4);
        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((cold_first, 4, true)));
        assert_eq!(cache.cold_exact[4], 0);
        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((cold_second, 4, true)));
        assert_eq!(cache.cold_exact[4], EMPTY_LARGE_INDEX);
    }

    #[test]
    fn stale_index_and_page_boundary_fall_back() {
        let mut cache = LargeRegionCache::new();
        let five = add_hot(&mut cache, 5);
        let four = add_hot(&mut cache, 4);
        cache.hot_exact[4] = 0;
        assert_eq!(cache.take_fit(4 * PAGE_SIZE), Some((four, 4, false)));
        assert_eq!(cache.hot_exact[4], EMPTY_LARGE_INDEX);
        assert_eq!(cache.take_fit(5 * PAGE_SIZE), Some((five, 5, false)));

        let mut boundary = LargeRegionCache::new();
        let large = add_hot(&mut boundary, LARGE_EXACT_MAX_PAGES as u32 + 1);
        assert_eq!(
            boundary.hot_exact[LARGE_EXACT_MAX_PAGES],
            EMPTY_LARGE_INDEX
        );
        assert_eq!(
            boundary.take_fit((LARGE_EXACT_MAX_PAGES as u32 + 1) as usize * PAGE_SIZE),
            Some((large, LARGE_EXACT_MAX_PAGES as u32 + 1, false))
        );
        let indexed = add_hot(&mut boundary, LARGE_EXACT_MAX_PAGES as u32);
        assert_eq!(boundary.hot_exact[LARGE_EXACT_MAX_PAGES], 0);
        assert_eq!(
            boundary.take_fit(LARGE_EXACT_MAX_PAGES * PAGE_SIZE),
            Some((indexed, LARGE_EXACT_MAX_PAGES as u32, false))
        );
    }
}

#[cfg(all(test, feature = "std"))]
mod header_probe_tests {
    use super::*;
    use crate::page::{LARGE_HEADER_SIZE, LARGE_MAGIC, PAGE_SIZE};

    /// Build a synthetic large region in a stack-ish buffer: header at
    /// `base`, user pointer 32 B later. Layout matches alloc_large_ex
    /// (header strictly below user ptr, mapped_size 64 KiB-multiple).
    struct FakeLarge {
        buf: Vec<u8>,
        base_off: usize,
    }

    impl FakeLarge {
        /// `mapped` must be a nonzero 64 KiB multiple; `user_off` is the
        /// user-pointer offset from base (must be > LARGE_HEADER_SIZE so
        /// the header sits strictly below `p`).
        fn new(mapped: usize, user_off: usize) -> Self {
            assert!(mapped > 0 && mapped & (PAGE_SIZE - 1) == 0);
            assert!(user_off >= LARGE_HEADER_SIZE && user_off < mapped);
            let mut buf = vec![0u8; mapped + PAGE_SIZE];
            // Place header at user_off - LARGE_HEADER_SIZE so large_header_of
            // finds it when probing the user pointer.
            let hdr = user_off - LARGE_HEADER_SIZE;
            unsafe {
                let h = buf.as_mut_ptr().add(hdr).cast::<LargeHeader>();
                (*h).magic = LARGE_MAGIC;
                (*h).mapped_size = mapped;
                (*h).base = buf.as_mut_ptr();
            }
            FakeLarge {
                buf,
                base_off: hdr,
            }
        }

        fn user(&mut self) -> *mut u8 {
            unsafe { self.buf.as_mut_ptr().add(self.base_off + LARGE_HEADER_SIZE) }
        }
    }

    #[test]
    fn large_header_accepts_well_formed() {
        let mut f = FakeLarge::new(PAGE_SIZE, LARGE_HEADER_SIZE + 64);
        let p = f.user();
        let got = unsafe { large_header_of(p) };
        assert!(got.is_some(), "valid header rejected");
        let (base, mapped) = got.unwrap();
        assert_eq!(mapped, PAGE_SIZE);
        assert_eq!(base as *const u8, f.buf.as_ptr());
    }

    #[test]
    fn large_header_rejects_bad_magic() {
        let mut f = FakeLarge::new(PAGE_SIZE, LARGE_HEADER_SIZE + 64);
        unsafe {
            let h = f.buf.as_mut_ptr().add(f.base_off).cast::<LargeHeader>();
            (*h).magic = 0xDEAD_BEEF_0000_0000;
        }
        let p = f.user();
        assert!(unsafe { large_header_of(p) }.is_none());
    }

    #[test]
    fn large_header_rejects_unmapped_size() {
        // Zero size and non-64K-multiple size both fail closed.
        for bad in [0usize, 4096, PAGE_SIZE + 1] {
            let mut f = FakeLarge::new(PAGE_SIZE, LARGE_HEADER_SIZE + 64);
            unsafe {
                let h = f.buf.as_mut_ptr().add(f.base_off).cast::<LargeHeader>();
                (*h).mapped_size = bad;
            }
            let p = f.user();
            assert!(
                unsafe { large_header_of(p) }.is_none(),
                "mapped_size {} accepted",
                bad
            );
        }
    }

    #[test]
    fn large_header_rejects_pointer_past_mapped_end() {
        // Probe a pointer whose header-slot lies in zeroed memory past the
        // region: magic miss (never reads out of bounds — header is at p-32,
        // still inside buf).
        let mut f = FakeLarge::new(PAGE_SIZE, LARGE_HEADER_SIZE + 64);
        let p = unsafe { f.buf.as_mut_ptr().add(PAGE_SIZE + 128) };
        assert!(unsafe { large_header_of(p) }.is_none());
        // Craft a header at the real slot whose base is *above* p, so
        // wrapping_sub yields a huge off >= mapped → reject.
        unsafe {
            let probe = f.buf.as_mut_ptr().add(PAGE_SIZE + 128);
            let h = probe.sub(LARGE_HEADER_SIZE).cast::<LargeHeader>();
            (*h).magic = LARGE_MAGIC;
            (*h).mapped_size = PAGE_SIZE;
            (*h).base = probe.add(64);
        }
        let p = unsafe { f.buf.as_mut_ptr().add(PAGE_SIZE + 128) };
        assert!(
            unsafe { large_header_of(p) }.is_none(),
            "base above p must reject (wrapping off >= mapped)"
        );
    }

    #[test]
    fn large_header_rejects_base_equal_to_user() {
        // off == 0 fails (header would overlap user pointer).
        let mut f = FakeLarge::new(PAGE_SIZE, LARGE_HEADER_SIZE);
        unsafe {
            let h = f.buf.as_mut_ptr().add(f.base_off).cast::<LargeHeader>();
            (*h).base = f.buf.as_mut_ptr().add(f.base_off + LARGE_HEADER_SIZE);
        }
        // user ptr = base_off + 32; header.base set to same → off==0.
        let p = f.user();
        // Note: large_header_of uses (*hdr).base, not the struct location.
        assert!(
            unsafe { large_header_of(p) }.is_none(),
            "off==0 must reject"
        );
    }

    #[cfg(all(unix, feature = "std"))]
    #[test]
    fn layout_dealloc_accepts_legacy_large_region() {
        unsafe {
            let mapped = 2 * PAGE_SIZE;
            let base = crate::sys::map_any(mapped);
            assert!(!base.is_null());
            let p = base.add(LARGE_HEADER_SIZE);
            let hdr = p.sub(LARGE_HEADER_SIZE).cast::<LargeHeader>();
            init_large_header(hdr, base, mapped, 70_000);
            register_large_region(base, mapped, hdr);
            dealloc_with_layout(p, 70_000, MIN_ALIGN);
        }
    }

    #[test]
    fn no_tls_fallback_refills_return_one_block() {
        unsafe {
            let (p, _) = take_one_small(0);
            assert!(!p.is_null());
            assert!((*p.cast::<*mut u8>()).is_null());
            HEAP.release_blocks(page::PageHeader::of(p), p, 1);

            let mclass = medium_class_for_size(MAX_SMALL_SIZE + 1);
            let (p, _) = take_one_medium(mclass);
            assert!(!p.is_null());
            assert!((*p.cast::<*mut u8>()).is_null());
            let span = SpanMaster::of(p);
            assert!(!span.is_null());
            MEDIUM_HEAP.release_blocks(mclass, span, p, p, 1);

            #[cfg(all(unix, feature = "std"))]
            {
                let bclass = big_class_for_size(MAX_MEDIUM_BLOCK + 1);
                let (p, _) = take_one_big(bclass);
                assert!(!p.is_null());
                assert!((*p.cast::<*mut u8>()).is_null());
                let span = crate::arena::big_table_get(p);
                assert!(!span.is_null());
                BIG_HEAP.release_blocks(span, p, 1);
            }
        }
    }
}
