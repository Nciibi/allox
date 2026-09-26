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
| tight-small 1T (64 B) | 54.53 M/s | 40.04 M/s | 46.89 M/s | 42.46 M/s | 20.60 M/s | 27.97 M/s | **1.16×** | 5.2 MiB |
| mixed-small 1T (16–4096 B) | 37.14 M/s | 7.40 M/s | 14.76 M/s | 12.42 M/s | 6.15 M/s | 10.64 M/s | **2.52×** | 16.0 MiB |
| tight-small 8T (64 B) | 257.56 M/s | 243.26 M/s | 218.66 M/s | 236.57 M/s | 1.56 M/s | 2.14 M/s | **1.06×** | 7.7 MiB |
| mixed-small 8T (16–4096 B) | 196.59 M/s | 37.41 M/s | 43.14 M/s | 57.36 M/s | 702 K/s | 1.87 M/s | **3.43×** | 92.9 MiB |
| mixed-all 1T (16–65536 B) | 17.43 M/s | 2.86 M/s | 5.50 M/s | 534 K/s | 435 K/s | 518 K/s | **3.17×** | 21.4 MiB |
| mixed-all 8T (16–65536 B) | 43.20 M/s | 11.39 M/s | 22.62 M/s | 1.31 M/s | 458 K/s | 1.26 M/s | **1.91×** | 117.9 MiB |
| medium-only 1T (16–64 K) | 19.68 M/s | 2.39 M/s | 5.60 M/s | 273 K/s | 366 K/s | 416 K/s | **3.51×** | 17.0 MiB |
| medium-only 8T (16–64 K) | 46.89 M/s | 11.16 M/s | 24.42 M/s | 618 K/s | 403 K/s | 1.25 M/s | **1.92×** | 91.2 MiB |
| large-only 1T (32 K–1 M) | 7.62 M/s | 345 K/s | 2.67 M/s | 17 K/s | 607 K/s | 279 K/s | **2.85×** | 7.1 MiB |
| big-tail 1T (64 K–256 K) | 17.77 M/s | 353 K/s | 6.71 M/s | 22 K/s | 469 K/s | 296 K/s | **2.65×** | 6.4 MiB |
| big-upper-tail 1T (256 K–1 M) | 11.24 M/s | 312 K/s | 1.79 M/s | 11 K/s | 467 K/s | 215 K/s | **6.26×** | 5.7 MiB |
| large-only 8T (32 K–1 M) | 31.54 M/s | 1.13 M/s | 24.78 M/s | 140 K/s | 458 K/s | 1.04 M/s | **1.27×** | 34.3 MiB |
| huge-only 1T (5–8 MiB) | 1.87 M/s | 171 K/s | 1.41 M/s | 1.3 K/s | 462 K/s | 131 K/s | **1.32×** | 5.2 MiB |
| zeroed-large 1T | 205 K/s | 0.6 /s | 2.4 K/s | 1.0 /s | 0.9 /s | 0.4 /s | **87×** | 5.2 MiB |
| prodcons 8T (remote free) | 37.87 M/s | 7.11 M/s | 30.17 M/s | 36.38 M/s | 901 K/s | 938 K/s | **1.04×** | 730.7 MiB |
| spawn-churn (thread churn) | 17.84 M/s | 2.47 M/s | 18.40 M/s | 7.00 M/s | 1.15 M/s | 1.51 M/s | **0.97×** | 15.1 MiB |
| spawn-empty (calibration) | 35 K/s | 35 K/s | 35 K/s | 34 K/s | 35 K/s | 34 K/s | **0.98×** | 4.9 MiB |
| json-ish 8T | 263.72 M/s | 239.47 M/s | 168.40 M/s | 221.02 M/s | 1.30 M/s | 2.05 M/s | **1.10×** | 6.0 MiB |
| request 8T | 453.87 M/s | 191.60 M/s | 361.60 M/s | 406.64 M/s | 1.24 M/s | 2.15 M/s | **1.12×** | 8.0 MiB |
| ecs 8T (realloc growth) | 92.75 M/s | 74.63 M/s | 1.92 M/s | 388 K/s | 1.05 M/s | 1.54 M/s | **1.24×** | 12.3 MiB |

