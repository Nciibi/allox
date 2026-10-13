//! TEMPORARY diagnostic — not a permanent test.
#![cfg(feature = "telemetry")]

#[test]
fn diag_scratch() {
    allox::set_thread_cache_budget(1024);
    let t0 = std::time::Instant::now();
    let before = allox::__diagnostics::volume_raw();
    let zeroed_idx = allox::__diagnostics::VOLUME_FIELDS
        .iter()
        .position(|n| *n == "zeroed_calls")
        .unwrap();
    let exit_idx = allox::__diagnostics::VOLUME_FIELDS
        .iter()
        .position(|n| *n == "exit_flushes")
        .unwrap();

    let worker_id = std::thread::spawn(|| {
        eprintln!("TIMING worker {:?} starting", std::thread::current().id());
        for _ in 0..2_000 {
            let p = unsafe { allox::calloc(512, 1) };
            unsafe { allox::free(p) };
        }
    })
    }).join().unwrap();
    eprintln!("TIMING worker was {:?}", worker_id);
    eprintln!("TIMING join returned at {:?} ms", t0.elapsed().as_millis());

    for i in 0..5_000 {
        let v = allox::__diagnostics::volume_raw();
        let zc = v[zeroed_idx].saturating_sub(before[zeroed_idx]);
        let ef = v[exit_idx].saturating_sub(before[exit_idx]);
        if zc > 0 {
            eprintln!(
                "TIMING counters visible after {:?} ms (iter {i}, zeroed_calls +{zc}, exit_flushes +{ef})",
                t0.elapsed().as_millis()
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let v = allox::__diagnostics::volume_raw();
    eprintln!(
        "TIMING NEVER VISIBLE after {:?} ms (exit_flushes +{})",
        t0.elapsed().as_millis(),
        v[exit_idx].saturating_sub(before[exit_idx])
    );
}