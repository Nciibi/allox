# allox

A pure-Rust, thread-cached general-purpose memory allocator.

**Zero dependencies. Zero build scripts. No C toolchain.** If `rustc` can
target it, `allox` builds for it — Windows, Linux, macOS, and any
cross-compilation target, without `cc`, without CMake, without per-target
C library setup.

```toml
[dependencies]
allox = "0.1"
```

## Usage

```rust
use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

fn main() {
    // Everything below allocates through allox.
    let v: Vec<u32> = (0..1000).collect();
    assert_eq!(v[999], 999);
}
```

Direct use:

```rust,ignore
unsafe {
    let p = allox::malloc(64);
    allox::free(p);
}
```

C ABI (for FFI/embedding scenarios): `allox_malloc`, `allox_calloc`,
`allox_realloc`, `allox_free`, `allox_aligned_alloc`.

Observability:

```rust,ignore
let s = allox::stats();
println!("{} pages mapped", s.mapped_pages);
allox::flush_current_thread(); // return this thread's caches (thread pools)
```

**Allocation telemetry** (`telemetry` feature): totals, live bytes, peak
usage, large-alloc counts, and a per-size-class allocation histogram.
Counters accumulate thread-locally without atomics and are published in
batches, so production hot paths pay only register adds (~4% worst-case,
zero with the feature off):

```toml
[dependencies]
allox = { version = "0.1", features = ["telemetry"] }
```

```rust,ignore
let t = allox::telemetry::snapshot();
println!("live: {} bytes across {} allocations", t.live_bytes, t.live_allocs);
println!("peak: {}", t.peak_live_bytes);
for (class, n) in t.per_class_allocs.iter().enumerate() {
    if *n > 0 { println!("class {class}: {n} allocs"); }
}
```

## Benchmarks

Median of 5 interleaved runs, Windows x86-64, `cargo bench` (ops/s,
higher is better). All comparators run identical workloads through their
`GlobalAlloc` implementations. talc uses `GlobalAllocSource` (dynamic
growth/shrink through the system allocator) — its strongest hosted
configuration, not a strawman arena.

| Workload | allox | talc | dlmalloc | system | vs talc | vs dlm | vs sys |
|---|---:|---:|---:|---:|---:|---:|---:|
| tight-small 1T (64 B) | 40.8 M/s | 26.3 M/s | 21.4 M/s | 11.0 M/s | **1.55×** | **1.91×** | **3.70×** |
| mixed-small 1T (16–4096 B) | 29.3 M/s | 9.3 M/s | 5.7 M/s | 7.8 M/s | **3.15×** | **5.17×** | **3.77×** |
| tight-small 8T (64 B) | 209.6 M/s | 2.0 M/s | 3.8 M/s | 44.9 M/s | **105×** | **55×** | **4.67×** |
| mixed-small 8T (16–4096 B) | 132.1 M/s | 1.8 M/s | 1.6 M/s | 26.6 M/s | **73×** | **82×** | **4.96×** |
| mixed-all 8T (16–65536 B) | 1.27 M/s | 1.08 M/s | 0.67 M/s | 53 K/s | **1.18×** | **1.91×** | **23.8×** |

5/5 wins against every comparator. Why the multi-threaded gaps are
structural: every allox fast path is lock-free per thread (sharded class
locks are touched only by batched slow paths), while single-heap
allocators serialize on one mutex. Reproduce with `cargo bench`.

Linux x86-64 (Ryzen 5 1600), median of 3 interleaved 2 s runs,
`cargo bench` (ops/s, higher is better). Same harness and workloads as
above, plus medium/large/producer-consumer/spawn-churn coverage and
mimalloc + snmalloc comparators (dev-only; the library stays zero-C).
dlmalloc omitted: 10×+ run-to-run variance on this box.

| Workload | allox | talc | system | mimalloc | vs talc | vs mim | vs sys |
|---|---:|---:|---:|---:|---:|---:|---:|
| tight-small 1T (64 B) | 54.3 M/s | 26.2 M/s | 44.9 M/s | 46.5 M/s | **2.07×** | **1.17×** | **1.21×** |
| mixed-small 1T (16–4096 B) | 29.1 M/s | 9.0 M/s | 6.7 M/s | 13.0 M/s | **3.23×** | **2.24×** | **4.33×** |
| tight-small 8T (64 B) | 231.3 M/s | 2.1 M/s | 227.5 M/s | 219.6 M/s | **112×** | **1.05×** | **1.02×** |
| mixed-small 8T (16–4096 B) | 151.6 M/s | 1.7 M/s | 31.7 M/s | 37.5 M/s | **86.9×** | **4.04×** | **4.78×** |
| mixed-all 1T (16–65536 B) | 2.25 M/s | 0.47 M/s | 3.14 M/s | 4.19 M/s | **4.83×** | 0.54× | 0.71× |
| mixed-all 8T (16–65536 B) | 12.4 M/s | 0.96 M/s | 13.7 M/s | 18.7 M/s | **13.0×** | 0.66× | 0.91× |
| large-only 1T (32K–1M) | 220 K/s | 205 K/s | 245 K/s | 690 K/s | **1.07×** | 0.32× | 0.90× |
| large-only 8T (32K–256K) | 750 K/s | 410 K/s | 730 K/s | 13.7 M/s | **1.83×** | 0.05× | **1.03×** |
| prodcons 8T (remote free) | 32.4 M/s | 1.0 M/s | 8.9 M/s | 26.6 M/s | **31.4×** | **1.22×** | **3.63×** |
| spawn-churn | 11.9 M/s | 2.4 M/s | 11.5 M/s | 12.9 M/s | **5.05×** | 0.92× | **1.04×** |

