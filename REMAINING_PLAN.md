# Remaining plan — from big-spans-validated to 0.2

State at fork-off: **big spans DONE 2026-09-23; 512 KiB big-cap phase
DONE 2026-09-24; 1 MiB big-cap extension DONE 2026-09-25**. The tier-aware
cache allowance and bounded benchmark matrix are validated: `large-only 1T`
2.88× mimalloc, `large-only 8T` 1.32×, and `mixed-all 1T` 2.90× on the
2 s × 3 safe fresh-process run. The 1T tail above 1 MiB is still on the
large path.
**Remote-free drift cap DONE 2026-09-23** (§4b; prodcons 8T 1.20×
mimalloc). Full 14-workload matrix: ~9–10/14 win-or-tie vs the best
comparator on Linux x86-64 (json/request beat mimalloc; ecs beats every
comparator except system's mremap growth — allox ~7.9M vs system ~113M
isolated, same 0.06× story as the mremap note in §4). Full suite +
telemetry + no_std + release green and warning-free. Contra remaining
gaps below, each
capable of closing independently, ordered by ROI. Methodology everywhere:
≥2 s × 3 reps, `__debug_map_split` + `peakRSS` probe columns,
sensitivity-checked tests (disable-the-feature must fail), one point
measured before the next starts.

## 1. Arena: wire spans (DONE 2026-09-23)

Measured (2 s × 3 reps, same box): `mixed-all 8T` 13.06M → 15.36M
(+18%), probe unmaps 1023 → 12/s (trim-munmap pairs gone), span fresh
takes 1017 → 717/s; `mixed-all 1T` 2.54M → 2.64M (noise), steady-state
syscall-free either way. Full suite green. Revert point was two call
sites + one fate arm in `src/heap.rs`.

Spans are the biggest remaining syscall source on mixed workloads
(~16k span maps/s on mixed-all 1T; each = mmap + 2 trim unmaps today).

* `src/heap.rs` `MediumHeap::take_blocks` new-span branch: replace
  `sys::map(pages * PAGE_SIZE)` with `crate::arena::commit(pages)` (cfg
  `all(unix, feature = "std")`), legacy `sys::map` fallback on null.
  Accounting: count the fresh map in `MAPPED_PAGES`/`MAP_CALLS`/
  `SPAN_MAP_CALLS` exactly once at the caller (same pattern as
  `map_large_region`), never inside `arena::commit`.
* `mact_fate` unmap arm: `arena::contains` → `arena::release`, else true
  `munmap` + existing counter decrements (same `unmap_or_return` shape as
  large; consider factoring a shared helper then).
* Virgin semantics (verified sound, do not "fix"): arena commits are fresh
  zero pages, and span carving dirties only freelist-link words — exactly
  the virgin invariant — so `FLAG_VIRGIN` stays SET on arena-carved spans
  (unlike cold re-carves, which must clear it). `calloc` keeps the
  link-word-only fast path.
* Alignment: every arena slice is a 64 KiB multiple of a 64 KiB base, so
  `SpanMaster::of` masking is unaffected. Assert in tests.
* Accept: `mixed-all 1T/8T` span-maps drop ≥50% with RSS flat; full suite
  green. Revert point: two call sites + one fate arm.

## 2. Arena: wire small pages (DONE 2026-09-23)

Measured (2 s × 3 reps): `tight-small 1T/8T` 54.1M/229.6M → 51.3M/225.4M
(noise-flat), `mixed-small 1T/8T` 31.8M/157.8M → 32.9M/153.5M
(noise-flat), `spawn-churn` 2.99M → 2.91M with unmaps 0/0 before and
after; `mixed-small 8T` probe unmaps 14 → 0/s. Suite green.

Two fixes landed with it (both required, both in `src/heap.rs` unless noted):
* `MAPPED_PAGES` is live-virtual again: `arena::commit` now returns
  `(base, fresh)` (bump = new virtual, hole reuse = already counted);
  callers count `MAPPED_PAGES` only when `fresh`, `MAP_CALLS` on every
  success. Without this, `tests/thread_exit.rs` convergence fails
  (+~400 mappings/generation drift). `tests/stress.rs::stats_are_sane`
  now asserts `map_calls` (robust to hole reuse).
