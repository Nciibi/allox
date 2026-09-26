# Changelog

## 0.1.0 — 2026-09-26

Initial release. Pure Rust, zero dependencies, no build script. MSRV 1.79.

- Small allocations (16 B–16 KiB, ~12.5% classes): lock-free per-thread
  caches with batched refill, sharded per-class heap, empty + cold
  (discarded-physical) page retention, virgin-page zero-init fast path, and
  a runtime static class-size table to avoid per-free const-table copies.
- Medium allocations (up to 65472 B): multi-page spans with sharded
  span heap, exact-fit selection, cold-span retention.
- Big allocations (65473–1 MiB, arena-backed on unix+std): multi-page
  spans with one meta chunk + pure data chunks (contiguous blocks across
  64 KiB boundaries), per-class sharded heap, page-indexed side-table
  lookup, cold-span retention, and a per-class active-span fast path for
  same-span refills. Non-arena targets fall back to the large path. (Sizes
  past 1 MiB and over-aligned requests stay on the large path.)
- Medium and big thread-cache blocks receive a separate 2x retention allowance
  while the small/remote-free budget remains independently bounded.
- Large/over-aligned: sharded exact-fit-first region caches with cold
  tier, adaptive 32/64 MiB per-thread stash, 32 MiB hot shards, virtual-
  memory arena backing (unix) with hole reuse and graceful legacy fallback.
  Large `calloc` discards exclusively-owned recycled regions when possible;
  arena-frontier large realloc can grow in place without copying.
