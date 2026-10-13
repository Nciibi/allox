//! TEMPORARY diagnostic — not a permanent test.
#![cfg(feature = "telemetry")]

#[test]
fn diag_scratch() {
    let idx = |name: &str| {
        allox::__diagnostics::VOLUME_FIELDS
            .iter()
            .position(|n| *n == name)
            .unwrap()
    };
    let (zc_i, ef_i) = (idx("zeroed_calls") as usize, idx("exit_flushes") as usize);
    let (RETIRE_I, SKIP_I) = (idx("remote_frees") as usize, idx("arena_fallbacks") as usize);
    let zc_i = zc_i as usize;
    allox::flush_current_thread();
    let before = allox::__diagnostics::volume_raw();

    let handles: Vec<_> = (0..2)
        .map(|_| {
std::thread::spawn(move || {
                let zc_i = zc_i;
                let inner_before = allox::__diagnostics::volume_raw();
                for _ in 0..2_000 {
                    let p = unsafe { allox::calloc(512, 1) };
                    unsafe { allox::free(p) };
                }
                let inner_after = allox::__diagnostics::volume_raw();
                eprintln!(
                    "TIMING worker self-view: global zeroed_calls moved {} \
                     (globals before={} after={})",
                    inner_after[zc_i].saturating_sub(inner_before[zc_i]),
                    inner_before[zc_i],
                    inner_after[zc_i]
                );
                // Now explicitly publish from inside the worker.
                allox::flush_current_thread();
                let flushed = allox::__diagnostics::volume_raw();
                eprintln!(
                    "TIMING worker after flush_current_thread: global zeroed_calls = {}",
                    flushed[zc_i]
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    for i in 0..200 {
        let v = allox::__diagnostics::volume_raw();
        eprintln!(
            "TIMING main iter {i}: zeroed +{}, exit_flushes +{}, retire +{}, skips +{}",
            v[zc_i].saturating_sub(before[zc_i]),
            v[ef_i].saturating_sub(before[ef_i])
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}