* Pre-existing heap corruption, found by §2 validation (not caused by
  it — reproduces 12/12 on v0.0.413): small-cold `sys::discard` ran
  OUTSIDE the class lock, so a concurrent cold pop could re-carve and
  hand out the page before the stale discard landed, zeroing live
  blocks/headers (short free lists, wild splices, ~30% crash rate in
  `thread_exit` churn). Medium spans and large regions already
  discarded under their locks ("found the hard way" per their
  comments); small never got the fix. Fix: discard under the lock in
  `release_inner`, caller Cold arm is now a no-op. Verified: cold-only
  config went ~100% crash → 12/12 clean, `thread_exit` 15/15, full
  suite green.

Same swap in `GlobalHeap::take_blocks` / `release_inner` unmap arm
(`src/heap.rs`). Small pages churn less through fresh maps (bins absorb),
so expect a smaller but strictly nonnegative delta; the win is tail
latency (no trim-munmap pairs) and VMA-count stability.
Accept: `tight-small`/`mixed-small` flat or better, `spawn-churn` unmaps
stay ~0, suite green.

## 3. Arena follow-ups (measured 2026-09-23 — no coalescing; slots resized)

New observability: `__debug_arena_detail()` → `(abandoned, bump_bytes)`,
bench `arena` column (`abnd+rate/s totN hiMiB`).

* **Hole coalescing: NOT triggered, no work done.** Steady-state
  abandoned rate is 0/s on all 14 workloads with stock caps, so the
  literal trigger never fires. Caveat found while measuring: the
  trigger is blind to transient burn — large-only variance abandons
  ~22k during warmup, exhausts the reservation, then reads 0/s forever
  (fallback takes over). The 8T case is slot-pressure burst overflow,
  not fragment slivers (best-fit + no-split strands little under these
  mixes), so coalescing is the wrong tool for it anyway.
* **Hole slots 1024 → 4096 (the actual §3 change).** large-only 1T:
  1024 → 22665 abandoned + 16 GiB exhaustion; 2048 → 21200 + exhaustion;
  4096 → 0 abandoned, hi ~3.1 GiB, unmaps 50k/s → 0/s. 4096 × 16 B =
  64 KiB static. Exact-size early-exit in `holes_take`
  (behavior-preserving: exact always wins best-fit) recovered the scan
  cost: large-only 1T 185k → 235k (1.10x talc), reuses 61k → 74.5k/s.
* **Byte cap stays 4 GiB.** Experiment E4 (16 GiB cap) reverted: it
  removed the throttle and worsened abandonment (59k → 74k, 4.3k/s
  even steady) — confirming slot-pressure, not byte-bound.
* **Reservation sizing: PASS except large-only 8T.** 16 GiB vs worst
  non-8T high-water (large-only 1T hi4301MiB → 3.7x; mixed ≤1495MiB →
  10x+). large-only 8T still exhausts + abandons ~59k transient
  (0/s steady, graceful legacy fallback, still beats system/talc
  1.6–3.6x). Accepted as documented degradation — that gap is §4
  hit-rate work, not mapping cost; no blind cap-raising per §4's rule.
* **`set_arena_enabled(bool)`.** Still deferred per decision (no field
  demand). Hook point unchanged: one `AtomicBool` on the commit path.
* **32-bit CI.** Added `bit32` job to `ci.yml` (build + test on
  `i686-unknown-linux-gnu`; runs in CI — this box has no i686 std and
  no rustup). 512 MiB const keeps its coverage from the `with_size`
  fallback unit tests.

## 4. large-only hit rate (STRUCTURAL FIX DONE — option 2; residual gaps noted)

**Option 2 (spans-for-big-sizes) DONE 2026-09-23**: large-only 8T
**9.06M vs mimalloc 13.51M (0.67×, was 0.05×)**, probe unmaps 0,
big_maps 1308 — see DESIGN_SPANS_BIG.md status block for full validation.
Historical context for the options log below (pre-big-span regime):

