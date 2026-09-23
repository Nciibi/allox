# Changelog

## 0.1.0 (unpublished)

Initial release. Pure Rust, zero dependencies, no build script. MSRV 1.79.

- Small allocations (16 B–16 KiB, ~12.5% classes): lock-free per-thread
  caches with batched refill, sharded per-class heap, empty + cold
  (discarded-physical) page retention, virgin-page zero-init fast path.
- Medium allocations (up to 65472 B): multi-page spans with sharded
  span heap, exact-fit selection, cold-span retention.
- Large/over-aligned: sharded exact-fit-first region caches with cold
  tier, per-thread stash, virtual-memory arena backing (unix) with hole
  reuse and graceful legacy fallback.
- Thread-exit flush (pthread key / FlsAlloc); contention-parking mutexes
  (SRWLock on Windows, pthread on unix). (Remote-free drift caps are
  *not* in this release — see ROADMAP_TO_BEST.md P1 step 4.)
- C ABI (`malloc`/`calloc`/`realloc`/`free`/`aligned_alloc`, zero sizes
  return null), `GlobalAlloc` impl with layout-routed free, `usable_size`,
  debug double-free/corrupt-pointer validation.
- Opt-in telemetry feature with per-class histograms (~4% worst-case
  overhead, zero when disabled). NOTE: the telemetry array dimension
  already covers small + medium classes and may still grow pre-1.0.
- Backends: Windows (VirtualAlloc), POSIX (mmap), wasm32 (memory.grow).
