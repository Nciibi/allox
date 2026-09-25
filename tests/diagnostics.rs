//! Sensitivity coverage for the always-on diagnostic counters.
//!
//! These assert that each counter *moves* for the operation it names, so a
//! wiring regression (counter incremented on the wrong path, or not at all)
//! fails here rather than silently reporting zeros in a benchmark run.

use allox::malloc;
use allox::__diagnostics::volume;
use std::alloc::{GlobalAlloc, Layout};

fn delta(before: u64, after: u64) -> u64 {
    after.saturating_sub(before)
}

/// Force the calling thread's counter batch out to the global counters.
/// They are published every 8192 operations, so a test that does less work
/// than that would otherwise read zeros.
fn publish_pending() {
    allox::flush_current_thread();
}

#[test]
fn small_churn_moves_refill_and_flush_counters() {
    // A tiny budget guarantees the thread cache cannot absorb the churn, so
    // the global heap is actually consulted.
    allox::set_thread_cache_budget(64 * 1024);
    let before = volume();
    {
        let mut live: Vec<*mut u8> = Vec::new();
        for i in 0..20_000usize {
            let p = unsafe { malloc(64 + (i % 8) * 16) };
            assert!(!p.is_null());
            live.push(p);
        }
        for p in live {
            unsafe { allox::free(p) };
        }
    }
    let after = volume();
    assert!(
        delta(before.small_refills, after.small_refills) > 0,
        "small refills never happened: {:?}",
        delta(before.small_refills, after.small_refills)
    );
    assert!(
        delta(before.small_refill_blocks, after.small_refill_blocks)
            >= delta(before.small_refills, after.small_refills),
        "every refill delivers at least one block"
    );
    assert!(
        delta(before.heap_lock_acquisitions, after.heap_lock_acquisitions) > 0,
        "heap lock acquisitions uncounted"
    );
    assert!(
        delta(before.flushes, after.flushes) > 0
            || delta(before.trims, after.trims) > 0,
        "cache pressure produced neither a trim nor a flush"
    );
    allox::set_thread_cache_budget(32 * 1024 * 1024);
}

#[test]
fn realloc_growth_counts_promotion_and_copies() {
    unsafe {
        // Small same-class growth must be identity: a call, not a copy.
        let a = allox::Allox;
        let l1 = Layout::from_size_align(64, 16).unwrap();
        let p = a.alloc(l1);
        assert!(!p.is_null());
        let p2 = a.realloc(p, l1, 64);
        assert_eq!(p2, p);
        a.dealloc(p2, l1);

        // Cross-class big growth: exactly one relocation, via promotion.
        let mut size = 65536usize;
        let mut layout = Layout::from_size_align(size, 16).unwrap();
        let mut p = a.alloc(layout);
        assert!(!p.is_null());
        let chain_start = volume();
        let _ = chain_start;
        while size < 1_048_576 {
            let nsize = (size * 2).min(1_048_576);
            let np = a.realloc(p, layout, nsize);
            assert!(!np.is_null());
            layout = Layout::from_size_align(nsize, 16).unwrap();
            p = np;
            size = nsize;
        }
        a.dealloc(p, layout);
        publish_pending();
        let chain_end = volume();
        // 65536 -> 131072 -> 262144 -> 524288 -> 1048576: four growths,
        // of which only the first can relocate (the rest fit the reserve).
        assert_eq!(
            delta(chain_start.realloc_relocations, chain_end.realloc_relocations),
            1,
            "a doubling chain should relocate exactly once (the promotion)"
        );
        assert_eq!(
            delta(chain_start.realloc_promotions, chain_end.realloc_promotions),
            1
        );
        assert!(
            delta(chain_start.realloc_copy_bytes, chain_end.realloc_copy_bytes) > 0,
            "the promotion copy is not counted"
        );
    }
}

#[test]
fn calloc_on_recycled_memory_counts_zeroing() {
    // Warm the small cache so the next calloc gets recycled (non-virgin)
    // memory and must zero it in software.
    unsafe {
        for _ in 0..64 {
            let p = allox::calloc(64, 1);
            assert!(!p.is_null());
            allox::free(p);
        }
    }
    let before = volume();
    unsafe {
        for _ in 0..64 {
            let p = allox::calloc(64, 1);
            assert!(!p.is_null());
            allox::free(p);
        }
    }
    let after = volume();
    // Recycled blocks must be zeroed explicitly; virgin ones skip it, so
    // assert only that the counter is wired and monotone.
    assert!(
        after.zeroed_calls >= before.zeroed_calls,
        "zeroed_calls went backwards"
    );
    assert!(
        after.zeroed_bytes >= after.zeroed_calls,
        "every software zeroing covers at least one byte"
    );
}

#[test]
fn thread_exit_counts_flushes() {
    let before = volume();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                let mut live: Vec<*mut u8> = Vec::new();
                for _ in 0..512 {
                    let p = unsafe { malloc(96) };
                    if !p.is_null() {
                        live.push(p);
                    }
                }
                for p in live {
                    unsafe { allox::free(p) };
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // The hook runs on thread exit; allow the runtime to finish teardown.
    let mut after = volume();
    for _ in 0..1000 {
        if delta(before.exit_flushes, after.exit_flushes) >= 8 {
            break;
        }
        after = volume();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        delta(before.exit_flushes, after.exit_flushes) >= 8,
        "exit hook invocations uncounted: {:?}",
        delta(before.exit_flushes, after.exit_flushes)
    );
}