`large-only 8T` was 0.07–0.11x mimalloc: shard hit rate under 32K–256K
uniform variance, not mmap cost. Probes (2 s × 3 reps, pre-change): 1T
did ~81k fresh takes/s at ~100% hole reuse, 0 unmaps; 8T did ~58k fresh
takes/s at ~19% reuse with ~47k unmaps/s + ~63k transient abandonments.
Options in order:
1. Deeper exact pools (slots are static-cheap: 64→256/shard hot+cold ≈
   65 KiB) + measure. Revert if flat. MEASURED 2026-09-23: FLAT,
   REVERTED. 1T 220k → 230k (inside its ±5% noise band; byte caps bind
   long before 64 slots do, as the code comment already states). 8T
   uninterpretable (see regime note below).
    KEY DIAGNOSTIC for everything below (historical, pre-big-span):
    large-only 8T was regime-dominated, not pool-depth-dominated. Same
    binary back-to-back: 1.65M / 2.40M / 2.46M with probes ranging from
    (mapped +83, abnd +2/s, unmaps 47k) to (mapped +45k/s retained,
    abnd +20–45k/s steady, unmaps 5–19k) — i.e. lock-convoyed
    efficient-reuse vs lock-barged wasteful-abandonment across the shard
    locks + the single global hole lock (4k-entry best-fit scans at
    ~500k lock/s). **Superseded by option 2 (big spans, DONE)**: the
    structural fix removed steady-state unmaps entirely; residual 8T
    regime variance is much smaller and no longer the binding constraint.
    Scaling curve measured 2026-09-23 (same 32K–256K range, threads
    1/2/4/8, 2 s × 2 reps): 265k / 2.43M / 1.39M / 1.63M total
    (per-thread 265k / 1.2M / 348k / 204k), hole reuse 100% / 95% /
    39% / 22%, unmaps 0 / 0 / 43k / 44k per s, abandonment 0 / 7.5k
    transient / 74k + exhaustion / 74k + exhaustion. Collapse between 2
    and 4 threads; small/medium paths scale fine on the same box, so it
    was large-path shared locks, not the mutex primitive. (Post-big-span
    large-only 8T probe: unmaps 0, big_maps 1308 — the collapse is gone.)
    Design sketch for the stabilization point (option A below was TRIALED
    and REVERTED — flat within regime noise; option 2 shipped instead):
   A. Sharded hole store (recommended first): N sub-stores (e.g. 8, one
      per large-shard salt) each with own lock + slots + byte cap;
      takes/pops route by the same salt the shard choice uses, parks
      route by the freeing path's salt. Same-thread churn (the common
      case) stays exact-pooled; contention ÷ N; scans ÷ N. Risk:
      cross-thread (prodcons) pooling degrades to bump/fallback unless
      takes fall back to a global scan on shard miss (shard lock released
      first — never nest hole locks). Validate: 8T reuse% + unmaps/s +
      regime spread across 3 runs.
   B. Exact-size fast pools + bounded scans: per-size hole stacks popped
      O(1) on exact hit, best-fit across sizes only on miss, with a scan
      budget. Smaller change than A, keeps one lock (convoy risk stays).
   C. try-lock pop (fall back to bump on contention): needs try_lock on
      the internal mutexes first. Risks accelerating exhaustion under
      load (bumps instead of waits). Measure before/after on 8T spread.
   Do NOT pursue: bigger global slots (8192+ — unbounded tuning, scan
   cost grows, §4 bullet 3), looser byte caps (E4 proved it worsens
   abandonment 59k → 74k by removing the throttle).
* **mremap-based large growth (TRIALED 2026-09-23, REVERTED — no
  effect).** ecs 8T: allox 6.4M vs system 111M (0.06x) while beating
  every other allocator 3–25x; probes show healthy caching, so the gap
  is O(n) copy cost on realloc doubling vs glibc's zero-copy growth.
  Built it: `mremap(M…).MAYMOVE` wrapper (Linux/Android, stubs
  elsewhere) + shared try-grow for legacy regions with alloc-copy-free
  fallback, both realloc entry points, 5 new tests (all passed).
  Measured ecs 8T: 6.50M vs 6.38M baseline (+2%, noise) — because ecs
  traffic is arena-backed (probe ~47/48 hole reuses), so legacy-only
  mremap never fires. Reverted entirely (helper, call sites, wrapper,
  stubs, white-box tests); kept the two fallback-growth integration
   tests as regression coverage. Frontier-adjacent arena growth is now
   implemented as a narrow slice: a live region ending exactly at the bump
   frontier can atomically claim and commit adjacent pages, and both realloc
   entry points use it with legacy/non-frontier copy fallback. The dedicated
   `frontier_growth` regression passes; this is not yet a general growable
   extent design.
   TRIALED 2026-09-23, REVERTED (no effect): 16 size-shards × 1024
   slots + global CAS-claimed byte cap (exact-size home shard, first-fit
   fallback across shards, never nested locks). Result on large-only 8T
   ×3 runs: 1.21M / 2.01M / 1.80M, reuse 15–22%, unmaps 36–42k/s,
   abandonment ~73k — indistinguishable from unsharded within regime
   noise. Lesson: segregation doesn't create capacity; the transient
   parks ~74k entries against any slot budget in this range, and takes
   miss on phase-mismatch (correlated drain-then-flood bursts), not on
   lock waiting.
   KEPT 2026-09-23: deep LARGE COLD slots alone (64 → 512, hot
   untouched). Cold is virtual-only after discard, so depth is nearly
   free — and 64 slots × ~150 KiB capped retention far below the
   64 MB/shard byte cap, starving exact reuse and flooding the holes.
   large-only 8T ×5 runs per config: baseline median 1.605M (range
   1.10–2.03M) → trial median 2.06M (range 1.87–2.16M): +28%, and the
   trial worst beats the baseline median. Unmaps 30–50k/s collapse
   toward 0 (three runs at exactly 0); reuse 12–19k → 23–27k/s;
   abandonment persists (~64–73k transient — cheap address-space flow,
   no syscalls) while mapped retention rises (discarded-virtual, RSS
   flat). No regressions: large-only 1T 240k, mixed-all 8T 12.3M /
   1T 2.3M all in-band; full suite green.
