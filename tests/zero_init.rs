//! Regression tests for zero-initialization guarantees:
//! virgin blocks (never allocated since page carve) must come back fully
//! zeroed except that the allocator clears their freelist link word, and
//! recycled blocks must be explicitly zeroed by calloc.

#[global_allocator]
static GLOBAL: allox::Allox = allox::Allox;

/// Single-threaded class-0 churn: every calloc must observe zeros.
#[test]
fn zero_after_churn() {
    unsafe {
        let mut ptrs: Vec<*mut u8> = Vec::new();
        for round in 0..200_000usize {
            match round % 3 {
                0 => {
                    ptrs.push(allox::malloc(8));
                }
                1 => {
                    let p = allox::calloc(1, 8);
                    assert!(!p.is_null());
                    for i in 0..8 {
                        assert_eq!(*p.add(i), 0, "round {} off {}", round, i);
                    }
                    ptrs.push(p);
                }
                _ => {
                    if !ptrs.is_empty() {
                        allox::free(ptrs.swap_remove(ptrs.len() - 1));
                    }
                }
            }
            if ptrs.len() > 5000 {
                allox::free(ptrs.swap_remove(0));
            }
        }
        for p in ptrs {
            allox::free(p);
        }
    }
}

#[test]
fn large_calloc_reuses_discarded_zero_regions() {
    const SIZE: usize = 300_000;
    const COUNT: usize = 40;

    unsafe {
        let mut original = [std::ptr::null_mut(); COUNT];
        for p in &mut original {
            *p = allox::calloc(1, SIZE);
            assert!(!p.is_null());
            std::ptr::write_bytes(*p, 0xA5, SIZE);
        }
        for &p in &original {
            allox::free(p);
        }

        let mut reused = [std::ptr::null_mut(); COUNT];
        for actual in &mut reused {
            *actual = allox::calloc(1, SIZE);
            assert!(!actual.is_null());
            assert!(std::slice::from_raw_parts(*actual, SIZE)
                .iter()
                .all(|&byte| byte == 0));
        }
        for p in reused {
            allox::free(p);
        }
    }
}

#[test]
fn medium_active_cache_flushes_and_reuses() {
    unsafe {
        for size in [20000usize, 32768, 50000] {
            let mut first = Vec::with_capacity(64);
            for i in 0..64 {
                let p = allox::malloc(size);
                assert!(!p.is_null());
                *p.add(size - 1) = i as u8;
                first.push(p);
            }
            for p in first {
                allox::free(p);
            }
            allox::flush_current_thread();
            let mut second = Vec::with_capacity(64);
            for _ in 0..64 {
                let p = allox::malloc(size);
                assert!(!p.is_null());
                second.push(p);
            }
            for p in second {
                allox::free(p);
            }
            allox::flush_current_thread();
        }
    }
}

/// Multi-threaded: exercises virgin tracking across threads sharing pages,
/// where one thread's refill drains a page another thread dirtied earlier.
#[test]
fn zero_across_threads() {
    let handles: Vec<_> = (0..4)
        .map(|t| {
            std::thread::spawn(move || unsafe {
                let mut live: Vec<*mut u8> = Vec::new();
                for i in 0..30_000usize {
                    if i % 2 == 0 || live.is_empty() {
                        let n = 1 + (i + t) % 64;
                        let p = allox::calloc(n, 1);
                        assert!(!p.is_null());
                        for b in 0..n {
                            assert_eq!(*p.add(b), 0);
                            *p.add(b) = 0xFF;
                        }
                        live.push(p);
                    } else {
                        allox::free(live.swap_remove(live.len() - 1));
                    }
                }
                for p in live {
                    allox::free(p);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
