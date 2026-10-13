//! WebAssembly linear-memory backing.
//!
//! `memory.grow` is the only primitive WASM offers: pages (64 KiB, matching
//! our page size) can be added but never returned. `unmap` is therefore a
//! no-op; the delayed-reclamation cache in the heap keeps churn from growing
//! memory unboundedly, and fully-dead pages beyond the cap simply remain
//! mapped — the same trade-off dlmalloc makes on this target.

use core::arch::wasm32;
use core::ptr;

/// Grow linear memory by `size` bytes (`size` must be a multiple of 64 KiB)
/// and return the start of the new region, or null on failure.
pub(crate) unsafe fn map(size: usize) -> *mut u8 {
    debug_assert!(size % crate::page::PAGE_SIZE == 0);
    let pages = size / crate::page::PAGE_SIZE;
    let old = wasm32::memory_grow(0, pages);
    if old == usize::MAX {
        return ptr::null_mut();
    }
    (old * crate::page::PAGE_SIZE) as *mut u8
}

/// WASM cannot release linear-memory pages; kept mapped by design.
pub(crate) unsafe fn unmap(_p: *mut u8, _size: usize) -> bool {
    false
}

/// Result of a purge request. See [`discard`].
pub(crate) struct Discarded {
    /// Physical pages were actually dropped.
    pub physical: bool,
    /// The range is still readable *and* reads as zero.
    pub zeroed: bool,
}

/// No physical/virtual split exists in linear memory, so there is nothing to
/// purge and nothing to recommit: `memory.grow` only ever adds, and a page
/// released back to the delayed-reclamation cache still holds its contents.
///
/// `zeroed` is false, so recycled WASM memory is always memset for `calloc`.
/// Sound on every backend, which is the point of asking.
pub(crate) unsafe fn discard(_p: *mut u8, _size: usize) -> Discarded {
    Discarded {
        physical: false,
        zeroed: false,
    }
}

/// No-op: a discarded range was never decommitted, so there is nothing to
/// bring back. Kept so callers do not have to know which backend they are on.
pub(crate) unsafe fn recommit(_p: *mut u8, _size: usize) {}
