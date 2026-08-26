use allox::{calloc, free, malloc, realloc, aligned_alloc, usable_size};

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
        for i in 0..cap {
            *p.add(i) = (i % 251) as u8;
        }
        while cap < 300_000 {
            let new_cap = cap * 2 + 7;
            let np = realloc(p, new_cap);
            assert!(!np.is_null());
            for i in 0..cap {
                assert_eq!(*np.add(i), (i % 251) as u8, "cap {} i {}", cap, i);
            }
            for i in cap..new_cap {
                // fresh bytes are writable
                *np.add(i) = 1;
            }
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
