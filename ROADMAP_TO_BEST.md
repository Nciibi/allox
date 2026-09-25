# Roadmap: pure-Rust, zero-C, still beats everyone

Goal: **library stays pure-Rust / zero-deps / no C toolchain, but beats C
allocators (mimalloc, jemalloc, snMalloc, system) on throughput.**
Benchmarks/tests alone may use C crates in `dev-dependencies` — that never
infects the library itself.

Constraints (assumed, plain language):

* `src/` keeps zero deps, no build script, `no_std` + `wasm32` keep working.
* Only `benches/`, `tests/`, `fuzz/` may add C crates for fair comparison.
* No breaking `Allox` / `malloc` / `free` / C ABI change unless explicitly noted.

## Why we lost historically (Linux evidence, Ryzen 5 1600 — mostly fixed)

* `mixed-all 8T (16-65536B)`: **now 12.1M vs system 14.1M (0.86x),
  vs mimalloc 18.6M (0.65x)** — per-op gap remains (REMAINING_PLAN §6);
  hit-rate/syscall gap closed by spans + arena (was 0.04x system).
* `large-only 8T (32K-256K)`: **now 9.06M vs mimalloc 13.5M (0.67x)**
  after big spans (was 0.05x); unmaps 0 in probe. The 1 MiB big-cap
  extension keeps this guard in-band and improves the 1T tail; >1 MiB
  remains large-path by design.
* `ecs 8T` **was** the last loss (≈0.06× system) because glibc grows via
  `mremap` (zero-copy) while allox realloc copied every step of a doubling
  chain. **CLOSED 2026-09-25**: the first cross-class growth now promotes
  the block once into a slack reserve and the rest of the chain grows in
  place — 9.59 → 92.09 M/s (9.6×) in a fresh 2 s × 3 A/B, peak RSS
  19.7 → 12.2 MiB, and 1.20× system in a 2 s × 5 confirmation. The
  earlier legacy-only `mremap` trial was flat because arena-backed traffic
  never hit that path; it was reverted and is still unnecessary.
* Historical bullets that motivated the original plan (all fixed by
  P0–P1): single global LARGE_CACHE spinlock + O(N) best-fit; per-page
  mmap+trim tax; 4214-page dead-thread leak; 0.04× mixed-all.
* `stress` dead-thread page accumulation: fixed by exit-flush (P1e) —
  `tests/thread_exit.rs` now asserts convergence.
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
  **DONE 2026-09-23** — pressure-gated owner check + `foreign_bytes`
  batched shed via `trim` (per-free `release_blocks` regressed prodcons
  to 0.60× and thrashed the arena; batched shed: prodcons 8T 32.7M,
  1.20× mimalloc, abnd 0/s). Owner fields on PageHeader/SpanMaster/BigMaster;
  correctness never reads `owner`.
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

* Fix doc drift: budget 32 MiB (`src/cache.rs:32`) vs 64 MiB in `DESIGN.md`,
  MSRV 1.70 vs 1.79 in `Cargo.toml:5`. **DONE 2026-09-23** — DESIGN.md
  reconciled (MSRV, budget, wasm status, medium/big tiers, module layout),
  CHANGELOG remote-free drift-cap claim corrected.
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
   P1d large cold + exact-fit → DONE (see results).
   P1e exit-flush + cold-array bugfix + small cold → DONE (see below).
   spans-for-big-sizes (DESIGN_SPANS_BIG) → **DONE 2026-09-23**: big
   tier `(65472, 262144]` via `BigMaster` + arena `BIG_MAP` + `BigHeap`;
   large-only 8T 0.05× → **0.67× mimalloc** (9.06M vs 13.51M), unmaps 0.
   BIG-CAP PHASE 2 → **DONE 2026-09-24**: top class raised to 524288;
   large-only 1T improved 152K → 193K ops/s in capped A/B, with mixed-all
   and small guards flat.
   BIG-CAP PHASE 3 → **DONE 2026-09-25**: top class raised to 1 MiB;
   boundary, telemetry, release, telemetry-enabled, and no_std checks pass.
   A separate 2x medium/big cache allowance and `BENCH_SAFE_LIVE=1` matrix
   measured `large-only 1T` at 2.88× mimalloc, `large-only 8T` at 1.32×,
   and `mixed-all 1T` at 2.90× in fresh 2 s × 3 runs.
   P1h growable extents → **DONE 2026-09-25** (see results below): first
   cross-class big growth promotes once into a slack reserve, the rest of
   the chain grows in place; `ecs 8T` 9.6× and the last structural loss
   (0.06× system) becomes 1.2×. NEXT: P2 futex/parking mutex (unix spin
   convoy hypothesis) + refill tuning, or mixed-all per-op
   (REMAINING_PLAN §6).

