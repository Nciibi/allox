//! Comparative allocator benchmark: allox vs system vs talc vs C allocators.
//!
//! All allocators run identical workloads; results are ops/s plus a
//! relative table. The process-global harness allocator is the system
//! allocator, while each measured allocator is called directly.
//!
//! P0 honest scoreboard: mimalloc + snmalloc are dev-only comparators — the
//! library itself stays zero-deps / no-C. jemalloc is behind the optional
//! `bench-jemalloc` feature (needs make+autoconf): CI-only, enable it where
//! those tools exist (`cargo bench --features bench-jemalloc`).
//!
//! Run with: cargo bench
//! Fast smoke: BENCH_SECS=1 BENCH_REPS=1 BENCH_ONLY="tight-small 1T" cargo bench
//! Machine-readable output: BENCH_OUTPUT=json cargo bench
//! Fresh-process JSONL: BENCH_FRESH=1 BENCH_OUTPUT=json cargo bench
//! Per-op latency percentiles: BENCH_P99=1024 (sampled 1-in-1024 calls)

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[global_allocator]
static HARNESS: System = System;

static ALLOX: allox::Allox = allox::Allox;

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
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        dlmalloc::GlobalDlmalloc.realloc(p, l, new_size)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        dlmalloc::GlobalDlmalloc.alloc_zeroed(l)
    }
}

static DLMALLOC: Dlmalloc = Dlmalloc;

// C comparators (dev-only; lib stays zero-C). Wrappers delegate the native
// GlobalAlloc operations so every workload below runs equivalently.
struct Mimalloc;
unsafe impl GlobalAlloc for Mimalloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        mimalloc::MiMalloc.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        mimalloc::MiMalloc.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        mimalloc::MiMalloc.realloc(p, l, new_size)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        mimalloc::MiMalloc.alloc_zeroed(l)
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
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        snmalloc_rs::SnMalloc.realloc(p, l, new_size)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        snmalloc_rs::SnMalloc.alloc_zeroed(l)
    }
}
static SNMALLOC: Snmalloc = Snmalloc;

// jemalloc (optional, `--features bench-jemalloc`): CI box has make+autoconf.
#[cfg(feature = "bench-jemalloc")]
struct Jemalloc;
#[cfg(feature = "bench-jemalloc")]
unsafe impl GlobalAlloc for Jemalloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        tikv_jemallocator::Jemalloc.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        tikv_jemallocator::Jemalloc.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        tikv_jemallocator::Jemalloc.realloc(p, l, new_size)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        tikv_jemallocator::Jemalloc.alloc_zeroed(l)
    }
}
#[cfg(feature = "bench-jemalloc")]
static JEMALLOC: Jemalloc = Jemalloc;

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
    ZeroedLarge,
    /// Producer-consumer: thread i allocs, thread (i+1)%N frees half the
    /// blocks via a ring handoff. Exposes remote-free / cache-drift costs.
    ProdCons,
    /// Spawn churn: repeatedly spawn a short-lived thread that does a burst
    /// of allocs/frees then exits. Exposes dead-thread reclamation costs.
    SpawnChurn,
    /// Spawn empty: threads spawn and immediately exit. Pure pthread
    /// spawn/join cost with zero allocator interaction (threads never arm
    /// the exit hook) — identical for every comparator, calibrates how
    /// much of spawn-churn is spawn vs allocator work.
    SpawnEmpty,
    /// JSON-ish: per-document burst of many tiny short-lived buffers
    /// (strings/numbers) plus a few growing Vec-like buffers (realloc
    /// doubling), everything freed at document end. Models serde-style
    /// parse churn: alloc-heavy, arena-lifetime frees, realloc growth.
    Json,
    /// Request-handler: per-request batch of tiny allocs (headers/strings)
    /// plus a couple of body buffers, ALL freed together at request end
    /// (pool lifetime). Models server request handling: bulk-free
    /// efficiency, short lifetimes, multithreaded.
    Request,
    /// ECS-archetype: a few large component buffers repeatedly
    /// realloc-grown (doubling) plus steady small component churn. Models
    /// game-engine storage: realloc growth path + mixed lifetimes.
    Ecs,
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
        name: "medium-only 1T",
        threads: 1,
        size_range: (16385, 65473),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "medium-only 8T",
        threads: 8,
        size_range: (16385, 65473),
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
        name: "big-tail 1T",
        threads: 1,
        size_range: (262145, 524289),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "big-upper-tail 1T",
        threads: 1,
        size_range: (524289, 1048577),
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
        name: "huge-only 1T",
        threads: 1,
        size_range: (5 * 1024 * 1024, 8 * 1024 * 1024),
        free_pct: 50,
        kind: Kind::Standard,
    },
    Workload {
        name: "zeroed-large 1T",
        threads: 1,
        size_range: (5 * 1024 * 1024, 8 * 1024 * 1024),
        free_pct: 50,
        kind: Kind::ZeroedLarge,
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
    Workload {
        name: "spawn-empty",
        threads: 4,
        size_range: (16, 4096),
        free_pct: 50,
        kind: Kind::SpawnEmpty,
    },
    Workload {
        name: "json-ish 8T",
        threads: 8,
        size_range: (16, 8192),
        free_pct: 50,
        kind: Kind::Json,
    },
    Workload {
        name: "request 8T",
        threads: 8,
        size_range: (8, 8192),
        free_pct: 50,
        kind: Kind::Request,
    },
    Workload {
        name: "ecs 8T",
        threads: 8,
        size_range: (16, 1048576),
        free_pct: 50,
        kind: Kind::Ecs,
    },
];

#[derive(Clone, Copy, Debug)]
struct RunSample {
    ops: u64,
    elapsed: Duration,
    rss_kib: u64,
    peak_rss_kib: u64,
}

impl RunSample {
    fn from_start(ops: u64, start: Instant) -> Self {
        let elapsed = start.elapsed();
        Self {
            ops,
            elapsed,
            rss_kib: current_rss_kib(),
            peak_rss_kib: peak_rss_kib(),
        }
    }

    fn ops_per_sec(&self) -> f64 {
        self.ops as f64 / self.elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
    }

    fn elapsed_ns(&self) -> u64 {
        self.elapsed.as_nanos().min(u64::MAX as u128) as u64
    }

