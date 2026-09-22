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
