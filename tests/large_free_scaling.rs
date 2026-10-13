//! Scaling probe for the non-arena large-region free path.
//!
//! Every `free()` of a pointer that is *not* an arena block goes through
//! `large_region_known`, which on non-arena targets (Windows, 32-bit,
//! no_std) resolves the region by walking `LARGE_REGISTRY` — a singly
//! linked list behind one process-global mutex. The walk is O(live large
//! regions) and is paid on every free and every realloc, not just on large
//! ones, because dispatch consults it before the small-page magic check.
//!
//! That makes free() cost scale with the number of *simultaneously live*
//! large regions, which is exactly the shape `tests/stress.rs` builds:
//! ~1/4 of its allocations land above `MAX_MEDIUM_BLOCK`, so the registry
//! holds tens of thousands of entries while ordinary small frees continue.
//!
//! This test is `#[ignore]`d because the signal is a timing ratio and the
//! default suite should not carry a several-minute probe. Run it with:
//!
//! ```text
//! cargo test --test large_free_scaling -- --ignored --nocapture
//! ```
//!
//! Expect it to pass trivially on arena targets (Linux/macOS with `std`),
//! where the lookup is a side-table read, and to be the slow case elsewhere.
//! It is a regression guard for P4.1, not a performance gate.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

/// Above `MAX_MEDIUM_BLOCK`, so every one of these registers a large region.
const LARGE: usize = 512 * 1024;

/// Time `free()` of `n` large regions, given `n` already registered.
///
/// Returns milliseconds. The regions are allocated inside so the registry
/// depth at free time is exactly `n`.
fn free_round_trip_ms(n: usize) -> f64 {
    let mut ptrs = Vec::with_capacity(n);
    for _ in 0..n {
        let p = unsafe { allox::malloc(LARGE) };
        assert!(!p.is_null());
        unsafe { *p = 1 };
        ptrs.push(p);
    }
    // Touch every page so the regions are resident and the cost is not
    // dominated by lazy faulting on first write.
    for p in &ptrs {
        unsafe {
            for i in (0..LARGE).step_by(4096) {
                *p.add(i) = 2;
            }
        }
    }
    let start = std::time::Instant::now();
    for p in ptrs {
        unsafe { allox::free(p) };
    }
    start.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore = "timing-ratio probe; run explicitly"]
fn large_free_does_not_scale_quadratically() {
    // Warm up: first-touch page faults, arena reservation and the first
    // refill of every class all land in the first call.
    free_round_trip_ms(64);

    let small = 1_000usize;
    let large = 16_000usize;

    let t_small = free_round_trip_ms(small);
    let t_large = free_round_trip_ms(large);

    let growth = size_of_large / size_of_small;
    println!(
        "free() of {small} regions: {t_small:.1} ms | {large} regions: {t_large:.1} ms | \
         region count {growth}x, time {:.1}x",
        t_large / t_small
    );

    allox::flush_current_thread();

    // A linear O(n) registry would make the time grow roughly in line with
    // the region count. The O(n^2) walk shows up as an order of magnitude
    // worse. The bound is deliberately loose (sub-quadratic rather than
    // exactly linear) so this does not become a flaky performance test: it
    // only needs to distinguish "grows with n" from "grows with n squared",
    // which differ by ~16x at these sizes.
    let ratio = t_large / t_small.max(0.001);
    assert!(
        ratio < growth as f64 * 4.0,
        "free() cost grew {ratio:.1}x for a {growth}x larger live large-region set; \
         expected well under {}x if the lookup is O(1) or O(n). \
         (This is the LARGE_REGISTRY linear-scan signature.)",
        growth * 4
    );
}

const size_of_small: f64 = 1_000.0;
const size_of_large: f64 = 16_000.0;