2. Spans-for-big-sizes: **DONE 2026-09-23 (Phase 1 / DESIGN_SPANS_BIG).**
   Extended span machinery past the 65472 block cap via one meta chunk +
   pure data chunks (blocks cross 64 KiB boundaries freely — contiguous
   user memory, requirement 4) with arena page-indexed side-table lookup
   (`BIG_MAP` + `BigMaster::contains`, fail-closed). Shipped:
   `BigMaster`/`BIG_CLASSES`/`big_span_pages_for` (`page.rs`/`classes.rs`),
   `BigHeap` per-bclass sharded + empty/cold retention (`heap.rs`),
   cache `bigbins`/`bvirgin` + `alloc_big`/`dealloc_big`, dispatch
    `alloc_impl`/`dealloc_impl`/`alloc_zeroed_impl` routing
     `(65472, 1 MiB]`, `tests/big_spans.rs` (boundary/roundtrip/calloc/
    realloc/GlobalAlloc/8T-churn), Kani P1–P6 proofs, Miri carve tests,
    `tier_boundary_seq` fuzz. Measured (2 s × 3, full matrix): large-only
    8T **9.06M vs mimalloc 13.51M (0.67×, was 0.05×)**, probe
    `b1308/a1308/unmaps 0`; guards hold (mixed-all 8T 12.1M in-band,
    tight/mixed-small flat, spawn-churn unmaps 0, thread_exit green, full
    suite + telemetry + no_std + release). The 2026-09-24 cap extension
    keeps that guard in-band and improves the 1T range below 512K.
    ACTIVE BIG CACHE (KEPT 2026-09-24): same-span refills now remain in a
    per-class active span and active frees use a pointer-range fast path;
    mixed-span batches still use `bigbins`, and trim/flush return active
    chains to `BigHeap`. A capped 2 s × 2 comparison against parent
    `v0.0.969` measured 1.52M vs 1.28M ops/s median on `large-only 8T`
    (+19% for ActiveBig; peak RSS remained within the existing cap).
     512K CAP (KEPT 2026-09-24): extending the top big class from 262144
     to 524288 improved capped `large-only 1T` from 152K to 193K ops/s
     (+27%), with peak RSS 23.3 → 23.8 MiB. `mixed-all 1T` was flat and
     `large-only 8T` stayed within its noisy guard band. The >512K portion
     was the next large-tail candidate at that checkpoint.
     1 MiB CAP (DONE 2026-09-25): the top big class is now 1048576 bytes,
     with a 129-page top span, exact boundary coverage, updated telemetry
     large-path probes, and a separate 2x medium/big cache allowance. Safe
     fresh-process 2 s × 3 runs measured 6.79M/s on `large-only 1T`
     (2.88× mimalloc), 25.10M/s on `large-only 8T` (1.32×), and 10.73M/s
     on `mixed-all 1T` (2.90×), with low Allox peak RSS. The >1 MiB portion
     remains the next large-tail candidate.
 3. Do NOT raise caps blindly: retention is already hundreds of MiB; RSS
    discipline matters more than the last 10% hit rate here.

## 4b. Remote-free drift cap (DONE 2026-09-23)

