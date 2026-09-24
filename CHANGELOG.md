# Changelog

## 0.1.0 (unpublished)

Initial release. Pure Rust, zero dependencies, no build script. MSRV 1.79.

- Small allocations (16 B–16 KiB, ~12.5% classes): lock-free per-thread
  caches with batched refill, sharded per-class heap, empty + cold
  (discarded-physical) page retention, virgin-page zero-init fast path.
- Medium allocations (up to 65472 B): multi-page spans with sharded
  span heap, exact-fit selection, cold-span retention.
- Big allocations (65473–524288 B, arena-backed on unix+std): multi-page
  spans with one meta chunk + pure data chunks (contiguous blocks across
  64 KiB boundaries), per-class sharded heap, page-indexed side-table
  lookup, cold-span retention, and a per-class active-span fast path for
  same-span refills. Non-arena targets fall back to the large path. (Sizes
  past 524288 B and over-aligned requests stay on the large path.)
- Large/over-aligned: sharded exact-fit-first region caches with cold
  tier, per-thread stash, virtual-memory arena backing (unix) with hole
  reuse and graceful legacy fallback. Large `calloc` skips redundant zeroing
  only after a successful discard, and arena-frontier large realloc can grow
  in place without copying.
- Thread-exit flush (pthread key / FlsAlloc); contention-parking mutexes
  (SRWLock on Windows, pthread on unix). Pressure-gated remote-free drift
  cap: under cache pressure, frees of blocks owned by another thread are
  counted and shed in batch via the existing chunked trim (never a
  lock-per-free release); ownership is a performance heuristic only.
- C ABI (`malloc`/`calloc`/`realloc`/`free`/`aligned_alloc`, zero sizes
  return null), `GlobalAlloc` impl with layout-routed free, `usable_size`,
  debug double-free/corrupt-pointer validation.
- Layout-routed frees derive size class from the layout LUT instead of
  loading a page/span header (~48% of free-path cycles on mixed-all
  before the change — `perf annotate`).
- Opt-in telemetry feature with per-class histograms (~4% worst-case
  overhead, zero when disabled). NOTE: the telemetry array dimension
  covers small + medium + big classes on arena targets and may still
  grow pre-1.0.
- Backends: Windows (VirtualAlloc), POSIX (mmap), wasm32 (memory.grow).
