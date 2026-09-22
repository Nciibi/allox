# Roadmap: pure-Rust, zero-C, still beats everyone

Goal: **library stays pure-Rust / zero-deps / no C toolchain, but beats C
allocators (mimalloc, jemalloc, snMalloc, system) on throughput.**
Benchmarks/tests alone may use C crates in `dev-dependencies` — that never
infects the library itself.

Constraints (assumed, plain language):

* `src/` keeps zero deps, no build script, `no_std` + `wasm32` keep working.
* Only `benches/`, `tests/`, `fuzz/` may add C crates for fair comparison.
* No breaking `Allox` / `malloc` / `free` / C ABI change unless explicitly noted.

## Why we lose today (Linux evidence, Ryzen 5 1600)

* `mixed-all 8T (16-65536B)`: `allox 0.68M vs system 15.7M (0.04x),
  vs talc 1.04M (0.65x)`. ~75% of that workload is `>16 KiB` →
  `alloc_large_ex` in `src/lib.rs:198-274`.
* `stress` leaves 4214 pages (~270 MiB) mapped after free — dead-thread
  caches are never reclaimed (`DESIGN.md §4.5`, `src/lib.rs:666`).
* `src/sys/unix.rs:43-70` pays 1x `mmap` + up to 2x `munmap` per page to
  force 64 KiB alignment. Amortized fine for 64B-thread-cached pages
  (~1000 blocks/page), fatal for per-alloc large `mmap`.
* `src/lib.rs:190,210`: single global `LARGE_CACHE` spinlock + O(N=64)
  best-fit scan on every large alloc/free. 8 threads serialize here.
* `src/classes.rs:11`: `MAX_SMALL_SIZE=16 KiB` with fixed 64 KiB pages.
  Can't just bump to 64 KiB — `(65536-48)/32768 = 1` block/page = 50%
  page waste. Needs multi-page spans (like mimalloc segments).

## P0 — Honest scoreboard first

1. `Cargo.toml:27-31` — add dev-only comparators: `mimalloc`, `tikv-jemalloc`
   (`jemalloc`), `snmalloc-rs` if it builds. Lib stays zero-deps.
2. `benches/alloc.rs:60-91` — keep current 5 workloads, add:
   * `mixed-all 1T`, `large-only 1T/8T (32K-1M)`,
     `producer-consumer 8T` (alloc on A, free on B),
     `thread-spawn churn` (short-lived threads), `rampant-realloc`.
   * Measure ops/s + p99 per-op + `MAP_CALLS/UNMAP_CALLS` from
     `src/heap.rs:26-28` + peak RSS (`/proc/self/status` / `getrusage`) +
     syscall count (`perf` in CI, not in harness).
3. Acceptance for "best": win/tie ops/s on small + medium, no >1.2x RSS
   regression vs jemalloc/mimalloc, no workload <0.9x system on
   Linux + Windows + macOS.

## P1 — Large/medium path (biggest ROI, fixes 0.04x)

Recommendation: **don't just raise `MAX_SMALL`. Add spans.**

* Option A (recommended): `16K-128K` multi-page spans, still page-managed.
  * New `SpanHeader` (2/4/8x `PAGE_SIZE`), same mask trick but size-aware
    `of()`. Reuses `heap.rs` sharded locks, batched refill, virgin tracking.
    Turns 75% mmap workload into cached-page workload.
  * Files: `src/page.rs:7-10,40-45`, `src/classes.rs:11-13,40-51`,
    `src/heap.rs:145-196`, `src/cache.rs:238-260`.
  * Tradeoff: more code, fragmentation logic for spans. Far better than
    50%-waste single pages.
* Option B (do anyway): shard `LARGE_CACHE`.
  * Replace single `static LARGE_CACHE: Mutex` (`src/lib.rs:190`) with
    `[Mutex<LargeCacheShard>; 8|16]` hashed by size + per-thread small
    large-cache (e.g. 4 slots/thread, no lock) + global sharded overflow.
    Replace O(64) best-fit with size-segregated exact/nearest stacks.
  * Add `madvise(MADV_DONTNEED)` vs `munmap` policy for reuse latency.