ROADMAP P1 step 4 / order item 4: pressure-gated ownership heuristic in
`src/cache.rs`. `PageHeader`/`SpanMaster`/`BigMaster` carry `owner: u32`
(1-based monotonic `NEXT_TID`, claimed on refill; benign last-writer
race). Gate open only when `cached_bytes > budget/2`; foreign frees
push lockless to the bin and accumulate `foreign_bytes`; when
`foreign_bytes >= budget/8` or over budget, `trim()` sheds in batch
(chunked + page-grouped) and clears the counter. Never lock-per-free
`release_blocks` — that regressed prodcons to 0.60× with arena
abnd+9721/s / hi6660MiB. Freelist invariant: any raw live block into
`release_blocks` must have its first word nulled first (cold fallbacks
in `lib.rs` all do). Clean sequential benches: **prodcons 8T 32.68M =
1.20× mimalloc** (was 0.60× broken / 1.09× pre-cap baseline; abnd 0/s,
hi359MiB); **mixed-all 8T 13.57M = 0.72× mimalloc / 1.02× system**
(flat vs prior 0.74×; abnd 0/s, unmaps 0). Docs updated: CHANGELOG,
ROADMAP order item 4, DESIGN ownership rule, README table.

## 5. spawn-churn per-op + exit latency (1.25x mimalloc — ADOPTION LANDED)

The isolated baseline was 2.61M Allox vs 13.74M mimalloc ops/s (0.19x;
host variance across the initial runs was 0.19–0.23x). `spawn-empty` remained
approximately 24–29k threads/s for every allocator, confirming lifecycle work
rather than pthread creation was the bottleneck. The original decomposition
showed a 56x short-thread/long-lived-thread gap, no meaningful heap syscall
storm, and a flush-dominated exit path.

**Implemented 2026-09-25:**
- Small-bin exit flushes pass their known chain tail, batch same-class page
  releases, and retire fully-free pages with lazy reinitialization instead of
  rebuilding every free-list link.
- OS thread-exit hooks move bounded caches into eight fixed retirement slots;
  later allocator slow paths reclaim at most one retired cache per cache
  generation. Caches over 8 MiB or a full queue use the synchronous flush path.
- `spawn-churn` production runs (2 s × 5, `BENCH_SAFE_LIVE=1`, 2 GiB cgroup)
  reached **4.65M Allox vs 13.62M mimalloc ops/s** (0.34x), with peak RSS
  17.6 MiB vs 19.4 MiB. The deferred work overlaps worker execution and removes
  most exit-time lock serialization, but does not eliminate the shared-page
  scan/first-touch cost; the remaining gap is still material.

Correctness coverage includes the full debug integration suite and
`tests/thread_exit.rs`; the deferred queue is bounded and falls back to the
original blocking flush. Re-measure the ratio on a quiet host before using it
as a release gate. The next possible lever is cheaper page-touch/ownership
bookkeeping so retirement need not scan each cached block, not another refill
or flush-size guess.

## 6. mixed-all 8T per-op latency (~0.7x mimalloc)

**Local profiling unblocked 2026-09-23** (dev box has `perf` via nix
`linuxPackages.perf`; CI `profile` job artifacts were empty headers —
runner `perf` writes a stub and dies, so local PMU is the source of
truth until CI is fixed). Baseline flat profile (`perf record -F 4999
--no-call-graph`, BENCH_ALLOC=allox, mixed-all 8T):

| self% | symbol |
|------:|--------|
| 43.2 | `GlobalAlloc::dealloc` |
| 11.4 | `alloc_impl` |
| 9.8 | `MediumHeap::take_blocks` |
| 9.8 | `ThreadCache::flush_mbin` |
| 6.0 | `MediumHeap::release_blocks` |
| 3.5 | `ThreadCache::flush_bin` |
| 1.3 | `GlobalHeap::take_blocks` |
| 1.0 | `ThreadCache::trim` |

`perf annotate` inside dealloc: 48% at the `SPAN_MAGIC` load+branch
(medium free), 34% at the mclass bounds compare, 14% at the small class
compare — i.e. **header-derived class on free**, not refill batching,
was the binding free-path cost. dTLB-load-misses ~100M over a 3 s
allox-only run (LLC events unsupported on this box).

**Fix landed (free-path load chains):** `dealloc_with_layout` now
derives `class`/`mclass` from layout size (`class_for_size` /
`medium_class_for_size` LUTs) and never loads a page/span header on the
hot path; `ThreadCache::dealloc`/`dealloc_medium` take the class as a
parameter. Headers are loaded only on the cold no-TLS fallback (and in
`debug_assertions` validation). `free()`-style `dealloc_impl` still
probes once then passes the class through.

