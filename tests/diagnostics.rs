//! Sensitivity coverage for the diagnostic counters.
//!
//! The batched volume counters (refills, flushes, trims, owner probes,
//! realloc work) are telemetry-gated, so their tests are too; a default
//! build has no such counters to move. The always-on ones (purges, exit
//! flushes) are checked here and white-box in `lib.rs`.
//!
//! These assert that each counter *moves* for the operation it names, so a
//! wiring regression (counter incremented on the wrong path, or not at all)
//! fails here rather than silently reporting zeros in a benchmark run.

use allox::malloc;
use allox::__diagnostics::volume;
#[cfg(feature = "telemetry")]
use std::alloc::{GlobalAlloc, Layout};

fn delta(before: u64, after: u64) -> u64 {
    after.saturating_sub(before)
}

/// Force the calling thread's counter batch out to the global counters.
/// They are published every 8192 operations, so a test that does less work
/// than that would otherwise read zeros. Only the telemetry-gated counters
/// need this, and they only exist in a telemetry build.
#[cfg(feature = "telemetry")]
fn publish_pending() {
    allox::flush_current_thread();
}

#[cfg(feature = "telemetry")]
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
    publish_pending();
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
        delta(before.flushes, after.flushes) > 0
            || delta(before.trims, after.trims) > 0,
        "cache pressure produced neither a trim nor a flush"
    );
    allox::set_thread_cache_budget(32 * 1024 * 1024);
}

#[cfg(feature = "telemetry")]
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

#[cfg(feature = "telemetry")]
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
    // These counters moved from two shared atomics per call to the thread
    // cache's `Pending` batch (REMAINING_PLAN 4d), so a read only sees them
    // after a publish. Without this the assertions below pass trivially at
    // zero and the test stops being sensitivity-checked.
    publish_pending();
    let before = volume();
    unsafe {
        for _ in 0..64 {
            let p = allox::calloc(64, 1);
            assert!(!p.is_null());
            allox::free(p);
        }
    }
    publish_pending();
    let after = volume();

    // Recycled blocks must be zeroed explicitly, so after the cache is warm
    // every one of these 64 calls must be counted: assert the counters
    // actually move, not merely that they are monotone.
    let calls = delta(before.zeroed_calls, after.zeroed_calls);
    let bytes = delta(before.zeroed_bytes, after.zeroed_bytes);
    assert!(calls > 0, "zeroed_calls did not move: {calls}");
    assert!(bytes >= calls, "every software zeroing covers at least a byte");
    // 64 B blocks, so bytes must be at least 64 per counted call.
    assert!(
        bytes >= calls * 64,
        "expected >= 64 bytes per 64-byte calloc, got {bytes} for {calls} calls"
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
