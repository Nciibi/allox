use std::ffi::c_void;

#[global_allocator]
static GLOBAL: allox::Allox = allox::Allox;

extern "C" {
    fn allox_malloc(size: usize) -> *mut c_void;
    fn allox_calloc(nmemb: usize, size: usize) -> *mut c_void;
    fn allox_realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn allox_free(ptr: *mut c_void);
    fn allox_aligned_alloc(align: usize, size: usize) -> *mut c_void;
    fn allox_malloc_usable_size(ptr: *mut c_void) -> usize;
}

#[test]
fn c_abi_roundtrip() {
    unsafe {
        let p = allox_malloc(123);
        assert!(!p.is_null());
        let p = allox_realloc(p, 4567);
        assert!(!p.is_null());
        *(p as *mut u8).add(4566) = 1;
        allox_free(p);

        let z = allox_calloc(64, 64);
        assert!(!z.is_null());
        assert_eq!(*(z as *mut u64), 0);
        allox_free(z);

        let a = allox_aligned_alloc(4096, 500);
        assert!(!a.is_null());
        assert_eq!(a as usize % 4096, 0);
        allox_free(a);

        allox_free(std::ptr::null_mut());

        assert_eq!(allox_malloc_usable_size(std::ptr::null_mut()), 0);
        let u = allox_malloc(100);
        assert!(!u.is_null());
        assert!(allox_malloc_usable_size(u) >= 100);
        allox_free(u);
    }
}