## P2 results — parking mutex on hosted unix (pthread, lazy init)

What shipped: `src/sys/unix.rs` gained a pthread-mutex `RawMutex`
(128 B opaque storage, one-time init under a guard spin, default attrs —
no asm, no deps, allocation-free by POSIX); `src/sys/mod.rs` gates are now
windows → SRWLock, unix+std → pthread, everything else → spin. Plus a
`mutex_survives_contention` unit test and a DESIGN §4.4 doc update.

Measured (Linux Ryzen 5 1600): mixed-all 8T flat (13.55M → 13.7M),
spawn-churn flat (2.86M → 2.72M), large-only 8T noisy (0.5–1.3M).
Hypothesis NOT confirmed — sharding + thread caches already keep these
locks uncontended, where pthread ≈ spin (one CAS either way). Kept anyway:
strictly more robust under preemption/oversubscription (spin convoys are
real, just not the binding constraint here), zero regressions, full suite
green (12 binaries), all feature combos warning-free. The remaining MT gaps
(then: spawn-churn 0.22x isolated, large-only 1T tail >1 MiB; now closed —
1.22x and 2.87x mimalloc in the refreshed matrix) are per-op/syscall
volume and class-cap edges, not lock parking — the 1 MiB big-cap phase
addressed the 1T tail slice, P1f the spawn-churn one.
4. P1 exit-flush + drift cap → DONE (exit-flush with P1e; drift cap
   2026-09-23, batched shed — prodcons 8T 1.20× mimalloc). P1f deferred
   retirement + direct adoption, P1g hot-path table/large cache, P1h
   growable extents: all DONE 2026-09-25.
5. P2 lock + tuning sweep → Linux matrix done (20/20 win-or-tie);
   Windows/macOS rows pending CI artifacts.
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

## P1d results — large cold tier + exact-fit-first (1 s/1 rep probes)

What shipped (`src/lib.rs`, `src/cache.rs`): large shards gained a cold
tier (discard physical via `sys::discard`, retain virtual; 64 MB/shard on
64-bit) with post-lock discard, mirroring span cold. Both shard (`take_fit`)
and thread-stash (`take_large_stash`) selection went exact-fit-first:
under size variance, best-fit eats big regions for small requests and
starves future big requests; exact-first preserves per-size reuse pools.

* `large-only 8T`: 455k → 1.31M (+2.7x), now beats talc/system (3.5x).
  Still 0.11x mimalloc (12M) — remaining gap is syscall volume (41k
  maps/s × over-map trim = ~160k VMA ops/s kernel-serialized across
  threads), not cache depth. Next levers: arena mapping (1 VMA op/miss
  instead of 3–4) or spans for big sizes (no syscalls at all).
* `large-only 1T`: flat at 150k (miss-rate bound under 32K–1M uniform
  variance; exact pools need more depth per size than 136 slots hold).
* `mixed-all 1T` tail effect: large maps 35k/s → 14/s (exact-fit keeps the
  64K-tail pool clean). Total maps 44k → 17.5k.
* No regressions: `mixed-small 8T` 141M, `tight-small 1T` 49M,
  `mixed-all 1T` 1.56M. Full suite green (incl. 33 s randomized churn).

Net: 6.5/10 workloads win-or-tie vs the BEST comparator (was: best
pure-Rust on small only). 1 s single samples understate steady state by
~2.7x on mixed-all (warmup); use ≥2 s × 3 reps for tuning decisions.

## P1e results — exit-flush + cold-array bugfix + small cold

What shipped: `src/thread_exit.rs` (pthread_key/FlsAlloc hook, slow-path
arming via `exit_armed`, blocking `flush_all` in the destructor — try-only
was measured to abandon ~everything under concurrent exits), `heap.rs`
`release_inner`/`mrelease_inner` cores, `cache.rs` flush regroup (blocking
only; try machinery deleted), `__debug_map_split` now 4-way, `spawn-empty`
bench workload (pure spawn calibration: ~25k threads/s for ALL allocators).