- Growable extents for packed big blocks: the first cross-class growth
  promotes once into a slack reserve (8x the new size, capped at the big
  cap, virtual-only so RSS still tracks live demand) and every later
  growth in the chain then runs in place — no copy, no syscall, no span
  traffic per step. `ecs 8T` 9.6x faster in a fresh 2 s x 3 A/B
  (9.59 -> 92.09 M/s) with peak RSS 19.7 -> 12.2 MiB, turning the last
  known loss (0.06x the system allocator's zero-copy `mremap` growth) into
  a 1.2x win. Arena targets only; everywhere else the existing
  alloc-copy-free path is unchanged.
- Thread-exit retirement (pthread key / FlsAlloc) uses a bounded fixed-slot
  queue; an empty worker can adopt a queued cache directly, otherwise later
  allocator slow paths reclaim it, with synchronous flush fallback for
  oversized/overflow caches. Small-page exit flushes use tail-aware grouped
  release and reinitialize fully-free pages lazily. Contention-parking
  mutexes (SRWLock on Windows, pthread on unix). Pressure-gated remote-free
  drift cap: under cache pressure, frees of blocks owned by another thread are
  counted and shed in batch via the existing chunked trim (never a lock-per-free
  release); ownership is a performance heuristic only.
- C ABI (`malloc`/`calloc`/`realloc`/`free`/`aligned_alloc`, zero sizes
  return null), `GlobalAlloc` impl with layout-routed free, `usable_size`,
  debug double-free/corrupt-pointer validation.
- Layout-routed frees derive size class from the layout LUT instead of
  loading a page/span header (~48% of free-path cycles on mixed-all
  before the change — `perf annotate`).
- Diagnostic counters (`__diagnostics::volume()`): thread-cache refills and
  flushes, trims, ownership probes, remote frees, software zeroing, arena
  fallbacks and parks, and realloc relocations with copied bytes. The
  per-event counters are telemetry-gated and batched per thread (one
  thread-local add per event, published every 8192); only the cold ones —
  purges, exit flushes, retired/adopted caches — are always on. This gating
  is load-bearing: a shared atomic per event cost 33% on `mixed-all 8T` and
  36% on `large-only 8T`.
- `telemetry::timing()` (telemetry builds only, since it needs a clock):
  cumulative lock-wait, discard and thread-exit-flush nanoseconds.
- Small-tier fast path: the virgin count moved into the per-class `Bin`
  (it fits the existing padding), so a small allocation touches one cache
  line instead of two, and `alloc` tests the small tier first with a single
  comparison. Neutral on the allocator-loop matrix, +2.5%/+1.3% on the
  process-global application benchmark at 1/4 threads.
- **Frameless fast paths.** The small/medium `alloc`, `free` and `realloc`
  fast paths now contain no calls, so they compile to leaves with no stack
  frame and no callee-saved registers; every slow path (refill, medium/big
  refill, drift-cap owner probe, exit-hook install, trim) is
  `#[inline(never)]` and reached by a tail jump. The cause was mechanical,
  not algorithmic: the slow paths were marked `#[inline]`, so inlining them
  into the fast paths forced two-to-five `push`/`pop` pairs and a 24-byte
  frame onto *every* small allocation and free to hold state that path never
  executes. A `call` to the exit-hook installer alone was enough to make the
  allocator spill every live value around it. Also removed: the bin-array
  bounds check, by masking the class index (a provable no-op, since the LUT
  and its saturating branch already yield an index below `NUM_CLASSES`) so
  the optimizer can see the range; and the per-block page-owner claim in the
  small refill, which re-derived the page header for every block in a batch
  that `fill_from_list` had carved from a single page.
  Measured (paired A/B, two prebuilt binaries, order-alternating, 3 reps,
  `taskset -c 0-7`): +5.9% on the process-global application benchmark at
  both 1 and 4 threads, and across the direct-call matrix `tight-small 8T`
  +6.6%, `request 8T` +18.4%, `json-ish 8T` +4.5%, `tight-small 1T` +3.5%,
  `ecs 8T` +1.6%, `mixed-small 8T` +1.3%, `mixed-all 8T` +0.7%,
  `medium-only 8T` +0.6%, `prodcons 8T` -1.5% (inside its documented ±15%
  bimodal spread, with slightly lower peak RSS). No workload regressed
  outside its own noise band. `spawn-churn` is bimodal on a loaded host in
  every build and is not usable as a gate without a quiet box.
- **Single-visit `realloc`.** A cross-class resize inside the small tier
  resolves both classes once and performs the allocate, copy and free under
  a single thread-cache visit, rather than redoing the tier dispatch, the
  class LUT and the TLS read for each half. Application containers double,
  so ~27% of an allocation-heavy workload's allocations arrive here.
  Zero-size layouts keep their dangling-pointer contract (allocate, copy
  nothing, free nothing). `realloc`'s identity test stays in the inlined
  fast path and everything that can move memory is out of line, so an
  identity resize no longer pays a five-register frame.
- **The two `calloc` counters are no longer shared atomics.**
  `zeroed_calls` and `zeroed_bytes` were the last per-event counters still
  updated unconditionally, with a `lock xadd` pair on *every* non-virgin
  `calloc`/`alloc_zeroed` call. A software-zeroing memset is per *call* for
  `calloc`-heavy code, not per page, so the plan's "only genuinely cold
  counters stay always-on" did not hold for these two — and this is precisely
  the shape that cost 33% on `mixed-all 8T` when the volume counters had it
  (REMAINING_PLAN 4d). They now accumulate in the thread cache's existing
  telemetry-gated `Pending` batch and publish every 8192 events, so a default
  build compiles them away entirely. **Consequence for consumers:**
  `__diagnostics::volume().zeroed_calls` / `.zeroed_bytes` now read 0 unless
  the crate is built with `--features telemetry` (the field positions in
  `VOLUME_FIELDS` are unchanged, so the positional ABI is intact).
  Measured (paired A/B, two prebuilt binaries, order-alternating, 3 reps) on a
  new `calloc`-churn workload added for this: **`zeroed-small 8T` +252%
  (30.0M -> 105.7M ops/s)** and `zeroed-small 1T` +6-13%, with the guard rows
  flat. The 8T figure is the contended case: eight threads were each doing
  ~26M `lock xadd` pairs per second onto one shared cache line.
- Measured and rejected: removing the per-operation `cached_bytes` byte
  accounting from the small fast paths. It is the obvious suspect for the
  remaining `zeroed-small 8T` gap — the same shape as the counter change
  above, and 4 of ~27 fast-path instructions. Ablated and paired A/B'd over
  5 reps, it is worth nothing: `tight-small 8T` -0.8%, `tight-small 1T`
  +2.5%, `mixed-small 8T` +3.0%, `request 8T` +1.3%, `json-ish 8T` -3.0%,
  i.e. noise in both directions. The byte counter is an independent
  accumulator off the critical dependency chain, so it never stalls the
  core. Recorded in REMAINING_PLAN 4d so the idea is not re-derived.
- Two new benchmark workloads, `zeroed-small 1T` and `zeroed-small 8T`:
  `calloc` churn sized to recycle, so every allocation comes from
  non-virgin memory and takes the software-zeroing path. The matrix had
  `zeroed-large` but nothing covering the small/medium zeroing path, which
  is where the counter change above lives.
- `tests/basic.rs::realloc_cross_class_shrink_relocates_so_the_free_still_routes`
  pins an invariant that looks like a missed optimization. mimalloc returns
  the same pointer for a cross-class `realloc` shrink within half a block;
  allox must relocate, because its free path is layout-routed rather than
  page-resolved, so the caller's post-`realloc` layout has to keep naming the
  block's own class. Returning `p` for a 33248-byte class-5 medium block
  shrunk to 24576 would route the caller's `dealloc` to a different class,
  which the debug validator aborts on and which in release corrupts
  `SmallBin`/`Bin` `virgin` accounting and can hand OS-dirty memory to a
  later `alloc_zeroed`. See REMAINING_PLAN 4d.
- Fixed the `wasm32-unknown-unknown` build: `tls::imp::flush` called
  `cache::reclaim_all` and `tls::imp::retire` called `cache::retire`
  unconditionally, but both are gated on `any(unix, windows)`, so the
  library did not compile for wasm32 despite the documented support. Both
  call sites now carry the same gate.
- `examples/app_workload.rs` can dump the per-class allocation histogram and
  allocation totals from `telemetry` (build with `--features app-allox,
  telemetry`); this is what identified the application workload as a pure
  small-tier shape (98.9% of allocations in classes 0–11, 16 B–192 B) with
  no medium, big or large traffic at all.
- Opt-in telemetry feature with per-class histograms (~4% worst-case
  overhead, zero when disabled). NOTE: the telemetry array dimension
  covers small + medium + big classes on arena targets and may still
  grow pre-1.0.
- Backends: Windows (VirtualAlloc), POSIX (mmap), wasm32 (memory.grow).
