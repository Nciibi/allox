use allox::{aligned_alloc, calloc, free, malloc, realloc, usable_size};
use std::alloc::{GlobalAlloc, Layout};

#[test]
fn zero_size_family_is_sound() {
    unsafe {
        // C-flavored free fns return null for zero size (conforming), so
        // every downstream entry point (free/realloc/usable_size) stays on
        // its null contract instead of probing headers of dangling pointers.
        assert!(malloc(0).is_null());
        assert!(calloc(0, 16).is_null());
        assert!(calloc(16, 0).is_null());
        assert!(aligned_alloc(16, 0).is_null());
        free(core::ptr::null_mut()); // noop, always sound
        assert_eq!(usable_size(core::ptr::null_mut()), 0);
        // realloc(null, n) == malloc(n).
        let p = realloc(core::ptr::null_mut(), 8);
        assert!(!p.is_null());
        core::ptr::write_bytes(p, 0xAB, 8);
        free(p);
        // realloc(live, 0) frees and returns null (C semantics).
        let q = malloc(64);
        assert!(!q.is_null());
        assert!(realloc(q, 0).is_null());
    }
}

#[test]
fn global_realloc_zero_layout_grows_fresh() {
    unsafe {
        let a = allox::Allox;
        // Zero-size allocs are dangling by Rust convention; growing one to a
        // nonzero size must hand out fresh memory, never the dangling
        // pointer back (same-class identity must not fire on size 0).
        let l0 = Layout::from_size_align(0, 16).unwrap();
        let p = a.alloc(l0);
        let q = a.realloc(p, l0, 8);
        assert_ne!(q, p, "realloc must not return the zero-size dangling pointer");
        assert!(!q.is_null());
        core::ptr::write_bytes(q, 0xAB, 8);
        a.dealloc(q, Layout::from_size_align(8, 16).unwrap());
        // Shrinking to zero still frees and yields a (dangling) pointer.
        let r = a.alloc(Layout::from_size_align(32, 8).unwrap());
        assert!(!r.is_null());
        let z = a.realloc(r, Layout::from_size_align(32, 8).unwrap(), 0);
        assert!(!z.is_null());
    }
}

#[test]
fn malloc_free_all_sizes_roundtrip() {
    unsafe {
        let mut ptrs = Vec::new();
        for size in 1..=64 * 1024usize {
            let p = malloc(size);
            assert!(!p.is_null(), "size {}", size);
            core::ptr::write_bytes(p, 0xAB, size);
            assert_eq!(*p.add(size - 1), 0xAB);
            ptrs.push((p, size));
        }
        for (p, _) in ptrs {
            free(p);
        }
    }
}

#[test]
fn calloc_is_zeroed() {
    unsafe {
        for size in [1usize, 16, 100, 4096, 20000] {
            let p = calloc(1, size);
            assert!(!p.is_null());
            for i in 0..size {
                assert_eq!(*p.add(i), 0, "offset {} size {}", i, size);
            }
            free(p);
        }
        let big = calloc(1024, 1024); // 1 MiB
        assert!(!big.is_null());
        free(big);
    }
}

#[test]
fn realloc_preserves_contents() {
    unsafe {
        let mut cap = 16usize;
        let mut p = malloc(cap);
        assert!(!p.is_null());
        let mut expected: Vec<u8> = (0..cap).map(|i| (i % 251) as u8).collect();
        core::ptr::copy_nonoverlapping(expected.as_ptr(), p, cap);
        while cap < 300_000 {
            let new_cap = cap * 2 + 7;
            let np = realloc(p, new_cap);
            assert!(!np.is_null());
            assert_eq!(core::slice::from_raw_parts(np, cap), &expected[..]);
            for i in cap..new_cap {
                // fresh bytes are writable
                *np.add(i) = 1;
            }
            expected.resize(new_cap, 1);
            p = np;
            cap = new_cap;
        }
        free(p);

        // shrink path through same class must be identity-safe
        let q = malloc(32);
        let q2 = realloc(q, 24);
        assert_eq!(q, q2);
        free(q2);
    }
}

#[test]
fn over_aligned_allocations_work() {
    unsafe {
        for align in [32usize, 64, 256, 4096, 65536] {
            let p = aligned_alloc(align, 1234);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "align {}", align);
            core::ptr::write_bytes(p, 0x5A, 1234);
            free(p);
        }
    }
}

#[test]
fn usable_size_covers_request() {
    unsafe {
        let p = malloc(100);
        assert!(usable_size(p) >= 100);
        free(p);
        let big = malloc(1 << 20);
        assert!(usable_size(big) >= 1 << 20);
        free(big);
    }
}

#[test]
fn many_small_churn() {
    unsafe {
        let mut live: Vec<(Vec<*mut u8>, usize)> = Vec::new();
        for round in 0..1000 {
            let size = (round * 37) % 4096 + 1;
            let mut batch = Vec::with_capacity(16);
            for _ in 0..16 {
                let p = malloc(size);
                assert!(!p.is_null());
                *p.add(size - 1) = 42;
                batch.push(p);
            }
            if round % 3 == 0 && !live.is_empty() {
                let (old, osz) = live.pop().unwrap();
                for &p in &old {
                    assert_eq!(*p.add(osz - 1), 42);
                    free(p);
                }
            }
            live.push((batch, size));
        }
        for (batch, osz) in live {
            for &p in &batch {
                assert_eq!(*p.add(osz - 1), 42);
                free(p);
            }
        }
    }
}
