# Allox performance upgrade plan

## Verdict

Allox is not broadly slower. The checked-in results show it winning or tying on small allocations, same-thread churn, and the current `prodcons 8T` workload. Its remaining losses are concentrated in:

1. **Medium allocations:** roughly 16–64 KiB mixed workloads.
2. **Large allocations and `realloc`:** especially the range above 1 MiB.
3. **Thread lifecycle:** short-lived threads and large first-touch/flush costs.
4. **Memory/tail behavior:** aggressive retention, synchronous purging, and incomplete latency/RSS evidence.
5. **Benchmark fairness:** the current harness can hide or distort the real gap.

The first upgrade is **measurement and correctness**, not another cache-size sweep.

---

## Current loss map

| Area | Evidence | Main reason |
|---|---|---|
| Small, same-thread | `README.md:97-105` | Already competitive or winning; protect this path |
| Small, 8-thread | `README.md:101-102` | Near mimalloc/system; avoid regressions |
| Mixed 16–64 KiB | `README.md:103-104` reports about `0.44x`/`0.72x` mimalloc | Medium span geometry, refill/flush work, owner probes, global budget loads |
| Large 32 KiB–1 MiB | `README.md:105-106` reports `0.40x`/`0.67x` mimalloc | Region-cache scans, arena reuse cost, big-tier retention, copy-based growth |
| Remote frees | `README.md:107` reports `1.20x` mimalloc, but only for one workload | Freed blocks go to the freeing thread's cache instead of an owner/page queue |
| Thread churn | `REMAINING_PLAN.md:245-266` reports roughly `0.23x` in isolation | Synchronous exit flush, eager free-list initialization, first-touch faults |
| Memory footprint | `src/heap.rs:387-399`, `src/lib.rs:257-272` | Multi-hundred-MiB/GiB virtual retention and fixed caps |
| Benchmark validity | `benches/alloc.rs:18-19`, `benches/alloc.rs:721-880` | Harness allocations use Allox for every comparator; no p99/raw data; RSS is cumulative `VmHWM` |

The checked-in numbers contain warnings about run-to-run variance and polluted profiling, so they are directional until reproduced with a clean harness.

---

# Phase 0 — Make the scoreboard trustworthy

This phase happens before structural allocator changes.

## 0.1 Fix benchmark isolation

The current benchmark installs Allox globally and then compares direct allocator calls. Tracking `Vec`s, channels, and thread bookkeeping for every comparator are allocated through Allox.

Create two benchmark modes:

### Direct allocator mode

- Use a neutral process-global allocator.
- Call each `GlobalAlloc` implementation directly.
- Keep tracking structures outside the timed region.
- Compare `alloc`, `dealloc`, `realloc`, and `alloc_zeroed` separately.

### Process-global allocator mode

- Build one binary per allocator.
- Install that allocator as the process `#[global_allocator]`.
- Run the complete application/workload in a fresh process.
- This is the only fair way to measure `Vec`, `String`, parser, and application behavior.

Additional requirements:

- One fresh process per allocator/workload/repetition.
- Warmup before measurement.
- Randomize or rotate allocator order.
- Pin threads to CPUs.
- Record CPU, kernel, compiler, target features, and allocator versions.
- Store raw JSON/CSV samples.
- Report median, confidence interval, p50/p95/p99/p99.9 latency.
- Count allocation, free, realloc, zeroed, and aligned operations separately.
- Measure current RSS after drain, peak RSS, VSZ, VMA count, minor faults, and syscalls independently.

The comparator wrappers currently forward only `alloc` and `dealloc` at `benches/alloc.rs:31-81`, while Allox overrides `realloc` and `alloc_zeroed` at `src/lib.rs:888-961`. JSON and ECS results therefore do not represent equivalent native implementations.

## 0.2 Add allocation counters

Add feature-gated counters for:

- Fast-path allocations/frees.
- Refills and trim operations.
- Bytes and blocks flushed.
- Owner probes.
- Remote-free operations.
- Page/span reuse and fresh mapping.
- Arena hole scans, exact hits, and split remainders.
- Bytes copied by `realloc`.
- Bytes zeroed by `calloc`.
- Lock acquisition/wait time.
- Purge time and bytes.
- Thread-exit flush time.
- Arena fallback rate.

Current telemetry is not sufficient: large regions returned to the per-thread stash exit before free telemetry is recorded at `src/lib.rs:531-548`.

## 0.3 Fix correctness blockers

These should be completed before trusting performance numbers.

### Fix the `owner` data race

`owner` is a plain `u32` in:

- `src/page.rs:50-53`
- `src/page.rs:121-123`
- `src/page.rs:239-241`

It is written after refill locks are released and read concurrently by pressured frees. “Benign last-writer race” is still a Rust data race and therefore undefined behavior.

Options:

1. Use a relaxed atomic owner field.
2. Move ownership writes under the owning lock.
3. Remove the heuristic entirely when replacing it with a real owner queue.

Run a concurrent refill/free stress test under ThreadSanitizer or an equivalent model.

### Arm the exit hook for free-only threads

`arm_exit_hook()` is called by slow paths, refills, trims, and large operations, but not by ordinary `dealloc`. A thread that only receives and frees pointers may never register its exit destructor.

Register the hook on the first cache mutation, including the first free, and test generations of free-only threads.

Also:

- Retry hook installation if it fails.
- Clear or update the pthread key value before destruction so the destructor is not invoked repeatedly.
- Fix the no-TLS fallback paths at `src/lib.rs:167-223`; they currently return only the first block from a refill chain and lose the rest.

### Correct the arena remainder test and implementation

`holes_take()` removes the entire selected hole at `src/arena.rs:233-264` but does not store the unused remainder.

The test named `split_remainder_stays_usable` at `src/arena.rs:652-667` can obtain the later allocation from the bump frontier, so it does not prove that the old remainder survived.

Implement real remainder insertion and assert that the later allocation uses the original address range.

### Clarify aligned `realloc`

The free-function `realloc()` allocates with ordinary `malloc()` at `src/lib.rs:1063-1070`, so an aligned pointer does not retain its alignment.

Choose one explicit contract:

- Preserve the original alignment.
- Reject aligned pointers.
- Add a separate aligned-realloc API.

Add tests for alignment after every realloc path.

### Establish real target coverage

The documented embedded/no-std promise needs verification:

- `src/sys/mod.rs:6-22` imports the `wasm` backend for non-Windows/non-Unix targets, but the module is only declared for `target_family = "wasm"`.
- Medium/big header reserve constants are hard-coded to 64 bytes in `src/classes.rs:85-95` and `src/classes.rs:229-243`, while structure size is pointer-width dependent.
- The 32-bit test at `tests/basic.rs:161-173` retains approximately 2 GiB of live allocations.
- WASM smoke testing does not install Allox as the global allocator and does not measure linear-memory growth.

Add compile and runtime matrices for Linux, Windows, macOS, 32-bit, WASM, and at least one embedded target.

---

# Phase 1 — Fix the medium allocator

This is the highest-confidence throughput opportunity.

## 1.1 Correct the capacity model

`span_pages_for()` uses aggregate space at `src/classes.rs:168-175`:

```text
64-byte header + 8 * block size
```

But medium blocks are carved independently inside each 64 KiB chunk at `src/page.rs:161-178`, with a 64-byte master header on page zero and 16 bytes on every later page.

Aggregate capacity is not actual capacity. Examples:

| Block size | Pages | Actual blocks | Address-space waste |
|---:|---:|---:|---:|
| 23,328 B | 3 | 6 | 28.8% |
| 33,248 B | 5 | 5 | 49.3% |
| 37,408 B | 5 | 5 | 42.9% |
| 42,096 B | 6 | 6 | 35.8% |
| 47,360 B | 6 | 6 | 27.7% |

First add a per-class capacity test that calculates:

