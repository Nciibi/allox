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
/// `size` must be multiple of 64 KiB (our page size).
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

// ---------------------------------------------------------------------------
// SRWLock-backed raw mutex: contended waiters park in the kernel instead of
// burning CPU. SRWLOCK is a single pointer initialized to zero, so it is
// const-constructible without any initialization call.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SrwLock(usize);

unsafe extern "system" {
    fn AcquireSRWLockExclusive(lock: *mut SrwLock);
    fn ReleaseSRWLockExclusive(lock: *mut SrwLock);
}

pub(crate) struct RawMutex(SrwLock);

impl RawMutex {
    pub(crate) const fn new() -> Self {
        RawMutex(SrwLock(0)) // SRWLOCK_INIT
    }

    #[inline]
    pub(crate) fn lock(&self) {
        unsafe { AcquireSRWLockExclusive(&self.0) }
    }

    #[inline]
    pub(crate) fn unlock(&self) {
        unsafe { ReleaseSRWLockExclusive(&self.0) }
    }
}
