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
//! let v: Vec<u8> = (0..1000).collect();
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
use core::cell::RefCell;
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

thread_local! {
    static CACHE: RefCell<cache::ThreadCache> =
        const { RefCell::new(cache::ThreadCache::new()) };
}

/// Run `f` with this thread's cache; if TLS is unavailable (thread exiting),
/// run the lock-based fallback instead. Never panics, never unwinds.
#[inline]
fn with_cache<R>(
    f: impl FnOnce(&mut cache::ThreadCache) -> R,
    fallback: impl FnOnce() -> R,
) -> R {
    let result = CACHE.try_with(|c| match c.try_borrow_mut() {
        Ok(mut cache) => Some(f(&mut cache)),
        Err(_) => None,
    });
    match result {
        Ok(Some(r)) => r,
        _ => fallback(),
    }
}

unsafe fn alloc_small(class: usize) -> *mut u8 {
    with_cache(
        |c| c.alloc(class),
        || {
            let (chain, _) = HEAP.take_blocks(class);
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

unsafe fn alloc_large(size: usize, align: usize) -> *mut u8 {
    let total = match size.checked_add(align).and_then(|v| v.checked_add(LARGE_HEADER_SIZE)) {
        Some(t) => t,
        None => return ptr::null_mut(),
    };
    let mapped = align_up(total.max(LARGE_HEADER_SIZE), page::PAGE_SIZE);
    let base = sys::map(mapped);
    if base.is_null() {
        return ptr::null_mut();
    }
    // Header lives directly before the user pointer: high alignment can push
    // the user pointer past the first 64 KiB boundary of the region, so the
    // region base is not a reliable place to find it.
    let ret = align_up(base as usize + LARGE_HEADER_SIZE, align);
    if ret + size > base as usize + mapped {
        sys::unmap(base, mapped);
        return ptr::null_mut();
    }
    let hdr = (ret - LARGE_HEADER_SIZE) as *mut LargeHeader;
    (*hdr).magic = LARGE_MAGIC;
    (*hdr).mapped_size = mapped;
    (*hdr).base = base;
    heap::MAPPED_PAGES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    ret as *mut u8
}

unsafe fn free_large(p: *mut u8) {
    let hdr = (p as usize - LARGE_HEADER_SIZE) as *mut LargeHeader;
    let mapped = (*hdr).mapped_size;
    let base = (*hdr).base;
    sys::unmap(base, mapped);
    heap::MAPPED_PAGES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
}

#[cold]
fn corrupt_pointer() -> ! {
    eprintln!("allox: free/realloc of pointer not owned by this allocator");
    std::process::abort()
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
    if *hdr == LARGE_MAGIC {
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

    unsafe fn realloc(
        &self,
        p: *mut u8,
        layout: core::alloc::Layout,
        new_size: usize,
    ) -> *mut u8 {
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
        let p = self.alloc(layout);
        if !p.is_null() && layout.size() != 0 {
            // Recycled pages are not guaranteed zero by the OS.
            ptr::write_bytes(p, 0, layout.size());
        }
        p
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
    let p = alloc_impl(total, 1);
    if !p.is_null() && total != 0 {
        ptr::write_bytes(p, 0, total);
    }
    p
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
    let magic = *(base as *const u64);
    if magic == page::PAGE_MAGIC {
        let page = base as *mut page::PageHeader;
        classes::CLASSES[(*page).class as usize]
    } else if magic == LARGE_MAGIC {
        let hdr = base as *mut LargeHeader;
        (*hdr).mapped_size - (p as usize - base)
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

/// Return this thread's cached free blocks to their pages.
///
/// Useful for thread-pool workers between tasks; otherwise blocks stay cached
/// until the pages naturally die. Deliberately *not* run in a TLS destructor:
/// see DESIGN.md §4.5 for why.
pub fn flush_current_thread() {
    with_cache(
        |c| unsafe { c.flush_all() },
        || {},
    );
}
