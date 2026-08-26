//! Telemetry feature tests. Run with: cargo test --features telemetry

#[global_allocator]
static GLOBAL: allox::Allox = allox::Allox;

use allox::telemetry::snapshot;

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

    assert_eq!(after.total_allocs - before.total_allocs, 10_000);
    assert_eq!(after.total_frees - before.total_frees, 5_000);
    assert_eq!(after.live_allocs_delta(before), 5_000);

    // Retained bytes are class-rounded: 100 B lands in the 112 B class.
    assert_eq!(after.bytes_in_delta(&before), 10_000 * 112);
    assert!(after.live_bytes >= 5_000 * 112 - before.live_bytes.min(0));

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
        let a = allox::malloc(64); // class of exactly 64
        let b = allox::malloc(65); // next class up
        allox::flush_current_thread();
        let mid = snapshot();

        let d64 = mid.per_class_allocs[0] - before.per_class_allocs[0];
        let d_rest: u64 = mid.per_class_allocs[1..].iter().sum::<u64>()
            - before.per_class_allocs[1..].iter().sum::<u64>();
        assert_eq!(d64, 1);
        assert_eq!(d_rest, 1);

        allox::free(a);
        allox::free(b);
    }
    allox::flush_current_thread();
}

impl TelemetryExt for allox::telemetry::Telemetry {}
trait TelemetryExt {
    fn live_allocs_delta(&self, before: &allox::telemetry::Telemetry) -> u64;
    fn bytes_in_delta(&self, before: &allox::telemetry::Telemetry) -> u64;
}
impl TelemetryExt for allox::telemetry::Telemetry {
    fn live_allocs_delta(&self, before: &Self) -> u64 {
        self.live_allocs.saturating_sub(before.live_allocs)
    }
    fn bytes_in_delta(&self, before: &Self) -> u64 {
        self.allocated_bytes.saturating_sub(before.allocated_bytes)
    }
}
