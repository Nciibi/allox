//! Thread-exit flush: a thread that never calls `flush_current_thread` must
//! not pin its caches behind it.
//!
//! The OS exit hook (pthread_key / FlsAlloc) flushes each thread's cache at
//! exit so the next generation reuses shared pools instead of mapping fresh.
//!
//! Methodology: ONE worker frees everything into its bins while staying
//! UNDER the trim budget (so nothing reaches shared pools during the run —
//! dead TLS bins are the only variable), then exits without flushing. The
//! main thread repeats the identical pattern: with the hook this maps
//! almost nothing fresh; without it, every block must be mapped fresh.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

/// Frees `n_small` 4 KiB blocks + `n_medium` 32 KiB blocks + `n_large`
/// 512 KiB regions, all into thread-local bins, then returns WITHOUT
/// flushing. Totals must stay under the 32 MiB trim budget so shared pools
/// stay cold and the dead cache is the only thing being tested.
fn churn_no_flush() {
    let mut small = Vec::with_capacity(3000);
    for _ in 0..3000 {
        let p = unsafe { allox::malloc(4096) };
        assert!(!p.is_null());
        unsafe {
            *p = 0xAB;
        }
        small.push(p);
    }
    let mut medium = Vec::with_capacity(200);
    for _ in 0..200 {
        let p = unsafe { allox::malloc(32768) };
        assert!(!p.is_null());
        unsafe {
            *p = 0xCD;
        }
        medium.push(p);
    }
    let mut large = Vec::with_capacity(20);
    for _ in 0..20 {
        let p = unsafe { allox::malloc(524288) };
        assert!(!p.is_null());
        unsafe {
            *p = 0xEF;
        }
        large.push(p);
    }
    for p in small {
        unsafe { allox::free(p) };
    }
    for p in medium {
        unsafe { allox::free(p) };
    }
    for p in large {
        unsafe { allox::free(p) };
    }
    // Deliberately NO flush — the exit hook must handle it.
}

#[test]
fn exiting_thread_releases_its_cache() {
    // One worker generation primes nothing shared (under-budget churn goes
    // to bins, and bins die with the thread unless the hook fires).
    std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(churn_no_flush)
        .unwrap()
        .join()
        .unwrap();
    allox::flush_current_thread();

    // Identical repeat on this thread: measures fresh maps only.
    let m0 = allox::stats().map_calls;
    churn_no_flush();
    allox::flush_current_thread();
    let new_maps = allox::stats().map_calls - m0;
    eprintln!("thread_exit: new_maps={}", new_maps);
    // Without the hook this is ~75 (47 small pages + ~20 spans + stash);
    // with it, shared pools serve everything.
    assert!(
        new_maps < 15,
        "exiting thread pinned its cache ({} fresh maps for repeat work)",
        new_maps
    );
}

#[test]
fn many_short_lived_threads_stay_bounded() {
    // Herd smoke test: concurrent exits must not abort, deadlock, or lose
    // unbounded memory (try path removed — exit flush blocks briefly).
    for _ in 0..3 {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::Builder::new()
                    .stack_size(1 << 20)
                    .spawn(churn_no_flush)
                    .unwrap()
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
    allox::flush_current_thread();
    let s = allox::stats();
    eprintln!("thread_exit herd: mapped_pages={}", s.mapped_pages);
}