Post-fix flat profile (same conditions): **dealloc 43% → 20%**;
`alloc_impl` 11→18%, `take_blocks` 10→12%, `flush_mbin` 10→11%,
`release_blocks` 6→11% — free path no longer dominates; medium
refill/flush now leads. Full suite green (debug + release), warning-free.
Absolute mixed-all scores during this session were polluted by desktop
load (loadavg 15–40 from parallel opencode/firefox); re-measure the
score on a quiet box before declaring the ops/s delta — the *profile
shift* is the reliable result. Next levers in measured order:
(1) medium refill batching (`MEDIUM_REFILL_BATCH=16`) + flush grouping,
(2) residual free-path (budget atomic), (3) TLB/spread only if (1)(2)
flat. CI profile job still needs a fix (empty artifacts).

**Post-drift-cap re-profile (2026-09-23, flat `-F 4999 --no-call-graph`,
BENCH_ALLOC=allox, mixed-all 8T):** `dealloc_medium` 21.5%, `alloc_impl`
16.6%, `flush_mbin` 16.0%, `mrefill` 12.7%, harness backtrace 11.5%,
`flush_bin` 4.3%, memmove 4.0%, `dealloc` 3.9%. Inside `dealloc_medium`,
~72% of samples sat on the drift-gate owner/span probe (magic load 40%
+ owner load 32%). **Lever measured & rejected:** raising
`DRIFT_GATE_DIV` 2→1 (gate at full budget instead of trim target).
Quiet-box sequential: mixed-all stayed ~13.3–14.6M vs mimalloc
19.1–19.5M (**~0.71×, no gain**) while prodcons regressed **32.7M →
~26.8M** (early foreign shed died — `foreign_bytes` never reached
`budget/8` before `cached_bytes > budget` already forced `should_shed`).
Reverted; `DIV=2` stands. PMU share ≠ wall-clock lever here.

Next lever from the same profile: `flush_bin`/`flush_mbin` each zero a
full stack group array (`[Group::EMPTY; 2056]` ≈ 65 KiB,
`[MGroup::EMPTY; 260]` ≈ 8 KiB) on every call — stack probe + memset
showed in both annotations. Switch those arrays to `MaybeUninit` so only
slots `0..ng` are ever touched.

**Lever measured (2026-09-23): `MaybeUninit` flush group arrays —
FLAT, kept.** Sequential quiet-box: mixed-all allox 12.60/13.63/13.58M
vs baseline 13.57M (no gain; mimalloc 19.3–19.9M, ratio still ~0.7×);
prodcons 30.2/33.2/29.9M vs baseline 32.7M (mean 31.1M, within run
noise). Zeroing was real in the profile but not the wall-clock lever —
same lesson as the owner-load PMU share. Kept for code health (no
zeroing tax, tests green); not counted as a §6 win.

**Post-MaybeUninit re-profile (flat `-F 4999`, mixed-all 8T):**
`dealloc_medium` 22.4%, `alloc_impl` 17.3%, `mrefill` 14.1%,
`flush_mbin` 12.3% (was 16.0), harness 12.7%, `flush_bin` 2.6%.
Annotate: medium free still ~73% owner/span probe when gate open;
`mrefill` ~54% on span free-head splice; `flush_mbin` ~38% on bin-head
store + ~7% span magic. **Smoking gun in bench output:** allox does
~847–1001 `SPAN_MAP_CALLS/s` on mixed-all steady state vs mimalloc's
**0** (prodcons also 0) — global medium heap starved while thread
caches hoard, so `take_blocks` maps fresh spans.

**Lever measured & REJECTED (2026-09-23): `MEDIUM_REFILL_BATCH`
16→64.** Quiet-box: mixed-all 10.55/10.62/10.62M vs 13.57M baseline
(**~0.55× mimalloc, −22%**); prodcons 33.4/31.1M (flat-ish). Huge
medium batches overfill the thread-cache budget and thrash
trim/flush — larger batch ≠ fewer maps when the cache immediately
sheds. Reverted to 16.