Two real bugs found along the way (both caught by the new tests):
1. Span cold list stored intrusive links INSIDE discarded spans — madvise
   zeroes them, destroying the list (silent virtual leak + remap churn in
   release; `debug_assert!(count > 0)` fire in debug). Fixed with
   array-stored (base, npages) like large cold. Lesson: never store
   metadata inside discardable ranges.
2. Test-metric design: mapped_pages can't distinguish leaks from
   intentional cold/empty retention. `tests/thread_exit.rs` now asserts
   cross-generation convergence (flat) instead of absolute counts, with a
   verified-sensitive threshold (hook disabled → +1050 mappings/gen linear).

* Exit-flush generations test: +1050 mappings/gen without hook vs
  187→203→223→232→228 (flat) with hook.
* `spawn-churn`: 1.04M → **2.86M** (small cold killed the 17k/s unmap storm;
  probe now shows 1 map, 0 unmaps), RSS GBs → 44 MB. Beats talc (1.46x),
  0.8x system, 0.22x mimalloc. Remaining gap decomposed via `spawn-empty`:
  spawn itself is 40µs for everyone; the rest is per-op + exit work needing
  P2 lock/parking work (unix spin-convoy hypothesis for the MT remainder).
* `mixed-all 8T`: 10.6M → 13.55M (0.71x mimalloc, beats system 12.4M).
* No regressions: `tight-small 1T` 44.6M, `mixed-small 1T` 28.9M.
  Full suite green (11 binaries incl. new `thread_exit`).

## P1f results — deferred thread-exit retirement (2026-09-25)

What shipped: `src/thread_exit.rs` now retires armed TLS caches into a bounded
fixed-slot queue instead of flushing every cache on the exiting thread.
`src/cache.rs` lets an empty worker adopt one queued cache directly, with
post-adoption small/medium/big bin checks; oversized or overflow caches use the
synchronous path. Small-page exit flushing also passes known tails, batches
same-class releases, and lazily reinitializes fully-free pages.

* `spawn-churn` (2 s × 5, `BENCH_SAFE_LIVE=1`, 2 GiB cgroup): **18.38M vs
  13.78M mimalloc** (1.33x), up from 2.61M vs 13.74M at the isolated baseline;
  peak RSS 18.5 MiB vs 19.4 MiB.
* `spawn-empty` remains allocator-independent; `mixed-all 8T`, `prodcons 8T`,
  `medium-only 8T`, and `large-only 8T` remain wins in capped checks. Full
  debug integration, telemetry, release, and thread-exit convergence tests pass.

The remaining lifecycle work is first-touch cost for workers that cannot adopt
a cache; the next experiment should target that, not guess another refill/flush
constant.

## P1g results — hot-path table and large-cache follow-up (2026-09-25)

* A runtime static class-size table removes a compiler-materialized 512-byte
  table copy from every small free. Fresh `tight-small 8T` is now 168.9 M/s
  versus 162.6 M/s for mimalloc.
* Zeroed-large recycled regions are discarded while exclusively owned before
  reuse, avoiding full memsets; adaptive large stash/shard caps and range-based
  side-table writes improve the 5–8 MiB path.
* `zeroed-large 1T` reaches 188.7 K/s and `huge-only 1T` 1.55 M/s in capped
  probes. ECS was still the known structural big-span realloc/copy gap —
  closed by P1h below.

## P1h results — growable extents for packed big blocks (2026-09-25)

What shipped: `try_promote_big_grow` in `src/lib.rs` (arena targets). Big
blocks are carved contiguously out of a shared span, so a cross-class
`realloc` growth cannot extend in place — the neighbours are live data —
and the generic path allocated + copied at every step. The first
cross-class growth now relocates **once** into a large region that reserves
slack (8x the new size, capped at the big cap, virtual-only: untouched
anonymous pages, so RSS still tracks live demand). Every later growth in
the chain fits the existing mapping and is served in place by the existing
`try_grow_large_frontier`, with no copy, no syscall and no span traffic.
Both `GlobalAlloc::realloc` and the free function use it; an already-large
block is excluded by a side-table check so a stale big entry can never
misroute it, and every failure falls back to the ordinary alloc-copy-free
path. `tests/big_growth_promote.rs` pins the contract (content preserved
at every step, in-place growth is sticky, both entry points, shrink
round-trip).

