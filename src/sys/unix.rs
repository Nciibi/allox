//! POSIX virtual memory via mmap/munmap.

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
}

/// Map `size` bytes of anonymous zero-initialized memory.
/// `size` must be a multiple of 64 KiB (our page size).
pub(crate) unsafe fn map(size: usize) -> *mut u8 {
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

pub(crate) unsafe fn unmap(p: *mut u8, size: usize) {
    let _ = munmap(p as *mut core::ffi::c_void, size);
}
