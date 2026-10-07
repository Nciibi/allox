//! TEMPORARY diagnostic, not a committed test.
//!
//! Question: is the growth in tests/thread_exit.rs a leak, or is the test
//! simply measuring inside the fill phase of bounded cold retention?
//! Answer by running far more generations and looking at the shape.
//!
//! Workload is scaled down (1/8 per thread) so the retention caps fill
//! relative to the churn and the answer arrives in a bounded number of
//! generations, and so total memory stays modest on a loaded box.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

const SCALE: usize = 8;

fn churn_no_flush() {
    let mut small = Vec::with_capacity(3000 / SCALE);
    for _ in 0..(3000 / SCALE) {
        let p = unsafe { allox::malloc(4096) };
        assert!(!p.is_null());
        unsafe { *p = 0xAB };
        small.push(p);
    }
    let mut medium = Vec::with_capacity(200 / SCALE);
    for _ in 0..(200 / SCALE) {
        let p = unsafe { allox::malloc(32768) };
        assert!(!p.is_null());
        unsafe { *p = 0xCD };
        medium.push(p);
    }
    let mut large = Vec::with_capacity(20 / SCALE);
    for _ in 0..(20 / SCALE) {
        let p = unsafe { allox::malloc(524288) };
        assert!(!p.is_null());
        unsafe { *p = 0xEF };
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
    allox::flush_current_thread();
}

#[test]
fn growth_shape_over_many_generations() {
    const GENERATIONS: usize = 30;
    allox::flush_current_thread();
    let mut mapped = Vec::new();
    for g in 0..GENERATIONS {
        one_generation(4);
        let m = allox::stats().mapped_pages;
        println!("gen {:>2}: mapped_pages={}", g, m);
        mapped.push(m);
    }

    println!("\n--- deltas ---");
    for g in 1..mapped.len() {
        let d = mapped[g].saturating_sub(mapped[g - 1]);
        println!("gen {:>2} delta={}", g, d);
    }

    // Tail window: last third of the run, which is the part that would expose
    // linear (leak) versus flat (bounded retention).
    let tail_start = (GENERATIONS * 2) / 3;
    let tail: u64 = mapped[tail_start..]
        .iter()
        .zip(mapped[tail_start - 1..mapped.len() - 1].iter())
        .map(|(a, b)| a.saturating_sub(*b))
        .sum();
    let head: u64 = mapped[1..=tail_start]
        .iter()
        .zip(mapped[..tail_start].iter())
        .map(|(a, b)| a.saturating_sub(*b))
        .sum();

    println!("\nper-generation delta, first third  total={} over {} gens", head, tail_start);
    println!("per-generation delta, last  third  total={} over {} gens", tail, GENERATIONS - tail_start);
    println!("full run growth={} pages", mapped[GENERATIONS - 1].saturating_sub(mapped[0]));
    println!("(workload is 1/{} of the committed test)", SCALE);
}