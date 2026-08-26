//! Comparative allocator benchmark: allox vs system vs talc.
//!
//! All allocators run identical workloads; results are ops/s plus a
//! relative table. The harness itself allocates through the process global
//! (= allox); that overhead is identical for all measured allocators.
//!
//! Run with: cargo bench

use std::alloc::{GlobalAlloc, Layout, System};
use std::time::{Duration, Instant};

#[global_allocator]
static GLOBAL: allox::Allox = allox::Allox;

use spinning_top::RawSpinlock;
use talc::{source::Claim, DefaultBinning, TalcLock};

// talc needs an initial arena; give it a generous one so large allocations
// don't fail. Lives in BSS: costs nothing on disk or RSS until touched.
static TALC: TalcLock<RawSpinlock, Claim> = TalcLock::new(unsafe {
    static mut ARENA: [u8; 512 * 1024 * 1024] = [0; 512 * 1024 * 1024];
    Claim::array(&raw mut ARENA)
});

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

struct Workload {
    name: &'static str,
    threads: usize,
    /// (min, max) allocation size; equal bounds = fixed size
    size_range: (usize, usize),
    /// fraction of ops that are frees (0..100)
    free_pct: u64,
}

const WORKLOADS: &[Workload] = &[
    Workload { name: "tight-small 1T", threads: 1, size_range: (64, 64), free_pct: 40 },
    Workload { name: "mixed-small 1T", threads: 1, size_range: (16, 4096), free_pct: 50 },
    Workload { name: "tight-small 8T", threads: 8, size_range: (64, 64), free_pct: 40 },
    Workload { name: "mixed-small 8T", threads: 8, size_range: (16, 4096), free_pct: 50 },
    Workload { name: "mixed-all 8T", threads: 8, size_range: (16, 65536), free_pct: 50 },
];

fn run<A: GlobalAlloc + Sync + 'static>(alloc: &'static A, wl: &Workload, seconds: u64) -> f64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for =
        |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let threads = wl.threads;
    let size_range = wl.size_range;
    let free_pct = wl.free_pct;

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut rng =
                        Rng(0x9E3779B97F4A7C15 ^ ((t as u64 + 1).wrapping_mul(0xD1B54A32D192ED03)));
                    let mut live: Vec<(*mut u8, usize)> = Vec::with_capacity(1024);
                    let mut ops = 0u64;
                    while Instant::now() < stop {
                        for _ in 0..10_000 {
                            let size = if size_range.0 == size_range.1 {
                                size_range.0
                            } else {
                                size_range.0
                                    + (rng.next() as usize) % (size_range.1 - size_range.0)
                            };
                            let p = unsafe { alloc.alloc(layout_for(size)) };
                            if p.is_null() {
                                return ops;
                            }
                            unsafe { *p = ops as u8 };
                            live.push((p, size));
                            if rng.next() % 100 < wl.free_pct && live.len() > 64 {
                                let idx = (rng.next() as usize) % live.len();
                                let (p, s) = live.swap_remove(idx);
                                unsafe { alloc.dealloc(p, layout_for(s)) };
                            }
                            ops += 1;
                        }
                        // keep memory bounded under low free_pct
                        if live.len() > 200_000 {
                            for (p, s) in live.drain(..) {
                                unsafe { alloc.dealloc(p, layout_for(s)) };
                            }
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
    total as f64 / seconds as f64
}

fn main() {
    const SECS: u64 = 3;

    struct Named(&'static str, &'static dyn SyncGlobalAlloc);
    trait SyncGlobalAlloc: GlobalAlloc + Sync {}
    impl<T: GlobalAlloc + Sync> SyncGlobalAlloc for T {}

    let allocators = [
        Named("allox", &GLOBAL),
        Named("talc ", &TALC),
        Named("system", &System),
    ];

    println!(
        "{:<18} {:>12} {:>12} {:>12} {:>10} {:>10}",
        "workload", "allox", "talc", "system", "a/talc", "a/sys"
    );
    println!("{}", "-".repeat(80));

    let mut wins_vs_talc = 0;
    let mut wins_vs_sys = 0;
    for wl in WORKLOADS {
        let mut scores = Vec::new();
        for a in &allocators {
            scores.push(run(a.1, wl, SECS));
        }
        let [allox_s, talc_s, sys_s] = [scores[0], scores[1], scores[2]];
        if allox_s > talc_s {
            wins_vs_talc += 1;
        }
        if allox_s > sys_s {
            wins_vs_sys += 1;
        }
        println!(
            "{:<18} {:>12.0} {:>12.0} {:>12.0} {:>9.2}x {:>9.2}x",
            wl.name,
            allox_s,
            talc_s,
            sys_s,
            allox_s / talc_s,
            allox_s / sys_s,
        );
    }

    println!("{}", "-".repeat(80));
    println!(
        "allox wins vs talc: {wins_vs_talc}/{}   vs system: {wins_vs_sys}/{}",
        WORKLOADS.len(),
        WORKLOADS.len()
    );
}
