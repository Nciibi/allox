//! Thread-exit flush: threads that never call `flush_current_thread` must
//! not pin their caches behind them.
//!
//! The OS exit hook (pthread_key / FlsAlloc) flushes each thread's cache at
//! exit. Metric is mapped-pages growth ACROSS generations of short-lived
//! threads: without the hook each generation pins its dead bins (unbounded
//! linear growth); with it, shared pools absorb the churn and growth
//! flattens into bounded empty/cold retention.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

/// One generation of short-lived churn: small + medium + large, everything
/// freed, nothing explicitly flushed.
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

fn one_generation(workers: usize) {
    let handles: Vec<_> = (0..workers)
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
    // Exit hooks already ran (synchronous at thread exit); clear only this
    // thread's own harness allocations before measuring.
    allox::flush_current_thread();
}

#[test]
fn short_lived_threads_do_not_accumulate() {
    allox::flush_current_thread();
    let mut mapped = Vec::new();
    for g in 0..5 {
        one_generation(4);
        let m = allox::stats().mapped_pages;
        eprintln!("generation {}: mapped_pages={}", g, m);
        mapped.push(m);
    }
    // Growth must flatten: shared retention is bounded (empty/cold caps),
    // so later generations add little. Dead TLS bins would add ~each
    // generation's full footprint (~100+ mappings) every time.
    let early = mapped[1].saturating_sub(mapped[0]);
    let late = mapped[4].saturating_sub(mapped[3]);
    eprintln!("early_delta={} late_delta={}", early, late);
    assert!(
        late < 150,
        "mapped keeps growing across generations (late delta {}) — dead caches?",
        late
    );
    assert!(
        mapped[4].saturating_sub(mapped[0]) < 600,
        "unbounded accumulation across generations: {:?}",
        mapped
    );
}

#[test]
fn free_only_thread_flushes_cached_blocks() {
    const COUNT: usize = 2048;
    const SIZE: usize = 4096;

    allox::flush_current_thread();
    let mut original = [core::ptr::null_mut(); COUNT];
    for p in &mut original {
        let ptr = unsafe { allox::malloc(SIZE) };
        assert!(!ptr.is_null());
        *p = ptr;
        unsafe { ptr.write(0xA5) };
    }
    allox::flush_current_thread();
    original.sort_unstable();

    let original_addr = original.as_ptr() as usize;
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let ptrs = unsafe {
                std::slice::from_raw_parts(original_addr as *const *mut u8, COUNT)
            };
            for p in ptrs {
                allox::free(*p);
            }
        });
    });

    allox::flush_current_thread();
    let mut reused = 0usize;
    let mut second = [core::ptr::null_mut(); COUNT];
    for p in &mut second {
        let ptr = unsafe { allox::malloc(SIZE) };
        assert!(!ptr.is_null());
        *p = ptr;
        if original.binary_search(&ptr).is_ok() {
            reused += 1;
        }
    }
    assert!(
        reused >= COUNT / 2,
        "free-only worker left its cache behind: only {}/{} addresses reused",
        reused,
        COUNT
    );

    for p in second {
        unsafe { allox::free(p) };
    }
    allox::flush_current_thread();
}
