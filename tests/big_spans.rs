//! Big-span integration tests (sizes past the 64 KiB chunk cap): routing
//! boundaries, roundtrips, realloc/calloc, and multithreaded churn.
//! Big spans exist only with the arena (unix + std); elsewhere these sizes
//! route to the large path and the same assertions hold by construction.

use allox::{free, malloc, realloc};
use std::alloc::{GlobalAlloc, Layout};

fn check_pattern(p: *mut u8, size: usize) {
    unsafe {
        for i in 0..size {
            assert_eq!(*p.add(i), (i % 251) as u8, "byte {}", i);
        }
    }
}

fn fill_pattern(p: *mut u8, size: usize) {
    unsafe {
        for i in 0..size {
            *p.add(i) = (i % 251) as u8;
        }
    }
}

#[test]
fn big_boundary_routing() {
    unsafe {
        let medium_top = 65536 - if cfg!(target_pointer_width = "64") { 64 } else { 48 };
        let m = malloc(medium_top);
        assert!(!m.is_null());
        assert_eq!(allox::usable_size(m), medium_top);
        free(m);

        let b0 = malloc(medium_top + 1);
        assert!(!b0.is_null());
        let u0 = allox::usable_size(b0);
        assert!(u0 >= 65473 && u0 <= 262144, "usable {}", u0);
        fill_pattern(b0, 65473);
        check_pattern(b0, 65473);
        free(b0);

        let b1 = malloc(262144);
        assert!(!b1.is_null());
        // Arena targets back this with the explicit 262144 top class;
        // elsewhere it rides the large path (usable >= request either way).
        #[cfg(all(unix, feature = "std"))]
        assert_eq!(allox::usable_size(b1), 262144);
        assert!(allox::usable_size(b1) >= 262144);
        free(b1);

        let l = malloc(262145);
        assert!(!l.is_null());
        assert!(allox::usable_size(l) >= 262145);
        free(l);
    }
}

#[test]
fn big_roundtrip_contents() {
    unsafe {
        for size in [70000usize, 100000, 150000, 200000, 262144] {
            let p = malloc(size);
            assert!(!p.is_null(), "size {}", size);
            fill_pattern(p, size);
            check_pattern(p, size);
            // Tail is writable.
            *p.add(size - 1) = 0xAB;
            assert_eq!(*p.add(size - 1), 0xAB);
            free(p);
        }
    }
}

#[test]
fn big_calloc_is_zeroed() {
    unsafe {
        for size in [70000usize, 131072, 262144] {
            let p = allox::calloc(1, size);
            assert!(!p.is_null(), "size {}", size);
            for i in [0, size / 2, size - 1] {
                assert_eq!(*p.add(i), 0, "offset {} size {}", i, size);
            }
            free(p);
        }
    }
}

#[test]
fn big_realloc_grows_and_shrinks() {
    unsafe {
        // Grow across big classes (may relocate or grow in place).
        let mut size = 70000usize;
        let mut p = malloc(size);
        assert!(!p.is_null());
        fill_pattern(p, size);
        for _ in 0..3 {
            let nsize = size * 2;
            if nsize > 262144 {
                break;
            }
            let np = realloc(p, nsize);
            assert!(!np.is_null());
            check_pattern(np, size);
            p = np;
            size = nsize;
        }
        // Shrink within big stays usable with prefix intact.
        let sp = realloc(p, 70000);
        assert!(!sp.is_null());
        check_pattern(sp, 70000);
        free(sp);

        // Same-size realloc is identity on arena targets (spans never
        // move) and a legal move elsewhere; either way the result holds
        // the requested size.
        let q = malloc(100000);
        assert!(!q.is_null());
        let q2 = realloc(q, 100000);
        assert!(!q2.is_null());
        fill_pattern(q2, 100000);
        check_pattern(q2, 100000);
        free(q2);
    }
}

#[test]
fn big_global_alloc_paths() {
    unsafe {
        let a = allox::Allox;
        for size in [65500usize, 100000, 262144] {
            let l = Layout::from_size_align(size, 16).unwrap();
            let p = a.alloc(l);
            assert!(!p.is_null(), "size {}", size);
            fill_pattern(p, size);
            check_pattern(p, size);
            a.dealloc(p, l);
        }
        // Realloc across the medium/big boundary moves legally.
        let lm = Layout::from_size_align(60000, 16).unwrap();
        let p = a.alloc(lm);
        assert!(!p.is_null());
        fill_pattern(p, 60000);
        let np = a.realloc(p, lm, 100000);
        assert!(!np.is_null());
        check_pattern(np, 60000);
        a.dealloc(np, Layout::from_size_align(100000, 16).unwrap());
    }
}

#[test]
fn big_churn_multithreaded() {
    let handles: Vec<_> = (0..8)
        .map(|t| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut rng = (0xB16 ^ ((t as u64 + 1).wrapping_mul(0x9E3779B9))) as u64;
                    let mut next_rng = || {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng
                    };
                    for _ in 0..200 {
                        // Sizes across the big range (and occasionally
                        // medium/large neighbors to cross the boundaries).
                        let r = (next_rng() % 100) as usize;
                        let size = if r < 70 {
                            65536 + (next_rng() as usize) % (262144 - 65536)
                        } else if r < 85 {
                            32768 + (next_rng() as usize) % 32704
                        } else {
                            262144 + (next_rng() as usize) % 262144
                        };
                        let p = unsafe { malloc(size) };
                        assert!(!p.is_null(), "size {}", size);
                        unsafe {
                            *p = (size % 251) as u8;
                            *p.add(size - 1) = 0xAB;
                            assert_eq!(*p.add(size - 1), 0xAB);
                            assert!(allox::usable_size(p) >= size);
                            free(p);
                        }
                    }
                })
                .unwrap()
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
