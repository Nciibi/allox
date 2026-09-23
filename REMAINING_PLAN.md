# Remaining plan — from big-spans-validated to 0.2

State at fork-off: **big spans DONE 2026-09-23** (DESIGN_SPANS_BIG
IMPLEMENTED; large-only 8T 0.05× → 0.67× mimalloc, unmaps 0). Full
14-workload matrix: ~9–10/14 win-or-tie vs the best comparator on Linux
x86-64 (json/request/ecs now beat mimalloc too — ecs 1.41× via big-span
realloc paths); full suite + telemetry + no_std + release green and
warning-free. Contra remaining gaps below, each capable of closing
independently, ordered by ROI. Methodology everywhere: ≥2 s × 3 reps,
`__debug_map_split` + `peakRSS` probe columns, sensitivity-checked tests
(disable-the-feature must fail), one point measured before the next starts.

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
   KEY DIAGNOSTIC for everything below: large-only 8T is regime-dominated,
   not pool-depth-dominated. Same binary back-to-back: 1.65M / 2.40M /
   2.46M with probes ranging from (mapped +83, abnd +2/s, unmaps 47k) to
   (mapped +45k/s retained, abnd +20–45k/s steady, unmaps 5–19k) — i.e.
   lock-convoyed efficient-reuse vs lock-barged wasteful-abandonment
   across the shard locks + the single global hole lock (4k-entry
   best-fit scans at ~500k lock/s). No pool-depth conclusion is drawable
   until the regime is stabilized (per-size/sharded hole pools? try-lock
   pop? take-path scan budgets?). That stabilization is the prerequisite
   point before option 2, not after it.
   Scaling curve measured 2026-09-23 (same 32K–256K range, threads
   1/2/4/8, 2 s × 2 reps): 265k / 2.43M / 1.39M / 1.63M total
   (per-thread 265k / 1.2M / 348k / 204k), hole reuse 100% / 95% /
   39% / 22%, unmaps 0 / 0 / 43k / 44k per s, abandonment 0 / 7.5k
   transient / 74k + exhaustion / 74k + exhaustion. Collapse between 2
   and 4 threads; small/medium paths scale fine on the same box, so it
   is large-path shared locks, not the mutex primitive.
   Design sketch for the stabilization point (no code yet — needs its
   own measurement-backed review before implementation):
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
  tests as regression coverage. Honest follow-up, NOT trialed:
  frontier-adjacent arena growth (extend bump when the region sits at
  the frontier) — narrow, uncertain hit rate, needs its own probe.
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
   `(65472, 262144]`, `tests/big_spans.rs` (boundary/roundtrip/calloc/
   realloc/GlobalAlloc/8T-churn), Kani P1–P6 proofs, Miri carve tests,
   `tier_boundary_seq` fuzz. Measured (2 s × 3, full matrix): large-only
   8T **9.06M vs mimalloc 13.51M (0.67×, was 0.05×)**, probe
   `b1308/a1308/unmaps 0`; guards hold (mixed-all 8T 12.1M in-band,
   tight/mixed-small flat, spawn-churn unmaps 0, thread_exit green, full
   suite + telemetry + no_std + release). Remaining large-only gap is the
   1T 32K–1M tail above 262144 (stays large-path by design — §7 open
   question 1) and lock-regime variance under 8T (see option A notes
   above; sharded holes already flat-reverted).
3. Do NOT raise caps blindly: retention is already hundreds of MiB; RSS
   discipline matters more than the last 10% hit rate here.

## 5. spawn-churn per-op + exit latency (0.22x mimalloc — DECOMPOSED, no lever pulled)

Measured 2026-09-23 (isolated 2 s × 3 reps): allox 2.89M vs mimalloc
12.64M (0.23x); `spawn-empty` ≈ 29k threads/s (≈34 µs spawn+join) for
EVERY allocator. Decomposition with a temporary same-churn driver:
short-lived threads (spawn+exit per 2000-op batch, like the bench)
2.84M vs long-lived threads (lifecycle amortized) **159M ops/s** —
56x. Steady-state per-op is excellent (consistent with `mixed-small 8T`
beating mimalloc 4.3x); probes show ~0 heap syscalls either way, and
1T→4T scaling (2.70M → 2.89M, 1.07x, vs system 3.4x) implicates
lifecycle serialization, not fast-path cost. The ~2.7 ms per short
thread is cold-start page faults on first-touch refills + exit-flush
grouping/locked releases/discards — structural for shared-page designs
(mimalloc wins via thread-owned segments freed wholesale, a
redesign-class difference, out of scope). None of the listed levers
(refill batch, flush chunk, re-carve init) moves a 2.7 ms lifecycle
dominated by faults + flush work; pulling them blind risks P2-matrix
regressions. STOP per the no-blind-experiments rule: next step is lock
+ fault profiling (PMU, see §6), or documenting 0.23x-isolated /
0.85x-full-process as the short-thread price. Note the 2x-spawn-cost
stop criterion as written is unreachable for ANY allocator (mimalloc
itself is ~18x spawn per thread) — it needs restating before reuse.

## 6. mixed-all 8T per-op latency (~0.7x mimalloc)

Was blocked on profiling, not ideas: no `perf`/PMU on the dev box.
Unblocked 2026-09-23: CI `profile` job added (best-effort `perf stat`
on mixed-all 8T + spawn-churn, software + hardware counters,
`continue-on-error`, artifacts uploaded). NEXT: read the first CI
artifacts, then work the profile — candidates are refill batching,
free-path load chains (`of`+`contains`), and TLB behavior on scattered
spans, in that order of suspicion. Still no blind experiments.

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
  - Fuzz: new `tier_boundary_seq` target (exact 16 KiB / 65472 / 262144
    edges + neighbors, mixed align, realloc/calloc/free); CI smoke-runs it.
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
* Linux table — DONE 2026-09-23: 10-workload table in README from a fresh
  2 s × 3 reps run (Ryzen 5 1600) with methodology note; dlmalloc omitted
  (10×+ run-to-run variance here), spawn-churn + large-only 8T carry
  context-sensitivity notes pointing at §5/§4.
* macOS numbers (allocator + exit hook + pthread mutex all have
  macOS-specific branches: `MAP_ANONYMOUS` value, `pthread_key_t` width,
  `MAP_FIXED` without `NOREPLACE`) — NOT doable here; pending CI/macOS box.
* 32-bit build + arena fallback tests in CI — DONE (§3 `bit32` job).
* CHANGELOG 0.1.0 entry — DONE (rewritten for what 0.1.0 actually
  contains: spans, sharded large/cold tiers, arena, exit flush, zero-size
  rules). API review — DONE 2026-09-23: full public surface audited,
  rustdoc `-D warnings` clean; fixed `mapped_pages` docs in `Stats` +
  `Telemetry` (live mappings per fresh take, any size — not 64 KiB
  pages); completed the C ABI with `allox_malloc_usable_size` + ffi
  test. Telemetry array covers small + medium and may still grow
  pre-1.0 — noted in CHANGELOG, acceptable. Version hygiene:
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