```text
floor((65536 - 64) / block)
+
(pages - 1) * floor((65536 - 16) / block)
```

Do not tune refill batches until this test is correct.

## 1.2 Prototype page-local active spans

Mimalloc's main lesson is not merely “use atomics”; it is **page-local free lists with temporal maintenance**.

Allox currently detaches blocks from global pages into per-class thread bins. A bin can contain blocks from many pages, which weakens locality and complicates ownership tracking.

Add an optional active-page path:

- One active page/span per thread and size class.
- Allocate directly from that page's free list.
- Periodically collect local frees and perform maintenance.
- Keep the current detached-chain path as a fallback during the experiment.

Expected benefits:

- Better spatial locality.
- Fewer page/span header walks.
- Fewer owner probes.
- Lower dTLB pressure.
- More predictable medium refill behavior.

This is particularly relevant to the profile in `REMAINING_PLAN.md:340-348`, where medium refill and flush remain prominent.

## 1.3 Remove the medium page-tail restriction

The 64 KiB subheader design is the reason medium allocations stop at 65,472 bytes and why some classes waste 30–50% of their span.

Prototype a generalized metadata scheme where:

- Metadata lives at the span base or in an arena page map.
- Data pages do not require in-band subheaders.
- Blocks may cross 64 KiB boundaries.
- `free()` without layout uses a compact page map.
- Layout-routed Rust frees continue to use the size-derived class and avoid the lookup.

This is structurally similar to the big-span design already documented in `DESIGN_SPANS_BIG.md:117-167`.

Trade-off: C-style `free()` gets one side-table lookup, but medium allocation density and refill efficiency improve substantially. Keep the current subheader implementation for non-arena targets initially.

## 1.4 Replace fixed refill batches with byte budgets

The current medium refill batch is 16 blocks at `src/heap.rs:372-375`.

A 16-block refill is reasonable for 16 KiB blocks but excessive for 32–64 KiB blocks. A fixed 64-block experiment already regressed badly, as documented in `REMAINING_PLAN.md:350-355`.

Use a byte-based policy such as:

```text
batch_blocks = clamp(target_refill_bytes / block_size, min, max)
```

Tune the target separately for small, medium, and big tiers.

Also:

- Assign ownership once per page/span, not once per returned block.
- Replace linear group searches in `flush_bin`/`flush_mbin` with a small hash/index structure.
- Cache the budget in TLS or check the global budget periodically instead of loading it on every free.
- Track per-bin bytes so `trim()` does not repeatedly scan every class.

### Medium acceptance gates

Proposed targets after the clean baseline:

- `mixed-all 1T`: at least `0.9x` mimalloc.
- `mixed-all 8T`: at least `0.9x` mimalloc.
- `larson`, `mstress`, and size-boundary sweeps: no regression.
- At least 20% fewer medium span refills or owner probes.
- No more than 3–5% regression in small allocation throughput.
- RSS no worse than the current baseline.

---

# Phase 2 — Overhaul large allocations, arena reuse, and realloc

## 2.1 Fix arena hole management

Current arena weaknesses:

- `holes_take()` always takes the lock, even when the hole list is empty.
- It scans up to 4096 entries.
- It removes the entire hole without splitting.
- Reuse still performs a fresh `MAP_FIXED` commit at `src/arena.rs:308-318`.

Add:

1. An atomic hole-count fast path.
2. Exact-size buckets.
3. Remainder insertion.
4. Best-fit fallback only on a miss.
5. Counters for lock wait, entries scanned, exact hit, split, and lost remainder.
6. Optional coalescing only after the counters show fragmentation is real.

Do not blindly repeat the previous sharding experiment; the repository records that it was flat under the tested workload.

## 2.2 Commit larger arena granules

Allox commits 64 KiB slices. Research designs from snmalloc, rpmalloc, and PartitionAlloc use larger reserve/commit layers.

Prototype 2 MiB commit granules while retaining 64 KiB allocator pages:

- One `mmap` operation commits many pages.
- A bitmap or state table tracks committed granules.
- `madvise` can operate on selected subranges.
- Large pages can be reused without one kernel mapping operation per span.

Measure:

- `mmap` calls per allocation.
- VMA count.
- Page faults.
- TLB misses.
- RSS and VSZ.
- Reuse rate.

## 2.3 Replace linear large-region scans

`LargeRegionCache::take_fit()` scans hot and cold arrays at `src/lib.rs:298-367`.

Replace the linear exact/best-fit search with:

- Size-segregated queues.
- A radix extent index keyed by page count.
- Separate exact, larger-fit, and arena-hole queues.
- A bounded best-fit fallback.

Instrument scan length before changing the data structure. The current code has large bounded loops, but a previous deeper/sharded experiment was flat because it did not improve the actual hit rate.

## 2.4 Raise or parameterize the big-tier cap

The 1 MiB phase is implemented: `src/classes.rs:258-261` now serves the
big tier through 1048576 bytes. A separate 2x medium/big cache allowance
keeps large-allocation locality without raising the small/remote-free
budget. With `BENCH_SAFE_LIVE=1` and a 2 GiB cgroup, repeated fresh-process
2 s × 3 runs measured Allox at 6.79M/s on `large-only 1T` (2.88× mimalloc),
25.10M/s on `large-only 8T` (1.32×), and 10.73M/s on `mixed-all 1T` (2.90×),
with low Allox peak RSS. Boundary, release, telemetry-enabled, and no_std
checks are green.

Next candidates:

- Optional 2 MiB classes.
- A second large-page geometry similar to rpmalloc's 64 KiB/1 MiB/4 MiB/16 MiB tiers.

Use the same contiguous-data layout and side-table lookup already proven for big spans.

Do not increase all caps blindly; measure RSS and retained virtual memory for every threshold.

## 2.5 Add growable large extents

Allox currently grows most allocations with allocation-copy-free at `src/lib.rs:900-957` and `src/lib.rs:1063-1072`.

Implement two paths:

### Arena-backed growable extent

Store both:

```text
requested_size
reserved_capacity
committed_pages
```

Reserve virtual capacity ahead of the requested size. On growth:

- Extend into adjacent uncommitted arena capacity.
- Update metadata.
- Avoid copying.
- Fall back to normal allocation if the neighboring range is occupied.

### Standalone Linux growth

Use `mremap` only for standalone mappings, preserving alignment and updating the header. Linux documents `mremap` as a way to implement efficient `realloc`, but the previous Allox trial was flat because it only applied to the legacy path while ECS traffic stayed arena-backed.

Add separate benchmarks for:

- Geometric growth.
- Growth with adjacent free capacity.
- Growth with neighboring live allocations.
- Large shrink.
- Cross-tier growth.
- Aligned growth.

## 2.6 Improve large `calloc`

`alloc_zeroed_impl()` treats recycled large regions as dirty and memsets them at `src/lib.rs:644-672`.

Track whether a region is:

- Freshly committed and zero.
- Successfully discarded and therefore zero on supported platforms.
- Dirty and requiring memset.
- Unknown because discard failed.

This can remove unnecessary large memsets while preserving correctness on WASM and platforms where discard is a no-op.

### Large acceptance gates

- `large-only 1T`: at least `0.8x` mimalloc.
- `large-only 8T`: at least `0.9x` mimalloc.
- Large geometric realloc: at least 50% fewer copied bytes in growable cases.
- `calloc` large: at least 30% fewer zeroing bytes in cold-reuse cases.
- No RSS increase beyond the agreed cap.

---

# Phase 3 — Make remote frees robust beyond the current benchmark

Allox's current policy is documented at `DESIGN.md:179-225`:

> freed blocks go to the freeing thread's cache

That works well when the freeing thread reuses them, but it causes migration and hoarding in asymmetric workloads.

## 3.1 Add page-local remote queues

Use a hybrid design inspired by mimalloc:

- Same-thread free: ordinary thread-local/page-local push.
- Remote free: one atomic push to a per-page or per-span remote list.
- Owner periodically drains the remote list into its local free list.
- No lock on the remote free fast path.
- Owner metadata uses atomics or stable ownership tokens.

This is a better fit than retaining only the pressure-gated `owner` heuristic.

## 3.2 Batch remote frees

The BatchIt research reports meaningful producer-consumer improvements from batching frees by destination slab. Apply the same idea to Allox:

- Keep a small per-thread cache of recently seen owners/pages.
- Accumulate remote blocks locally.
- Publish a batch to the owner queue periodically.
- Bound delayed reuse and retained memory.

This should be benchmarked against the current `prodcons 8T`, not assumed to improve it.

## 3.3 Handle dead owners

If an owner exits:

- Mark its pages/spans abandoned.
- Allow another thread to claim them.
- Let remote frees either return directly to the new owner or enqueue to the abandoned structure.
- Track live objects separately from cached free objects.

This is required for real thread-pool workloads, not only the current benchmark.

### Remote acceptance gates

- Retain at least current `prodcons 8T` performance.
- Improve `xmalloc-testN`, `larson`, `mstress`, and high remote-fraction workloads.
- Target at least 20% improvement on a producer-consumer workload where Allox currently falls behind.
- Bound remote-free latency and retained memory.

---

# Phase 4 — Reduce lifecycle and first-touch cost

The current exit hook performs a blocking full flush at `src/thread_exit.rs:42-52`.

## 4.1 Lazy provisioning

PartitionAlloc's design avoids writing free-list pointers across an entire newly allocated span. It provisions slots as they are needed, reducing first-touch page faults.

Apply this to medium and big spans:

- Track unprovisioned capacity.
- Provision one 64 KiB chunk or a small batch on demand.
- Keep the unprovisioned range OS-zero.
- Maintain virgin/calloc semantics explicitly.
- Measure first allocation latency and minor faults.

This is likely more valuable than changing the mutex implementation.

## 4.2 Detach caches at thread exit

Instead of synchronously flushing every bin during thread teardown:

1. Detach the thread's `ThreadCache` in O(1).
2. Push it onto a preallocated retired-cache queue.
3. Let another thread or a maintenance worker drain it.
4. Return fully-free pages/spans in bulk.
5. Leave live allocations associated with an abandoned owner structure.

Add a bounded fallback for platforms without a maintenance worker.

## 4.3 Expand lifecycle benchmarks

Separate:

- Cache-only frees before exit.
- Explicit `flush_current_thread()`.
- Live allocations retained until exit.
- Remote frees after owner exit.
- Many short-lived threads.
- Thread pools with idle periods.

The current spawn workload explicitly frees all remaining blocks, so it does not test abandoned live memory.

### Lifecycle acceptance gates

- No linear mapping growth across thread generations.
- Full-process spawn workload at least `0.8x` mimalloc, or a documented workload-specific exception.
- Lower p99 thread-exit latency.
- Lower minor faults per short-lived thread.
- No RSS growth after retired caches are drained.

---

# Phase 5 — Memory, tail latency, and platform polish

## 5.1 Adaptive cache sizing

The current default base budget is 32 MiB per thread at `src/cache.rs:29-33`,
with a separate 2x medium/big allowance and large stash.

Adopt ideas from tcmalloc:

- Per-class maximum lengths.
- Low-water-mark tracking.
- Slow-start growth.
- Active/idle thread cache budgets.
- Global cache budget shared across active threads.
- Cache scavenging when a thread becomes idle.

This should reduce both RSS and trim churn.

## 5.2 Decay and background purge

Jemalloc's documented options separate dirty-page decay, muzzy-page decay, and background purging. Implement a safe pure-Rust equivalent:

- Mark cold pages/spans with timestamps or generations.
- Purge incrementally during allocation slow paths.
- Optionally use one hosted maintenance thread.
- Move purge work outside class locks only after exclusive ownership is transferred.
- Keep cooperative purging for `no_std` and WASM.