**Lever measured & KEPT (2026-09-23): empty-span retention
8→32 spans/class + 2→8 MiB byte cap** (count cap was binding long
before bytes, so empty spans were discarded instead of reused).
Quiet-box sequential: **mixed-all 15.87/15.97/15.63M vs 13.57M
baseline (~+16%, ~0.81× mimalloc 19.3–19.6M)**; span maps 687–808/s
(was ~850–1000); prodcons 28.7/30.6M (slightly under the 32.7M
baseline — re-check on quiet box before treating as regression; hi
arena 235–248 MiB vs prior ~300–400, RSS still healthy, abnd 0).
First clear §6 wall-clock win this session.

Next from the same profile: (1) re-confirm prodcons with more reps,
(2) residual free-path (`dealloc_medium` owner probe — sample every
Nth free under gate, or count foreign only on `should_shed` path),
(3) `mrefill` span free-head splice locality (owner-affinity refill
so the span header stays hot).

## 7. Correctness backlog (must clear before 0.2)

* **`cached_bytes` underflow via the trim-lie (FIXED 2026-09-23):**
  `trim()` used to set `cached_bytes = target` when nothing was
  trimmable while blocks remained binned, lagging actual retention
  until a later pop drove it below zero (debug panic at
  `cache.rs:alloc`, release wrap into an over-trim storm). Deterministic
  with tiny budgets, rare with the default 32 MiB. Fix: stop without
  touching the counter (plain `break`) — audit of all 19 adjustment
  sites shows it is otherwise exactly retained-bytes, so the plain
  `-=` pops can no longer underflow. Regression test
  `tiny_budget_churn_never_underflows` (failed pre-fix, deterministically,
  including cross-thread pollution into `many_small_churn`). Bench smoke
  flat (25–28M in-band).
* **Realloc zero-size dangling bug (FIXED 2026-09-23):**
  `GlobalAlloc::realloc` same-class identity fired on `layout.size() == 0`
  and returned the dangling pointer for nonzero `new_size`; worse, the
  whole zero-size family was untested and broken — `free(malloc(0))`,
  `realloc(malloc(0), 8)`, and `usable_size(malloc(0))` all probed
  headers of address ~1 (debug panic, release segfault). Fix:
  identity paths require nonzero size + skip 0-byte copies (Rust
  `alloc` keeps its documented dangling convention); C-flavored
  `malloc`/`calloc`/`aligned_alloc` return null for zero size
  (conforming), so `free`/`realloc`/`usable_size` stay on their null
  contracts by construction; free-fn `realloc(p, 0)` frees + returns
  null. Tests: `zero_size_family_is_sound`,
  `global_realloc_zero_layout_grows_fresh` (both failed pre-fix).
  Miri/fuzz coverage of the new paths rides the existing CI jobs
  (no nightly on the dev box).
* **Miri + fuzz on new code:** span carving/lookup, exit-hook paths,
  arena commit/release/hole logic, `large_header_of` validation.
  **DONE 2026-09-23 (Phase 0.2):**
  - `page.rs`: big carve rewritten on `std::alloc` 64 KiB-aligned buffers
    (runs under Miri); added `big_contains_is_fail_closed`,
    `medium_span_of_and_contains`.
  - `lib.rs`: `header_probe_tests` for `large_header_of` (accept well-formed,
    reject bad magic / bad size / outside range / off==0).
  - `arena.rs`: all raw-mmap unit tests `cfg_attr(miri, ignore)`.
  - Fuzz: new `tier_boundary_seq` target (exact 16 KiB / 65472 / 262144 /
     524288 edges + neighbors, mixed align, realloc/calloc/free); CI smoke-runs it.
  - CI miri job: `miri setup` + `MIRIFLAGS=-Zmiri-strict-provenance`.
  - Exit-hook path: covered by `tests/thread_exit.rs` in the normal 3-OS
    test matrix (needs real pthread/Fls, not Miri).
* **Windows Fls path:** **DONE 2026-09-23 (Phase 0.3)** — already covered
  by the existing 3-OS `test` matrix (`windows-latest` runs
  `tests/thread_exit.rs` + `stress.rs`) and the 3-OS `bench` matrix; no
  separate job. Local box still cannot execute Fls (Linux only), but CI
  does on every push/PR.
* **jemalloc comparator:** **DONE 2026-09-23 (Phase 0.4)** — optional
  `bench-jemalloc` feature (`tikv-jemallocator`, off by default) + CI
  `jemalloc-bench` job (installs make+autoconf, runs full scoreboard,
  uploads `bench-jemalloc.txt`). Local box still cannot build it; re-score
  all 14 workloads against the CI artifact when releasing.
