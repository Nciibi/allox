//! Thread-exit flush: short-lived threads that never call
//! `flush_current_thread` must not pin their caches behind them.
//!
//! The OS exit hook (pthread_key / FlsAlloc) flushes each thread's cache at
//! exit so the next generation reuses shared pools instead of mapping fresh.
//! Metric is reuse (new maps for a repeat workload), not raw mapped counts:
//! empty/cold retention legitimately keeps virtual mappings by design.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

/// Same churn pattern for every generation: small blocks with a live set.
fn churn_small(seed: u64) {
    let mut live: Vec<(*mut u8, usize)> = Vec::with_capacity(512);
    for i in 0..20_000usize {
        let size = 64 + ((i as u64 * 37 + seed) % 4000) as usize;
        let p = unsafe { allox::malloc(size) };
        assert!(!p.is_null());
        unsafe {
            *p = 0xAB;
        }
        live.push((p, size));
        if live.len() > 512 {
            let (fp, _) = live.swap_remove(0);
            unsafe { allox::free(fp) };
        }
    }
    for (fp, _) in live {
        unsafe { allox::free(fp) };
    }
    // Deliberately NO flush — the exit hook must handle it.
}

/// Medium spans + large regions, freed but never flushed.
fn churn_medium_large(seed: u64) {
    let mut live = Vec::new();
    for i in 0..2000usize {
        let size = 20_000 + ((i as u64 * 7919 + seed) % 200_000) as usize;
        let p = unsafe { allox::malloc(size) };
        assert!(!p.is_null());
        unsafe {
            *p = 0xCD;
        }
        live.push((p, size));
        if live.len() > 128 {
            let (fp, _) = live.swap_remove(0);
            unsafe { allox::free(fp) };
        }
    }
    for (fp, _) in live {
        unsafe { allox::free(fp) };
    }
}

fn spawn_workers<F>(n: usize, f: F)
where
    F: Fn(u64) + Send + Sync + Copy + 'static,
{
    let handles: Vec<_> = (0..n)
        .map(|t| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || f(t as u64 * 0x9E3779B9 + 7))
                .unwrap()
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn dead_thread_caches_are_reclaimed() {
    // Warm-up generation on workers: populates shared pools.
    spawn_workers(8, churn_small);
    allox::flush_current_thread();

    // Repeat the same work on this thread: with exit-flush, everything the
    // workers freed is back in shared pools, so almost no new maps.
    // Without it, dead TLS pins tens of MB and this maps fresh pages.
    let m0 = allox::stats().map_calls;
    churn_small(0x1234);
    churn_small(0x5678);
    allox::flush_current_thread();
    let new_maps = allox::stats().map_calls - m0;
    eprintln!("thread_exit small: new_maps={}", new_maps);
    assert!(
        new_maps < 300,
        "dead threads pinned caches ({} fresh maps for repeat work)",
        new_maps
    );
}

#[test]
fn medium_and_large_caches_reclaimed_on_exit() {
    spawn_workers(4, churn_medium_large);
    allox::flush_current_thread();

    let m0 = allox::stats().map_calls;
    churn_medium_large(0x1234);
    churn_medium_large(0x5678);
    allox::flush_current_thread();
    let new_maps = allox::stats().map_calls - m0;
    eprintln!("thread_exit medium/large: new_maps={}", new_maps);
    assert!(
        new_maps < 300,
        "dead threads pinned caches ({} fresh maps for repeat work)",
        new_maps
    );
}