**19/20 win-or-tie against the best comparator**, and the twentieth
(`spawn-churn`, 0.97×) is a near-tie inside its own run-to-run spread —
see the note on that row below. `spawn-empty` is the pthread calibration
row (allocator-independent by construction: every allocator reads the
same 35 K/s, so the row measures the host's thread-creation rate, not
the allocator).

`spawn-churn` is genuinely bimodal on a loaded host, in *every* build
including the ones before this release: a run either retires its
short-lived thread caches into adoptable slots (~18–20 M ops/s, ~18.5 MiB
peak RSS) or fails to, and the page releases that follow are purged
rather than recycled (~8.8 M ops/s, ~14.8 MiB peak RSS). The 17.84 M/s
above is the median of 10 paired fresh-process samples taken on a quiet
box, where 9 of 10 landed in the fast mode. Do not gate a release on this
row without a quiet host and enough repetitions to separate the modes.

`prodcons 8T` peak RSS is the one number here that moves a lot between
runs (238 MiB to 731 MiB observed): remote frees push the drift cap, and
the cap's shed target is a fraction of a per-thread budget, so how much
virtual the arena holds on to depends on which threads happened to be
remote-freeing. Throughput is stable across that range.

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

| threads | allox | system | mimalloc | snmalloc | vs mimalloc | vs snmalloc | allox peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | **78.6k** | 53.8k | 74.3k | 74.3k | **1.06×** | **1.06×** | 3.1 MiB |
| 4 | **250.8k** | 205.0k | 268.1k | 280.1k | 0.94× | 0.90× | 4.0 MiB |

Median of 3–5 × 2 s, fresh process per sample, `taskset -c 0-7`,
interleaved with the comparators inside each repetition.

**This is the one mode allox does not lead, and it is reported here because
a scoreboard that only contains the flattering mode is not a scoreboard.**
Single-threaded allox now leads both C comparators; the 4-thread row is
still behind, and the deficit is a scaling effect, not a per-operation one
— the same build is 1.06× mimalloc at one thread and 0.94× at four.

What was fixed to get here (paired A/B, two prebuilt binaries, order-
alternating, 3 reps: +5.9% on this workload at both 1 and 4 threads):
the small `alloc`/`free`/`realloc` fast paths were carrying a stack frame
and five callee-saved register pairs that existed only for their slow-path
bodies, and the bounds check on the bin array survived every inlining
decision. See REMAINING_PLAN §4d for the measurements, the disassembly
before/after, and what is left — chiefly that 27% of this workload's
allocations are `realloc` relocations, and eliminating those needs a
per-page free bitmap (a structural change to the small tier, not a
tuning change).

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
- **Frameless fast paths**: the small/medium `alloc`, `free` and `realloc`
  fast paths contain no calls at all, so they compile to a leaf with no
  stack frame and no callee-saved registers. Every slow path (refill,
  medium/big refill, drift-cap probe, exit-hook install, trim) is
  `#[inline(never)]` and reached by a tail jump, and a small allocation
  costs one TLS read, one class-table load and one bin pop. This is not
  cosmetic: it was worth 5–19% across the benchmark matrix, because a
  single `call` anywhere in a hot path forces the register allocator to
  spill every live value around it.
- **Single-visit `realloc`**: a cross-class resize inside the small tier
  resolves both classes once and does the allocate, copy and free under
  one thread-cache visit, instead of redoing the tier dispatch and TLS
  read for each half.
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

v0.1.0 released — working and tested (unit, integration as
`#[global_allocator]`, multi-threaded randomized stress with full integrity
verification, C ABI, zero-size and double-free validation). Green in
debug, release, `telemetry` and `--no-default-features`, warning-free, and
`cargo package` verifies clean. Fastest allocator measured on 19 of the 20
direct-call workloads, and on the process-global application workload
single-threaded; the 4-thread application row is the one place it does
not lead (see above). Not yet audited; API may still change before 0.2.
The Windows table predates the span/arena/exit-flush/fast-path work
(re-verify via CI bench artifacts); macOS results pending CI runs on that
platform.

## Development

```text
cargo test          # full test suite
cargo test --release
cargo bench         # throughput comparison vs system allocator
```

License: MIT OR Apache-2.0
