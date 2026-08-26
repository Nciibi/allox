//! Telemetry feature tests. Run with: cargo test --features telemetry
//!
//! Counters are process-global, so everything runs in one #[test] to stay
//! deterministic regardless of Cargo's default parallel test threads.

#[global_allocator]
static GLOBAL: allox::Allox = allox::Allox;

use allox::telemetry::{snapshot, Telemetry};

fn delta(after: &Telemetry, before: &Telemetry, f: impl Fn(&Telemetry) -> u64) -> u64 {
    f(after).saturating_sub(f(before))
}

#[test]
fn telemetry_accounting() {
    // ---- Exact single-threaded accounting after a forced flush ----
    allox::flush_current_thread();
    let mut ptrs = Vec::with_capacity(10_000); // reserve before baseline
    let before = snapshot();

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
    allox::flush_current_thread();
    let after = snapshot();

    assert_eq!(delta(&after, &before, |t| t.total_allocs), 10_000);
    assert_eq!(delta(&after, &before, |t| t.total_frees), 5_000);
    // 100 B rounds up to the 112 B size class.
    assert_eq!(delta(&after, &before, |t| t.allocated_bytes), 10_000 * 112);
    assert_eq!(delta(&after, &before, |t| t.freed_bytes), 5_000 * 112);

    // Cleanup.
    for p in &ptrs[5_000..] {
        unsafe { allox::free(*p) };
    }
    allox::flush_current_thread();

    // ---- Per-class routing ----
    let before = snapshot();
    unsafe {
        let a = allox::malloc(16); // exactly the first size class
        let b = allox::malloc(17); // second class
        allox::flush_current_thread();
        let mid = snapshot();

        assert_eq!(delta(&mid, &before, |t| t.per_class_allocs[0]), 1);
        let rest: u64 = mid.per_class_allocs[1..].iter().sum();
        let rest_before: u64 = before.per_class_allocs[1..].iter().sum();
        assert_eq!(rest - rest_before, 1);
        allox::free(a);
        allox::free(b);
    }

    // ---- Large allocations are counted too ----
    let before = snapshot();
    unsafe {
        let p = allox::malloc(1 << 20);
        assert!(!p.is_null());
        allox::flush_current_thread();
        let mid = snapshot();
        assert_eq!(delta(&mid, &before, |t| t.large_allocs), 1);
        allox::free(p);
    }
}
