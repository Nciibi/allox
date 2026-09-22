//! Comparative allocator benchmark: allox vs system vs talc vs C allocators.
//!
//! All allocators run identical workloads; results are ops/s plus a
//! relative table. The harness itself allocates through the process global
//! (= allox); that overhead is identical for all measured allocators.
//!
//! P0 honest scoreboard: mimalloc + snmalloc are dev-only comparators — the
//! library itself stays zero-deps / no-C. jemalloc needs make+autoconf and
//! is CI-only (see ROADMAP_TO_BEST.md); enable it where those tools exist.
//!
//! Run with: cargo bench
//! Fast smoke: BENCH_SECS=1 BENCH_REPS=1 BENCH_ONLY="tight-small 1T" cargo bench

use std::alloc::{GlobalAlloc, Layout, System};
use std::time::{Duration, Instant};

#[global_allocator]
static GLOBAL: allox::Allox = allox::Allox;

use spinning_top::RawSpinlock;
use talc::{source::GlobalAllocSource, TalcLock};

// talc configured for a fair hosted fight: GlobalAllocSource lets it claim
// AND release memory dynamically through the system allocator, exactly like
// allox's OS-backed pages. (Its documented Claim-based setup cannot grow,
// which made large workloads degenerate into OOM.)
static TALC: TalcLock<RawSpinlock, GlobalAllocSource<std::alloc::System>> =
    TalcLock::new(GlobalAllocSource::new(std::alloc::System));

// Third comparator: dlmalloc — the pure-Rust port that is wasm32's default.
struct Dlmalloc;
unsafe impl std::alloc::GlobalAlloc for Dlmalloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        dlmalloc::GlobalDlmalloc.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        dlmalloc::GlobalDlmalloc.dealloc(p, l)
    }
}

static DLMALLOC: Dlmalloc = Dlmalloc;

// C comparators (dev-only; lib stays zero-C). Wrappers delegate alloc/dealloc
// so every workload below runs identically through each allocator.
struct Mimalloc;
unsafe impl GlobalAlloc for Mimalloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        mimalloc::MiMalloc.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        mimalloc::MiMalloc.dealloc(p, l)
    }
}
static MIMALLOC: Mimalloc = Mimalloc;

struct Snmalloc;
unsafe impl GlobalAlloc for Snmalloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        snmalloc_rs::SnMalloc.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        snmalloc_rs::SnMalloc.dealloc(p, l)
    }
}
static SNMALLOC: Snmalloc = Snmalloc;

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

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Standard churn: each thread allocs/frees its own blocks.
    Standard,
    /// Producer-consumer: thread i allocs, thread (i+1)%N frees half the
    /// blocks via a ring handoff. Exposes remote-free / cache-drift costs.
    ProdCons,
    /// Spawn churn: repeatedly spawn a short-lived thread that does a burst
    /// of allocs/frees then exits. Exposes dead-thread reclamation costs.
    SpawnChurn,
}

struct Workload {
    name: &'static str,
    threads: usize,
    /// (min, max) allocation size; equal bounds = fixed size
    size_range: (usize, usize),
    /// fraction of ops that are frees (0..100)
    free_pct: u64,
    kind: Kind,
}

const WORKLOADS: &[Workload] = &[
    Workload {
        name: "tight-small 1T",
        threads: 1,
        size_range: (64, 64),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "mixed-small 1T",
        threads: 1,
        size_range: (16, 4096),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "tight-small 8T",
        threads: 8,
        size_range: (64, 64),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "mixed-small 8T",
        threads: 8,
        size_range: (16, 4096),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "mixed-all 8T",
        threads: 8,
        size_range: (16, 65536),
        free_pct: 50,
        kind: Kind::Standard,
    },
    // --- P0 additions: isolate the known gaps ---
    Workload {
        name: "mixed-all 1T",
        threads: 1,
        size_range: (16, 65536),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "large-only 1T",
        threads: 1,
        size_range: (32768, 1048576),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "large-only 8T",
        threads: 8,
        size_range: (32768, 262144),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "prodcons 8T",
        threads: 8,
        size_range: (16, 4096),
        free_pct: 50,
        kind: Kind::ProdCons,
    },
    Workload {
        name: "spawn-churn",
        threads: 4,
        size_range: (16, 4096),
        free_pct: 50,
        kind: Kind::SpawnChurn,
    },
];

fn run<A: GlobalAlloc + Sync + ?Sized>(alloc: &'static A, wl: &Workload, seconds: u64) -> f64 {
    match wl.kind {
        Kind::Standard => run_standard(alloc, wl, seconds),
        Kind::ProdCons => run_prodcons(alloc, wl, seconds),
        Kind::SpawnChurn => run_spawn_churn(alloc, wl, seconds),
    }
}