    fn ns_per_op(&self) -> f64 {
        if self.ops == 0 {
            0.0
        } else {
            self.elapsed_ns() as f64 / self.ops as f64
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TimingSummary {
    mean_ns_per_op: f64,
    min_run_ns_per_op: f64,
    max_run_ns_per_op: f64,
    p50_run_ns_per_op: f64,
    p95_run_ns_per_op: f64,
    p99_run_ns_per_op: f64,
    p99_9_run_ns_per_op: f64,
}

#[derive(Clone, Copy, Debug)]
struct SampleSummary {
    median_ops_per_sec: f64,
    mean_ops_per_sec: f64,
    min_ops_per_sec: f64,
    max_ops_per_sec: f64,
    total_ops: u64,
    total_elapsed_ns: u64,
    timing: TimingSummary,
}

struct AlloxDiagnostics {
    map_delta: u64,
    unmap_delta: u64,
    mapped_delta: i64,
    span_maps: u64,
    span_unmaps: u64,
    small_maps: u64,
    small_unmaps: u64,
    arena_reuses: u64,
    arena_commits: u64,
    big_maps: u64,
    big_unmaps: u64,
    abandoned_delta: u64,
    abandoned_total: u64,
    arena_high_water: u64,
    /// Delta of every always-on volume counter, parallel to
    /// `allox::__diagnostics::VOLUME_FIELDS` (refills, flushes, copied
    /// bytes, purges, arena fallbacks, ...).
    volume: Vec<(String, u64)>,
    /// Cumulative lock-wait / purge / exit-flush nanoseconds. Only present
    /// in `telemetry` builds, where those counters are compiled.
    timing_ns: Option<[u64; 3]>,
    probe: RunSample,
}

fn run<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> RunSample {
    let start = Instant::now();
    let ops = match wl.kind {
        Kind::Standard => run_standard(alloc, wl, seconds),
        Kind::ZeroedLarge => run_zeroed_large(alloc, wl, seconds),
        Kind::ProdCons => run_prodcons(alloc, wl, seconds),
        Kind::SpawnChurn => run_spawn_churn(alloc, wl, seconds),
        Kind::SpawnEmpty => run_spawn_empty(wl, seconds),
        Kind::Json => run_json(alloc, wl, seconds),
        Kind::Request => run_request(alloc, wl, seconds),
        Kind::Ecs => run_ecs(alloc, wl, seconds),
    };
    RunSample::from_start(ops, start)
}

fn run_standard<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> u64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let threads = wl.threads;
    let size_range = wl.size_range;
    let free_pct = wl.free_pct;
    let live_batch = if std::env::var_os("BENCH_SAFE_LIVE").is_some() {
        16
    } else {
        10_000
    };

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
                        for _ in 0..live_batch {
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
    total
}

fn run_zeroed_large<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> u64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let threads = wl.threads;
    let size_range = wl.size_range;
    let free_pct = wl.free_pct;
    let live_batch = if std::env::var_os("BENCH_SAFE_LIVE").is_some() {
        16
    } else {
        10_000
    };

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
                        for _ in 0..live_batch {
                            let size = if size_range.0 == size_range.1 {
                                size_range.0
                            } else {
                                size_range.0 + (rng.next() as usize) % (size_range.1 - size_range.0)
                            };
                            let layout = layout_for(size);
                            let p = unsafe { alloc.alloc_zeroed(layout) };
                            if p.is_null() {
                                return ops;
                            }
                            unsafe { *p = ops as u8 };
                            live.push((p, size));
                            live_bytes += size;
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

    handles.into_iter().map(|h| h.join().unwrap()).sum()
}

/// Producer-consumer: each thread allocates, then hands every other block to
/// its ring neighbour for freeing. Same total ops as standard, but frees are
/// remote — the case snmalloc/mimalloc optimize and bump-pointer thread
/// caches punish.
fn run_prodcons<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> u64 {
    use std::sync::mpsc::{channel, Receiver, Sender};
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let n = wl.threads;
    let (min, max) = wl.size_range;

    // Ring channels: thread i sends to (i+1)%n. Pointers cross threads as
    // usize (raw *mut u8 is !Send); cast back on receipt.
    let mut senders: Vec<Sender<Option<(usize, usize)>>> = Vec::new();
    let mut receivers: Vec<Option<Receiver<Option<(usize, usize)>>>> = Vec::new();
    for _ in 0..n {
        let (tx, rx) = channel::<Option<(usize, usize)>>();
        senders.push(tx);
        receivers.push(Some(rx));
    }
    // Each thread gets its own incoming rx plus a clone of the next tx.
    let mut handles = Vec::new();
    for t in 0..n {
        let rx = receivers[t].take().unwrap();
        let tx_next = senders[(t + 1) % n].clone();
        let stop_time = stop;
        handles.push(
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut rng = Rng(0x51ED ^ ((t as u64 + 1).wrapping_mul(0x9E3779B9)));
                    let mut ops = 0u64;
                    let mut local: Vec<(*mut u8, usize)> = Vec::with_capacity(256);
                    let mut i = 0u64;
                    let mut incoming_done = false;
                    'produce: while Instant::now() < stop_time {
                        for _ in 0..1000 {
                            let size = min + (rng.next() as usize) % (max - min);
                            let p = unsafe { alloc.alloc(layout_for(size)) };
                            if p.is_null() {
                                break 'produce;
                            }
                            unsafe { *p = ops as u8 };
                            ops += 1;
                            i += 1;
                            if i % 2 == 0 {
                                if tx_next.send(Some((p as usize, size))).is_err() {
                                    break 'produce;
                                }
                            } else {
                                local.push((p, size));
                            }
                            loop {
                                match rx.try_recv() {
                                    Ok(Some((rp, rs))) => unsafe {
                                        alloc.dealloc(rp as *mut u8, layout_for(rs))
                                    },
                                    Ok(None) => {
                                        incoming_done = true;
                                        break;
                                    }
                                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                        incoming_done = true;
                                        break;
                                    }
                                }
                            }
                            if local.len() > 512 {
                                let (lp, ls) = local.swap_remove(0);
                                unsafe { alloc.dealloc(lp, layout_for(ls)) };
                            }
                        }
                    }
                    for (lp, ls) in local.drain(..) {
                        unsafe { alloc.dealloc(lp, layout_for(ls)) };
                    }
                    let _ = tx_next.send(None);
                    while !incoming_done {
                        match rx.recv() {
                            Ok(Some((rp, rs))) => unsafe {
                                alloc.dealloc(rp as *mut u8, layout_for(rs))
                            },
                            Ok(None) | Err(_) => break,
                        }
                    }
                    ops
                })
                .unwrap(),
        );
    }
    drop(senders);
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    total
}

/// Spawn churn: loop spawning short-lived threads that burst-allocate then
/// exit without explicit flush. Measures dead-thread reclamation (allox's
/// documented weak spot: cached blocks of dead threads stay `used` until
/// pages die naturally).
fn run_spawn_churn<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> u64 {
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
    ops
}

