//! TEMPORARY diagnostic — not a permanent test.
#![cfg(feature = "telemetry")]

#[test]
fn diag_scratch() {
    let idx = |name: &str| {
        allox::__diagnostics::VOLUME_FIELDS
            .iter()
            .position(|n| *n == name)
            .unwrap() as usize
    };
    let zc_i = idx("zeroed_calls");
    let ef_i = idx("exit_flushes");
    // Temporary probes: remote_frees = retire() entered, arena_fallbacks =
    // retire() skipped because the TLS reentrancy guard was set.
    let retire_i = idx("remote_frees");
    let skip_i = idx("arena_fallbacks");
    let nz_i = idx("owner_probes");

    allox::flush_current_thread();
    let before = allox::__diagnostics::volume_raw();

    let handles: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..2_000 {
                    let p = unsafe { allox::calloc(512, 1) };
                    unsafe { allox::free(p) };
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    for i in 0..200 {
        let v = allox::__diagnostics::volume_raw();
        eprintln!(
            "TIMING iter {i}: zeroed +{}, exit_flushes +{}, retire +{}, skips +{}, note_zeroed_calls +{}",
            v[zc_i].saturating_sub(before[zc_i]),
            v[ef_i].saturating_sub(before[ef_i]),
            v[retire_i].saturating_sub(before[retire_i]),
            v[skip_i].saturating_sub(before[skip_i]),
            v[nz_i].saturating_sub(before[nz_i]),
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}