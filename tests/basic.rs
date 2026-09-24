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
fn tiny_budget_churn_never_underflows() {
    // Guards the trim accounting invariant (cached_bytes == retained bytes).
    // Restores the default budget on drop so parallel tests are unaffected.
    struct RestoreBudget;
    impl Drop for RestoreBudget {
        fn drop(&mut self) {
            allox::set_thread_cache_budget(32 * 1024 * 1024);
        }
    }
    let _guard = RestoreBudget;
    unsafe {
        // Zero budget: every dealloc trips trim, usually with no single bin
        // over the 64-block trim threshold. The old code lied
        // `cached_bytes = target` on those epochs, lagging actual retention
        // until a later pop drove the counter below zero (debug panic in
        // `alloc`, silent wrap + over-trim storm in release).
        allox::set_thread_cache_budget(0);
        let mut live = Vec::with_capacity(256);
        for i in 0..5000usize {
            let size = 16 + (i * 37) % 4096;
            let p = allox::malloc(size);
            assert!(!p.is_null());
            core::ptr::write_bytes(p, 0xAB, size.min(4096));
            live.push(p);
            if live.len() > 64 {
                allox::free(live.remove(0));
            }
        }
        for p in live {
            allox::free(p);
        }
        allox::flush_current_thread();
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
fn large_realloc_grows_without_copy_loss() {
    // Doubling growth 64 KiB -> 1 MiB: contents must survive every step
    // whether the kernel grows in place or relocates (or falls back to
    // alloc-copy-free on non-Linux / arena regions). The original 64 KiB
    // keeps its pattern; grown tails keep the 0x5A written each step.
    unsafe {
        const ORIG: usize = 65536;
        let mut size = ORIG;
        let mut p = malloc(size);
        assert!(!p.is_null());
        for i in 0..size {
            *p.add(i) = (i % 251) as u8;
        }
        while size < 1048576 {
            let nsize = size * 2;
            let np = realloc(p, nsize);
            assert!(!np.is_null());
            for i in 0..ORIG {
                assert_eq!(*np.add(i), (i % 251) as u8, "lost byte at {}", i);
            }
            for i in ORIG..size {
                assert_eq!(*np.add(i), 0x5A, "lost tail byte at {}", i);
            }
            assert!(usable_size(np) >= nsize);
            // Fresh tail is writable.
            core::ptr::write_bytes(np.add(size), 0x5A, nsize - size);
            p = np;
            size = nsize;
        }
        // Shrink path keeps the prefix and stays usable (prefix is inside
        // the original 64 KiB, so it still holds the initial pattern).
        let sp = realloc(p, 4096);
        assert!(!sp.is_null());
        for i in 0..4096 {
            assert_eq!(*sp.add(i), (i % 251) as u8, "shrink byte at {}", i);
        }
        free(sp);
    }
}

#[test]
fn global_realloc_large_grows_without_copy_loss() {
    use std::alloc::{GlobalAlloc, Layout};
    unsafe {
        let a = allox::Allox;
        const ORIG: usize = 70000; // large tier, odd size (not class-round)
        let mut size = ORIG;
        let mut layout = Layout::from_size_align(size, 16).unwrap();
        let mut p = a.alloc(layout);
        assert!(!p.is_null());
        for i in 0..size {
            *p.add(i) = (i % 251) as u8;
        }
        for _ in 0..3 {
            let nsize = size * 2 + 123;
            let np = a.realloc(p, layout, nsize);
            assert!(!np.is_null());
            for i in 0..ORIG {
                assert_eq!(*np.add(i), (i % 251) as u8, "lost byte at {}", i);
            }
            for i in ORIG..size {
                assert_eq!(*np.add(i), 0x5A, "lost tail byte at {}", i);
            }
            assert!(np as usize % 16 == 0, "alignment lost");
            core::ptr::write_bytes(np.add(size), 0x5A, nsize - size);
            p = np;
            layout = Layout::from_size_align(nsize, 16).unwrap();
            size = nsize;
        }
        a.dealloc(p, layout);
    }
}

#[test]
fn malloc_free_all_sizes_roundtrip() {
    unsafe {
        for batch_start in (1..=64 * 1024usize).step_by(256) {
            let batch_end = (batch_start + 256).min(64 * 1024 + 1);
            let mut ptrs = Vec::with_capacity(batch_end - batch_start);
            for size in batch_start..batch_end {
                let p = malloc(size);
                assert!(!p.is_null(), "size {}", size);
                core::ptr::write_bytes(p, 0xAB, size);
                assert_eq!(*p.add(size - 1), 0xAB);
                ptrs.push((p, size));
            }
            for (p, size) in ptrs {
                assert_eq!(*p.add(size - 1), 0xAB);
                free(p);
            }
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
fn aligned_realloc_preserves_alignment() {
    unsafe {
        for align in [32usize, 256, 4096, 65536] {
            let p = aligned_alloc(align, 1234);
            assert!(!p.is_null(), "align {}", align);
            assert_eq!(p as usize % align, 0, "initial align {}", align);
            for i in 0..1234 {
                *p.add(i) = (i as u8).wrapping_add(align as u8);
            }
            let np = realloc(p, 8192);
            assert!(!np.is_null(), "realloc align {}", align);
            assert_eq!(np as usize % align, 0, "realloc align {}", align);
            for i in 0..1234 {
                assert_eq!(*np.add(i), (i as u8).wrapping_add(align as u8));
            }
            free(np);
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