/// Spawn-exit with no allocator interaction: isolates pthread spawn/join
/// latency so spawn-churn numbers can be decomposed. Reported in threads/s
/// (not alloc ops/s) — expect identical scores for every comparator.
fn run_spawn_empty(wl: &Workload, seconds: u64) -> u64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let mut threads = 0u64;
    while Instant::now() < stop {
        let mut batch = Vec::new();
        for _ in 0..wl.threads {
            batch.push(
                std::thread::Builder::new()
                    .stack_size(1 << 20)
                    .spawn(|| {})
                    .unwrap(),
            );
        }
        for h in batch {
            h.join().unwrap();
        }
        threads += wl.threads as u64;
    }
    threads
}

/// JSON-ish: per document, allocate ~200 tiny buffers (strings/numbers,
/// 16–256 B, freed at 90% rate interleaved) plus 3 Vec-like buffers grown
/// by realloc doubling (64 B → 8 KiB). Frees everything still live at
/// document end. Models serde-style parse churn through GlobalAlloc
/// (realloc exercises the same-class identity + grow paths).
fn run_json<A: GlobalAlloc + Sync + ?Sized>(alloc: &'static A, wl: &Workload, seconds: u64) -> u64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let handles: Vec<_> = (0..wl.threads)
        .map(|t| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut rng = Rng(0x150A ^ ((t as u64 + 1).wrapping_mul(0xD1B54A32D192ED03)));
                    let mut ops = 0u64;
                    while Instant::now() < stop {
                        // One document: tiny values + growing buffers.
                        let mut live: Vec<(*mut u8, Layout)> = Vec::with_capacity(256);
                        for _ in 0..200 {
                            let size = 16 + (rng.next() as usize) % 240;
                            let l = layout_for(size);
                            let p = unsafe { alloc.alloc(l) };
                            if p.is_null() {
                                return ops;
                            }
                            unsafe { *p = ops as u8 };
                            live.push((p, l));
                            ops += 1;
                            if rng.next() % 10 < 9 && !live.is_empty() {
                                let idx = (rng.next() as usize) % live.len();
                                let (fp, fl) = live.swap_remove(idx);
                                unsafe { alloc.dealloc(fp, fl) };
                            }
                        }
                        for _ in 0..3 {
                            let mut bl = layout_for(64);
                            let mut bp = unsafe { alloc.alloc(bl) };
                            if bp.is_null() {
                                return ops;
                            }
                            let mut bsize = 64usize;
                            while bsize < 8192 {
                                let nsize = bsize * 2;
                                let np = unsafe { alloc.realloc(bp, bl, nsize) };
                                if np.is_null() {
                                    break;
                                }
                                bp = np;
                                bl = Layout::from_size_align(nsize, 16).expect("layout");
                                bsize = nsize;
                                ops += 1;
                            }
                            unsafe { alloc.dealloc(bp, bl) };
                        }
                        for (fp, fl) in live.drain(..) {
                            unsafe { alloc.dealloc(fp, fl) };
                        }
                    }
                    ops
                })
                .unwrap()
        })
        .collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    total
}

/// Request-handler: per request, ~100 tiny allocs (8–128 B headers/strings)
/// plus 2 body buffers (2–8 KiB), ALL freed together at request end (pool
/// lifetime — no interleaved frees). Models server request handling.
fn run_request<A: GlobalAlloc + Sync + ?Sized>(
    alloc: &'static A,
    wl: &Workload,
    seconds: u64,
) -> u64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let handles: Vec<_> = (0..wl.threads)
        .map(|t| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut rng = Rng(0xBEACE ^ ((t as u64 + 1).wrapping_mul(0xD1B54A32D192ED03)));
                    let mut ops = 0u64;
                    while Instant::now() < stop {
                        let mut live: Vec<(*mut u8, Layout)> = Vec::with_capacity(128);
                        for _ in 0..100 {
                            let size = 8 + (rng.next() as usize) % 120;
                            let l = layout_for(size);
                            let p = unsafe { alloc.alloc(l) };
                            if p.is_null() {
                                return ops;
                            }
                            unsafe { *p = ops as u8 };
                            live.push((p, l));
                            ops += 1;
                        }
                        for _ in 0..2 {
                            let size = 2048 + (rng.next() as usize) % 6144;
                            let l = layout_for(size);
                            let p = unsafe { alloc.alloc(l) };
                            if p.is_null() {
                                return ops;
                            }
                            unsafe { *p = ops as u8 };
                            live.push((p, l));
                            ops += 1;
                        }
                        // Pool free: everything at request end.
                        for (fp, fl) in live.drain(..) {
                            unsafe { alloc.dealloc(fp, fl) };
                        }
                    }
                    ops
                })
                .unwrap()
        })
        .collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    total
}

/// ECS-archetype: 4 large component buffers (64 KiB–1 MiB) repeatedly
/// realloc-grown (doubling, then freed), plus steady small component churn
/// (16–256 B, 50% frees). Models game-engine storage: realloc growth path
/// with big regions plus background small churn.
fn run_ecs<A: GlobalAlloc + Sync + ?Sized>(alloc: &'static A, wl: &Workload, seconds: u64) -> u64 {
    let stop = Instant::now() + Duration::from_secs(seconds);
    let layout_for = |n: usize| Layout::from_size_align(n.max(1), 16).expect("layout");
    let handles: Vec<_> = (0..wl.threads)
        .map(|t| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let mut rng = Rng(0xEC5 ^ ((t as u64 + 1).wrapping_mul(0xD1B54A32D192ED03)));
                    let mut ops = 0u64;
                    let mut small_live: Vec<(*mut u8, Layout)> = Vec::with_capacity(512);
                    while Instant::now() < stop {
                        // Grow 4 archetype buffers then drop them.
                        for _ in 0..4 {
                            let mut bsize = 65536usize;
                            let mut bl = layout_for(bsize);
                            let mut bp = unsafe { alloc.alloc(bl) };
                            if bp.is_null() {
                                return ops;
                            }
                            while bsize < 1048576 {
                                let nsize = (bsize * 2).min(1048576);
                                let np = unsafe { alloc.realloc(bp, bl, nsize) };
                                if np.is_null() {
                                    break;
                                }
                                bp = np;
                                bl = Layout::from_size_align(nsize, 16).expect("layout");
                                bsize = nsize;
                                ops += 1;
                            }
                            unsafe { alloc.dealloc(bp, bl) };
                        }
                        // Background small-component churn.
                        for _ in 0..200 {
                            let size = 16 + (rng.next() as usize) % 240;
                            let l = layout_for(size);
                            let p = unsafe { alloc.alloc(l) };
                            if p.is_null() {
                                return ops;
                            }
                            unsafe { *p = ops as u8 };
                            small_live.push((p, l));
                            ops += 1;
                            if rng.next() % 2 == 0 && !small_live.is_empty() {
                                let idx = (rng.next() as usize) % small_live.len();
                                let (fp, fl) = small_live.swap_remove(idx);
                                unsafe { alloc.dealloc(fp, fl) };
                            }
                        }
                        if small_live.len() > 4096 {
                            for (fp, fl) in small_live.drain(..) {
                                unsafe { alloc.dealloc(fp, fl) };
                            }
                        }
                    }
                    for (fp, fl) in small_live {
                        unsafe { alloc.dealloc(fp, fl) };
                    }
                    ops
                })
                .unwrap()
        })
        .collect();

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    total
}

