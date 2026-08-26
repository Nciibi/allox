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

use crate::classes::{class_for_size, MAX_SMALL_SIZE, MIN_ALIGN};
use crate::page::{align_up, LargeHeader, LARGE_HEADER_SIZE, LARGE_MAGIC, PAGE_MASK};
use core::alloc::GlobalAlloc;
use core::ptr;
use heap::HEAP;

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

/// Cache of recently freed large regions, recycled on the next matching
/// large allocation instead of paying unmap+map syscalls. Fixed table —
/// the allocator must never allocate internally. Worst-case retention is
/// `LARGE_CACHE_CAP_BYTES`.
const LARGE_CACHE_SLOTS: usize = 64;
const LARGE_CACHE_CAP_BYTES: usize = 64 * 1024 * 1024;

struct LargeRegionCache {
    len: usize,
    bytes: usize,
    entries: [(*mut u8, u32); LARGE_CACHE_SLOTS], // (base, mapped_pages)
}

// Raw pointers are only touched while holding the enclosing mutex.
unsafe impl Send for LargeRegionCache {}

impl LargeRegionCache {
    const fn new() -> Self {
        LargeRegionCache {
            len: 0,
            bytes: 0,
            entries: [(ptr::null_mut(), 0); LARGE_CACHE_SLOTS],
        }
    }
}

static LARGE_CACHE: sys::Mutex<LargeRegionCache> =
    sys::Mutex::new(LargeRegionCache::new());

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

    // Best-fit region from the recycle cache.
    {
        let mut c = LARGE_CACHE.lock();
        let mut best: Option<usize> = None;
        for i in 0..c.len {
            let (_, pages) = c.entries[i];
            if (pages as usize) * page::PAGE_SIZE >= mapped
                && best.map_or(true, |b| c.entries[i].1 < c.entries[b].1)
            {
                best = Some(i);
            }
        }
        if let Some(i) = best {
            let last = c.len - 1;
            let (base, pages) = (
                c.entries[i].0,
                core::mem::replace(&mut c.entries[i].1, 0),
            );
            c.entries[i] = c.entries[last];
            c.len = last;
            c.bytes -= pages as usize * page::PAGE_SIZE;
            drop(c);
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

    let base = sys::map(mapped);
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

    // Park the region for reuse instead of unmapping.
    let mut unmap_now = false;
    {
        let mut c = LARGE_CACHE.lock();
        let slot_ok = c.len < LARGE_CACHE_SLOTS
            && c.bytes + mapped <= LARGE_CACHE_CAP_BYTES;
        if slot_ok {
            let idx = c.len;
            c.entries[idx] = (base, (mapped / page::PAGE_SIZE) as u32);
            c.len = idx + 1;
            c.bytes += mapped;
        } else {
            unmap_now = true;
        }
    }
    if unmap_now {
        sys::unmap(base, mapped);
        heap::MAPPED_PAGES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        heap::UNMAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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
    if align > MIN_ALIGN || size > MAX_SMALL_SIZE {
        return alloc_large(size, align);
    }
    alloc_small(class_for_size(size))
}

/// Like `alloc_impl` but zeroes the allocation. Virgin small blocks only
/// need their freelist-link word cleared; recycled large regions are memset.
unsafe fn alloc_zeroed_impl(size: usize, align: usize) -> *mut u8 {
    debug_assert!(align.is_power_of_two());
    if size == 0 {
        return align.max(1) as *mut u8;
    }
    if align > MIN_ALIGN || size > MAX_SMALL_SIZE {
        let (p, fresh) = alloc_large_ex(size, align);
        if !p.is_null() && !fresh {
            // Recycled region: dirtied by its previous life.
            ptr::write_bytes(p, 0, size);
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

unsafe fn dealloc_impl(p: *mut u8) {
    if p.is_null() {
        return;
    }
    let masked_magic = *((p as usize & !PAGE_MASK) as *const u64);
    if masked_magic == page::PAGE_MAGIC {
        dealloc_small(p);
        return;
    }
    let hdr = (p as usize - LARGE_HEADER_SIZE) as *const LargeHeader;
    if (*hdr).magic == LARGE_MAGIC {
        free_large(p);
        return;
    }
    corrupt_pointer()
}

unsafe impl GlobalAlloc for Allox {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        alloc_impl(layout.size(), layout.align())
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: core::alloc::Layout) {
        if layout.size() == 0 {
            return;
        }
        dealloc_impl(p)
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
    let base = p as usize & !PAGE_MASK;
    if *(base as *const u64) == page::PAGE_MAGIC {
        let page = base as *mut page::PageHeader;
        classes::CLASSES[(*page).class as usize]
    } else if (*(p.wrapping_sub(LARGE_HEADER_SIZE) as *const LargeHeader)).magic == LARGE_MAGIC {
        let hdr = (p as usize - LARGE_HEADER_SIZE) as *const LargeHeader;
        (*hdr).mapped_size - (p as usize - (*hdr).base as usize)
    } else {
        0
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
        /// Allocation count per size class; index `i` covers class `i`
        /// whose block size is internal but stable for a given build.
        pub per_class_allocs: [u64; 64],
    }

    /// Read the current telemetry snapshot.
    pub fn snapshot() -> Telemetry {
        let t = &crate::heap::TELEMETRY;
        let total_allocs = t.total_allocs.load(Relaxed);
        let total_frees = t.total_frees.load(Relaxed);
        let allocated_bytes = t.bytes_in.load(Relaxed);
        let freed_bytes = t.bytes_out.load(Relaxed);
        let mut per_class = [0u64; 64];
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
                per_class_allocs: [0; 64],
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
