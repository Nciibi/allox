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
    let old = wasm32::memory_grow(0, pages as u32);
    if old == usize::MAX as u32 {
        return ptr::null_mut();
    }
    (old as usize * crate::page::PAGE_SIZE) as *mut u8
}

/// WASM cannot release linear-memory pages; kept mapped by design.
pub(crate) unsafe fn unmap(_p: *mut u8, _size: usize) {}