fn median(v: &mut [f64]) -> f64 {
    percentile(v, 0.5)
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let index = (((v.len() - 1) as f64) * p).round() as usize;
    v[index.min(v.len() - 1)]
}

fn summarize(samples: &[RunSample]) -> SampleSummary {
    if samples.is_empty() {
        return SampleSummary {
            median_ops_per_sec: 0.0,
            mean_ops_per_sec: 0.0,
            min_ops_per_sec: 0.0,
            max_ops_per_sec: 0.0,
            total_ops: 0,
            total_elapsed_ns: 0,
            timing: TimingSummary {
                mean_ns_per_op: 0.0,
                min_run_ns_per_op: 0.0,
                max_run_ns_per_op: 0.0,
                p50_run_ns_per_op: 0.0,
                p95_run_ns_per_op: 0.0,
                p99_run_ns_per_op: 0.0,
                p99_9_run_ns_per_op: 0.0,
            },
        };
    }

    let mut rates = Vec::with_capacity(samples.len());
    let mut run_ns = Vec::with_capacity(samples.len());
    let mut total_ops = 0u64;
    let mut total_elapsed_ns = 0u64;
    let mut min_rate = f64::INFINITY;
    let mut max_rate: f64 = 0.0;
    let mut min_run_ns = f64::INFINITY;
    let mut max_run_ns: f64 = 0.0;
    for sample in samples {
        let rate = sample.ops_per_sec();
        let ns = sample.ns_per_op();
        rates.push(rate);
        run_ns.push(ns);
        total_ops = total_ops.saturating_add(sample.ops);
        total_elapsed_ns = total_elapsed_ns.saturating_add(sample.elapsed_ns());
        min_rate = min_rate.min(rate);
        max_rate = max_rate.max(rate);
        min_run_ns = min_run_ns.min(ns);
        max_run_ns = max_run_ns.max(ns);
    }
    let mean_rate = rates.iter().sum::<f64>() / rates.len() as f64;
    let median_rate = median(&mut rates);
    let mut p50_values = run_ns.clone();
    let mut p95_values = run_ns.clone();
    let mut p99_values = run_ns.clone();
    let mut p99_9_values = run_ns;
    let timing = TimingSummary {
        mean_ns_per_op: if total_ops == 0 {
            0.0
        } else {
            total_elapsed_ns as f64 / total_ops as f64
        },
        min_run_ns_per_op: min_run_ns,
        max_run_ns_per_op: max_run_ns,
        p50_run_ns_per_op: percentile(&mut p50_values, 0.5),
        p95_run_ns_per_op: percentile(&mut p95_values, 0.95),
        p99_run_ns_per_op: percentile(&mut p99_values, 0.99),
        p99_9_run_ns_per_op: percentile(&mut p99_9_values, 0.999),
    };
    SampleSummary {
        median_ops_per_sec: median_rate,
        mean_ops_per_sec: mean_rate,
        min_ops_per_sec: min_rate,
        max_ops_per_sec: max_rate,
        total_ops,
        total_elapsed_ns,
        timing,
    }
}

#[cfg(target_os = "linux")]
fn status_kib(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn current_rss_kib() -> u64 {
    status_kib("VmRSS:")
}

#[cfg(not(target_os = "linux"))]
fn current_rss_kib() -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn peak_rss_kib() -> u64 {
    status_kib("VmHWM:")
}

#[cfg(not(target_os = "linux"))]
fn peak_rss_kib() -> u64 {
    0
}

struct Named(&'static str, &'static dyn SyncGlobalAlloc);

trait SyncGlobalAlloc: GlobalAlloc + Sync {}

impl<T: GlobalAlloc + Sync> SyncGlobalAlloc for T {}

// ---------------------------------------------------------------------------
// Per-operation latency sampling (BENCH_P99=1|64|1024...)
//
// A clock pair costs ~20-25 ns; a small thread-cache pop costs ~5 ns, so
// timing every call would measure the timer, not the allocator. Instead we
// sample 1-in-N calls and subtract a calibrated clock-pair cost, which makes
// the reported percentiles the *distribution* of per-op latency (order
// statistics, not a time series) at ~0.02 ns/op of overhead at 1/1024.
//
// Samples land in a process-wide buffer rather than thread-local storage:
// worker threads are short-lived (spawn-churn) and their TLS would be gone
// before the run finished. Two relaxed atomics per *sampled* call is noise.
// ---------------------------------------------------------------------------

const LATENCY_CAPACITY: usize = 1 << 18;
static LATENCY_SAMPLES: [AtomicU32; LATENCY_CAPACITY] =
    [const { AtomicU32::new(0) }; LATENCY_CAPACITY];
static LATENCY_LEN: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Per-thread sampling tick. Deliberately *not* a shared atomic: a
    /// contended `fetch_add` on every allocator call cost 3x throughput on
    /// `mixed-all 8T` (40.8M -> 12.0M ops/s) before this was thread-local.
    static LATENCY_TICK: Cell<u64> = const { Cell::new(0) };
}

/// Median cost of a back-to-back clock pair, in nanoseconds. Every sample
/// carries one clock pair, so this is subtracted from it.
fn calibrate_clock_overhead() -> u32 {
    let mut samples: Vec<u64> = Vec::with_capacity(1024);
    for _ in 0..1024 {
        let t0 = Instant::now();
        let t1 = Instant::now();
        samples.push(t1.duration_since(t0).as_nanos() as u64);
    }
    samples.sort_unstable();
    samples[samples.len() / 2] as u32
}

/// Round a requested rate up to a power of two so the hot-path test is a
/// mask rather than a division.
fn sampling_mask(every: u64) -> u64 {
    if every <= 1 {
        return 0;
    }
    every.next_power_of_two() - 1
}

fn latency_reset() {
    LATENCY_LEN.store(0, Ordering::Relaxed);
}

