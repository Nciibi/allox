//! Process-global allocator benchmark: an application-shaped workload run
//! with the measured allocator installed as `#[global_allocator]`.
//!
//! `benches/alloc.rs` calls each `GlobalAlloc` implementation directly, so
//! the harness's own bookkeeping never runs through the allocator under
//! test. That is the right way to compare allocator loops, but it cannot
//! measure what applications actually do: `Vec`/`String`/`HashMap` growth,
//! `Box`, arena-style bulk teardown, and per-thread allocator warm-up are
//! all invisible to a direct-call harness. This binary closes that gap.
//!
//! One process per allocator (the honest way: a global allocator is chosen
//! at compile/link time in real programs, and process state must not leak
//! between runs). The backend is chosen at runtime through a proxy, so one
//! binary covers every comparator.
//!
//! ```text
//! cargo run --release --example app_workload                     # all backends, fresh child each
//! APP_ALLOC=allox APP_SECS=5 cargo run --release --example app_workload
//! APP_P99=1024 ...                                               # sampled per-call latency
//! ```
//!
//! Bookkeeping is deliberately hoisted out of the timed region (corpus and
//! thread handles are built first), but note that with a process-global
//! allocator the harness's own allocations *do* go through the allocator
//! under test — that is inherent to this mode and part of what it measures.

#[allow(unused_imports)]
use std::alloc::{GlobalAlloc, Layout, System as SystemAlloc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// The installed global allocator
//
// One backend per binary, selected by a cargo feature: the allocation path
// is a direct call into the chosen allocator with no dispatch, no atomic
// load, and no wrapper. (An earlier runtime-dispatch proxy through a
// `#[global_allocator]` was measured and rejected: one relaxed atomic load
// per call cost allox 11% and talc 3% on the same workload, i.e. it
// penalised the fastest allocator the most.)
// ---------------------------------------------------------------------------

#[cfg(feature = "app-allox")]
type Backend = allox::Allox;
#[cfg(feature = "app-system")]
type Backend = SystemAlloc;
#[cfg(feature = "app-mimalloc")]
type Backend = mimalloc::MiMalloc;
#[cfg(feature = "app-snmalloc")]
type Backend = snmalloc_rs::SnMalloc;
#[cfg(feature = "app-talc")]
type Backend =
    talc::TalcLock<spinning_top::RawSpinlock, talc::source::GlobalAllocSource<SystemAlloc>>;
// No backend feature: the system allocator, so a plain `cargo build` /
// `cargo test` still compiles this example. scripts/app_bench.sh always
// passes an explicit backend.
#[cfg(not(any(
    feature = "app-allox",
    feature = "app-system",
    feature = "app-mimalloc",
    feature = "app-snmalloc",
    feature = "app-talc"
)))]
type Backend = SystemAlloc;

#[cfg(feature = "app-talc")]
static BACKEND: Backend =
    talc::TalcLock::new(talc::source::GlobalAllocSource::new(SystemAlloc));
#[cfg(feature = "app-allox")]
static BACKEND: Backend = allox::Allox;
#[cfg(feature = "app-system")]
static BACKEND: Backend = SystemAlloc;
#[cfg(feature = "app-mimalloc")]
static BACKEND: Backend = mimalloc::MiMalloc;
#[cfg(feature = "app-snmalloc")]
static BACKEND: Backend = snmalloc_rs::SnMalloc;
#[cfg(not(any(
    feature = "app-allox",
    feature = "app-system",
    feature = "app-mimalloc",
    feature = "app-snmalloc",
    feature = "app-talc"
)))]
static BACKEND: Backend = SystemAlloc;

