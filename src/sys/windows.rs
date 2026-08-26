//! Windows virtual memory via VirtualAlloc/VirtualFree.

use core::ptr;

const MEM_RESERVE: u32 = 0x2000;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;

extern "system" {
    fn VirtualAlloc(
        addr: *mut core::ffi::c_void,
        size: usize,
        alloc_type: u32,
        protect: u32,
    ) -> *mut core::ffi::c_void;
    fn VirtualFree(addr: *mut core::ffi::c_void, size: usize, free_type: u32) -> i32;
}

/// Reserve and commit `size` bytes of zero-initialized memory.
/// `size` must be a multiple of 64 KiB (our page size).
pub(crate) unsafe fn map(size: usize) -> *mut u8 {
    let p = VirtualAlloc(
        ptr::null_mut(),
        size,
        MEM_RESERVE | MEM_COMMIT,
        PAGE_READWRITE,
    );
    p as *mut u8
}

pub(crate) unsafe fn unmap(p: *mut u8, _size: usize) {
    let _ = VirtualFree(p as *mut core::ffi::c_void, 0, MEM_RELEASE);
}
