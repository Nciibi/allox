# Changelog

## 0.1.0 (unpublished)

Initial release.

- Thread-cached page allocator: 64 KiB pages, ~12.5% size classes (16 B to 16 KiB), direct-mapped class lookup.
- Lock-free per-thread fast paths; sharded per-class heap mutexes; batched slow paths.
- Delayed page reclamation and virgin-page zero-init fast path for calloc.
- Large/over-aligned allocations via directly mapped tagged regions; invalid frees abort; debug double-free detection.
- Backends: Windows (VirtualAlloc), POSIX (mmap), wasm32 (memory.grow).
- Opt-in telemetry feature with per-class histograms (~4% worst-case overhead, zero when disabled).
- Pure Rust, zero dependencies, no build script. MSRV 1.79.