#[cfg(any(
    all(feature = "app-allox", any(
        feature = "app-system",
        feature = "app-mimalloc",
        feature = "app-snmalloc",
        feature = "app-talc"
    )),
    all(feature = "app-system", any(
        feature = "app-mimalloc",
        feature = "app-snmalloc",
        feature = "app-talc"
    )),
    all(feature = "app-mimalloc", any(feature = "app-snmalloc", feature = "app-talc")),
    all(feature = "app-snmalloc", feature = "app-talc"),
))]
compile_error!(
    "pick exactly one backend feature: app-allox | app-system | app-mimalloc | app-snmalloc | app-talc"
);

/// Which allocator this binary was built with, for `APP_ALLOC` verification.
const BACKEND_NAME: &str = if cfg!(feature = "app-allox") {
    "allox"
} else if cfg!(feature = "app-system") {
    "system"
} else if cfg!(feature = "app-mimalloc") {
    "mimalloc"
} else if cfg!(feature = "app-snmalloc") {
    "snmalloc"
} else {
    "talc"
};

/// Forwards every `GlobalAlloc` call to [`BACKEND`]. `SAMPLING` is a const
/// generic, so the un-sampled build has no branch at all: `if SAMPLING`
/// folds away and the methods become plain forwards.
struct Sampled<const SAMPLING: bool> {
    inner: &'static Backend,
}

// SAFETY: every method forwards to a `GlobalAlloc` implementation with the
// same arguments and returns its result unchanged, so the contract is
// exactly that of the inner allocator. The sampling wrapper only reads a
// clock and appends to a preallocated static buffer.
unsafe impl<const SAMPLING: bool> GlobalAlloc for Sampled<SAMPLING> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if SAMPLING && latency_due() {
            let t0 = Instant::now();
            let p = self.inner.alloc(layout);
            latency_record(t0);
            return p;
        }
        self.inner.alloc(layout)
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if SAMPLING && latency_due() {
            let t0 = Instant::now();
            self.inner.dealloc(p, layout);
            latency_record(t0);
            return;
        }
        self.inner.dealloc(p, layout)
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if SAMPLING && latency_due() {
            let t0 = Instant::now();
            let np = self.inner.realloc(p, layout, new_size);
            latency_record(t0);
            return np;
        }
        self.inner.realloc(p, layout, new_size)
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if SAMPLING && latency_due() {
            let t0 = Instant::now();
            let p = self.inner.alloc_zeroed(layout);
            latency_record(t0);
            return p;
        }
        self.inner.alloc_zeroed(layout)
    }
}

#[cfg(feature = "app-p99")]
#[global_allocator]
static GLOBAL: Sampled<true> = Sampled { inner: &BACKEND };
#[cfg(not(feature = "app-p99"))]
#[global_allocator]
static GLOBAL: Sampled<false> = Sampled { inner: &BACKEND };

// ---------------------------------------------------------------------------
// Sampled per-call latency (--features app-p99, APP_P99=<every>)
// ---------------------------------------------------------------------------

const LATENCY_CAPACITY: usize = 1 << 18;
static LATENCY_SAMPLES: [AtomicU32; LATENCY_CAPACITY] =
    [const { AtomicU32::new(0) }; LATENCY_CAPACITY];
static LATENCY_LEN: AtomicUsize = AtomicUsize::new(0);
static CLOCK_OVERHEAD_NS: AtomicU32 = AtomicU32::new(0);
/// Sampling divisor as a mask; only read in `app-p99` builds.
static SAMPLE_MASK: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Per-thread sampling tick. A shared atomic here would cost real
    /// throughput on every call, which is exactly what this benchmark
    /// cannot afford.
    static LOCAL_TICK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn clock_overhead_ns() -> u32 {
    let mut samples: Vec<u64> = Vec::with_capacity(1024);
    for _ in 0..1024 {
        let t0 = Instant::now();
        let t1 = Instant::now();
        samples.push(t1.duration_since(t0).as_nanos() as u64);
    }
    samples.sort_unstable();
    samples[samples.len() / 2] as u32
}