* `ecs 8T` (fresh 2 s × 3 processes, allox-only A/B vs `v0.0.1255`):
  **9.59 → 92.09 M/s (9.6×)**, peak RSS 19.7 → 12.2 MiB. Against the
  comparators: 1.23× system, 50× mimalloc (2 s × 5 confirmation:
  91.45 vs 76.27 M/s = 1.20×).
* Full 20-workload A/B against the pre-promotion build: every other
  workload flat inside run-to-run spread (`mixed-all 8T` 46.0 → 44.9,
  `large-only 8T` 31.7 → 31.9, `prodcons 8T` 40.5 → 38.4 both bimodal,
  `json-ish 8T` 239.6 → 246.3, `spawn-churn` bimodal in both builds).
* Full matrix is now **20/20 win-or-tie vs the best comparator** (README
  table refreshed from that run). Full suite green: debug, release,
  telemetry, no_std.

## Phase 0 results — `map_any` + fault-safe dispatch (1 s/1 rep probes)

What shipped: `sys::map_any` (plain 4 KiB `mmap`, unix-only; alias to `map`
elsewhere) used by `alloc_large_ex` — large regions are offset-header
located, never masked, so 64 KiB alignment bought nothing. Required a
dispatch overhaul when stress segfaulted: unaligned bases let masked
pre-checks round *outside* the region into unmapped memory. Fix:
`dealloc_impl`/`usable_size`/free-`realloc` check the large-offset header
first (always in-bounds for live pointers) with range validation
(`large_header_of`: magic + nonzero 64 KiB-multiple size + header strictly
below `p` + `p` in range), and `GlobalAlloc::{dealloc,realloc}` route by
contract layout via `dealloc_with_layout` (zero probing reads, debug
verified). Bonus: layout routing skips ~4 loads + branches per free.

* `large-only 1T`: 150k → 210k (+40%), vs system 0.84x (was 0.6x).
* `mixed-all 1T`: 1.56M → 2.24M (+44%).
* `tight-small 1T`: flat at 43.7M. Full suite (10 binaries) + telemetry +
  no_std + release green. Next: Phase 1 arena (reservation + MAP_FIXED
  commits + hole lists), which additionally kills span/page trim and buys
  locality.
* Recheck (post-Phase-0 review): free-function `realloc` probed spans
  unconditionally — reachable with live unaligned large pointers, same
  fault class. Gated on `!old_large_ok`. Also flagged (pre-existing, out
  of scope): `GlobalAlloc::realloc` same-class identity with
  `layout.size() == 0` can return the dangling pointer for nonzero
  `new_size`; needs its own fix + test.

## Phase 1 results — arena core + LARGE wiring (1 s probes unless noted)

What shipped: `src/arena.rs` (new, unix+`std` only) — 16 GiB (64-bit) /
512 MiB (32-bit) `PROT_NONE` reservation, lazy once-per-process init with
aligned-trim, lock-free bump CAS, value-array hole stacks (1024 slots,
best-fit + split, 4 GiB/256 MiB byte caps), `MAP_FIXED` commits with
`ret == base` verification, discard-before-park release ordering, universal
legacy fallback (null = unavailable, never OOM), `ARENA_COMMITS/REUSES/
ABANDONED` hidden counters. `src/lib.rs`: `map_large_region` (arena then
legacy) and `unmap_or_return` (arena holes vs true munmap) with
exactly-once fresh-map accounting; `__debug_map_split` now 6-way with
arena reuses in the probe line. Five arena unit tests (alignment+zeroing,
exhaustion fallback, hole reuse counting, split-remainder reuse,
no-clobber canary) on isolated instances — including one failure that
corrected the test, not the code (holes don't coalesce by design).

* `large-only 1T`: 150k → 230k (+60% over pre-arena; map_any alone gave
  +40%), ties system (0.96x), 21k arena reuses/probe.
* `large-only 8T`: 455k → 1.17M (beats system 1.26x, 2.75x talc).
  Still 0.07–0.09x mimalloc — remaining gap is shard hit rate under
  32K–256K uniform variance (exact pools need more depth), not mmap cost.
* `mixed-all 1T`: 1.83M, probe essentially syscall-free (16 maps).
* Full suite green (11 binaries incl. 5 arena tests) + telemetry + no_std
  + release, all warning-free. Next, one at a time: spans, then pages.
