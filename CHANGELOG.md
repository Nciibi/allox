# Changelog

## 0.1.0 (unpublished)

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
- Diagnostic counters (`__diagnostics::volume()`, always compiled): thread-cache
  refills and flushes, trims, ownership probes, remote frees, retired/adopted
  caches, software zeroing, discards (purges), arena fallbacks and parks, and
  realloc relocations with copied bytes. Slow-path counters cost two relaxed
  adds; the realloc ones batch through thread-local accumulators because a
  shared atomic per realloc measured -20% at four threads.
- `telemetry::timing()` (telemetry builds only, since it needs a clock):
  cumulative lock-wait, discard and thread-exit-flush nanoseconds.
- Small-tier fast path: the virgin count moved into the per-class `Bin`
  (it fits the existing padding), so a small allocation touches one cache
  line instead of two, and `alloc` tests the small tier first with a single
  comparison. Neutral on the allocator-loop matrix, +2.5%/+1.3% on the
  process-global application benchmark at 1/4 threads.
- Opt-in telemetry feature with per-class histograms (~4% worst-case
  overhead, zero when disabled). NOTE: the telemetry array dimension
  covers small + medium + big classes on arena targets and may still
  grow pre-1.0.
- Backends: Windows (VirtualAlloc), POSIX (mmap), wasm32 (memory.grow).
