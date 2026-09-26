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

/// A cold small page whose backend `discard` actually landed is provably
/// zero again, so `calloc` may serve from it without a memset. This is the
/// fast path added 2026-09-26; the test is built to catch the opposite bug,
/// which is a **memory disclosure**: if the "provably zero" flag were ever
/// set for a page that was not really discarded, `calloc` would hand back the
/// `0xFF` written below.
///
/// It forces the page all the way cold rather than relying on the empty-page
/// cache: `EMPTY_PAGE_CACHE_PER_CLASS` is 4, and a 64 KiB page of 16 B blocks
/// holds ~4095 of them, so this allocates and frees ~40k blocks to push at
/// least six pages into the cold tier where the discard runs.
#[test]
fn calloc_from_a_discarded_cold_page_is_zeroed() {
    unsafe {
        // 400k * 16 B = 6.4 MiB live, ~98 pages. `EMPTY_PAGE_CACHE_PER_CLASS`
        // is 4 (~16k blocks) and is consulted *before* the cold list, so the
        // count has to be far larger than that or the empty list would satisfy
        // nearly every calloc below and the cold path would go untested.
        const N: usize = 400_000;
        let mut ptrs: Vec<*mut u8> = Vec::with_capacity(N);
        for _ in 0..N {
            let p = allox::malloc(16);
            assert!(!p.is_null());
            // Poison every byte, so a non-zeroed reuse is unmistakable.
            core::ptr::write_bytes(p, 0xFF, 16);
            ptrs.push(p);
        }
        // Confirm the discard path actually ran, so this test cannot pass
        // vacuously by never reaching the cold tier.
        let purges_before = allox::__diagnostics::volume().purge_calls;
        for p in ptrs.drain(..) {
            allox::free(p);
        }
        // Freeing only files the blocks into this thread's cache; the pages
        // do not become fully free (and so never reach the cold tier) until
        // the cache hands them back. Force that.
        allox::flush_current_thread();
        let purges_after = allox::__diagnostics::volume().purge_calls;
        // At least 8 pages discarded proves the cold list is deep enough to
        // dominate the recycle loop below.
        assert!(
            purges_after >= purges_before + 8,
            "only {} pages reached the cold tier, so the discarded-page path is \
             barely exercised (purge_calls {purges_before} -> {purges_after})",
            purges_after - purges_before
        );

        // Now recycle: every one of these must come back zeroed, whether it
        // came from the empty-page cache, a discarded cold page, or fresh.
        for i in 0..N {
            let p = allox::calloc(1, 16);
            assert!(!p.is_null(), "calloc {i} returned null");
            for b in 0..16 {
                assert_eq!(
                    *p.add(b),
                    0,
                    "calloc {i} returned stale data at byte {b} (0x{:02X})",
                    *p.add(b)
                );
            }
            // Dirty it again so the next round cannot be served by luck.
            core::ptr::write_bytes(p, 0xFF, 16);
            allox::free(p);
        }
    }
}
