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
| tight-small 1T (64 B) | 52.52 M/s | 44.77 M/s | 48.00 M/s | 41.54 M/s | 20.48 M/s | 26.47 M/s | **1.09×** | 5.3 MiB |
| mixed-small 1T (16–4096 B) | 38.16 M/s | 7.18 M/s | 14.78 M/s | 12.49 M/s | 6.08 M/s | 10.46 M/s | **2.58×** | 16.1 MiB |
| tight-small 8T (64 B) | 257.18 M/s | 240.73 M/s | 216.89 M/s | 231.07 M/s | 1.62 M/s | 2.11 M/s | **1.07×** | 7.7 MiB |
| mixed-small 8T (16–4096 B) | 196.03 M/s | 37.17 M/s | 40.42 M/s | 57.87 M/s | 707 K/s | 1.85 M/s | **3.39×** | 93.0 MiB |
| mixed-all 1T (16–65536 B) | 17.51 M/s | 2.79 M/s | 5.10 M/s | 543 K/s | 424 K/s | 515 K/s | **3.43×** | 21.4 MiB |
| mixed-all 8T (16–65536 B) | 41.85 M/s | 11.25 M/s | 22.21 M/s | 1.30 M/s | 440 K/s | 1.12 M/s | **1.88×** | 118.4 MiB |
| medium-only 1T (16–64 K) | 19.55 M/s | 2.36 M/s | 5.54 M/s | 276 K/s | 395 K/s | 417 K/s | **3.53×** | 16.9 MiB |
| medium-only 8T (16–64 K) | 47.62 M/s | 11.08 M/s | 24.41 M/s | 618 K/s | 413 K/s | 991 K/s | **1.95×** | 91.0 MiB |
| large-only 1T (32 K–1 M) | 7.36 M/s | 340 K/s | 2.52 M/s | 17 K/s | 497 K/s | 279 K/s | **2.91×** | 7.3 MiB |
| big-tail 1T (64 K–256 K) | 17.79 M/s | 344 K/s | 6.73 M/s | 22 K/s | 387 K/s | 308 K/s | **2.64×** | 6.6 MiB |
| big-upper-tail 1T (256 K–1 M) | 10.98 M/s | 309 K/s | 1.75 M/s | 11 K/s | 516 K/s | 215 K/s | **6.28×** | 5.8 MiB |
| large-only 8T (32 K–1 M) | 31.53 M/s | 1.14 M/s | 25.19 M/s | 141 K/s | 454 K/s | 1.02 M/s | **1.25×** | 34.5 MiB |
| huge-only 1T (5–8 MiB) | 1.82 M/s | 166 K/s | 1.40 M/s | 1 K/s | 439 K/s | 130 K/s | **1.30×** | 5.2 MiB |
| zeroed-small 1T (16–256 B) | 28.90 M/s | 18.63 M/s | 25.80 M/s | 21.38 M/s | 13.42 M/s | 18.40 M/s | **1.12×** | 5.4 MiB |
| zeroed-small 8T (16–256 B) | 100.45 M/s | 111.86 M/s | 124.27 M/s | 148.91 M/s | 1.19 M/s | 1.49 M/s | **0.67×** | 8.6 MiB |
| zeroed-large 1T | 206 K/s | 612.6 /s | 2 K/s | 980.0 /s | 1 K/s | 413.6 /s | **86.63×** | 5.3 MiB |
| prodcons 8T (remote free) | 38.65 M/s | 7.16 M/s | 25.19 M/s | 31.45 M/s | 888 K/s | 1.10 M/s | **1.23×** | 384.5 MiB |
| spawn-churn (thread churn) | 19.29 M/s | 2.96 M/s | 16.13 M/s | 6.89 M/s | 1.01 M/s | 1.59 M/s | **1.20×** | 18.5 MiB |
| spawn-empty (calibration) | 36 K/s | 35 K/s | 35 K/s | 34 K/s | 35 K/s | 34 K/s | **1.02×** | 4.9 MiB |
| json-ish 8T | 258.37 M/s | 238.08 M/s | 167.16 M/s | 219.43 M/s | 1.32 M/s | 2.02 M/s | **1.09×** | 6.0 MiB |
| request 8T | 449.32 M/s | 191.74 M/s | 364.98 M/s | 405.00 M/s | 1.24 M/s | 2.15 M/s | **1.11×** | 8.0 MiB |
| ecs 8T (realloc growth) | 93.03 M/s | 74.16 M/s | 2.07 M/s | 380 K/s | 1.05 M/s | 1.50 M/s | **1.25×** | 12.3 MiB |

