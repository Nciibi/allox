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
higher is better). Comparators run identical workloads through their
`GlobalAlloc` implementations. talc uses its documented global-allocator
configuration with a 512 MiB claimed arena.

| Workload | allox | talc | system | allox/talc | allox/sys |
|---|---:|---:|---:|---:|---:|
| tight-small 1T (64 B) | 47.6 M/s | 29.2 M/s | 10.9 M/s | **1.63×** | **4.36×** |
| mixed-small 1T (16–4096 B) | 28.9 M/s | 9.6 M/s | 8.1 M/s | **3.02×** | **3.59×** |
| tight-small 8T (64 B) | 220.2 M/s | 2.1 M/s | 51.4 M/s | **102×** | **4.29×** |
| mixed-small 8T (16–4096 B) | 142.3 M/s | 1.9 M/s | 29.2 M/s | **75×** | **4.87×** |
| mixed-all 8T (16–65536 B) | 106 K/s | 0 (OOM) | 53 K/s | **∞** | **2.00×** |

Why the multi-threaded gaps are structural, not tuning: every allox
allocation fast path is lock-free per thread (sharded class locks are only
touched by batched slow paths), while single-heap allocators serialize on
one mutex. Reproduce with `cargo bench`.

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
