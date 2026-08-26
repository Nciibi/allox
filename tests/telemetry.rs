//! Telemetry feature tests. Run with: cargo test --features telemetry

#[global_allocator]
static GLOBAL: allox::Allox = allox::Allox;

use allox::telemetry::{snapshot, Telemetry};

fn delta(after: &Telemetry, before: &Telemetry, f: impl Fn(&Telemetry) -> u64) -> u64 {
    f(after).saturating_sub(f(before))
}

/// Exact single-threaded accounting after a forced flush.
#[test]
fn exact_counts_after_flush() {
    allox::flush_current_thread();
    let before = snapshot();

    let mut ptrs = Vec::new();
    for i in 0..10_000u32 {
        unsafe {
            let p = allox::malloc(100);
            assert!(!p.is_null());
            *p = i as u8;
            ptrs.push(p);
        }
    }
    for p in &ptrs[..5_000] {
        unsafe { allox::free(*p) };
    }

    // Publish this thread's pending deltas.
    allox::flush_current_thread();
    let after = snapshot();

    assert_eq!(delta(&after, &before, |t| t.total_allocs), 10_000);
    assert_eq!(delta(&after, &before, |t| t.total_frees), 5_000);
    assert_eq!(
        delta(&after, &before, |t| t.allocated_bytes),
        10_000 * 112 // 100 B rounds up to the 112 B class
    );
    assert_eq!(
        delta(&after, &before, |t| t.freed_bytes),
        5_000 * 112
    );

    // Peak must have grown.
    assert!(after.peak_live_bytes > before.peak_live_bytes || before.total_allocs == 0);

    // Cleanup so other tests start from a quiet heap.
    for p in &ptrs[5_000..] {
        unsafe { allox::free(*p) };
    }
}

/// Per-class counters route sizes to the expected class bucket.
#[test]
fn per_class_routing() {
    allox::flush_current_thread();
    let before = snapshot();

    unsafe {
        let a = allox::malloc(64); // exactly the first class
        let b = allox::malloc(65); // next class up
        allox::flush_current_thread();
        let mid = snapshot();

        assert_eq!(delta(&mid, &before, |t| t.per_class_allocs[0]), 1);
        let rest: u64 = mid.per_class_allocs[1..].iter().sum();
        let rest_before: u64 = before.per_class_allocs[1..].iter().sum();
        assert_eq!(rest - rest_before, 1);

        allox::free(a);
        allox::free(b);
    }
    allox::flush_current_thread();
}
