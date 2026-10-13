//! TEMPORARY diagnostic — not a permanent test.
#![cfg(feature = "telemetry")]

use allox::__diagnostics::VOLUME_FIELDS;

#[test]
fn diag_scratch() {
    allox::set_thread_cache_budget(1024);
    let before = allox::__diagnostics::volume_raw();
    let t = std::thread::spawn(|| {
        for _ in 0..2_000 {
            let p = unsafe { allox::calloc(512, 1) };
            unsafe { allox::free(p) };
        }
    });
    t.join().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after = allox::__diagnostics::volume_raw();

    println!("{:<26} {:>10} {:>10}", "counter", "before", "after");
    for (i, name) in VOLUME_FIELDS.iter().enumerate() {
        let d = after[i].saturating_sub(before[i]);
        println!("{:<26} {:>10} {:>10}  (+{d})", name, before[i], after[i]);
    }
}