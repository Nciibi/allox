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

pub(crate) unsafe fn unmap(p: *mut u8, _size: usize) -> bool {
    VirtualFree(p as *mut core::ffi::c_void, 0, MEM_RELEASE) != 0
}

/// Tell the OS the range is no longer needed but keep it reserved: physical
/// pages are dropped, the virtual reservation survives for reuse.
///
/// # Returns `false` on Windows, always
///
/// The return value means "this range now reads as zero", and on Windows no
/// available call actually delivers that:
///
/// - `VirtualFree(base, size, MEM_RESET)` — the documented way to drop the
///   physical pages — fails with `ERROR_INVALID_PARAMETER` (87) on every
///   tested Windows build, for a one-step `RESERVE|COMMIT` region and for a
///   separate `RESERVE` then `COMMIT`, and for both page-multiple and
///   unaligned sizes.
/// - `VirtualAlloc(base, size, MEM_COMMIT | MEM_RESET)` also fails with 87.
/// - `VirtualAlloc(base, size, MEM_RESET)` — what this used to call —
///   *succeeds* (non-null, `GetLastError() == 0`) and then does not zero
///   anything: every probed byte still held the pattern written before it.
///   That is the worst of the three, because a false success is
///   indistinguishable from a real discard to every caller.
/// - `VirtualFree(base, size, MEM_DECOMMIT)` does drop the physical pages,
///   but a subsequent read faults (`STATUS_ACCESS_VIOLATION`) instead of
///   returning zero. Installing an SEH handler to catch that is not an
///   option for a general-purpose allocator.
///
/// Measured directly; each case run in its own process because the
/// `MEM_DECOMMIT` variant faults the process on read.
///
/// Reporting `false` is what keeps `calloc` correct: callers use this flag to
/// skip a `memset`, so claiming a zeroing that did not happen hands callers
/// recycled bytes (this is what `tests/big_spans.rs::
/// big_active_reuse_preserves_calloc_zeroing` caught). The cost is that
/// recycled regions are always memset on Windows and cold physical pages are
/// not dropped, i.e. a throughput and RSS cost on this backend alone. The
/// unix backend's `madvise(MADV_DONTNEED)` genuinely provides both halves and
/// is unaffected. Recovering the optimization here needs a recommit step
/// wherever a retained region is handed back out, not just a call swap.
pub(crate) unsafe fn discard(_p: *mut u8, size: usize) -> bool {
    // Still counted: the caller asked to purge this range, and the counters
    // are how a Windows build's inability to purge becomes visible instead of
    // silently costing RSS forever.
    crate::counters::bump(&crate::counters::VOLUME.purge_calls, 1);
    crate::counters::bump(&crate::counters::VOLUME.purge_bytes, size as u64);
    false
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
        unsafe { AcquireSRWLockExclusive(&self.0 as *const SrwLock as *mut SrwLock) }
    }

    #[inline]
    pub(crate) fn unlock(&self) {
        unsafe { ReleaseSRWLockExclusive(&self.0 as *const SrwLock as *mut SrwLock) }
    }
}