#[inline]
fn latency_record(elapsed_ns: u64, overhead: u32) {
    let value = elapsed_ns.saturating_sub(overhead as u64) as u32;
    let index = LATENCY_LEN.fetch_add(1, Ordering::Relaxed);
    if index < LATENCY_CAPACITY {
        LATENCY_SAMPLES[index].store(value, Ordering::Relaxed);
    }
}

/// Percentiles over the sampled per-op latencies, or `None` when sampling
/// is off or nothing was recorded.
fn latency_summary() -> Option<[u64; 4]> {
    let len = LATENCY_LEN.load(Ordering::Relaxed).min(LATENCY_CAPACITY);
    if len == 0 {
        return None;
    }
    let mut values: Vec<u32> = (0..len)
        .map(|index| LATENCY_SAMPLES[index].load(Ordering::Relaxed))
        .collect();
    values.sort_unstable();
    let at = |q: f64| -> u64 {
        let position = ((values.len() as f64 - 1.0) * q).round() as usize;
        values[position.min(values.len() - 1)] as u64
    };
    Some([at(0.50), at(0.90), at(0.99), at(0.999)])
}

/// Wraps a comparator and samples per-call latency. `mask == 0` disables
/// sampling entirely and the hot path is a thread-local increment plus a
/// mask test (no atomics, no clock).
struct Probed {
    inner: &'static dyn SyncGlobalAlloc,
    mask: u64,
    overhead: u32,
}

impl Probed {
    #[inline]
    fn new(inner: &'static dyn SyncGlobalAlloc, every: u64) -> Self {
        let mask = sampling_mask(every);
        let overhead = if mask == 0 { 0 } else { calibrate_clock_overhead() };
        Probed {
            inner,
            mask,
            overhead,
        }
    }

    /// True on 1-in-N calls. The tick lives in thread-local storage so the
    /// decision never touches a shared cache line.
    #[inline]
    fn due(&self) -> bool {
        if self.mask == 0 {
            return false;
        }
        LATENCY_TICK.with(|tick| {
            let value = tick.get();
            tick.set(value.wrapping_add(1));
            value & self.mask == 0
        })
    }

    #[inline]
    fn record(&self, start: Instant) {
        latency_record(start.elapsed().as_nanos() as u64, self.overhead);
    }
}

// SAFETY: every method forwards to `inner` with the same arguments and
// returns its result unchanged, so all `GlobalAlloc` contracts (pointer
// validity, layout match, no use-after-free) are exactly the inner
// allocator's.
unsafe impl GlobalAlloc for Probed {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.due() {
            return self.inner.alloc(layout);
        }
        let t0 = Instant::now();
        let p = self.inner.alloc(layout);
        self.record(t0);
        p
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if !self.due() {
            return self.inner.dealloc(p, layout);
        }
        let t0 = Instant::now();
        self.inner.dealloc(p, layout);
        self.record(t0);
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if !self.due() {
            return self.inner.realloc(p, layout, new_size);
        }
        let t0 = Instant::now();
        let np = self.inner.realloc(p, layout, new_size);
        self.record(t0);
        np
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !self.due() {
            return self.inner.alloc_zeroed(layout);
        }
        let t0 = Instant::now();
        let p = self.inner.alloc_zeroed(layout);
        self.record(t0);
        p
    }
}

/// Cumulative allocator timings, present only in `telemetry` builds.
fn timing_counters() -> Option<[u64; 3]> {
    if cfg!(feature = "telemetry") {
        #[cfg(feature = "telemetry")]
        {
            let t = allox::telemetry::timing();
            return Some([t.lock_wait_ns, t.purge_ns, t.exit_flush_ns]);
        }
    }
    None
}

fn format_f64_vec(values: &[f64]) -> String {
    let mut text = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            text.push(',');
        }
        text.push_str(&format!("{:.3}", value));
    }
    text.push(']');
    text
}

fn format_u64_vec(values: &[u64]) -> String {
    let mut text = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            text.push(',');
        }
        text.push_str(&value.to_string());
    }
    text.push(']');
    text
}

fn print_raw_samples(
    allocators: &[Named],
    selected: &[usize],
    runs: &[Vec<RunSample>],
    warmups: &[Option<RunSample>],
    latency: &[Option<[u64; 4]>],
) {
    for &index in selected {
        let samples = &runs[index];
        let rates: Vec<f64> = samples.iter().map(RunSample::ops_per_sec).collect();
        let elapsed_ms: Vec<f64> = samples
            .iter()
            .map(|sample| sample.elapsed.as_secs_f64() * 1000.0)
            .collect();
        let ops: Vec<u64> = samples.iter().map(|sample| sample.ops).collect();
        let ns_per_op: Vec<f64> = samples.iter().map(RunSample::ns_per_op).collect();
        let rss: Vec<u64> = samples.iter().map(|sample| sample.rss_kib).collect();
        let peak: Vec<u64> = samples.iter().map(|sample| sample.peak_rss_kib).collect();
        let warmup_ops = warmups[index]
            .map(|sample| sample.ops.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  raw {:<8} warmup_ops={} ops/s={} ops={} elapsed_ms={} ns/op={} rss_kib={} peak_kib={}",
            allocators[index].0.trim(),
            warmup_ops,
            format_f64_vec(&rates),
            format_u64_vec(&ops),
            format_f64_vec(&elapsed_ms),
            format_f64_vec(&ns_per_op),
            format_u64_vec(&rss),
            format_u64_vec(&peak),
        );
        let timing = summarize(samples).timing;
        println!(
            "       timing ns/op mean={:.2} p50={:.2} p95={:.2} p99={:.2} p99.9={:.2} (per-run aggregate)",
            timing.mean_ns_per_op,
            timing.p50_run_ns_per_op,
            timing.p95_run_ns_per_op,
            timing.p99_run_ns_per_op,
            timing.p99_9_run_ns_per_op,
        );
        if let Some([p50, p90, p99, p999]) = latency[index] {
            println!(
                "       call latency ns p50={} p90={} p99={} p99.9={} (sampled, clock overhead subtracted)",
                p50, p90, p99, p999
            );
        }
    }
}

/// One line per allox run: the volume counters that moved, so a throughput
/// number can be read together with the work behind it (refills, flushes,
/// copied bytes, purges, ...).
fn print_volume_counters(diagnostics: &AlloxDiagnostics) {
    let mut parts: Vec<String> = Vec::new();
    for (name, value) in &diagnostics.volume {
        if *value == 0 {
            continue;
        }
        parts.push(format!("{}={}", name, value));
    }
    if parts.is_empty() {
        return;
    }
    println!("       counters {}", parts.join(" "));
    if let Some([lock, purge, exit]) = diagnostics.timing_ns {
        println!(
            "       timing ns lock_wait={} purge={} exit_flush={} (telemetry build)",
            lock, purge, exit
        );
    }
}

