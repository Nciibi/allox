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

Linux x86-64 (Ryzen 5 1600), **one fresh process per allocator/workload/
repetition**, median of 3 × 2 s runs, `taskset -c 0-7`, `BENCH_FRESH=1
BENCH_SAFE_LIVE=1` in a 3 GiB cgroup (ops/s, higher is better). Every
comparator runs the identical workload through its `GlobalAlloc` impl
(mimalloc/snmalloc/talc/jemalloc are dev-only; the library stays zero-C).
`vs best` is against the strongest comparator in that row.

| Workload | allox | system | mimalloc | snmalloc | dlmalloc | talc | vs best | allox peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| tight-small 1T (64 B) | 50.25 M/s | 44.84 M/s | 49.45 M/s | 42.53 M/s | 18.60 M/s | 27.73 M/s | **1.02×** | 5.0 MiB |
| mixed-small 1T (16–4096 B) | 36.23 M/s | 7.02 M/s | 13.99 M/s | 12.47 M/s | 6.03 M/s | 10.22 M/s | **2.59×** | 15.7 MiB |
| tight-small 8T (64 B) | 225.9 M/s | 237.4 M/s | 220.7 M/s | 226.8 M/s | 1.60 M/s | 2.11 M/s | 0.95× | 7.5 MiB |
| mixed-small 8T (16–4096 B) | 185.8 M/s | 36.6 M/s | 41.4 M/s | 55.8 M/s | 728 K/s | 1.82 M/s | **3.33×** | 92.6 MiB |
| mixed-all 1T (16–65536 B) | 16.86 M/s | 2.70 M/s | 4.97 M/s | 504 K/s | 433 K/s | 501 K/s | **3.39×** | 21.1 MiB |
| mixed-all 8T (16–65536 B) | 40.78 M/s | 11.03 M/s | 21.27 M/s | 1.28 M/s | 434 K/s | 1.19 M/s | **1.92×** | 117.7 MiB |
| medium-only 1T (16–64 K) | 18.56 M/s | 2.31 M/s | 5.25 M/s | 256 K/s | 372 K/s | 402 K/s | **3.53×** | 16.7 MiB |
| medium-only 8T (16–64 K) | 41.98 M/s | 10.22 M/s | 22.50 M/s | 607 K/s | 445 K/s | 1.01 M/s | **1.87×** | 90.8 MiB |
| large-only 1T (32K–1M) | 7.17 M/s | 327 K/s | 2.50 M/s | 14.9 K/s | 509 K/s | 265 K/s | **2.87×** | 7.0 MiB |
| big-tail 1T (64K–256K) | 17.76 M/s | 343 K/s | 6.68 M/s | 22.5 K/s | 409 K/s | 302 K/s | **2.66×** | 6.2 MiB |
| big-upper-tail 1T (256K–1M) | 11.23 M/s | 311 K/s | 1.79 M/s | 11.1 K/s | 579 K/s | 213 K/s | **6.29×** | 5.6 MiB |
| large-only 8T (32K–256K) | 32.02 M/s | 1.11 M/s | 24.45 M/s | 137 K/s | 434 K/s | 1.01 M/s | **1.31×** | 34.1 MiB |
| huge-only 1T (5–8 MiB) | 1.97 M/s | 171 K/s | 1.39 M/s | 1.3 K/s | 495 K/s | 129 K/s | **1.42×** | 5.0 MiB |
| zeroed-large 1T | 209 K/s | 0.6 K/s | 2.4 K/s | 1.0 K/s | 0.9 K/s | 0.4 K/s | **87.3×** | 5.0 MiB |
| prodcons 8T (remote free) | 41.10 M/s | 6.44 M/s | 27.11 M/s | 32.93 M/s | 895 K/s | 1.04 M/s | **1.25×** | 238.8 MiB |
| spawn-churn | 18.96 M/s | 2.22 M/s | 15.50 M/s | 6.60 M/s | 864 K/s | 1.46 M/s | **1.22×** | 18.2 MiB |
| spawn-empty (calibration) | 35.5 K/s | 35.5 K/s | 34.2 K/s | 34.4 K/s | 35.1 K/s | 34.4 K/s | 1.00× | 4.7 MiB |
| json-ish 8T | 242.2 M/s | 245.9 M/s | 165.3 M/s | 224.9 M/s | 1.38 M/s | 2.03 M/s | 0.98× | 5.8 MiB |
| request 8T | 394.1 M/s | 187.6 M/s | 358.0 M/s | 402.8 M/s | 1.26 M/s | 2.13 M/s | 0.98× | 7.9 MiB |
| ecs 8T (realloc growth) | 92.09 M/s | 74.69 M/s | 1.85 M/s | 370 K/s | 783 K/s | 1.52 M/s | **1.23×** | 12.2 MiB |

**20/20 win-or-tie against the best comparator.** The three near-ties
(`tight-small 8T` 0.95×, `json-ish 8T` 0.98×, `request 8T` 0.98×) are
inside the run-to-run spread of the row they lose, and `spawn-empty` is
the pthread calibration row (allocator-independent by construction).

