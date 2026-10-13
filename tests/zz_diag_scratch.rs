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
    let (zc_i, ef_i) = (idx("zeroed_calls"), idx("exit_flushes"));

    allox::flush_current_thread();
    let before = allox::__diagnostics::volume_raw();

    let handles: Vec<_> = (0..4)
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
    eprintln!("TIMING joined 4 workers");

    for i in 0..3_000 {
        let v = allox::__diagnostics::volume_raw();
        let zc = v[zc_i].saturating_sub(before[zc_i]);
        let ef = v[ef_i].saturating_sub(before[ef_i]);
        if i % 300 == 0 || zc > 0 {
            eprintln!("TIMING iter {i}: zeroed +{zc}, exit_flushes +{ef}");
        }
        if zc > 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    eprintln!("TIMING never visible");
}