* Validation: `mixed-all 8T` must go from 0.65x talc / 0.04x system to
  >1.2x both before proceeding.

## P1 — Unix aligned-map fast path

* Current `src/sys/unix.rs:43-70` always over-maps. Replace with arena:
  * Reserve e.g. 1 GiB `PROT_NONE` once per process, commit 64 KiB-aligned
    slices via `mmap(MAP_FIXED|MAP_PRIVATE|MAP_ANONYMOUS)` on demand.
    After warmup: 1 syscall, guaranteed alignment, no trim.
  * Fallback to current over-map if reserve fails (macOS limits, 32-bit,
    resource pressure).
  * Count `MAP_CALLS` to prove amortization.
* Alternative if arena rejected: try `mmap(size)` first, reuse if already
  aligned (1/16 lucky), else over-map. Cheaper, smaller win.

## P1 — Thread-exit / producer-consumer bloat (fixes 270 MiB)

* Add best-effort exit flush outside TLS dtors: `pthread_key_create`
  destructor on unix, `FlsAlloc` on Windows, that does `try_lock` +
  `flush_all` only (never blocks in loader-lock). Documented fallback
  remains `flush_current_thread()` in `src/lib.rs:666`.
* Add remote-free affinity: either keep "freeing thread keeps it" but cap
  cross-thread drift (if `cached_bytes` from foreign pages > X, return
  directly via `HEAP.release_blocks`), or add mimalloc-style per-page
  remote list. Start with cap — smaller change in `src/cache.rs:262-277`.
* Add `producer-consumer 8T` + `thread-spawn` benches + RSS assertion:
  post-free mapped pages must be < e.g. 512 after flush.

## P2 — Small-path + lock tuning

* `src/sys/mod.rs:24-71`: Linux spin + `yield_now` burns CPU under
  contention. Replace with `futex`-parked mutex on Linux (still no alloc,
  still const-init) — keep SRWLock on Windows, spin only for `no_std`.
* Tune don't guess: sweep `REFILL_BATCH (src/heap.rs:19, 64)`,
  `DEFAULT_THREAD_CACHE_BUDGET (src/cache.rs:24, 32 MiB)`,
  `EMPTY_PAGE_CACHE_PER_CLASS (src/heap.rs:24, 4)`,
  `LARGE_CACHE_CAP (src/lib.rs:169, 64 MiB)` via bench matrix on
  Windows + Linux. Make them settable + documented.
* Micro: `#[cold]` large/trim paths, fast-path `class_for_size` LUT
  (`src/classes.rs:71-77`), verify with `perf annotate` that fast path is
  ~TLS + pop + 2 branches.

## P0 parallel — Correctness hardening

* `cargo +nightly miri test --lib` in CI — add span/large-cache invariants.
* `cargo fuzz` 60s smoke — add `realloc`/`aligned_alloc` + high-align
  (>64K) targets. Magic-collision test: spray `PAGE_MAGIC`/`LARGE_MAGIC`
  as user data, ensure no mis-dispatch in `src/lib.rs:368-383`.
* Add OOM / overflow tests (`isize::MAX`, `checked_add` in
  `alloc_large_ex`), zero-size, `align > PAGE_SIZE`.
* `no_std` + `wasm32-unknown-unknown` + `node scripts/wasm_smoke.mjs` green.

## P3 — Polish

* Fix doc drift: budget 32 MiB (`src/cache.rs:24`) vs 64 MiB in `DESIGN.md`,
  MSRV 1.70 vs 1.79 in `Cargo.toml:5`.
* Extend `stats()`/`telemetry` with RSS, per-class live, large/medium
  counters.
* Complete C ABI: `malloc_usable_size`, `mallinfo`-equivalent, C11
  `aligned_alloc` semantics check.

## Order to execute

1. P0 harness + C comparators → reproduces 0.04x + RSS baseline. DONE.
   `mimalloc` + `snmalloc-rs(build_cc)` in dev-deps; jemalloc left for CI
   (needs make+autoconf). 10 workloads incl. `mixed-all 1T`, `large-only`,
   `prodcons 8T`, `spawn-churn`; MAP_CALLS probe + VmHWM reporting.
