//! POSIX virtual memory via mmap/munmap, plus a pthread-backed mutex.
//!
//! Everything here avoids heap allocation so the allocator can never
//! recursively re-enter itself through `GlobalAlloc` (POSIX mutex ops are
//! allocation-free by standard).

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
const MAP_ANONYMOUS: i32 = 0x1000;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
const MAP_ANONYMOUS: i32 = 0x20;

const MAP_PRIVATE: i32 = 0x02;
const PROT_READ_WRITE: i32 = 0x03;
const MADV_DONTNEED: i32 = 4;

extern "C" {
    fn mmap(
        addr: *mut core::ffi::c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut core::ffi::c_void;
    fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
    fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: i32) -> i32;
}

/// Map `size` bytes of anonymous zero-initialized memory, 64 KiB-aligned.
/// `size` must be a multiple of 64 KiB (our page size).
///
/// `mmap` only guarantees 4 KiB alignment, but the allocator finds page
/// headers by masking address bits (`p & !PAGE_MASK`), so the base must be
/// 64 KiB-aligned. We over-map by one page and trim the slack.
pub(crate) unsafe fn map(size: usize) -> *mut u8 {
    const ALIGN: usize = 64 * 1024;
    let total = match size.checked_add(ALIGN) {
        Some(t) => t,
        None => return core::ptr::null_mut(),
    };
    let raw = mmap(
        core::ptr::null_mut(),
        total,
        PROT_READ_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    ) as usize;
    if raw as usize == usize::MAX {
        return core::ptr::null_mut();
    }
    let aligned = (raw + ALIGN - 1) & !(ALIGN - 1);
    let prefix = aligned - raw;
    let suffix = total - prefix - size;
    if prefix != 0 {
        let _ = munmap(raw as *mut core::ffi::c_void, prefix);
    }
    if suffix != 0 {
        let _ = munmap((aligned + size) as *mut core::ffi::c_void, suffix);
    }
    aligned as *mut u8
}

pub(crate) unsafe fn unmap(p: *mut u8, size: usize) {
    let _ = munmap(p as *mut core::ffi::c_void, size);
}

/// Map `size` bytes without alignment guarantees (kernel 4 KiB suffices).
/// For large regions only: they are found by offset header, never by address
/// masking (see `alloc_large_ex`), so 64 KiB alignment buys nothing and the
/// over-map+trim of [`map`] would waste 2 extra VMA ops per miss.
pub(crate) unsafe fn map_any(size: usize) -> *mut u8 {
    let p = mmap(
        core::ptr::null_mut(),
        size,
        PROT_READ_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    if p as usize == usize::MAX {
        return core::ptr::null_mut();
    }
    p as *mut u8
}

/// Drop physical pages but keep the virtual reservation: the range faults
/// back (zero-filled) on next access. Best-effort — failure just means the
/// caller must treat the range as still dirty.
pub(crate) unsafe fn discard(p: *mut u8, size: usize) {
    let _ = madvise(p as *mut core::ffi::c_void, size, MADV_DONTNEED);
}

// ---------------------------------------------------------------------------
// pthread mutex: contended waiters park in the kernel instead of burning CPU
// (and convoy-collapsing when a lock holder is preempted, which pure
// spinlocks do under load). Uncontended lock is one userspace CAS — same as
// spinning — plus a predictable branch for lazy init. Only compiled with
// `std` (needs libc); no_std unix targets keep the spin version.
// ---------------------------------------------------------------------------

#[cfg(feature = "std")]
mod pthread_mutex {
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, Ordering};

    extern "C" {
        fn pthread_mutex_init(
            mutex: *mut core::ffi::c_void,
            attr: *const core::ffi::c_void,
        ) -> i32;
        fn pthread_mutex_lock(mutex: *mut core::ffi::c_void) -> i32;
        fn pthread_mutex_unlock(mutex: *mut core::ffi::c_void) -> i32;
    }

    /// Opaque storage for pthread_mutex_t. 128 bytes covers every unix we
    /// target (glibc/musl ~40, Darwin ~64); init writes only its own size.
    /// Zeroed storage is never interpreted — `ensure_init` runs
    /// pthread_mutex_init before any other use.
    pub(crate) struct RawMutex {
        inited: AtomicBool,
        mutex: UnsafeCell<[usize; 16]>,
    }

    /// Serializes first-time init only; uncontended afterwards.
    static INIT_GUARD: AtomicBool = AtomicBool::new(false);

    impl RawMutex {
        pub(crate) const fn new() -> Self {
            RawMutex {
                inited: AtomicBool::new(false),
                mutex: UnsafeCell::new([0; 16]),
            }
        }

        #[inline]
        fn ensure_init(&self) {
            if !self.inited.load(Ordering::Acquire) {
                while INIT_GUARD.swap(true, Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                if !self.inited.load(Ordering::Relaxed) {
                    // SAFETY: storage exclusively ours (guard held), attr NULL
                    // selects the default (non-recursive, no heap use).
                    unsafe {
                        pthread_mutex_init(
                            self.mutex.get().cast::<core::ffi::c_void>(),
                            core::ptr::null(),
                        );
                    }
                    self.inited.store(true, Ordering::Release);
                }
                INIT_GUARD.store(false, Ordering::Release);
            }
        }

        #[inline]
        pub(crate) fn lock(&self) {
            self.ensure_init();
            // SAFETY: initialized above; default mutexes never allocate and
            // are released before any user code runs, so no reentrancy.
            unsafe {
                pthread_mutex_lock(self.mutex.get().cast::<core::ffi::c_void>());
            }
        }

        #[inline]
        pub(crate) fn unlock(&self) {
            // SAFETY: only called paired with lock(), hence initialized.
            unsafe {
                pthread_mutex_unlock(self.mutex.get().cast::<core::ffi::c_void>());
            }
        }
    }

    // pthread ops are thread-safe; UnsafeCell access is fenced by the mutex.
    unsafe impl Sync for RawMutex {}
}

#[cfg(feature = "std")]
pub(crate) use pthread_mutex::RawMutex;
