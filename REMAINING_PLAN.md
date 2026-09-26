# Remaining plan — from big-spans-validated to 0.2

State at fork-off: **big spans DONE 2026-09-23; 512 KiB big-cap phase
DONE 2026-09-24; 1 MiB big-cap extension DONE 2026-09-25; growable-extent
promotion DONE 2026-09-25** (§4c). The tier-aware cache allowance and
bounded benchmark matrix are validated: `large-only 1T` 2.88× mimalloc,
`large-only 8T` 1.32×, and `mixed-all 1T` 2.90× on the 2 s × 3 safe
fresh-process run. The 1T tail above 1 MiB is still on the large path.
**Remote-free drift cap DONE 2026-09-23** (§4b). **Frameless fast paths +
single-visit `realloc` DONE 2026-09-26** (§4d) — the small/medium
`alloc`/`free`/`realloc` fast paths no longer contain a call, so they carry
no stack frame and no callee-saved registers. Paired A/B: **+5.9% on the
process-global app benchmark at 1 and 4 threads**, and `tight-small 8T`
+6.6%, `request 8T` +18.4%, `json-ish 8T` +4.5%, `tight-small 1T` +3.5%
with no workload regressing outside its own noise band.
**Full 20-workload matrix re-measured 2026-09-26: 19/20 win-or-tie vs the
best comparator** (fresh process per sample, 2 s × 3, `BENCH_SAFE_LIVE=1`,
3 GiB cgroup, `taskset -c 0-7`; README table refreshed from that run). The
twentieth is `spawn-churn` at 0.97×, which is bimodal in *every* build
including `v0.0.1276` and is not usable as a gate without a quiet host.
**Still open and now the binding constraint: the 4-thread app-shape row**
(0.94× mimalloc / 0.90× snmalloc, though 1.06× each at one thread and 0.99×
mimalloc on four distinct physical cores). Single-threaded allox leads
every comparator on the app shape; what remains is a scaling deficit, and
the structural fix is a per-page free bitmap to make `realloc` grow in
place (§4d candidate 1). Full suite green and warning-free in debug,
release, telemetry and no_std; the `wasm32` build, broken before this
session, is fixed (§4d). Contra remaining gaps below, each capable of
closing independently, ordered by ROI. Methodology everywhere: ≥2 s × 3
reps, paired A/B between two prebuilt binaries with order-alternating runs
for anything under ~5%, `peakRSS` probe columns, sensitivity-checked tests
(disable-the-feature must fail), one point measured before the next
starts.

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
    extent design. **The ecs gap itself was closed the next day by §4c
    (big-block promotion), which removes the copy from the arena path
    that `mremap` could never have reached — so no `mremap` is needed.**
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

## 4c. Growable extents for packed big blocks (DONE 2026-09-25)

The last structural loss: `ecs 8T` at 0.06× the system allocator, whose
glibc grows a doubling chain with zero-copy `mremap`. §4's `mremap` trial
was flat because ecs traffic is arena-backed, so the copy had to be
removed from the arena path itself.

Shipped: `try_promote_big_grow` in `src/lib.rs` (arena targets). A big
block is carved contiguously out of a shared span, so it can never grow in
place — the neighbours are live data. The *first* cross-class growth now
relocates once into a large region whose reserve is
`BIG_GROW_SLACK_FACTOR (8) x new_size`, capped at `MAX_BIG_BLOCK`, and
virtual-only (untouched anonymous pages, so RSS tracks live demand). Every
later growth in the chain fits that mapping and is served in place by the
existing `try_grow_large_frontier`: no copy, no syscall, no span traffic
per step. Both `GlobalAlloc::realloc` and the free function route through
it; a live large region is excluded by an `arena::large_table_get` check
so a stale big-table entry can never misroute an already-promoted block,
and every failure path falls back to the ordinary alloc-copy-free route.
`tests/big_growth_promote.rs` pins content preservation at every step,
sticky in-place growth, both entry points, and a shrink round-trip.

