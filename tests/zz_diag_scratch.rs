//! TEMPORARY diagnostic — not a permanent test.
#![cfg(feature = "telemetry")]

#[test]
fn diag_scratch() {
    allox::set_thread_cache_budget(1024);
    let t0 = std::time::Instant::now();
    let before = allox::__diagnostics::volume_raw();
    let idx = |name: &str| {
        allox::__diagnostics::VOLUME_FIELDS
            .iter()
            .position(|n| *n == name)
            .unwrap()
    };
    let (zc_i, ef_i) = (idx("zeroed_calls"), idx("exit_flushes"));

    let handle = std::thread::spawn(|| {
        eprintln!("TIMING worker {:?} starting", std::thread::current().id());
        for _ in 0..2_000 {
            let p = unsafe { allox::calloc(512, 1) };
            unsafe { allox::free(p) };
        }
        eprintln!("TIMING worker {:?} done working", std::thread::current().id());
    });
    handle.join().unwrap();
    eprintln!(
        "TIMING main {:?} joined worker {:?} at {:?} ms",
        std::thread::current().id(),
        handle.thread().id(),
        t0.elapsed().as_millis()
    );

    for i in 0..5_000 {
        let v = allox::__diagnostics::volume_raw();
        let zc = v[zc_i].saturating_sub(before[zc_i]);
        let ef = v[ef_i].saturating_sub(before[ef_i]);
        if zc > 0 {
            eprintln!(
                "TIMING counters visible after {:?} ms (iter {i}, zeroed +{zc}, exit_flushes +{ef})",
                t0.elapsed().as_millis()
            );
            return;
        }
        if i % 500 == 0 {
            eprintln!("TIMING  iter {i}: zeroed +0, exit_flushes +{ef}");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    eprintln!("TIMING NEVER VISIBLE");
}