`ecs 8T` is the growable-extent workload: a 64 KiB buffer realloc-doubled
to 1 MiB, four at a time per thread, plus small-component churn. It used
to be the project's one structural loss (≈0.06× the system allocator,
whose glibc grows with zero-copy `mremap`). Big blocks are carved
contiguously out of a shared span, so growing one in place was impossible
and every step of the chain copied. The first cross-class growth now
relocates **once** into a large region that reserves slack up to the big
cap; every later step then grows in place, with no copy, no syscall and
no span traffic. Controlled A/B (same box, same session, fresh 2 s × 3
processes): **9.59 M/s → 92.09 M/s (9.6×), peak RSS 19.7 → 12.2 MiB**, and
1.20× the system allocator in a 2 s × 5 confirmation (91.45 vs 76.27 M/s).

The direct comparison uses the system allocator for harness bookkeeping.
Use `BENCH_OUTPUT=json` for machine-readable samples. Use
`BENCH_FRESH=1` to run every allocator/workload/repetition in a fresh
process; fresh-process output is JSONL and gives per-process RSS values.
Use `BENCH_SAFE_LIVE=1` with a memory cgroup for large runs to bound the
benchmark's transient live set to 16 operations per drain check.

### Latency and diagnostics

`BENCH_P99=1024` samples 1-in-1024 allocator calls and reports per-call
p50/p90/p99/p99.9 (clock-pair cost calibrated out; measured within noise of
an unsampled run). On `mixed-all 8T` that reads allox p99 **670 ns** versus
mimalloc 1080, system 3330 and snmalloc 4740.

`allox::__diagnostics::volume()` exposes 23 counters (refills, flushes,
trims, owner probes, remote frees, zeroed bytes, purges, arena fallbacks,
realloc relocations and copied bytes, ...). The per-event ones are
**telemetry-gated** — a default build compiles them out and pays nothing;
build with `--features telemetry` to read them. Only the genuinely cold ones
(purges, exit flushes, retired caches, software zeroing) are always on.
`telemetry::timing()` adds nanosecond lock-wait, purge and exit-flush totals.

That gating is not a detail. The first version shipped one shared atomic per
event and cost **33% on `mixed-all 8T` and 36% on `large-only 8T`**; batching
brought that to 0–3.6%, and gating it to the feature brought it to zero. At
4M trims/s and 12M owner probes/s, threads ping-pong one cache line faster
than the work being counted. If you add a counter, check its event rate
before you check its position in the source.

### Process-global application benchmark

The table above measures allocator loops. `examples/app_workload.rs`
measures the other thing: a `String`/`Vec`/`HashMap` application workload
with the allocator installed as the process `#[global_allocator]`, one
binary per allocator and one fresh process per run:

```text
scripts/app_bench.sh                    # all backends
APP_SECS=10 APP_THREADS=8 scripts/app_bench.sh
APP_P99=1024 scripts/app_bench.sh       # per-call latency
```

This is where allox is **not** ahead: 238.9k docs/s against mimalloc's
270.4k and snmalloc's 279.5k (system 203.1k, talc 12.3k) — 0.88× mimalloc.
Single-threaded the three tie; the gap is scaling plus the fact that 76% of
this workload's `realloc` calls relocate (containers double, every doubling
crosses a size class). See REMAINING_PLAN §4d for the profile and the
candidate fixes. It is reported here because a scoreboard that only
contains the flattering mode is not a scoreboard.

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
  tracked in arena/legacy side metadata; invalid frees are detected and abort.
- **Growable extents**: `realloc` growth that crosses size classes promotes
  the block once into a slack reserve (virtual-only, capped at the big cap)
  and then grows in place; a region ending exactly at the arena bump
  frontier can also claim adjacent pages atomically. Turns a doubling chain
  into one copy instead of one per step.
- **Delayed page reclamation**: fully-freed pages are kept mapped (capped at
  4 per class, ~16 MiB worst case) and recycled on the next refill instead of
  paying unmap/map syscalls on churn.
- **Zero-init fast path**: `calloc`/`alloc_zeroed` from never-used ("virgin")
  memory skips the memset — only the freelist link word is cleared. Recycled
  memory is explicitly zeroed unless a successful discard proved it was already
  zero.
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

With `default-features = false` allox builds against `core` alone on targets
with a supported memory backend (currently Unix, Windows, and WASM): the
per-thread cache becomes a single global cache behind the allocator's own
spin mutex (single-threaded targets; the allocator never re-enters that
lock). Corruption diagnostics use `panic!` instead of `abort()` — pair with
`panic = "abort"` in your profile as usual. Bare-metal targets without a
backend fail at compile time until a backend is supplied.
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
tables above. Not yet audited; API may still change before 0.2.
Windows table predates the span/arena/exit-flush work (re-verify via CI
bench artifacts); macOS results pending CI runs on that platform.

## Development

```text
cargo test          # full test suite
cargo test --release
cargo bench         # throughput comparison vs system allocator
```

License: MIT OR Apache-2.0
