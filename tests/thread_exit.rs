//! Thread-exit flush: short-lived threads that never call
//! `flush_current_thread` must not pin their caches behind them.
//!
//! The OS exit hook (pthread_key / FlsAlloc) best-effort flushes each
//! thread's cache at exit using try-locks only.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

#[test]
fn dead_thread_caches_are_reclaimed() {
    allox::flush_current_thread();
    let base = allox::stats().mapped_pages;

    let handles: Vec<_> = (0..8)
        .map(|t| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut live: Vec<(*mut u8, usize)> = Vec::with_capacity(512);
                    for i in 0..20_000usize {
                        let size = 64 + ((i * 37 + t * 101) % 4000);
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
                })
                .unwrap()
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // Exit hooks run synchronously at thread exit, so by join everything is
    // already flushed; clear the main thread's own harness allocations too.
    allox::flush_current_thread();
    let after = allox::stats().mapped_pages;
    eprintln!("thread_exit: base={} after={}", base, after);
    assert!(
        after <= base + 64,
        "dead threads pinned {} extra pages (base {} -> {})",
        after.saturating_sub(base),
        base,
        after
    );
}

#[test]
fn medium_and_large_caches_reclaimed_on_exit() {
    allox::flush_current_thread();
    let base = allox::stats().mapped_pages;

    let handles: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                let mut live = Vec::new();
                // Medium spans + large regions, freed but never flushed.
                for i in 0..2000usize {
                    let size = 20_000 + (i * 7919) % 200_000;
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
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    allox::flush_current_thread();
    let after = allox::stats().mapped_pages;
    eprintln!("thread_exit medium/large: base={} after={}", base, after);
    // Large regions unmap on flush; medium spans return to shared lists or
    // bounded empty/cold retention — either way, no per-thread pinning.
    assert!(
        after <= base + 128,
        "dead threads pinned {} extra pages (base {} -> {})",
        after.saturating_sub(base),
        base,
        after
    );
}