#[inline]
fn latency_due() -> bool {
    let mask = SAMPLE_MASK.load(Ordering::Relaxed);
    if mask == 0 {
        return false;
    }
    LOCAL_TICK.with(|tick| {
        let value = tick.get();
        tick.set(value.wrapping_add(1));
        value & mask == 0
    })
}

fn latency_record(start: Instant) {
    let overhead = CLOCK_OVERHEAD_NS.load(Ordering::Relaxed) as u64;
    let elapsed = start.elapsed().as_nanos() as u64;
    let value = elapsed.saturating_sub(overhead) as u32;
    let index = LATENCY_LEN.fetch_add(1, Ordering::Relaxed);
    if index < LATENCY_CAPACITY {
        LATENCY_SAMPLES[index].store(value, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Application-shaped workload
// ---------------------------------------------------------------------------

/// A parsed document node: the shapes real parsers produce (owned strings,
/// growable lists, ordered key/value maps, boxed nesting).
#[derive(Debug)]
#[allow(dead_code)] // `Num` payloads are read by the transform, not the walk
enum Node {
    Str(String),
    Num(f64),
    Nums(Vec<f64>),
    Bytes(Vec<u8>, Vec<u8>),
    List(Vec<Node>),
    Map(Vec<(String, Node)>),
}

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

const WORDS: [&str; 12] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliett",
    "kilo", "lima",
];

/// Build one document. Width dominates depth so a document is allocation-
/// heavy and CPU-light: the interesting variable is how the allocator
/// services many small growing containers, not how fast we can walk a tree.
/// Every backend sees identical work (the RNG is seeded per document).
fn build_document(rng: &mut Rng, depth: u32) -> Node {
    // Past the depth cap only the three non-recursive arms are in range,
    // so the recursion is bounded by construction.
    let arms = if depth >= 3 { 3 } else { 5 };
    match rng.next() % arms {
        0 => {
            // A string built by repeated appends: the classic growth path
            // (several reallocs per string, amortized doubling).
            let mut s = String::with_capacity(8);
            let words = 4 + rng.next() % 12;
            for _ in 0..words {
                s.push_str(WORDS[(rng.next() % WORDS.len() as u64) as usize]);
                s.push(' ');
                if rng.next() % 3 == 0 {
                    s.push_str(WORDS[(rng.next() % WORDS.len() as u64) as usize]);
                    s.push(' ');
                }
            }
            Node::Str(s)
        }
        1 => {
            // A growable numeric series: one Vec, many reallocs.
            let count = 8 + rng.next() % 24;
            let mut values = Vec::with_capacity(count as usize);
            for _ in 0..count {
                values.push((rng.next() % 100_000) as f64 / 100.0);
            }
            Node::Nums(values)
        }
        2 => {
            // Byte buffer churn (header/body shaped): pushes, then a drain.
            let count = 16 + rng.next() % 48;
            let mut buffer: Vec<u8> = Vec::with_capacity(count as usize);
            for _ in 0..count {
                buffer.push((rng.next() % 251) as u8);
            }
            let tail: Vec<u8> = buffer.drain(..count as usize / 2).collect();
            Node::Bytes(buffer, tail)
        }
        3 => {
            let items = 4 + rng.next() % 12;
            let mut list = Vec::with_capacity(items as usize);
            for _ in 0..items {
                list.push(build_document(rng, depth + 1));
            }
            Node::List(list)
        }
        _ => {
            let entries = 3 + rng.next() % 8;
            let mut map = Vec::with_capacity(entries as usize);
            for i in 0..entries {
                let mut key = String::with_capacity(12);
                key.push_str(WORDS[(rng.next() % WORDS.len() as u64) as usize]);
                key.push('-');
                key.push_str(
                    WORDS[((i as u64 + rng.next() % 5) % WORDS.len() as u64) as usize],
                );
                map.push((key, build_document(rng, depth + 1)));
            }
            Node::Map(map)
        }
    }
}

/// One "request": build a document, index its strings, then drop
/// everything (arena-lifetime teardown, the shape a parse-and-release
/// service has).
///
/// `APP_INDEX=0` drops the string index, leaving a much more
/// allocation-dense and CPU-light document (useful for seeing the
/// allocator's contribution without the hashing work on top).
fn process_document(seed: u64) -> (usize, usize) {
    if std::env::var("APP_INDEX").map(|v| v == "0").unwrap_or(false) {
        return process_document_dense(seed);
    }
    let mut rng = Rng(seed | 1);
    let tree = build_document(&mut rng, 0);

    let mut index: HashMap<String, usize> = HashMap::with_capacity(32);
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    let mut stack = vec![&tree];
    while let Some(node) = stack.pop() {
        nodes += 1;
        match node {
            Node::Str(s) => {
                *index.entry(s.clone()).or_insert(0) += 1;
            }
            Node::Nums(values) => bytes += values.len(),
            Node::Bytes(buffer, tail) => bytes += buffer.len() + tail.len(),
            Node::Num(_) => {}
            Node::List(items) => stack.extend(items.iter()),
            Node::Map(entries) => {
                for (key, value) in entries {
                    *index.entry(key.clone()).or_insert(0) += 1;
                    stack.push(value);
                }
            }
        }
    }
    (nodes, bytes + index.len())
}

/// Allocation-dense variant: build and drop, no indexing pass.
fn process_document_dense(seed: u64) -> (usize, usize) {
    let mut rng = Rng(seed | 1);
    let tree = build_document(&mut rng, 0);
    let mut nodes = 0usize;
    let mut stack = vec![&tree];
    while let Some(node) = stack.pop() {
        nodes += 1;
        match node {
            Node::List(items) => stack.extend(items.iter()),
            Node::Map(entries) => stack.extend(entries.iter().map(|(_, value)| value)),
            _ => {}
        }
    }
    (nodes, 0)
}

fn main() {
    let seconds: u64 = std::env::var("APP_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    let threads: usize = std::env::var("APP_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4);
    let every: usize = std::env::var("APP_P99")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1024);

    if let Ok(requested) = std::env::var("APP_ALLOC") {
        if !requested.is_empty() && requested != BACKEND_NAME {
            eprintln!(
                "warning: APP_ALLOC={requested} but this binary is built with {BACKEND_NAME}; \
                 run it through scripts/app_bench.sh so the backend features match"
            );
        }
    }

    // Only compiled with `--features app-p99`; in the default build the
    // sampling branch does not exist at all.
    if cfg!(feature = "app-p99") {
        let mask = if every <= 1 { 0 } else { every.next_power_of_two() - 1 };
        SAMPLE_MASK.store(mask, Ordering::Relaxed);
        if mask != 0 {
            CLOCK_OVERHEAD_NS.store(clock_overhead_ns(), Ordering::Relaxed);
        }
    }

    // Warm-up outside the timed region: the first documents would otherwise
    // measure process-start page faults rather than the allocator.
    let mut checksum = 0usize;
    for i in 0..64u64 {
        let (nodes, strings) = process_document(i + 1);
        checksum += nodes + strings;
    }

    let stop = Instant::now() + std::time::Duration::from_secs(seconds);
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            std::thread::spawn(move || {
                let mut ops = 0u64;
                let mut local = 0usize;
                let mut seed =
                    0xA11CE_u64 ^ ((t as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                while Instant::now() < stop {
                    // Batched so the clock read is not the workload's cost.
                    for _ in 0..64 {
                        seed = seed
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1_442_695_040_888_963_407);
                        let (nodes, strings) = process_document(seed >> 20);
                        local += nodes + strings;
                        ops += 1;
                    }
                }
                (ops, local)
            })
        })
        .collect();

    let mut docs = 0u64;
    for handle in handles {
        let (ops, local) = handle.join().expect("worker");
        docs += ops;
        checksum += local;
    }

    let elapsed = seconds.max(1) as f64;
    let docs_per_sec = docs as f64 / elapsed;
    let ns_per_doc = if docs > 0 {
        elapsed * 1e9 / docs as f64
    } else {
        0.0
    };
    let (p99_ns, samples) = latency_percentiles();
    println!(
        "RESULT {} {} {} {} {}",
        BACKEND_NAME,
        docs_per_sec,
        ns_per_doc,
        peak_rss_kib(),
        p99_ns
    );
    println!(
        "{}: {:.0} docs/s, {:.1} ns/document, peak RSS {:.1} MiB, {} p99 latency samples{}",
        BACKEND_NAME,
        docs_per_sec,
        ns_per_doc,
        peak_rss_kib() as f64 / 1024.0,
        samples,
        if samples > 0 {
            format!(", p99 {} ns", p99_ns)
        } else {
            String::new()
        }
    );
    // Keep the checksum observable so the optimizer cannot drop the workload.
    if checksum == usize::MAX {
        println!("unreachable {checksum}");
    }
    // Allox-only: the always-on volume counters turn "slower at 4 threads"
    // into a specific work mix (refills, flushes, copied bytes, trims).
    if BACKEND_NAME == "allox" {
        let mut parts: Vec<String> = Vec::new();
        for (name, value) in allox::__diagnostics::VOLUME_FIELDS
            .iter()
            .zip(allox::__diagnostics::volume_raw().iter())
        {
            if *value > 0 {
                parts.push(format!("{name}={value}"));
            }
        }
        println!("RESULT_COUNTERS {}", parts.join(" "));
        // With `--features telemetry` the per-class histogram turns "slower
        // than mimalloc" into a work mix: which tiers and which class sizes
        // the application shape actually spends its allocations in.
        #[cfg(feature = "telemetry")]
        {
            let t = allox::telemetry::snapshot();
            println!(
                "RESULT_TELEMETRY allocs={} frees={} alloc_bytes={} free_bytes={} \
                 live_peak={} large={} maps={} unmaps={}",
                t.total_allocs,
                t.total_frees,
                t.allocated_bytes,
                t.freed_bytes,
                t.peak_live_bytes,
                t.large_allocs,
                t.map_calls,
                t.unmap_calls
            );
            let total: u64 = t.per_class_allocs.iter().sum();
            let mut rows: Vec<(usize, u64)> = t
                .per_class_allocs
                .iter()
                .enumerate()
                .filter(|(_, n)| **n > 0)
                .map(|(c, n)| (c, *n))
                .collect();
            // Busiest classes first: the head of this list is the work mix.
            rows.sort_by_key(|(_, n)| core::cmp::Reverse(*n));
            for (class, n) in rows.into_iter().take(16) {
                println!(
                    "RESULT_CLASS class={class} allocs={n} share={:.2}%",
                    n as f64 * 100.0 / total.max(1) as f64
                );
            }
        }
    }
    // Threads are joined and the workload is dropped; skip teardown so the
    // reported peak RSS is the steady-state figure, not teardown noise.
    std::process::exit(0);
}

fn latency_percentiles() -> (u64, usize) {
    let len = LATENCY_LEN.load(Ordering::Relaxed).min(LATENCY_CAPACITY);
    if len == 0 {
        return (0, 0);
    }
    let mut values: Vec<u32> = (0..len)
        .map(|i| LATENCY_SAMPLES[i].load(Ordering::Relaxed))
        .collect();
    values.sort_unstable();
    let position = ((values.len() as f64 - 1.0) * 0.99).round() as usize;
    (values[position.min(values.len() - 1)] as u64, len)
}

fn peak_rss_kib() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmHWM:") {
                    if let Some(kib) = rest.split_whitespace().next() {
                        return kib.parse().unwrap_or(0);
                    }
                }
            }
        }
    }
    0
}