fn summary_for_name(
    allocators: &[Named],
    summaries: &[Option<SampleSummary>],
    name: &str,
) -> Option<f64> {
    allocators
        .iter()
        .position(|allocator| allocator.0.trim() == name)
        .and_then(|index| summaries[index].map(|summary| summary.median_ops_per_sec))
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            control if control.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => escaped.push(other),
        }
    }
    escaped
}

fn json_string(value: &str) -> String {
    format!("\"{}\"", json_escape(value))
}

fn json_number(value: f64) -> String {
    if value.is_finite() {
        format!("{:.6}", value)
    } else {
        "null".to_string()
    }
}

fn append_json_sample(output: &mut String, sample: RunSample) {
    output.push_str(&format!(
        "{{\"ops\":{},\"elapsed_ns\":{},\"ops_per_sec\":{},\"ns_per_op\":{},\"rss_kib\":{},\"peak_rss_kib\":{}}}",
        sample.ops,
        sample.elapsed_ns(),
        json_number(sample.ops_per_sec()),
        json_number(sample.ns_per_op()),
        sample.rss_kib,
        sample.peak_rss_kib,
    ));
}

fn append_json_samples(output: &mut String, samples: &[RunSample]) {
    output.push('[');
    for (index, sample) in samples.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        append_json_sample(output, *sample);
    }
    output.push(']');
}

fn append_json_summary(output: &mut String, summary: &SampleSummary) {
    let timing = summary.timing;
    output.push_str(&format!(
        "{{\"median_ops_per_sec\":{},\"mean_ops_per_sec\":{},\"min_ops_per_sec\":{},\"max_ops_per_sec\":{},\"total_ops\":{},\"total_elapsed_ns\":{},\"timing\":{{\"mean_ns_per_op\":{},\"min_run_ns_per_op\":{},\"max_run_ns_per_op\":{},\"p50_run_ns_per_op\":{},\"p95_run_ns_per_op\":{},\"p99_run_ns_per_op\":{},\"p99_9_run_ns_per_op\":{}}}}}",
        json_number(summary.median_ops_per_sec),
        json_number(summary.mean_ops_per_sec),
        json_number(summary.min_ops_per_sec),
        json_number(summary.max_ops_per_sec),
        summary.total_ops,
        summary.total_elapsed_ns,
        json_number(timing.mean_ns_per_op),
        json_number(timing.min_run_ns_per_op),
        json_number(timing.max_run_ns_per_op),
        json_number(timing.p50_run_ns_per_op),
        json_number(timing.p95_run_ns_per_op),
        json_number(timing.p99_run_ns_per_op),
        json_number(timing.p99_9_run_ns_per_op),
    ));
}

fn append_json_allocator(
    output: &mut String,
    name: &str,
    warmup: Option<RunSample>,
    samples: &[RunSample],
    summary: Option<SampleSummary>,
    latency: Option<[u64; 4]>,
) {
    output.push_str("{\"name\":");
    output.push_str(&json_string(name));
    output.push_str(",\"warmup\":");
    if let Some(sample) = warmup {
        append_json_sample(output, sample);
    } else {
        output.push_str("null");
    }
    output.push_str(",\"samples\":");
    append_json_samples(output, samples);
    output.push_str(",\"summary\":");
    if let Some(summary) = summary {
        append_json_summary(output, &summary);
    } else {
        output.push_str("null");
    }
    // Sampled per-call latency percentiles in ns (BENCH_P99); null when
    // sampling is off.
    output.push_str(",\"call_latency_ns\":");
    match latency {
        Some([p50, p90, p99, p999]) => output.push_str(&format!(
            "{{\"p50\":{},\"p90\":{},\"p99\":{},\"p99_9\":{}}}",
            p50, p90, p99, p999
        )),
        None => output.push_str("null"),
    }
    output.push('}');
}

fn append_json_diagnostics(output: &mut String, diagnostics: &AlloxDiagnostics) {
    output.push_str(&format!(
        "{{\"map_delta\":{},\"unmap_delta\":{},\"mapped_delta\":{},\"span_maps\":{},\"span_unmaps\":{},\"small_maps\":{},\"small_unmaps\":{},\"arena_reuses\":{},\"arena_commits\":{},\"big_maps\":{},\"big_unmaps\":{},\"abandoned_delta\":{},\"abandoned_total\":{},\"arena_high_water\":{},\"volume\":{{",
        diagnostics.map_delta,
        diagnostics.unmap_delta,
        diagnostics.mapped_delta,
        diagnostics.span_maps,
        diagnostics.span_unmaps,
        diagnostics.small_maps,
        diagnostics.small_unmaps,
        diagnostics.arena_reuses,
        diagnostics.arena_commits,
        diagnostics.big_maps,
        diagnostics.big_unmaps,
        diagnostics.abandoned_delta,
        diagnostics.abandoned_total,
        diagnostics.arena_high_water,
    ));
    // Only non-zero counters: the full set is 25 fields and most are zero on
    // any given workload, which would drown the signal in JSONL output.
    for (index, (name, value)) in diagnostics.volume.iter().enumerate() {
        if *value == 0 {
            continue;
        }
        if index != 0 {
            output.push(',');
        }
        output.push_str(&format!("\"{}\":{}", name, value));
    }
    output.push_str("},\"timing_ns\":");
    match &diagnostics.timing_ns {
        Some([lock, purge, exit]) => {
            output.push_str(&format!(
                "{{\"lock_wait\":{},\"purge\":{},\"exit_flush\":{}}}",
                lock, purge, exit
            ));
        }
        None => output.push_str("null"),
    }
    output.push_str(",\"probe\":");
    append_json_sample(output, diagnostics.probe);
    output.push('}');
}

