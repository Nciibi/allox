# Remaining plan — from arena-large-wired to 0.2

State at fork-off: 7–8/11 bench workloads win-or-tie vs the best comparator
on Linux x86-64; full suite + telemetry + no_std + release green and
warning-free. Contra remaining gaps below, each-capable of closing
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

## 3. Arena follow-ups (only if measured demand appears)

* **Hole coalescing.** Current holes never merge: sustained size variance
  fragments the store into unusable slivers while `abandoned` climbs.
  Trigger: `ARENA abandoned` counter growing >1k/s in any steady bench.
  Design: address-ordered free structure or boundary tags; keep exact
  stacks as the fast path, coalesce on pop-miss only.
* **`set_arena_enabled(bool)`.** Deferred per decision (kill-switch only
  if field issues demand it). Hook point is designed in: one
  `AtomicBool` checked on the commit path, default on. Do not add env-var
  probing (allocation-free constraint makes env awkward).
* **Reservation sizing validation.** Log arena high-water (bump max) in
  benches; confirm 16 GiB headroom is ≥4x worst measured (else grow the
  const). 32-bit 512 MiB path gets its coverage from forced-fallback
  tests, not from winning benches.
* **32-bit CI.** Build + unit tests on `i686-unknown-linux-gnu`; arena
  tests must pass with the 512 MiB const (exhaustion test already covers
  the fallback shape).

## 4. large-only hit rate (structural; arena only made misses cheaper)

`large-only 8T` is 0.07–0.11x mimalloc: shard hit rate under 32K–256K
uniform variance, not mmap cost. Options in order:
1. Deeper exact pools (slots are static-cheap: 64→256/shard hot+cold ≈
   65 KiB) + measure. Revert if flat.
2. Spans-for-big-sizes: extend span machinery past the 65472 block cap.
   Requires sub-header redesign (blocks bigger than a 64 KiB chunk can't
   dodge per-page headers — chunk-group headers or whole-span carve with
   end-header + size-aligned spans). Biggest change on this list; needs
   its own design doc + carving proofs + debug validators before code.
3. Do NOT raise caps blindly: retention is already hundreds of MiB; RSS
   discipline matters more than the last 10% hit rate here.

## 5. spawn-churn per-op + exit latency (0.22x mimalloc)

Decomposed via `spawn-empty` (spawn ≈ 40µs for everyone): the rest is
refill frequency on fresh-thread bins + exit-flush grouping/locks/
discards. Levers: refill batch sizing for cold threads, exit-flush chunk
tuning, cold re-carve cost (init loop per page). Measure each with the
spawn-empty baseline subtracted; stop when per-thread overhead is within
2x of spawn cost itself.

## 6. mixed-all 8T per-op latency (~0.7x mimalloc)

Blocked on profiling, not ideas: no `perf`/PMU on the dev box. Get
`perf stat`/`annotate` (or macOS Instruments) into CI first, then work
the profile — candidates are refill batching, free-path load chains
(`of`+`contains`), and TLB behavior on scattered spans, in that order of
suspicion. No blind experiments (the count-cap regression taught this).

## 7. Correctness backlog (must clear before 0.2)

* **Realloc zero-size dangling bug (flagged, pre-existing):**
  `GlobalAlloc::realloc` same-class identity with `layout.size() == 0`
  can return the dangling pointer for nonzero `new_size`. Fix + targeted
  test (`realloc(dangling_0_layout, 8)` must not return the dangling).
* **Miri + fuzz on new code:** span carving/lookup, exit-hook paths,
  arena commit/release/hole logic, `large_header_of` validation.
  Miri needs nightly + mocked syscalls for map/unmap (existing pattern);
  add fuzz targets for alloc/free/realloc sequences crossing small/
  medium/large boundaries.
* **Windows Fls path is reasoning-only.** Never executed here. Needs
  Windows CI running `thread_exit` + stress + benches before any release
  claims cover it.
* **jemalloc comparator** needs `make`/autoconf (absent here) — CI-only,
  then re-score all 11 workloads against it.

## 8. Cross-platform + release 0.2 checklist

* Re-verify the README Windows table post-spans/cold/exit/arena/mutex
  (every number in it predates this work). Publish a Linux table next to
  it with the ≥2 s × 3 reps methodology note.
* macOS numbers (allocator + exit hook + pthread mutex all have
  macOS-specific branches: `MAP_ANONYMOUS` value, `pthread_key_t` width,
  `MAP_FIXED` without `NOREPLACE`).
* 32-bit build + arena fallback tests in CI.
* CHANGELOG 0.1.0 entry, API review (telemetry array already grew pre-1.0
  — acceptable, note it), version-bump hygiene, `cargo publish --dry-run`.
* External audit scoping for the unsafe core (page/heap/cache/lib
  unsafe blocks + new arena/exit code). Not a launch blocker for 0.2
  (README already says unaudited), but schedule it.

## Explicitly out of scope

* NUMA awareness, huge pages, `Allocator` trait impl (DESIGN non-goals).
* jemalloc-style background purge threads or decay timers (our caps +
  cold retention cover the steady state; revisit only on month-long
  RSS-growth evidence).
* OOM policy beyond returning null.