Measured (fresh process per sample, 2 s × 3, `BENCH_SAFE_LIVE=1`,
`taskset -c 0-7`, allox-only A/B vs `v0.0.1255` on the same box and
session): **`ecs 8T` 9.59 → 92.09 M/s (9.6×)**, peak RSS 19.7 → 12.2 MiB.
Comparators in the same regime: system 74.69 M/s (1.23×), mimalloc
1.85 M/s, talc 1.52 M/s, dlmalloc 783 K/s, snmalloc 370 K/s; a 2 s × 5
confirmation gave 91.45 vs 76.27 M/s (1.20×). Probe: 0 unmaps, 0–1 arena
commit, 18 MiB arena high-water.

Full 20-workload A/B against the same pre-promotion build, everything
else flat inside run-to-run spread: `tight-small 8T` 227 → 235 M/s,
`mixed-small 8T` 183 → 182, `mixed-all 8T` 46.0 → 44.9, `mixed-all 1T`
17.8 → 18.1, `medium-only 8T` 47.7 → 48.5, `large-only 1T` 7.47 → 7.72,
`large-only 8T` 31.7 → 31.9, `huge-only 1T` 1.88 → 1.96, `prodcons 8T`
40.5 → 38.4 (bimodal in both builds), `spawn-churn` bimodal in both,
`json-ish 8T` 239.6 → 246.3, `request 8T` 381.7 → 396.3. Full suite green:
debug, release, telemetry, no_std (73 + 74 + 74 tests).

