//! Windows virtual memory via VirtualAlloc/VirtualFree.

use core::ptr;

const MEM_RESERVE: u32 = 0x2000;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;

extern "system" {
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

/// Result of a purge request. See [`discard`].
pub(crate) struct Discarded {
    /// Physical pages were actually dropped, so committed memory fell even
    /// though the virtual reservation survives for reuse.
    pub physical: bool,
    /// The range is still readable *and* reads as zero, which is what lets a
    /// recycled range serve `calloc` without a memset.
    pub zeroed: bool,
}

const MEM_DECOMMIT: u32 = 0x4000;

extern "system" {
    fn VirtualAlloc(
        addr: *mut core::ffi::c_void,
        size: usize,
        alloc_type: u32,
        protect: u32,
    ) -> *mut core::ffi::c_void;
}

/// Drop physical pages while keeping the virtual reservation, so the range
/// can be handed out again without a fresh `VirtualAlloc` round trip.
///
/// # What actually works on Windows, and what does not
///
/// The obvious move — `VirtualFree(base, size, MEM_RESET)` — *succeeds* and
/// then zeroes nothing: every probed byte still held the pattern written
/// before it. A false success there is worse than a failure, because it is
/// indistinguishable from a real purge to every caller.
///
/// Nor can "decommitted reads as zero" be had directly:
///
/// - `VirtualFree(base, size, MEM_DECOMMIT)` does drop the physical pages,
///   but a subsequent read faults (`STATUS_ACCESS_VIOLATION`) instead of
///   returning zero. Installing an SEH handler to catch that is not an option
///   for a general-purpose allocator.
/// - `VirtualAlloc(base, size, MEM_COMMIT | MEM_RESET)` fails with
///   `ERROR_INVALID_PARAMETER` (87), as does `VirtualFree(..., MEM_RESET)`.
///   Measured directly; each case run in its own process because the
///   `MEM_DECOMMIT` variant faults the process on read.
///
/// So the contract is split in two, and the *fix* the old comment asked for
/// is the second half: keep the range readable by never letting a decommitted
/// page be read, and [`recommit`] it on the way out. Everything that hands a
/// retained range back out calls `recommit` first, which is what makes
/// `MEM_DECOMMIT` usable at all.
///
/// The payoff: Windows now actually returns committed memory on its cold
/// retention tiers instead of holding every page it ever touched. The cost:
/// one extra `VirtualAlloc(MEM_COMMIT)` on the reuse path, against a
/// `VirtualFree(MEM_DECOMMIT)` we were already paying to discard.
///
/// `Discarded::zeroed` is always false — no Win32 call provides a
/// decommitted-and-readable-as-zero primitive — so recycled regions are
/// still memset on Windows. That is the conservative direction: reporting a
/// zeroing that did not happen is what
/// `tests/big_spans.rs::big_active_reuse_preserves_calloc_zeroing` was
/// written to catch.
pub(crate) unsafe fn discard(p: *mut u8, size: usize) -> Discarded {
    // Still counted: the caller asked to purge this range, and the counters
    // are how a Windows build's purge behaviour becomes visible instead of
    // silently costing memory forever.
    crate::counters::bump(&crate::counters::VOLUME.purge_calls, 1);
    crate::counters::bump(&crate::counters::VOLUME.purge_bytes, size as u64);
    let physical = size != 0
        && VirtualFree(p.cast::<core::ffi::c_void>(), size, MEM_DECOMMIT) != 0;
    Discarded {
        physical,
        zeroed: false,
    }
}

/// Make a range that [`discard`] decommitted usable again.
///
/// Must be called before *any* access to a discarded range. Commits the
/// pages at `PAGE_READWRITE`; the contents are undefined, which is why
/// `Discarded::zeroed` is false and callers memset as needed.
pub(crate) unsafe fn recommit(p: *mut u8, size: usize) {
    if size == 0 {
        return;
    }
    // SAFETY: `p`/`size` describe a slice of a region this allocator
    // reserved with `VirtualAlloc(MEM_RESERVE)`, so a commit-only
    // `VirtualAlloc` at that address is the documented way to bring it
    // back. Failure leaves the range unusable, which the caller's `debug_assert`
    // and the null-`ret` bounds check downstream will surface.
    let ok = VirtualAlloc(
        p.cast::<core::ffi::c_void>(),
        size,
        MEM_COMMIT,
        PAGE_READWRITE,
    );
    debug_assert!(
        !ok.is_null(),
        "VirtualAlloc(MEM_COMMIT) failed to recommit a discarded region"
    );
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
