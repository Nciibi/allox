//! TEMPORARY §5 experiment (delete after): same 16-4096B churn through
//! short-lived threads (spawn+exit+flush per batch, like spawn-churn) vs
//! long-lived threads (lifecycle amortized). Distinguishes lifecycle costs
//! from per-op costs.

use std::alloc::{GlobalAlloc, Layout};
use std::time::Instant;

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

fn churn_burst(seed: u64, ops: usize) -> u64 {
    let mut rng = Rng(seed);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let mut live: Vec<(*mut u8, usize)> = Vec::with_capacity(256);
    let mut n = 0u64;
    for _ in 0..ops {
        let size = 16 + (rng.next() as usize) % (4096 - 16);
        let p = unsafe { GLOBAL.alloc(layout_for(size)) };
        if p.is_null() {
            break;
        }
        unsafe { *p = 0xAB };
        live.push((p, size));
        n += 1;
        if rng.next() % 2 == 0 && !live.is_empty() {
            let idx = (rng.next() as usize) % live.len();
            let (fp, fs) = live.swap_remove(idx);
            unsafe { GLOBAL.dealloc(fp, layout_for(fs)) };
        }
    }
    for (fp, fs) in live {
        unsafe { GLOBAL.dealloc(fp, layout_for(fs)) };
    }
    n
}

fn main() {
    let mode = std::env::var("MODE").unwrap_or("short".to_string());
    let secs: u64 = std::env::var("SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let t0 = Instant::now();
    let stop = t0 + std::time::Duration::from_secs(secs);
    let mut ops = 0u64;
    if mode == "short" {
        let mut seed = 0xC0FFEEu64;
        while Instant::now() < stop {
            let mut batch = Vec::new();
            for _ in 0..4 {
                seed = seed.wrapping_mul(0x2545F4914F6CDD1D).wrapping_add(1);
                let s = seed;
                batch.push(
                    std::thread::Builder::new()
                        .stack_size(1 << 20)
                        .spawn(move || churn_burst(s, 2000))
                        .unwrap(),
                );
            }
            for h in batch {
                ops += h.join().unwrap();
            }
        }
    } else {
        // long-lived: 4 persistent workers churn until stop.
        let mut batch = Vec::new();
        for t in 0..4 {
            batch.push(
                std::thread::Builder::new()
                    .stack_size(1 << 20)
                    .spawn(move || {
                        let mut n = 0u64;
                        let mut seed = 0xC0FFEEu64 + t * 0x9E3779B9;
                        while Instant::now() < stop {
                            seed = seed.wrapping_mul(0x2545F4914F6CDD1D).wrapping_add(1);
                            n += churn_burst(seed, 2000);
                        }
                        n
                    })
                    .unwrap(),
            );
        }
        for h in batch {
            ops += h.join().unwrap();
        }
    }
    let dt = t0.elapsed().as_secs_f64();
    println!("mode={} ops={} ops/s={:.0}", mode, ops, ops as f64 / dt);
}