fn run_standard<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> f64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
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
                    let mut live_bytes = 0usize;
                    let mut ops = 0u64;
                    while Instant::now() < stop {
                        for _ in 0..10_000 {
                            let size = if size_range.0 == size_range.1 {
                                size_range.0
                            } else {
                                size_range.0 + (rng.next() as usize) % (size_range.1 - size_range.0)
                            };
                            let p = unsafe { alloc.alloc(layout_for(size)) };
                            if p.is_null() {
                                return ops;
                            }
                            unsafe { *p = ops as u8 };
                            live.push((p, size));
                            live_bytes += size;
                            // Steady state: always free when over half full,
                            // so the tracking Vec stays cache-resident and
                            // never becomes the benchmark.
                            if live.len() > 4096
                                || (rng.next() % 100 < free_pct && !live.is_empty())
                            {
                                let idx = (rng.next() as usize) % live.len();
                                let (p, s) = live.swap_remove(idx);
                                live_bytes -= s;
                                unsafe { alloc.dealloc(p, layout_for(s)) };
                            }
                            ops += 1;
                        }
                        // keep resident memory bounded regardless of size mix
                        if live_bytes > 96 * 1024 * 1024 {
                            for (p, s) in live.drain(..) {
                                live_bytes -= s;
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

/// Producer-consumer: each thread allocates, then hands every other block to
/// its ring neighbour for freeing. Same total ops as standard, but frees are
/// remote — the case snmalloc/mimalloc optimize and bump-pointer thread
/// caches punish.
fn run_prodcons<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> f64 {
    use std::sync::mpsc::{channel, Receiver, Sender};
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let n = wl.threads;
    let (min, max) = wl.size_range;

    // Ring channels: thread i sends to (i+1)%n. Pointers cross threads as
    // usize (raw *mut u8 is !Send); cast back on receipt.
    let mut senders: Vec<Sender<(usize, usize)>> = Vec::new();
    let mut receivers: Vec<Option<Receiver<(usize, usize)>>> = Vec::new();
    for _ in 0..n {
        let (tx, rx) = channel::<(usize, usize)>();
        senders.push(tx);
        receivers.push(Some(rx));
    }
    // Each thread gets its own incoming rx plus a clone of the next tx.
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles = Vec::new();
    for t in 0..n {
        let rx = receivers[t].take().unwrap();
        let tx_next = senders[(t + 1) % n].clone();
        let stop_c = stop_flag.clone();
        let stop_time = stop;
        handles.push(
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut rng = Rng(0x51ED ^ ((t as u64 + 1).wrapping_mul(0x9E3779B9)));
                    let mut ops = 0u64;
                    let mut local: Vec<(*mut u8, usize)> = Vec::with_capacity(256);
                    let mut i = 0u64;
                    while Instant::now() < stop_time
                        && !stop_c.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        for _ in 0..1000 {
                            let size = min + (rng.next() as usize) % (max - min);
                            let p = unsafe { alloc.alloc(layout_for(size)) };
                            if p.is_null() {
                                return ops;
                            }
                            unsafe { *p = ops as u8 };
                            ops += 1;
                            i += 1;
                            if i % 2 == 0 {
                                // hand off to neighbour for remote free
                                let _ = tx_next.send((p as usize, size));
                            } else {
                                local.push((p, size));
                            }
                            // drain incoming remote frees + some local frees
                            while let Ok((rp, rs)) = rx.try_recv() {
                                unsafe { alloc.dealloc(rp as *mut u8, layout_for(rs)) };
                            }
                            if local.len() > 512 {
                                let (lp, ls) = local.swap_remove(0);
                                unsafe { alloc.dealloc(lp, layout_for(ls)) };
                            }
                        }
                    }
                    // drain remainder
                    while let Ok((rp, rs)) = rx.try_recv() {
                        unsafe { alloc.dealloc(rp as *mut u8, layout_for(rs)) };
                    }
                    for (lp, ls) in local.drain(..) {
                        unsafe { alloc.dealloc(lp, layout_for(ls)) };
                    }
                    ops
                })
                .unwrap(),
        );
    }
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    total as f64 / seconds as f64
}

