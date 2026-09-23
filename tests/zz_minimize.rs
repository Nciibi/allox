//! TEMPORARY minimizer (to be deleted): which size class triggers the
//! thread_exit corruption? MODE=small|medium|large selects the churn.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

fn churn(mode: &str) {
    match mode {
        "small" => {
            let mut v = Vec::with_capacity(3000);
            for _ in 0..3000 {
                let p = unsafe { allox::malloc(4096) };
                assert!(!p.is_null());
                unsafe { *p = 0xAB };
                v.push(p);
            }
            for p in v {
                unsafe { allox::free(p) };
            }
            // FLUSH=1: flush explicitly before thread exit (hook becomes no-op).
            if std::env::var("FLUSH").as_deref() == Ok("1") {
                allox::flush_current_thread();
            }
        }
        "medium" => {
            let mut v = Vec::with_capacity(200);
            for _ in 0..200 {
                let p = unsafe { allox::malloc(32768) };
                assert!(!p.is_null());
                unsafe { *p = 0xCD };
                v.push(p);
            }
            for p in v {
                unsafe { allox::free(p) };
            }
        }
        "large" => {
            let mut v = Vec::with_capacity(20);
            for _ in 0..20 {
                let p = unsafe { allox::malloc(524288) };
                assert!(!p.is_null());
                unsafe { *p = 0xEF };
                v.push(p);
            }
            for p in v {
                unsafe { allox::free(p) };
            }
        }
        "single" => {
            // Same small churn, but on ONE long-lived thread (the test
            // thread): no thread lifecycle, no concurrency at all.
            for _ in 0..5 {
                let mut v = Vec::with_capacity(3000);
                for _ in 0..3000 {
                    let p = unsafe { allox::malloc(4096) };
                    assert!(!p.is_null());
                    unsafe { *p = 0xAB };
                    v.push(p);
                }
                for p in v {
                    unsafe { allox::free(p) };
                }
                allox::flush_current_thread();
            }
        }
        _ => panic!("MODE=small|medium|large"),
    }
}

#[test]
fn zz_minimize() {
    if let Ok(b) = std::env::var("BUDGET_MB") {
        if let Ok(mb) = b.parse::<usize>() {
            allox::set_thread_cache_budget(mb * 1024 * 1024);
        }
    }
    let mode: &'static str = match std::env::var("MODE").unwrap().as_str() {
        "small" => "small",
        "medium" => "medium",
        "large" => "large",
        "single" => "single",
        _ => panic!("MODE=small|medium|large|single"),
    };
    if mode == "single" {
        churn(mode);
        return;
    }
    allox::flush_current_thread();
    for _ in 0..5 {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                std::thread::Builder::new()
                    .stack_size(1 << 20)
                    .spawn(move || churn(mode))
                    .unwrap()
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        allox::flush_current_thread();
    }
}
