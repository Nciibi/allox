//! Zero-dependency benchmark harness (no criterion): measures allocation
//! throughput of `allox` against the system allocator.
//!
//! Run with: cargo bench

use std::alloc::{GlobalAlloc, Layout, System};
use std::time::{Duration, Instant};

#[global_allocator]
static GLOBAL: allox::Allox = allox::Allox;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn bench<A: GlobalAlloc + Sync + ?Sized>(
    name: &str,
    alloc: &'static A,
    threads: usize,
    seconds: u64,
    size_range: (usize, usize),
) {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n, 16).unwrap();

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut rng = Rng(0xDA2_2025 ^ ((t as u64 + 1) * 0x9E3779B97F4A7C15));
                    let mut ops = 0u64;
                    let mut live: Vec<(*mut u8, usize)> = Vec::with_capacity(1024);
                    while Instant::now() < stop {
                        for _ in 0..10_000 {
                            let size = if size_range.0 == size_range.1 {
                                size_range.0
                            } else {
                                size_range.0 + (rng.next() as usize) % (size_range.1 - size_range.0)
                            };
                            let p = unsafe { alloc.alloc(layout_for(size)) };
                            if p.is_null() {
                                std::process::exit(2);
                            }
                            unsafe { *p = ops as u8 };
                            live.push((p, size));
                            if live.len() > 512 && rng.next() % 2 == 0 {
                                let idx = (rng.next() as usize) % live.len();
                                let (p, s) = live.swap_remove(idx);
                                unsafe { alloc.dealloc(p, layout_for(s)) };
                            }
                            ops += 1;
                        }
                    }
                    for (p, s) in live {
                        unsafe { alloc.dealloc(p, layout_for(s)) };
                    }
                    ops
                })
                .unwrap()
        })
        .collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    println!(
        "{:<28} {:>3} thread(s) {:>6}-{:<6} B  {:>12.0} ops/s",
        name,
        threads,
        size_range.0,
        size_range.1,
        total as f64 / seconds as f64
    );
}

fn run_suite(label: &str, alloc: &'static dyn GlobalAllocSync) {
    for &(threads, range) in &[
        (1, (16, 16)),
        (1, (16, 4096)),
        (4, (16, 256)),
        (8, (16, 4096)),
        (1, (20000, 65536)),
    ] {
        bench(label, alloc, threads, 3, range);
    }
}

/// Helper to pass either allocator behind a dyn pointer.
trait GlobalAllocSync: GlobalAlloc + Sync {}
impl<T: GlobalAlloc + Sync> GlobalAllocSync for T {}

fn main() {
    println!("=== allox benchmark suite (ops = alloc + some dealloc) ===");
    run_suite("allox", &GLOBAL);
    println!();
    run_suite("system", &System);
}
