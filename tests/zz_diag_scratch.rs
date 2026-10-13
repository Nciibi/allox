//! TEMPORARY diagnostic — not a permanent test.
#![cfg(feature = "telemetry")]

use allox::__diagnostics::VOLUME_FIELDS;

fn show(label: &str, before: [u64; 23], after: [u64; 23]) {
    println!("--- {label}");
    for (i, name) in VOLUME_FIELDS.iter().enumerate() {
        let d = after[i].saturating_sub(before[i]);
        if d != 0 {
            println!("    {name:<26} +{d}");
        }
    }
}

#[test]
fn diag_scratch() {
    // --- phase 1: main thread, default budget ---
    let b = allox::__diagnostics::volume_raw();
    for _ in 0..2_000 {
        let p = unsafe { allox::calloc(512, 1) };
        unsafe { allox::free(p) };
    }
    allox::flush_current_thread();
    let a = allox::__diagnostics::volume_raw();
    show("main thread, default budget", b, a);

    // --- phase 2: main thread, tiny budget ---
    allox::set_thread_cache_budget(1024);
    let b = allox::__diagnostics::volume_raw();
    for _ in 0..2_000 {
        let p = unsafe { allox::calloc(512, 1) };
        unsafe { allox::free(p) };
    }
    allox::flush_current_thread();
    let a = allox::__diagnostics::volume_raw();
    show("main thread, tiny budget", b, a);

    // --- phase 3: worker thread, tiny budget ---
    let b = allox::__diagnostics::volume_raw();
    std::thread::spawn(|| {
        for _ in 0..2_000 {
            let p = unsafe { allox::calloc(512, 1) };
            unsafe { allox::free(p) };
        }
    })
    .join()
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let a = allox::__diagnostics::volume_raw();
    show("worker thread, tiny budget", b, a);

    // --- phase 4: worker thread, explicit flush (bypasses the exit hook) ---
    let b = allox::__diagnostics::volume_raw();
    std::thread::spawn(|| {
        for _ in 0..2_000 {
            let p = unsafe { allox::calloc(512, 1) };
            unsafe { allox::free(p) };
        }
        allox::flush_current_thread();
    })
    .join()
    .unwrap();
    let a = allox::__diagnostics::volume_raw();
    show("worker thread + explicit flush", b, a);
}