fn run_fresh_processes() {
    let exe = std::env::current_exe().expect("benchmark executable path");
    let secs = std::env::var("BENCH_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(3)
        .max(1);
    let reps = std::env::var("BENCH_REPS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5)
        .max(1);
    let workload_filter = std::env::var("BENCH_ONLY").unwrap_or_default();
    let allocator_filter = std::env::var("BENCH_ALLOC").unwrap_or_default();
    #[allow(unused_mut)]
    let mut allocator_names = vec!["allox", "talc", "dlmalloc", "system", "mimalloc", "snmalloc"];
    #[cfg(feature = "bench-jemalloc")]
    allocator_names.push("jemalloc");

    for workload in WORKLOADS {
        if !workload_filter.is_empty() && !workload.name.contains(&workload_filter) {
            continue;
        }
        for repetition in 0..reps {
            for offset in 0..allocator_names.len() {
                let allocator = allocator_names[(offset + repetition) % allocator_names.len()];
                if !allocator_filter.is_empty() && !allocator.contains(&allocator_filter) {
                    continue;
                }
                let output = std::process::Command::new(&exe)
                    .env("BENCH_CHILD", "1")
                    .env("BENCH_FRESH", "0")
                    .env("BENCH_ALLOC", allocator)
                    .env("BENCH_ONLY", workload.name)
                    .env("BENCH_SECS", secs.to_string())
                    .env("BENCH_REPS", "1")
                    .env("BENCH_WARMUP", "0")
                    .env("BENCH_OUTPUT", "json")
                    .output();
                match output {
                    Ok(output) if output.status.success() => {
                        print!("{}", String::from_utf8_lossy(&output.stdout));
                    }
                    Ok(output) => {
                        eprint!("{}", String::from_utf8_lossy(&output.stderr));
                        std::process::exit(1);
                    }
                    Err(error) => {
                        eprintln!("failed to spawn fresh benchmark process: {}", error);
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

fn main() {
    let fresh = std::env::var("BENCH_FRESH")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let child = std::env::var("BENCH_CHILD")
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if fresh && !child {
        run_fresh_processes();
        return;
    }
    let output_name = std::env::var("BENCH_OUTPUT")
        .or_else(|_| std::env::var("BENCH_FORMAT"))
        .unwrap_or_else(|_| "text".to_string());
    let json_flag = std::env::var("BENCH_JSON")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let json_output = json_flag || output_name.eq_ignore_ascii_case("json");
    let secs = std::env::var("BENCH_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3)
        .max(1);
    let reps = std::env::var("BENCH_REPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5)
        .max(1);
    let warmup_secs = std::env::var("BENCH_WARMUP")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| secs.min(1));
    if let Ok(mb) = std::env::var("BENCH_BUDGET_MB") {
        if let Ok(mb) = mb.parse::<usize>() {
            if mb > 0 {
                allox::set_thread_cache_budget(mb * 1024 * 1024);
                eprintln!("  budget overridden to {} MiB/thread", mb);
            }
        }
    }

    // Per-op latency sampling: `BENCH_P99=1` samples every call, any other
    // positive value samples 1-in-N. Unset (the default) leaves every
    // comparator untouched, so throughput numbers are unaffected.
    let p99_every: u64 = std::env::var("BENCH_P99")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    #[cfg_attr(not(feature = "bench-jemalloc"), allow(unused_mut))]
    let mut allocators: Vec<Named> = vec![
        Named("allox", &ALLOX),
        Named("talc ", &TALC),
        Named("dlmalloc", &DLMALLOC),
        Named("system", &HARNESS),
        Named("mimalloc", &MIMALLOC),
        Named("snmalloc", &SNMALLOC),
    ];
    #[cfg(feature = "bench-jemalloc")]
    allocators.push(Named("jemalloc", &JEMALLOC));
    if p99_every > 0 {
        for allocator in &mut allocators {
            let probed: &'static dyn SyncGlobalAlloc =
                Box::leak(Box::new(Probed::new(allocator.1, p99_every)));
            allocator.1 = probed;
        }
        latency_reset();
    }

    let filter = std::env::var("BENCH_ONLY").unwrap_or_default();
    let alloc_filter = std::env::var("BENCH_ALLOC").unwrap_or_default();
    let alloc_idx: Vec<usize> = allocators
        .iter()
        .enumerate()
        .filter(|(_, allocator)| {
            alloc_filter.is_empty() || allocator.0.trim().contains(&alloc_filter)
        })
        .map(|(index, _)| index)
        .collect();
    if alloc_idx.is_empty() {
        eprintln!("BENCH_ALLOC={alloc_filter} matched no allocators");
        return;
    }

    if !json_output {
        let mut header = format!("{:<15}", "workload");
        for &allocator_index in &alloc_idx {
            header.push_str(&format!(" {:>11}", allocators[allocator_index].0.trim()));
        }
        header.push_str(&format!(
            " {:>9} {:>10} {:>10} {:>10} {:>10} {:>16}",
            "a/talc", "rssKiB", "peakRSS", "mapcalls", "unmaps", "arena"
        ));
        println!("{}", header);
        println!("{}", "-".repeat(header.len()));
        println!(
            "config: seconds={} reps={} warmup_seconds={} harness=system",
            secs, reps, warmup_secs
        );
    }

    let mut json = String::from(
        "{\"schema\":\"allox.bench.v1\",\"harness_allocator\":\"system\",\"config\":{",
    );
    json.push_str(&format!(
        "\"seconds\":{},\"repetitions\":{},\"warmup_seconds\":{},",
        secs, reps, warmup_secs
    ));
    json.push_str(&format!(
        "\"workload_filter\":{},\"allocator_filter\":{},",
        json_string(&filter),
        json_string(&alloc_filter)
    ));
    json.push_str("\"timing_unit\":\"nanoseconds per workload operation\",\"timing_percentiles\":\"per-run aggregate ns/op\"},\"allocators\":[");
    for (index, &allocator_index) in alloc_idx.iter().enumerate() {
        if index != 0 {
            json.push(',');
        }
        json.push_str(&json_string(allocators[allocator_index].0.trim()));
    }
    json.push_str("],\"results\":[");

    let mut first_result = true;
    for workload in WORKLOADS {
        if !filter.is_empty() && !workload.name.contains(&filter) {
            continue;
        }

        let mut runs: Vec<Vec<RunSample>> = (0..allocators.len())
            .map(|_| Vec::with_capacity(reps))
            .collect();
        let mut warmups: Vec<Option<RunSample>> = (0..allocators.len()).map(|_| None).collect();
        if warmup_secs > 0 {
            for &allocator_index in &alloc_idx {
                let allocator = &allocators[allocator_index];
                eprintln!("  warmup {} / {}...", workload.name, allocator.0);
                warmups[allocator_index] = Some(run(allocator.1, workload, warmup_secs));
            }
        }
        let mut latency_by_allocator: Vec<Option<[u64; 4]>> =
            (0..allocators.len()).map(|_| None).collect();
        for _ in 0..reps {
            for &allocator_index in &alloc_idx {
                let allocator = &allocators[allocator_index];
                eprintln!("  running {} / {}...", workload.name, allocator.0);
                latency_reset();
                let sample = run(allocator.1, workload, secs);
                if p99_every > 0 {
                    // Last rep wins: percentiles over a whole 2 s run are
                    // what we report, and merging runs would need per-rep
                    // sample buffers.
                    latency_by_allocator[allocator_index] = latency_summary();
                }
                runs[allocator_index].push(sample);
            }
        }

        let summaries: Vec<Option<SampleSummary>> = (0..allocators.len())
            .map(|index| {
                if runs[index].is_empty() {
                    None
                } else {
                    Some(summarize(&runs[index]))
                }
            })
            .collect();
        let medians: Vec<Option<f64>> = summaries
            .iter()
            .map(|summary| summary.map(|summary| summary.median_ops_per_sec))
            .collect();

        let allox_index = allocators
            .iter()
            .position(|allocator| allocator.0.trim() == "allox");
        let diagnostics = if let Some(allocator_index) =
            allox_index.filter(|index| alloc_idx.contains(index))
        {
            let s0 = allox::stats();
            let (d0sp, d0su, d0sm, d0smu, d0ac, d0aru, d0bm, d0bmu) = allox::__debug_map_split();
            let (d0abnd, _) = allox::__debug_arena_detail();
            let v0 = allox::__diagnostics::volume_raw();
            let probe = run(allocators[allocator_index].1, workload, 1);
            let s1 = allox::stats();
            let (d1sp, d1su, d1sm, d1smu, d1ac, d1aru, d1bm, d1bmu) = allox::__debug_map_split();
            let (d1abnd, d1hi) = allox::__debug_arena_detail();
            let v1 = allox::__diagnostics::volume_raw();
            let volume = allox::__diagnostics::VOLUME_FIELDS
                .iter()
                .zip(v0.iter().zip(v1.iter()))
                .map(|(name, (before, after))| {
                    ((*name).to_string(), after.saturating_sub(*before))
                })
                .collect();
            let timing_ns = timing_counters();
            Some(AlloxDiagnostics {
                map_delta: s1.map_calls.saturating_sub(s0.map_calls),
                unmap_delta: s1.unmap_calls.saturating_sub(s0.unmap_calls),
                mapped_delta: s1.mapped_pages as i64 - s0.mapped_pages as i64,
                span_maps: d1sp.saturating_sub(d0sp),
                span_unmaps: d1su.saturating_sub(d0su),
                small_maps: d1sm.saturating_sub(d0sm),
                small_unmaps: d1smu.saturating_sub(d0smu),
                arena_reuses: d1aru.saturating_sub(d0aru),
                arena_commits: d1ac.saturating_sub(d0ac),
                big_maps: d1bm.saturating_sub(d0bm),
                big_unmaps: d1bmu.saturating_sub(d0bmu),
                abandoned_delta: d1abnd.saturating_sub(d0abnd),
                abandoned_total: d1abnd,
                arena_high_water: d1hi,
                volume,
                timing_ns,
                probe,
            })
        } else {
            None
        };

        if !json_output {
            let format_optional = |value: Option<f64>| match value {
                Some(value) => format!("{:>11.0}", value),
                None => format!("{:>11}", "-"),
            };
            let allox_score = summary_for_name(&allocators, &summaries, "allox");
            let talc_score = summary_for_name(&allocators, &summaries, "talc");
            let ratio = match (allox_score, talc_score) {
                (Some(allox_score), Some(talc_score)) if talc_score > 0.0 => {
                    allox_score / talc_score
                }
                _ => 0.0,
            };
            let mapcalls = diagnostics.as_ref().map_or_else(
                || "-".to_string(),
                |diagnostics| {
                    format!(
                        "{}/{}/{}/{}/a{}/b{}",
                        diagnostics.map_delta,
                        diagnostics.span_maps,
                        diagnostics.small_maps,
                        diagnostics.mapped_delta,
                        diagnostics.arena_reuses,
                        diagnostics.big_maps
                    )
                },
            );
            let unmaps = diagnostics.as_ref().map_or_else(
                || "-".to_string(),
                |diagnostics| {
                    format!(
                        "{}/{}/b{}",
                        diagnostics.unmap_delta, diagnostics.span_unmaps, diagnostics.big_unmaps
                    )
                },
            );
            let arena = diagnostics.as_ref().map_or_else(
                || "-".to_string(),
                |diagnostics| {
                    format!(
                        "abnd+{}/s tot{} hi{}MiB",
                        diagnostics.abandoned_delta,
                        diagnostics.abandoned_total,
                        diagnostics.arena_high_water / (1024 * 1024)
                    )
                },
            );
            let mut row = format!("{:<15}", workload.name);
            for &allocator_index in &alloc_idx {
                row.push_str(&format_optional(medians[allocator_index]));
            }
            row.push_str(&format!(
                " {:>8.2}x {:>10} {:>10} {:>10} {:>10} {:>16}",
                ratio,
                current_rss_kib(),
                peak_rss_kib(),
                mapcalls,
                unmaps,
                arena,
            ));
            println!("{}", row);
            print_raw_samples(
                &allocators,
                &alloc_idx,
                &runs,
                &warmups,
                &latency_by_allocator,
            );
            if let Some(diagnostics) = &diagnostics {
                print_volume_counters(diagnostics);
            }
        }

        if !first_result {
            json.push(',');
        }
        first_result = false;
        json.push_str("{\"workload\":");
        json.push_str(&json_string(workload.name));
        json.push_str(&format!(
            ",\"threads\":{},\"size_min\":{},\"size_max\":{},\"free_pct\":{},\"allocators\":[",
            workload.threads, workload.size_range.0, workload.size_range.1, workload.free_pct
        ));
        for (index, &allocator_index) in alloc_idx.iter().enumerate() {
            if index != 0 {
                json.push(',');
            }
            append_json_allocator(
                &mut json,
                allocators[allocator_index].0.trim(),
                warmups[allocator_index],
                &runs[allocator_index],
                summaries[allocator_index],
                latency_by_allocator[allocator_index],
            );
        }
        json.push_str("],\"allox_diagnostics\":");
        if let Some(diagnostics) = &diagnostics {
            append_json_diagnostics(&mut json, diagnostics);
        } else {
            json.push_str("null");
        }
        json.push('}');
    }

    json.push_str("]}\n");
    if json_output {
        print!("{}", json);
    } else {
        println!("{}", "-".repeat(60));
        println!("raw samples are per repetition; timing percentiles are over aggregate ns/op values, avoiding a timer around every allocator call.");
        println!("rssKiB = current VmRSS; peakRSS = VmHWM; both are KiB on Linux and 0 elsewhere.");
        println!("allox counters are sampled only when allox is selected by BENCH_ALLOC.");
        println!("BENCH_OUTPUT=json or BENCH_JSON=1 emits one machine-readable JSON document.");
        #[cfg(feature = "bench-jemalloc")]
        println!("jemalloc column present (--features bench-jemalloc); absent when feature off.");
    }
}