Do not simply move `madvise()` after unlocking; the repository already documents the race that caused live-memory corruption.

## 5.3 Metadata and TLB pressure

Possible later improvements:

- Compact metadata structures.
- Optional metadata huge-page advice where supported.
- Smaller radix metadata map for medium/large blocks.
- Cache-line separation for frequently mutated counters.
- Alignment of per-class state.

Use `dTLB-load-misses` and cache-miss measurements before and after each change.

## 5.4 Cross-platform release gates

Before calling the project finished:

- Fix the no-std backend `cfg`.
- Make 32-bit header reserves pointer-width correct.
- Add NetBSD/FreeBSD/musl/AArch64 smoke coverage.
- Build a real WASM global-allocator example.
- Measure WASM linear-memory growth.
- Add a standalone C consumer and exported-symbol smoke test.
- Clarify that “zero C dependency” means no C build dependency; hosted Unix/Windows still call OS C ABI functions such as `mmap`, `madvise`, and pthread APIs.
- Test the declared Rust 1.79 MSRV.
- Bound the 32-bit all-size test.

---

# Low-level tuning order

Only after the structural work:

1. Byte-based refill batch sizes.
2. Per-class cache limits.
3. Empty/cold retention caps.
4. Medium span geometry.
5. Large extent index shape.
6. Commit granularity.
7. `#[cold]`/`#[inline]` annotations.
8. PGO and native-target builds.

Do not prioritize these first:

- More mutex parking work: the existing Unix parking change was measured flat.
- Simply increasing medium refill from 16 to 64: it regressed by about 22%.
- Simply increasing arena hole slots: prior experiments caused abandonment and exhaustion.
- Repeating legacy-only `mremap`: prior trial was flat because arena-backed traffic bypassed it.
- Reporting one aggregate ops/s score: it hides tail latency, RSS, and operation-mix differences.

---

# Research sources