Known limits (documented, not fixed): a growth past the big cap, or past
a reserve that is not frontier-adjacent, still falls back to
alloc-copy-free, so a non-doubling growth pattern can still relocate
after having settled in place; the reserve is up to 8× the requested
bytes (bounded by the arena's own caps and by the big cap), which is
virtual-only but does consume arena reservation. No `mremap` wrapper is
involved — none is needed.

## 4d. Process-global app shape: the first workload allox loses
   (FAST-PATH WORK LANDED 2026-09-26; per-page bitmap still OPEN)

The direct-call matrix (`benches/alloc.rs`) measures allocator loops. The
process-global app benchmark (`examples/app_workload.rs`, one binary per
allocator via `--features app-allox|app-system|...`, driven by
`scripts/app_bench.sh`) measures what an application does: `String`/`Vec`
growth, `HashMap` churn, recursive tree build + bulk drop, and the harness's
own allocations. It is the first mode where allox is **not** ahead.

Measured (4 threads, 4 s, fresh process per backend, Ryzen 5 1600):

| backend | docs/s | ns/document | peak RSS |
|---|---:|---:|---:|
| allox | 238,944 | 4,185 | 3.9 MiB |
| system | 203,072 | 4,924 | 3.2 MiB |
| mimalloc | 270,352 | 3,699 | 3.8 MiB |
| snmalloc | 279,504 | 3,578 | 6.9 MiB |
| talc | 12,272 | 81,486 | 2.7 MiB |

allox was 1.18× system but **0.88× mimalloc / 0.85× snmalloc**. Single
threaded the three fast allocators tied (allox 72.1k, mimalloc 72.9k,
snmalloc 72.7k docs/s), so the gap was entirely scaling: allox 2.90× from 1
to 4 threads, mimalloc 3.59×, snmalloc 3.87×.

**Status after the 2026-09-26 fast-path work: the per-operation cost is no
longer the problem, the scaling still is.** Paired A/B (two prebuilt
binaries, order-alternating, 3 reps, `taskset -c 0-7`): **+5.9% at both 1
and 4 threads**. Single-threaded allox now *leads* both C comparators
(78.6k vs 74.3k / 74.3k = 1.06× each); the 4-thread row moved 0.88× →
**0.94× mimalloc / 0.90× snmalloc**. What is left is candidate 1 below, and
it is a structural change rather than a tuning change.

Where it goes (`perf`, same binary/workload, 4T): allox spends **25.6%** of
samples in allocator code against mimalloc's **18.6%** — `alloc_impl` 7.6%
(≈6.4 ns/call vs mimalloc's ≈3.6 ns), `ThreadCache::dealloc` 6.1%,
`memmove` 5.6% vs 4.4%. The volume counters say why the copies are there:
**31.1M relocating `realloc`s copying 1.03 GB** in the same run, ~33 bytes
each, 76% of all `realloc` calls. App containers double, and every doubling
crosses a size class, so each growth is a full alloc + copy + free.

**What the workload actually is** (measured, not assumed — build the
example with `--features app-allox,telemetry`; the histogram dump added for
this is in the CHANGELOG). 69.6M allocations in 2 s at 4 threads, 158
allocations per document, and **98.9% of them land in small classes 0–11
(16 B–192 B)**: `medium_refills=4`, `big_refills=0`, `large=0`, 33 map
calls, 0 unmaps. There is no medium/big/large story here at all. **27% of
all allocations are relocating `realloc`s** (19.0M of 69.6M, copying
626 MB, ~33 B each) — containers double and every doubling crosses a class.
So this is a pure small-tier, realloc-heavy benchmark, and per-operation
cost is the only lever that matters short of removing the relocations.

Candidate levers, in measured order of promise:

1. **Grow in place into the adjacent block** (what mimalloc/snmalloc do with
   a per-page free bitmap). allox's pages are one class per 64 KiB, but the
   free list is intrusive through the blocks and class-locked, so checking
   and unlinking the neighbour is O(free blocks) under a lock — which is
   exactly why DESIGN.md records it as "rejected: expected net loss". Making
   it O(1) needs a per-page free **bitmap** (64 KiB page of 32 B blocks =
   2048 bits = 256 B of page metadata) or a doubly-linked free list. This is
   a real structural change to the small tier and is the only lever that
   attacks the copies themselves. **STILL OPEN, and now the binding
   constraint.** Note the arithmetic that makes it hard here specifically:
   the classes are 16, 32, 48, 64, 80, … so a 32 B block cannot grow into a
   48 B class out of its own page (48/32 = 1.5), even though 16→32 is
   exactly 2:1. In-place growth within a one-class-per-page design only
   works for the power-of-two steps, and this workload's chain
   (16→32→64→128) happens to be all of them — but the class table also
   produces 48/64/80/… in real doubling sequences, so a general solution
   needs the bitmap, not a special case.
2. ~~**Cheaper relocation round trip**~~ — **DONE 2026-09-26.** See below.
3. ~~Fast-path diet~~ — **DONE 2026-09-26, and it was worth more than
   expected.** See below.

**Landed 2026-09-26 (fast-path work).** No `perf` on the dev box (the nix
store has only the unbuilt derivations), so the diagnosis came from
disassembling the release binary rather than a profile, and it was not the
algorithmic story anyone expected. Before:

* `alloc_impl`'s small fast path began `push %r14; push %rbx; sub $0x18,%rsp`
  and ended with the matching epilogue — six instructions of frame traffic on
  every small allocation, holding registers for the large/medium/big bodies
  it never runs. Cause: `refill`, `mrefill` and `bigrefill` were marked
  `#[inline]`, so LLVM inlined the class lock, the owner-claim walk and the
  chain split into the fast path.
* The small free was an out-of-line tail jump to `ThreadCache::dealloc`,
  which itself carried `push r15/r14/rbx`. Two causes: `dealloc` had no
  `#[inline]`, and `arm_exit_hook` inlined a `call ensure_hook` into it — a
  single call anywhere in the hot path forces every live value into
  callee-saved registers around it.
* `bins[class]` kept a `cmp $0x3f; ja` bounds check that can never fire, on
  both the alloc and the free path.
* `GlobalAlloc::realloc` carried a five-register frame for the big-block
  promotion and frontier-growth paths, paid even by identity resizes that
  move nothing.

After: `alloc`, `dealloc` and the `realloc` identity test are call-free
leaves (verified by disassembly of the release binary — no `push`, no
`sub rsp`, no bounds check); every slow path is `#[inline(never)]` and
reached by a tail jump; the class index is masked so its range is provable
(a no-op, but it deletes the check); the small refill claims the page owner
once per *page* rather than once per block, since `fill_from_list` carves a
run from one page before moving on; and `realloc_small` resolves both
classes once and does allocate + copy + free under a single `with_cache`.

**Measured, paired A/B (two prebuilt binaries, order-alternating pairs, 3
reps, `taskset -c 0-7` — the rule below, applied):**

| workload | before | after | delta |
|---|---:|---:|---:|
| app 1T (docs/s) | 73.9k | 78.2k | **+5.7%** |
| app 4T (docs/s) | 238.2k | 252.1k | **+5.9%** |
| `tight-small 8T` | 236.8M | 252.3M | **+6.6%** |
| `request 8T` | 378.5M | 448.1M | **+18.4%** |
| `json-ish 8T` | 256.0M | 267.5M | +4.5% |
| `tight-small 1T` | 51.8M | 53.6M | +3.5% |
| `ecs 8T` | 93.3M | 94.8M | +1.6% |
| `mixed-small 8T` | 198.2M | 200.8M | +1.3% |
| `mixed-all 8T` | 45.9M | 46.2M | +0.7% |
| `medium-only 8T` | 48.1M | 48.4M | +0.6% |
| `prodcons 8T` | 38.7M | 38.1M | −1.5% (peak RSS 456 → 434 MiB) |

`request 8T` is the row §4d's earlier A/B recorded as regressing 4.4% from
the fast-path change; it is now **+18.4%**, which settles that open question
— the trade the plan was weighing does not exist once the frame is gone.

**`prodcons 8T` and `spawn-churn` are the two rows that lie.** A single
sweep showed `prodcons` −16.7% and `spawn-churn` −4.6%; the paired A/B put
`prodcons` at −1.5% with *better* RSS, so the first was host noise, as §4d
warned. `spawn-churn` is genuinely bimodal in **every** build, including
`v0.0.1276`: eight paired fresh-process samples of each build produced
base {8.5, 8.8, 9.1, 19.7, 19.8, 20.0, 20.0} and
new {8.3, 8.6, 8.9, 11.7, 12.7, 13.2, 14.6, 20.0} M/s — fully overlapping
distributions. The slow mode is identifiable in the counters: when
short-lived thread caches cannot be adopted, their page releases are purged
rather than recycled, and `purge_bytes` runs 35 MB in the fast mode versus
811 MB in the slow one. On a quiet box, 10 paired samples gave allox 17.84M
vs mimalloc 18.40M. Not a gate without a quiet host.

**One measurement caveat worth recording.** This box has 4 physical cores
with SMT, and `taskset -c 0-7` is a *mixed* set (CPUs 0 and 1 each have a
sibling in the set, CPUs 2–5 are distinct cores). The app-shape 4-thread
ratio is sensitive to that: on `taskset -c 0-3` (4 distinct physical cores,
no SMT sharing) the same two binaries read allox 0.99× mimalloc, versus
0.90× on `0-7`. allox loses ~9% when SMT siblings share a core and
mimalloc loses ~2%. The README keeps `0-7` for comparability with every
earlier measurement, but the honest reading of the 4-thread row is "at
parity on distinct physical cores, behind on an SMT-shared set".

Landed while measuring this (v0.0.1270): the virgin count moved from a
parallel `ThreadCache::virgin` array into the small `Bin` (it fits the
struct's existing padding), so a small alloc/dealloc touches one cache line
instead of two; `alloc_impl` now tests the small tier first, as one
comparison, before any tier routing. Interleaved A/B against `v0.0.1269`:
app benchmark +2.5% at 1 thread / +1.3% at 4 threads, direct-call matrix
neutral (`mixed-all 8T` 29.6 vs 29.6, `large-only 8T` 20.6 vs 20.2,
`medium-only 8T` 31.5 vs 32.1 M/s). Full suite green. Not claimed as a
speedup on the allocator loops — the honest summary is "cheaper hot path,
same measured throughput there".

**Instrumentation lesson (the important part, and it cost real money):** the
first version of these counters incremented a *shared atomic per event*.
That is not a small tax, it is a structural one, and it shipped as a
regression before it was caught:

| counter | event rate (mixed-all 8T) | measured cost |
|---|---:|---:|
| `realloc_relocations` / `realloc_copy_bytes` | ~10M/s | −20% process-global app, 4T |
| `trims` | ~4M/s | −11% mixed-all 8T |
| `owner_probes` / `remote_frees` | ~12M/s | −24% mixed-all 8T, −20% large-only 8T |
| `heap_lock_acquisitions` (one global, inside all 64 class locks) | per lock | could not be batched; removed |

Cumulatively that was **−33.7% on `mixed-all 8T`** and **−35.6% on
`large-only 8T`** against the pre-session build — invisible in the
single-workload spot checks, obvious in a paired A/B. All volume counters
now accumulate in the owning thread's `Pending` batch and publish every
8192 events (one thread-local add per event); only per-syscall and
per-thread events (purges, exit flushes, retired/adopted caches, software
zeroing) keep a direct atomic. The lock counter is gone entirely:
`telemetry::timing()` keeps lock-wait, and `flushes`/`*_refills` proxy the
traffic.

**Fixed (v0.0.1275):** the batched counters are now **telemetry-gated**, like
every other per-op counter in this crate. A default build compiles the
diagnostics out entirely and pays zero; `cargo bench --features telemetry`
turns them on when you want the numbers. Only genuinely cold counters stay
always-on — purges (one per `madvise`), exit flushes / retired / adopted
caches (one per thread), software zeroing (one per memset that had to run).

Paired A/B, pre-session `v0.0.1266` vs the final tree, order-alternating
pairs on the same box (the box had ~2 cores of foreign load, so only paired
comparisons mean anything):

| workload | v0.0.1266 | final | delta |
|---|---:|---:|---:|
| tight-small 8T | 236.4 | 244.2 | **+3.3%** |
| prodcons 8T | 39.0 | 39.6 | +1.6% (spread ±15%, noise) |
| mixed-all 8T | 46.4 | 46.1 | −0.6% |
| large-only 8T | 32.4 | 32.1 | −1.0% |
| request 8T | 403.4 | 385.6 | **−4.4%** |

With the counters compiled out, `tight-small 8T` +3.3%, `request 8T`
−4.4%, everything else flat. That is the whole remaining delta, and it is
the fast-path change (`SmallBin::virgin` + small-tier-first in
`alloc_impl`) — the only source of movement left, and a provably smaller
one (one cache line instead of two per small alloc, two fewer branches on
the hot path). At ±3-4% on a single workload, code layout explains as much
as the change does, and the two rows move in *opposite* directions.

**If the rule is "no workload may regress", the honest move is to revert
the fast-path change**: it buys +3.3% on `tight-small 8T` and ~+1% on the
app shape, and costs ~4% on `request 8T`. Both rows are scoreboard-critical
(0.96× system and 0.94× snmalloc as measured on 2026-09-25), so this is a
genuine small trade rather than a clear win.

**RESOLVED 2026-09-26 (§4d): the trade does not exist.** `request 8T` was
the row paying for that change, and once the fast paths stopped carrying a
frame for their slow-path bodies the same work reads **+18.4%** on
`request 8T` and **+6.6%** on `tight-small 8T`. The cost was never the
change; it was the register spills the change made the compiler add.

**Rule this establishes:** a counter's cost is a property of its event rate,
not of where it sits. Before believing any benchmark that a change helped,
run the *paired* A/B against the previous build on several workloads at
once — three of today's apparent findings (prodcons −9%, mixed-all −27%,
request −4%) were measurement artifacts, and one (mixed-all −33%) was a
real regression that only the paired view exposed.

## 5. spawn-churn per-op + exit latency (1.33x mimalloc — ADOPTION LANDED)

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
  generation. A worker with an empty cache adopts the queued cache directly,
  avoiding the scan and heap round-trip; post-adoption bin checks cover small,
  medium, and big tiers. Caches over 8 MiB or a full queue use synchronous flush.
- `spawn-churn` production runs (2 s × 5, `BENCH_SAFE_LIVE=1`, 2 GiB cgroup)
  reached **18.38M Allox vs 13.78M mimalloc ops/s** (1.33x), with peak RSS
  18.5 MiB vs 19.4 MiB. The adoption path removes most exit-time scanning and
  lock serialization for the common all-freed thread pattern.
- A runtime static class-size table removes the hot small-free table copy;
  `zeroed-large` now discards recycled regions before reuse, and deeper bounded
  large caches/range-based side-table writes improve `huge-only`. ECS was a
  separate growable-extent problem because packed big-span realloc still
  copied; that is now §4c (closed 2026-09-25, 9.6× on `ecs 8T`).

Correctness coverage includes the full debug integration suite and
`tests/thread_exit.rs`; the deferred queue is bounded and falls back to the
original blocking flush. Re-measure the ratio on a quiet host before using it
as a release gate. The next possible lever is reducing first-touch cost for
workers that cannot adopt a cache, not another refill or flush-size guess.

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
