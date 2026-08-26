//! C ABI exports (`allox_malloc`, `allox_free`, ...).

use core::ffi::c_void;

use crate::{calloc, free, malloc, realloc};

#[no_mangle]
pub extern "C" fn allox_malloc(size: usize) -> *mut c_void {
    unsafe { malloc(size) as *mut c_void }
}

#[no_mangle]
pub extern "C" fn allox_calloc(nmemb: usize, size: usize) -> *mut c_void {
    unsafe { calloc(nmemb, size) as *mut c_void }
}

/// # Safety
/// `ptr` must be null or a live allocation of this allocator.
#[no_mangle]
pub unsafe extern "C" fn allox_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    realloc(ptr as *mut u8, size) as *mut c_void
}

/// # Safety
/// `ptr` must be null or a live allocation of this allocator.
#[no_mangle]
pub unsafe extern "C" fn allox_free(ptr: *mut c_void) {
    free(ptr as *mut u8)
}

#[no_mangle]
pub extern "C" fn allox_aligned_alloc(align: usize, size: usize) -> *mut c_void {
    unsafe { crate::aligned_alloc(align, size) as *mut c_void }
}