**21/22 win-or-tie against the best comparator.** `spawn-empty` is the
pthread calibration row (allocator-independent by construction: every
allocator reads the same 35 K/s, so the row measures the host's
thread-creation rate, not the allocator) and `spawn-churn` is a near-tie
inside its own run-to-run spread — see the note on that row below.

**The one real loss, `zeroed-small 8T` at 0.67×, is a row added on
2026-09-26 and it is the honest result of adding it.** 8-thread `calloc`
churn sized to recycle, so every allocation takes the software-zeroing
path. Adding it immediately exposed what it was built to find: the
`zeroed_calls`/`zeroed_bytes` counters were two shared `lock xadd`s per
`calloc` call, and moving them into the thread cache's batched telemetry
took this row from 30.0 M/s to ~100 M/s — a 3.4× improvement that is still
not enough: snmalloc reads ~149 M/s and mimalloc ~124 M/s. Single-threaded
(`zeroed-small 1T`) allox leads at 1.12×.

**Three explanations for the remainder were tested and all three are dead
ends**, which is worth stating because the obvious one is not the answer:
the per-operation `cached_bytes` accounting (ablated: −0.8% to +3.0%, i.e.
noise — the byte counter is an independent accumulator off the critical
dependency chain so it never stalls the core); the `memset` call itself
(replaced with an inline 16-byte store loop, the way mimalloc inlines its
small clear: **−12.6%**, because glibc's `memset` is AVX2/ERMS-vectorised
and beats a scalar loop even at 16–256 B); and the relocation rate (parity,
see the application-shape section). The one hypothesis left untested is the
**virginity rate** — allox's `SmallBin::virgin` is a per-class count, so
once a class is warm every subsequent `calloc` memsets, and a design that
satisfies more of them from never-used memory does strictly less zeroing
work. That is a policy question, not a tuning knob.

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
are always on: purges (one per `madvise`), exit flushes, and retired/adopted
caches (one per thread). `telemetry::timing()` adds nanosecond lock-wait,
purge and exit-flush totals.

`zeroed_calls` and `zeroed_bytes` used to be in the always-on set, on the
reasoning that a software-zeroing memset is rare. It is not: it is per
*`calloc` call* for any code that zeroes recycled memory, so those two were
two shared `lock xadd`s per call. They moved into the batched telemetry on
2026-09-26, which is **+252% on `zeroed-small 8T`** — eight threads doing
~26M atomic pairs per second onto one cache line. They therefore now read 0
unless you build with `--features telemetry`, like the other per-event
counters; their positions in `VOLUME_FIELDS` are unchanged.

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
  spill every live value around it. glibc converged on the identical change
  independently in 2025 (splitting `__libc_malloc` into a frameless tcache
  fast path plus an `__attribute_noinline` tail-called slow path, reporting
  "significant performance gains since `__libc_malloc` doesn't need to setup
  a frame"), which is a good sign that this is the shape and not a local
  artifact. The thread-local accessor is already the optimal one on stable
  Rust: the release binary compiles it to a bare `mov %fs:0x0` with no
  `LocalKey::with` call and no initialization guard.
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
`cargo package` verifies clean. Fastest allocator measured on 21 of the 22
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