/// Spawn churn: loop spawning short-lived threads that burst-allocate then
/// exit without explicit flush. Measures dead-thread reclamation (allox's
/// documented weak spot: cached blocks of dead threads stay `used` until
/// pages die naturally).
fn run_spawn_churn<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> f64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let (min, max) = wl.size_range;
    let mut ops = 0u64;
    let mut seed = 0xC0FFEEu64;
    while Instant::now() < stop {
        let mut batch = Vec::new();
        for _ in 0..wl.threads {
            seed = seed.wrapping_mul(0x2545F4914F6CDD1D).wrapping_add(1);
            let s = seed;
            batch.push(
                std::thread::Builder::new()
                    .stack_size(1 << 20)
                    .spawn(move || {
                        let mut rng = Rng(s);
                        let mut local_ops = 0u64;
                        let mut live: Vec<(*mut u8, usize)> = Vec::with_capacity(256);
                        for _ in 0..2000 {
                            let size = min + (rng.next() as usize) % (max - min);
                            let p = unsafe { alloc.alloc(layout_for(size)) };
                            if p.is_null() {
                                break;
                            }
                            unsafe { *p = 0xAB };
                            live.push((p, size));
                            local_ops += 1;
                            if rng.next() % 2 == 0 && !live.is_empty() {
                                let idx = (rng.next() as usize) % live.len();
                                let (fp, fs) = live.swap_remove(idx);
                                unsafe { alloc.dealloc(fp, layout_for(fs)) };
                            }
                        }
                        // Free half, leak the rest into the dead thread's
                        // cache (no explicit flush — that is the test).
                        for (fp, fs) in live {
                            unsafe { alloc.dealloc(fp, layout_for(fs)) };
                        }
                        local_ops
                    })
                    .unwrap(),
            );
        }
        for h in batch {
            ops += h.join().unwrap();
        }
    }
    // Elapsed-time normalisation: caller divides by seconds.
    ops as f64 / seconds as f64
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Peak RSS in KiB (Linux only; 0 elsewhere). Read from /proc/self/status.
#[cfg(target_os = "linux")]
fn peak_rss_kib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("VmHWM:") {
            return v
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}
#[cfg(not(target_os = "linux"))]
fn peak_rss_kib() -> u64 {
    0
}

fn main() {
    let secs: u64 = std::env::var("BENCH_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let reps: usize = std::env::var("BENCH_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    // P2 tuning hook: override the per-thread cache budget (MiB, 0 = default
    // 32 MiB). Medium blocks blow through small-tuned budgets instantly, so
    // this isolates budget-induced trim churn from structural costs.
    if let Ok(mb) = std::env::var("BENCH_BUDGET_MB") {
        if let Ok(mb) = mb.parse::<usize>() {
            if mb > 0 {
                allox::set_thread_cache_budget(mb * 1024 * 1024);
                eprintln!("  budget overridden to {} MiB/thread", mb);
            }
        }
    }

    struct Named(&'static str, &'static dyn SyncGlobalAlloc);
    trait SyncGlobalAlloc: GlobalAlloc + Sync {}
    impl<T: GlobalAlloc + Sync> SyncGlobalAlloc for T {}

    let allocators = [
        Named("allox", &GLOBAL),
        Named("talc ", &TALC),
        Named("dlmalloc", &DLMALLOC),
        Named("system", &System),
        Named("mimalloc", &MIMALLOC),
        Named("snmalloc", &SNMALLOC),
    ];

    println!(
        "{:<15} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11} {:>9} {:>10} {:>10} {:>10}",
        "workload",
        "allox",
        "talc",
        "dlmalloc",
        "system",
        "mimalloc",
        "snmalloc",
        "a/talc",
        "mapcalls",
        "unmaps",
        "peakRSS",
    );
    println!("{}", "-".repeat(137));

    let filter = std::env::var("BENCH_ONLY").unwrap_or_default();
    for wl in WORKLOADS {
        if !filter.is_empty() && !wl.name.contains(&filter) {
            continue;
        }
        // Median of REPS runs per allocator: interleaved so thermal drift
        // affects all allocators equally.
        let mut scores = vec![vec![]; allocators.len()];
        for _ in 0..reps {
            for (i, a) in allocators.iter().enumerate() {
                eprintln!("  running {} / {}...", wl.name, a.0);
                scores[i].push(run(a.1, wl, secs));
            }
        }
        let medians: Vec<f64> = scores
            .iter_mut()
            .map(|s| median(s.as_mut_slice()))
            .collect();
        // Syscall + RSS diagnostics: snapshot allox counters around one extra
        // allox-only probe run so numbers reflect steady-state behaviour.
        let s0 = allox::stats();
        let _ = run(&GLOBAL, wl, secs.min(1).max(1));
        let s1 = allox::stats();
        let map_delta = s1.map_calls.saturating_sub(s0.map_calls);
        let rss = peak_rss_kib();
        let allox_s = medians[0];
        let talc_s = medians[1];
        println!(
            "{:<15} {:>11.0} {:>11.0} {:>11.0} {:>11.0} {:>11.0} {:>11.0} {:>8.2}x {:>10} {:>10}",
            wl.name,
            medians[0],
            medians[1],
            medians[2],
            medians[3],
            medians[4],
            medians[5],
            allox_s / talc_s.max(1.0),
            map_delta,
            rss,
        );
    }

    println!("{}", "-".repeat(125));
    println!("note: harness Vecs allocate through allox (process global); identical for all.");
    println!("mapcalls = allox MAP_CALLS delta during 1s probe; peakRSS = VmHWM KiB (linux).");
}