- [mimalloc design and v3 architecture](https://microsoft.github.io/mimalloc/) — page-local free lists, local/remote lists, page stealing, first-class heaps.
- [mimalloc 2026 Microsoft Research article](https://www.microsoft.com/en-us/research/blog/mimalloc-a-high-performance-scalable-memory-allocator-for-the-modern-era/) — scalability versus memory sharing, page-local allocation/free paths, thread-pool behavior.
- [Mimalloc: Free List Sharding in Action](https://www.microsoft.com/en-us/research/publication/mimalloc-free-list-sharding-in-action/) — locality and three-list page design.
- [snmalloc repository](https://github.com/microsoft/snmalloc) — owner-aware message passing and remote-free batching.
- [snmalloc paper](https://doi.org/10.1145/3315573.3329980) — batched remote deallocation design.
- [BatchIt preprint](https://www.microsoft.com/en-us/research/wp-content/uploads/2024/05/preprint_batchit.pdf) — batching remote frees by slab.
- [rpmalloc](https://github.com/mjansson/rpmalloc) and [benchmark notes](https://github.com/mjansson/rpmalloc/blob/develop/BENCHMARKS.md) — tiered page geometry, per-page remote frees, reserve/commit/decommit.
- [jemalloc manual](https://jemalloc.net/jemalloc.3.html) and [tuning guide](https://github.com/jemalloc/jemalloc/blob/dev/TUNING.md) — decay, background purging, arenas, metadata THP.
- [TCMalloc design](https://github.com/google/tcmalloc/blob/master/docs/design.md) — transfer caches, low-water marks, scavenging, adaptive thread caches.
- [PartitionAlloc design](https://chromium.googlesource.com/chromium/src/+/HEAD/base/allocator/partition_allocator/PartitionAlloc.md) — lazy provisioning and super-page layout.
- [mimalloc-bench](https://github.com/daanx/mimalloc-bench) — realistic and adversarial workloads such as `larson`, `mstress`, `rptest`, `xmalloc-testN`, and `sh8bench`.
- [`mremap(2)`](https://man7.org/linux/man-pages/man2/mremap.2.html), [`mmap(2)`](https://man7.org/linux/man-pages/man2/mmap.2.html), and [`madvise(2)`](https://man7.org/linux/man-pages/man2/madvise.2.html) — OS behavior underlying the large/realloc/purge paths.

## Recommended implementation order

1. Fair benchmark harness and counters.
2. Owner race, free-only exit hook, arena remainder, aligned realloc, and target gates.
3. Medium capacity correction and byte-based refill policy.
4. Page-local active spans and medium metadata redesign.
5. Arena hole fast path, extent index, larger commit granules, and large cache.
6. Large zeroed/realloc/alignment improvements.
7. Remote owner queues and batched frees.
8. Lazy provisioning and asynchronous lifecycle reclamation.
9. Adaptive retention, decay, metadata locality, and release/platform work.

## Phase 0 implementation status

Implemented in the first implementation pass:

- Neutral System harness allocator for direct comparator calls.
- Native `realloc` and `alloc_zeroed` forwarding for comparator wrappers.
- Warmup, raw per-run samples, JSON/JSONL output, current RSS, peak RSS, and per-run aggregate latency percentiles.
- `BENCH_FRESH=1` fresh-process JSONL mode.
- Correct Allox diagnostic gating when `BENCH_ALLOC` selects another allocator.
- Relaxed-atomic owner metadata and focused concurrency coverage.
- Free-only thread exit-hook registration, retryable hook installation, and one-shot destructor value handling.
- No-TLS refill-tail return paths.
- Arena hole remainder preservation with address-specific regression coverage.
- Aligned free-function `realloc` alignment preservation and regression coverage.
- Target-width header-reserve constants, bounded all-size roundtrip coverage, and a real WASM global-allocator smoke path.
- Large allocation/free telemetry on stash and recycle-cache paths.
- Explicit compile-time failure for unsupported bare-metal targets without a memory backend.
- Trusted arena/legacy large-allocation metadata dispatch, including fallback deallocation, forged-header resistance, symmetric requested-byte telemetry, and WASM unmap accounting.
- Producer-consumer shutdown/drain correctness and a direct exit-hook flush regression probe.

Deferred to the next Phase 0 slice:

- Per-operation p99 sampling without timing overhead in throughput runs.
- Full lock-wait, purge, and realloc-copy counters.
- Process-global application benchmark binaries.
- A real bare-metal memory backend.

## Phase 1 implementation status

Implemented and rechecked:

- Exact per-chunk medium capacity modeling shared by sizing, refill, and all-class carve tests.
- Capacity-derived refill caps, with the fixed 16-block cap retained only as an upper bound.
- One owner claim per contiguous source span during medium refills.
- Dedicated medium-only 1T/8T benchmark workloads.
- Adversarial large-header, arena-direct-large, legacy-large fallback, telemetry symmetry, and producer-consumer drain regressions.
- Thread-local page-local medium free chains with single-span refill validation, active-chain flushing, and fallback to the existing mixed-span bin when a refill crosses spans.
- Actual-capacity target-eight span geometry, with invariant tests proving every class reaches the target.
- Arena page-indexed medium metadata with cross-page blocks; legacy/non-arena spans retain the subheader fallback.
- Focused active-cache flush/reuse coverage plus randomized multithreaded stress.

The active-chain prototype measured better on the focused medium workloads and was neutral-to-positive on the 5x5-second `mixed-all 8T` comparison. Target-eight geometry improved medium-only 1T throughput, kept 8T and mixed-all within the acceptance band, and increased RSS by roughly 1–3% on the focused runs. Arena metadata removal further improved the focused medium and mixed-all runs while preserving RSS within the measured band. The first byte-budget experiment (`256 KiB` target) was measured against the capacity-derived cap and rejected: medium-only 1T/8T and mixed-all throughput regressed on this host.

## Phase 2 implementation status

Implemented the first arena-reuse and large-cache slices:

- Atomic hole-count gate avoids taking the hole mutex when no reusable holes exist.
- Bounded exact-page index accelerates common small-hole reuse, with linear best-fit fallback for large holes.
- Hole scan, hit, split, and empty-fast-path counters are feature-gated behind `telemetry` and exposed through hidden diagnostics.
- Normal builds pay only for the atomic hole-count gate; diagnostic counter atomics compile out when telemetry is disabled.
- The sharded `LargeRegionCache` now has fixed hot/cold exact-page indexes across the full cacheable page range (1,024 pages on 64-bit, 128 on narrower targets), preserving exact hot/cold precedence and the existing best-fit fallback.
- Swap-with-last removal repairs both indexes, including stale-index fallback; focused tests cover precedence, accounting, duplicates, removal, and the boundary.
- The new 5–8 MiB `huge-only 1T` workload measures the extended range; the full index measured about 12.7 K ops/s versus 12.5 K with the old 64-page bound.
- Regression coverage verifies exact reuse, stale-index fallback, remainder splitting, and counter updates.
- Frontier-adjacent large realloc growth now atomically extends only live arena regions ending at the bump frontier; both realloc entry points use it, with side-table/counter updates and copy fallback for every other case.

The 2 MiB commit-granule prototype was measured and removed. Rounding fresh arena mappings up to 2 MiB units did not reduce mapping operations for already large requests, added hole/prefix-tail bookkeeping, and regressed the focused 5–8 MiB workload from roughly 12.2 K ops/s to 10.3 K ops/s. Triggering it for 256 KiB big spans was also rejected after a roughly 7% large-only regression. A deferred coalescing retry and temporary boundary index were also rejected after capped A/B probes; the next safe direction is the narrow frontier-adjacent growth slice now implemented, not per-allocation granule overmapping.

### Baseline comparison

A controlled comparison was run against the pre-roadmap commit `1c32ee2` using the same System harness, separate fresh processes, `taskset -c 0-7`, `BENCH_SECS=3`, `BENCH_REPS=5`, and one-second warmups. The current build has telemetry-only hole counters and the full large-cache index. These are directional allocator-loop results, not process-global application benchmarks.

| Workload | Current median | Baseline median | Delta | Current p99 ns/op | Baseline p99 ns/op | Current / baseline RSS KiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed-all 8T | 17.06 M/s | 10.61 M/s | +60.8% | 62.7 | 140.2 | 24,096 / 22,064 |
| large-only 1T | 268.8 K/s | 266.7 K/s | +0.8% | 6,783.6 | 28,681.5 | 10,124 / 9,760 |
| large-only 8T | 9.08 M/s | 8.74 M/s | +3.9% | 112.4 | 118.7 | 12,408 / 11,056 |
| prodcons 8T | 27.47 M/s | 25.83 M/s | +6.3% | 38.2 | 43.6 | 15,008 / 14,664 |
| json-ish 8T | 136.07 M/s | 139.96 M/s | -2.8% | 7.5 | 7.3 | 8,104 / 7,840 |
| ecs 8T | 7.81 M/s | 7.73 M/s | +1.1% | 128.1 | 129.9 | 12,044 / 11,772 |

The pinned comparison shows the counter-gating fix removes the earlier large/ECS regression signal; large-only and ECS are neutral-to-positive, while JSON is slightly slower. The mixed result has high baseline variance, so its +60.8% median should not be treated as a stable speedup without more application-shaped runs. RSS remains modestly higher on most workloads.

A simple adjacent-hole coalescing prototype was also measured and rejected. It improved ECS 8T by 4.2% and prodcons 8T by 5.5% over the pre-coalescing build, but regressed mixed-all 8T by 5.8% and json-ish 8T by 2.1%; the unconditional neighbor scan cost more than the fragmentation reduction saved. A later deferred-trigger and temporary-boundary-index retry did not beat the no-coalescing control on capped large-only A/B probes, so no coalescing code remains. The telemetry-gated hole counters remain.