Small + remote-free paths win or tie everywhere; medium/large variance
(mixed-all 8T per-op, large-only hit rate) and short-thread churn are the
known gaps — see REMAINING_PLAN.md. Two context notes: spawn-churn is
pool-state sensitive (0.23x mimalloc isolated, 0.92x in full-suite
order); large-only 8T varies run to run (lock-regime dynamics, same
reference). Full six-allocator output (incl. snmalloc) in harness runs.

## Design

mimalloc-inspired, adapted for Rust's world:

- **Size classes**: ~12.5% geometric growth from 16 B to 16 KiB — internal
  fragmentation never exceeds ~12.5%. Direct-mapped lookup table:
  size → class is one shift and one load.
- **64 KiB pages** hold blocks of one class; pointer → page header is a
  single bit-mask, no lookup tables.
- **Per-thread free lists**: allocation and deallocation fast paths take no
  locks and perform no atomic operations. Freed blocks stay in the freeing
  thread's cache — they almost always come back to the same thread.
- **Byte-budgeted caches**: thread caches grow freely and are trimmed only
  when a thread's total exceeds its budget (biggest bin first); flush/refill
  round-trips through the global heap were measured to cost 5× on mixed
  workloads, so they are made rare rather than fast.
- **Sharded global heap**: each size class' partial-page list has its own
  mutex; slow paths are batched (~64 blocks per lock acquisition).
- **Large / over-aligned allocations** are served by directly mapped regions
  tagged with a magic header; invalid frees are detected and abort.
- **Delayed page reclamation**: fully-freed pages are kept mapped (capped at
  4 per class, ~16 MiB worst case) and recycled on the next refill instead of
  paying unmap/map syscalls on churn.
- **Zero-init fast path**: `calloc`/`alloc_zeroed` from never-used ("virgin")
  memory skips the memset — only the freelist link word is cleared. Recycled
  memory is still always explicitly zeroed.
- **Debug builds validate every free**: pointer bounds, class alignment, and
  double-free detection.
- **No TLS destructors, no allocation inside the allocator**: const-init
  thread-local state; a spin-then-yield internal mutex that cannot allocate;
  explicit `flush_current_thread()` for thread pools.

See [DESIGN.md](DESIGN.md) for the full architecture document, research
notes, and rationale.

## Why not X?

| Alternative | The problem allox solves |
|---|---|
| System allocator | HeapAlloc lock contention on Windows; no observability |
| `jemallocator` / `mimalloc` | Require a working C toolchain for every target; break cross-compilation |
| `talc` / other pure-Rust allocators | Linked-list designs without thread caching or virtual-memory integration — they scale poorly past a few threads |

## no_std / embedded

```toml
allox = { version = "0.1", default-features = false }
```

With `default-features = false` allox builds against `core` alone: the
per-thread cache becomes a single global cache behind the allocator's own
spin mutex (embedded targets are single-threaded; the allocator never
re-enters that lock). Corruption diagnostics use `panic!` instead of
`abort()` — pair with `panic = "abort"` in your profile as usual.
The `wasm32` backend works with or without the `std` feature.

## WebAssembly

allox compiles for `wasm32-unknown-unknown` with no imports and no build
tooling — linear memory grows via `memory.grow` (64 KiB pages, matching our
page size). Since WASM cannot release memory pages, freed pages are recycled
through the delayed-reclamation cache and dead pages beyond the cap stay
mapped (the same trade-off dlmalloc makes).

```text
cargo build --target wasm32-unknown-unknown --example wasm_smoke
node scripts/wasm_smoke.mjs target/wasm32-unknown-unknown/debug/examples/wasm_smoke.wasm
```

## Status

v0.1 — working and tested (unit, integration as `#[global_allocator]`,
multi-threaded randomized stress with full integrity verification, C ABI).
Fastest pure-Rust allocator on the benchmarked hosted workloads as of the
table above. Not yet audited; API may still change before 0.2.
Linux/macOS results pending CI runs on those platforms.

## Development

```text
cargo test          # full test suite
cargo test --release
cargo bench         # throughput comparison vs system allocator
```

License: MIT OR Apache-2.0