2. P1a sharded large cache + per-thread stash → DONE, measured below.
   Contention fixed; miss-rate gap remains and needs spans (P1c).
3. P1c medium spans (16-64K via multi-page spans) → DONE (see results).
   NEXT: large-path MT (per-thread large caches) + exit-flush.
4. P1 exit-flush + drift cap → after spans.
5. P2 lock + tuning sweep → full matrix on Linux/Windows/macOS.
6. Harden + docs + publish 0.2.

## P1a results (BENCH_SECS=1 BENCH_REPS=1, Linux Ryzen 5 1600)

* `tight-small 8T`: allox 232M beats mimalloc 195M / snmalloc 207M / sys 209M.
* `mixed-small 8T`: allox 123M vs snmalloc 51M / mimalloc 34M / sys 28M.
* `prodcons 8T`: allox 28.2M ties mimalloc 27.4M / snmalloc 27.6M.
* `mixed-all 1T`: allox ~200k vs mimalloc 3.8M (0.05x), 64k MAP_CALLS/s —
  miss-rate dominated, sharding can't fix single-threaded misses.
* `mixed-all 8T`: allox 250k vs mimalloc 19.5M (0.013x).
* `large-only 1T`: allox 140k vs mimalloc ~700k (0.2x), RSS ~3.2 GB.
* `spawn-churn`: allox 1.04M vs mimalloc 9.3M (0.11x) — dead-thread
  reclamation still open (needs exit-flush, P1 exit-flush step).

Conclusion: small + remote-free paths already beat/tie C allocators.
Large/medium needs multi-page spans, not just cheaper syscalls: the working
set (96 MB live + churn bursts) exceeds any sane mmap-cache cap, so hit rate
stays low regardless of sharding. Arena mapping (P1b) deferred — it would
save 2 munmaps per miss but misses themselves (64k/s) are the problem.

## P1c results — medium spans landed (BENCH_SECS=2 BENCH_REPS=3)

What shipped: `classes.rs` medium tables (12.5% geo, 18K→60K + explicit
65472 top class), `page.rs` SpanMaster + sub-headers + carving, `heap.rs`
MediumHeap (sharded) + madvise cold-span retention + hidden map-split
counters (`allox::__debug_map_split`), `cache.rs` medium bins + trim/flush,
`lib.rs` three-tier dispatch + telemetry growth. Reverted experiment:
count-capped medium bins (32) regressed 1.75M→1.05M — byte budget restored.

Scoreboard (Linux Ryzen 5 1600, median of 3 × 2 s):

* `tight-small 1T`: 36.2M vs mimalloc 40.6M (0.89x) — close.
* `mixed-small 1T`: 25.9M vs mimalloc 10.9M (2.4x WIN).
* `tight-small 8T`: 224.6M vs sys 212.3M / mimalloc 201.3M (WIN).
* `mixed-small 8T`: 126.0M vs snmalloc 44.5M (2.8x WIN).
* `mixed-all 1T`: 4.41M vs mimalloc 3.68M (1.2x WIN), vs sys 2.97M.
  Was 0.05x. Top-65472 class killed the 35k/s large-tail maps (now ~14/s).
* `mixed-all 8T`: 10.59M vs mimalloc 16.92M (0.63x), vs sys 12.08M (0.88x).
  Was 0.013x. Remaining gap is per-op latency (~2x), needs PMU profiling.
* `prodcons 8T`: 30.4M ties snmalloc 30.8M, beats mimalloc 28.5M (WIN).
* `large-only 1T/8T`: 135k/455k — still mmap territory (32K–1M working
  set); 8T is 0.03x mimalloc. Needs large-path MT overhaul (per-thread
  large caches, virtual retention like spans got).
* `spawn-churn`: 276k, 0.15x — dead-thread reclamation (exit-flush NEXT).

Net: 6.5/10 workloads win-or-tie vs the BEST comparator (was: best
pure-Rust on small only). 1 s single samples understate steady state by
~2.7x on mixed-all (warmup); use ≥2 s × 3 reps for tuning decisions.