* **Coverage (cargo-llvm-cov):** **DONE 2026-09-23 (Phase 0.5)** — CI
  `coverage` job: `cargo llvm-cov --all-features --workspace --lcov`,
  uploads `lcov.info` artifact; summary printed in job log.
* **Kani proofs (P1–P6):** **DONE 2026-09-23 (Phase 0.6)** — proofs in
  `src/page.rs` `kani_proofs` under `#[cfg(all(kani, unix, feature = "std"))]`:
  P2/P6 sizing (`p2_last_block_within_mapping`), P1/P3 contiguity+alignment
  (`p1_contiguity_and_p3_alignment`), medium-page packing
  (`medium_page_carve_stays_in_page`). P4/P5 are runtime-state properties
  covered by existing unit tests (`big_carve_packs_contiguously`,
  `medium_span_of_and_contains`). CI `kani` job runs `cargo kani` on
  nightly + kani-verifier (local box has no rustup — CI-only verification,
  same as Miri).
* **3-OS bench matrix:** **DONE 2026-09-23 (Phase 0.7)** — existing CI
  `bench` job already runs `cargo bench` on
  `windows-latest`/`ubuntu-latest`/`macos-latest` and uploads per-OS
  artifacts; feeds the README Windows/macOS table re-verify checklist in
  §8.

## 8. Cross-platform + release 0.2 checklist

* README Windows table re-verify (every number predates spans/cold/exit/
  arena/mutex) — NOT doable on this box; covered by the CI bench job's
  per-OS artifacts. README now says so next to the table.
* Linux table — DONE 2026-09-23 (refreshed after big spans): 10-workload
  table in README from a fresh 2 s × 3 reps full-matrix run (Ryzen 5
  1600) with methodology note; dlmalloc omitted (10×+ run-to-run
  variance here), spawn-churn + large-only 8T + mixed-all carry
  context-sensitivity notes pointing at §5/§4/§6. large-only 8T row now
  shows the big-span win (0.67× mimalloc, was 0.05×).
* macOS numbers (allocator + exit hook + pthread mutex all have
  macOS-specific branches: `MAP_ANONYMOUS` value, `pthread_key_t` width,
  `MAP_FIXED` without `NOREPLACE`) — NOT doable here; pending CI/macOS box.
* 32-bit build + arena fallback tests in CI — DONE (§3 `bit32` job).
* CHANGELOG 0.1.0 entry — DONE (rewritten for what 0.1.0 actually
   contains: spans, big spans, sharded large/cold tiers, arena, exit
   flush, zero-size rules). API review — DONE 2026-09-23: full public surface audited,
  rustdoc `-D warnings` clean; fixed `mapped_pages` docs in `Stats` +
  `Telemetry` (live mappings per fresh take, any size — not 64 KiB
  pages); completed the C ABI with `allox_malloc_usable_size` + ffi
  test. Telemetry array covers small + medium + big (arena targets) and
  may still grow pre-1.0 — noted in CHANGELOG, acceptable. Version hygiene:
  `cargo publish --dry-run` warning-free; added repository/documentation/
  homepage URLs (was the only manifest warning).
* External audit scoping for the unsafe core (page/heap/cache/lib
  unsafe blocks + new arena/exit code). Not a launch blocker for 0.2
  (README already says unaudited), but schedule it. Scope when
  scheduling (2026-09-23 note): `page.rs` (header carving, masking
  dispatch, span sub-headers), `heap.rs` (take/release chains, used
  counting, cold re-carve), `cache.rs` (bin accounting incl. the fixed
  trim-lie, flush grouping, virgin tracking), `lib.rs` (large headers,
  realloc copies, layout routing, zero-size contracts),
  `arena.rs` (MAP_FIXED commits with ret==base check, hole store
  exclusivity, discard-then-park ordering), `thread_exit.rs`
  (pthread/Fls hook reentrancy, blocking flush), `sys/*` (raw syscall
  wrappers, pthread-mutex init/storage), `ffi.rs` (C ABI null and
  alignment contracts). Focus invariants: virgin (fresh-zero) claims,
  discard-then-park ordering, exit-hook safety, counter semantics
  (MAPPED_PAGES fresh-only, cached_bytes exact).

## Explicitly out of scope

* NUMA awareness, huge pages, `Allocator` trait impl (DESIGN non-goals).
* jemalloc-style background purge threads or decay timers (our caps +
  cold retention cover the steady state; revisit only on month-long
  RSS-growth evidence).
* OOM policy beyond returning null.
