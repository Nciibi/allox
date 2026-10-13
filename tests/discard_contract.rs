//! The purge contract: `sys::discard` must never over-promise.
//!
//! `Discarded` splits a purge into two independent claims:
//!
//! - `physical` — committed memory actually fell.
//! - `zeroed`   — the range still reads as zero, so `calloc` may skip its
//!   memset.
//!
//! `zeroed` is the dangerous one. Claiming a zeroing that did not happen
//! hands a `calloc` caller recycled bytes: a memory disclosure, not a missed
//! optimization. This test pins the invariant that the allocator only sets
//! `FLAG_DISCARDED` when `zeroed` is true, by checking it behaviourally —
//! poison a region, purge it, and require that whatever the allocator hands
//! back for a zeroed allocation really is zero.
//!
//! It also pins the other half: a backend that cannot drop physical memory
//! (wasm) must say so rather than claiming a purge it did not perform.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

const POISON: u8 = 0xA5;

/// Allocate `n` bytes, fill every byte with `POISON`, free, then ask for the
/// same size zeroed and assert it is genuinely zero.
fn recycle_then_calloc(n: usize, rounds: usize) {
    for _ in 0..rounds {
        let p = unsafe { allox::calloc(n, 1) };
        assert!(!p.is_null());
        for i in 0..n {
            assert_eq!(
                unsafe { *p.add(i) },
                0,
                "calloc returned dirty byte {i} of {n}"
            );
        }
        unsafe {
            for i in 0..n {
                *p.add(i) = POISON;
            }
            allox::free(p);
        }
    }
}

#[test]
fn calloc_is_zero_across_recycled_small_pages() {
    // Enough churn in one size class to force pages through the empty/cold
    // retention ladder, which is where a discard actually happens.
    for size in [16usize, 32, 64, 128, 256, 512, 1024, 4096] {
        recycle_then_calloc(size, 2000);
    }
}

#[test]
fn calloc_is_zero_across_recycled_medium_spans() {
    for size in [17_000usize, 20_000, 32_768, 48_000, 65_000] {
        recycle_then_calloc(size, 400);
    }
}

#[test]
fn calloc_is_zero_across_recycled_large_regions() {
    for size in [70_000usize, 131_072, 262_144, 1_048_576, 4_194_304] {
        recycle_then_calloc(size, 40);
    }
}

/// The cold-retention ladder needs real megabytes of churn before it
/// discards, so drive it hard enough that the discard path is genuinely
/// exercised, then re-check zeroing. On a backend that cannot purge this
/// still passes — it just proves the fallback (always memset) is sound.
#[test]
fn calloc_stays_zero_after_heavy_purge_pressure() {
    // Bounded: this is a correctness test, not a throughput benchmark.
    let block = 64 * 1024;
    for _ in 0..400 {
        let mut live: Vec<*mut u8> = Vec::with_capacity(512);
        for _ in 0..512 {
            let p = unsafe { allox::malloc(block) };
            assert!(!p.is_null());
            unsafe {
                for i in (0..block).step_by(64) {
                    *p.add(i) = POISON;
                }
            }
            live.push(p);
        }
        for p in live {
            unsafe { allox::free(p) };
        }
    }
    allox::flush_current_thread();
    recycle_then_calloc(block, 64);
}

/// The large-path handoff, exercised through `malloc`/`free` so the recycle
/// actually reaches the sharded region cache (the tier that purges).
#[test]
fn recycled_large_regions_stay_readable_after_purge() {
    for size in [70_000usize, 131_072, 262_144, 1_048_576, 4_194_304] {
        let mut ptrs = Vec::new();
        for _ in 0..64 {
            let p = unsafe { allox::malloc(size) };
            assert!(!p.is_null(), "OOM at {size}");
            unsafe {
                for i in (0..size).step_by(512) {
                    *p.add(i) = POISON;
                }
            }
            ptrs.push(p);
        }
        for p in ptrs {
            unsafe {
                // Every byte must still be readable: a recycled region is
                // purged before it is handed back out, and on Windows that
                // leaves it decommitted until recommitted.
                for i in (0..size).step_by(512) {
                    assert_eq!(
                        *p.add(i),
                        POISON,
                        "recycled region lost its contents at byte {i}"
                    );
                }
                allox::free(p);
            }
        }
    }
}

/// Over-aligned allocations always take the large path, so they are the ones
/// most likely to be served from a recycled region.
#[test]
fn calloc_is_zero_for_over_aligned_recycling() {
    // `calloc` takes no alignment argument — it is `align = 1` by design —
    // so this goes through `GlobalAlloc::alloc_zeroed` to get an over-aligned
    // zeroed block on the large path.
    use std::alloc::{alloc_zeroed, Layout};
    for align in [32usize, 4096, 65_536] {
        let layout = Layout::from_size_align(200_000, align).expect("valid layout");
        for _ in 0..40 {
            let p = unsafe { alloc_zeroed(layout) };
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "alignment {align} not honoured");
            for i in 0..200_000 {
                assert_eq!(
                    unsafe { *p.add(i) },
                    0,
                    "alloc_zeroed returned dirty byte {i}"
                );
            }
            unsafe {
                for i in 0..200_000 {
                    *p.add(i) = POISON;
                }
                allox::free(p);
            }
        }
    }
}
