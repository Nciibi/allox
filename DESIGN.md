# `allox` — Design Document

A production-grade, pure-Rust memory allocator. Zero dependencies, zero build
scripts, zero C toolchains. Works everywhere `rustc` does.

## 1. Why this exists

| Existing option | Problem |
|---|---|
| System allocator | Slow under threads on Windows (HeapAlloc lock); no control |
| `jemallocator`, `mimalloc` crate, `rpmalloc` | Require a C toolchain per target; break cross-compilation; `cc` in build.rs |
| `talc`, `galloc`, `dlmalloc-rs` (pure Rust) | Single global heap / linked-list designs; little or no thread caching, no virtual-memory integration, weak multi-core scaling |

Niche: mimalloc-class *thread-cached page allocator* with none of the C
baggage. Secondary differentiators: built-in observability, fuzz/Miri-first
testing culture, Windows treated as first-class.

## 2. Goals / Non-goals

Goals:
- G1 Correctness first: sound for every `GlobalAlloc` contract; Miri-clean;
  randomized stress tests in CI.
- G2 Zero dependencies, zero `cc`. Stable Rust, MSRV 1.70.
- G3 Fast single-thread fast path: TLS read + pop + a couple of branches.
- G4 Multi-core scaling: thread-local caches; locks only on slow paths.
- G5 Cross-platform: Windows, Linux, macOS out of the box.
- G6 Observable: live stats API at near-zero cost.
- G7 `#[global_allocator]` + C ABI (`allox_malloc/free/calloc/realloc/aligned_alloc`).

Non-goals (v1): NUMA awareness, huge pages, `Allocator` trait impl,
wasm32 (later), OOM policy beyond returning null.

## 3. Research summary

### 3.1 What makes mimalloc fast (MSR-TR-2019-18)
1. Free-list sharding: one free list per 64 KiB page, not per size class ->
   allocations stay local in memory (>25% win measured in Lean).
2. Three lists per page: allocation list, local-free list, atomic cross-thread
   free list (single CAS per remote free; contention spreads over thousands of
   lists).
3. Temporal cadence: the allocation list empties periodically, forcing a slow
   path that amortizes maintenance work.
4. O(1) pointer -> metadata by masking address bits (aligned segments).
5. No bump pointer: pages are born with a full free list; one code path.

### 3.2 Competitor landscape (2026)
- talc: best pure-Rust no_std allocator; its own docs recommend
  jemalloc/mimalloc for hosted systems. Linked-list + binning, one heap.
- lol_alloc / galloc / rlsf: simplicity/embedded oriented; not competitors.
- jemallocator-rs / mimalloc-rs: perf gold standard; require C toolchain.

### 3.3 Rust-specific constraints (verified)
| Constraint | Consequence |
|---|---|
| Unwinding across `GlobalAlloc` is UB | Internal failures -> null or abort, never panic |
| `std::sync::Mutex` may allocate -> recursion hazard | Ship our own tiny mutex (SRWLock / pthread / spin) |
| TLS-with-Drop from `GlobalAlloc` historically crashes (rust#116390); Windows loader-lock deadlocks in TLS dtors | Const-initialized TLS, no destructor; explicit flush API |
| Optimizer may assume allocations never happen | Never make `GlobalAlloc` behavior depend on side effects |
| `Layout::align` can exceed block alignment | Over-aligned requests routed to dedicated mapped regions |

## 4. Architecture

```
+----------------------------------------------------------+
| Public API: Allox (GlobalAlloc) . C ABI . stats          |
+----------------------------------------------------------+
| cache: ThreadCache - per-thread bins, intrusive freelist |
+----------------------------------------------------------+
| heap: GlobalHeap - partial-page lists, mutex             |
+----------------------------------------------------------+
| page: PageHeader (64 KiB pages, one size class each)     |
+----------------------------------------------------------+
| sys: map/unmap - VirtualAlloc | mmap                     |
+----------------------------------------------------------+
```

### 4.1 Memory layout

Small allocations (size <= MAX_SMALL = 16 KiB, align <= 16):

```
64 KiB PAGE (OS-mapped, 64 KiB-aligned)
+--------------+-------------------------------+
| PageHeader   | b0 | b1 | ... blocks class k  |
| magic,class  |                               |
| used,freecnt | free blocks: word0 = next ptr |
| prev,next    |                               |
+--------------+-------------------------------+
```

- Pointer -> header: `p & !(PAGE_SIZE - 1)`. No lookup tables.
- Page is in a doubly-linked partial list iff it has spare free blocks.
- `used` counts blocks held outside the page (in caches or live). A page is
  unmapped only when `used == 0`, so no page is ever unmapped while any thread
  still caches one of its blocks.

Large / over-aligned allocations (>16 KiB or align > 16):

```
mapped region (multiple of 64 KiB)
+-------------+---------------+------------------+
| LargeHeader | pad to align  | user memory      |
| LARGE_MAGIC |               |                  |
| mapped_size |               |                  |
+-------------+---------------+------------------+
user_ptr = align_up(base + HDR_SIZE, align)   // recomputed on free
```

`dealloc(p)` reads the magic at `p & !PAGE_MASK` and dispatches:
`PAGE_MAGIC` -> small path, `LARGE_MAGIC` -> unmap, else -> corrupt-pointer
abort. Debug builds additionally walk the page freelist to catch double frees.

### 4.2 Size classes
Generated at compile time: start 16 B, grow ~12.5% rounded up to 16 B, cap
16384 B (~58 classes). Internal fragmentation <= 12.5%, same bound as
mimalloc/tcmalloc. Minimum block = 16 B = max useful fundamental alignment.
A 1 KiB direct-mapped table (index `(size+15)/16`) turns class lookup into
a shift and a load; measured necessary after the scan version showed up in
mixed-size profiles.

### 4.3 Allocation paths

```
alloc(size, align):
  size==0             -> dangling(align)
  align>16 or >16 KiB -> large_alloc: map region, write LargeHeader
  else:
    p = cache.bin[class].pop()            -- FAST PATH
    if p == null:
      page = heap.acquire_page(class)     -- lock, or fresh map
      move up-to-32 blocks page->bin      -- batching amortizes the lock
      p = bin.pop()
    calloc: explicit zeroing (recycled pages are not zeroed)

dealloc(p):
  base = p & !(PAGE_SIZE-1); hdr = *base
  PAGE_MAGIC: validate(debug) ; cache.bin[hdr.class].push(p)
              if bin.len > LIMIT: flush grouped by page under heap lock
  LARGE_MAGIC: sys.unmap(base, hdr.mapped_size)
  else: abort (corrupt pointer)
```

Ownership rule (v1): a freed block goes to the *freeing* thread's cache
regardless of which thread allocated it. Blocks carry no affinity; correctness
never depends on ownership, only performance.

**Cache retention policy (measured, important):** freed blocks are extremely
likely to be re-allocated by the same thread; round-tripping them through the
global heap costs ~5x on mixed workloads (lock + list surgery + re-carve).
Therefore thread caches grow without per-bin limits and are trimmed only when
the thread's aggregate cached bytes exceed THREAD_CACHE_BUDGET (64 MiB),
halving the largest bin first. Worst-case overhead: budget bytes per thread.
Trim passes are chunked (2048 blocks) to bound stack use for huge bins.

realloc: same-class small resize is identity; otherwise alloc-copy-free.
alloc_zeroed: alloc + explicit zero (OS-zero guarantee only holds for fresh
pages; recycled cache memory must be zeroed in software).

Thread-local state is const-initialized `UnsafeCell<ThreadCache>` — no lazy
init, no destructor, no borrow-flag cost on the fast path. Aliasing is sound
because the allocator never invokes user code while the cache is borrowed,
so reentrant allocation cannot occur.

### 4.4 Concurrency model
- Sharded locking: each of the ~64 size classes has its own mutex guarding its
  partial-page list; no code path ever holds two class locks at once. Thread
  caches are lock-free; contention only on batched refill/trim (amortized
  ~64 blocks per lock acquisition, spread over 64 independent locks).
- `sys::Mutex`: on Windows, an SRWLock — contended threads park in the kernel
  instead of burning CPU (SRWLOCK is a zero-initialized pointer, so it stays
  const-constructible). On other platforms, adaptive spin with bounded
  spinning and OS yield where available. It can never allocate, eliminating
  the recursion hazard inside `GlobalAlloc`.
- Note: because freed blocks go to the *freeing* thread's cache (no block
  ownership), mimalloc-style atomic cross-thread free lists are unnecessary;
  lock sharding addresses the remaining contention directly.

### 4.4.1 Measured results (Windows x86-64, median of 5)
See README.md for the full four-way table (allox / talc / dlmalloc / system).
Headline: 5/5 wins vs all comparators; multi-threaded wins are structural
(lock-free fast paths vs a single global heap mutex). Lessons worth preserving:
1. Flush/refill round-trips dominated mixed workloads until caches became
   byte-budgeted and effectively unlimited per thread.
2. Benchmark harnesses must keep their tracking structures cache-resident,
   or they measure themselves (an early run understated our single-threaded
   speed by ~10x due to random access into a 24 MB live-set vector).

PageHeader (repr(C, align(16)), 48 bytes):
magic: u64, prev/next: *mut PageHeader, free_head: *mut u8,
free_count: u16, used: u16, class: u16, flags: u16.

ThreadCache: `[Bin; N_CLASSES]` where Bin { head: *mut u8, len: u32 },
plus total count. Const-initializable so TLS needs no lazy init or Drop.

### 4.5 The thread-exit problem (deliberate trade-off)
TLS destructors from inside an allocator risk rust#116390-style breakage and
Windows loader-lock deadlocks. Decision: const-initialized, destructor-less
TLS. Cached blocks of dead threads stay accounted as `used` on their pages
until those pages are naturally reclaimed; `allox_flush_thread()` exists for
explicit reclamation (documented for thread-pool users). Revisit with a
best-effort try-lock flush registered via a mechanism outside TLS dtors.

## 5. Public API

```rust
pub struct Allox;                       // ZST, impl GlobalAlloc
pub fn flush_current_thread();          // return cached blocks to pages
pub mod stats { pub struct Stats {...}; pub fn snapshot() -> Stats; }
// C ABI (#[no_mangle], extern "C")
pub extern "C" fn allox_malloc(size) -> *mut c_void;
pub extern "C" fn allox_calloc(nmemb, size) -> *mut c_void;
pub extern "C" fn allox_realloc(ptr, size) -> *mut c_void;
pub extern "C" fn allox_free(ptr);
pub extern "C" fn allox_aligned_alloc(align, size) -> *mut c_void;
```

## 6. Testing strategy

1. Unit tests per module (classes monotonic, page carving, list ops).
2. Integration tests using Allox as `#[global_allocator]`: std collections,
   String/Vec churn across threads.
3. Randomized stress test (xorshift PRNG, no deps): thousands of live
   allocations, random sizes 1..=200_000, checksum verification, multi-thread.
4. Miri: `cargo +nightly miri test` on the non-OS-touching core (sys mocked).
5. Fuzzing (cargo-fuzz) around alloc/dealloc sequences in debug validation mode.
6. Benchmarks (criterion, dev-dep only): vs System, talc, jemallocator on
   xmalloc-like threaded workload; tracked in CI artifacts.
7. Self-hosting smoke: build/test real crates with Allox as global allocator.

## 7. Milestones

- M0 Scaffold: Cargo.toml, sys layer (win/unix), mutex, CI config. ✅
- M1 Single-threaded correctness: classes, pages, small/large paths,
  aligned/calloc/realloc, unit + integration tests green. ✅
- M2 Multi-threaded: ThreadCache, global heap, flush, stress tests. ✅
- M3 Hardening: debug double-free detection, stats API, Miri clean,
  fuzz targets, MSRV check. ✅ (Miri via CI; local fuzz target pending)
- M4 Performance: sharded per-class locks, direct-mapped class LUT,
  byte-budgeted thread caches, comparative benchmark rig vs talc/system
  with median-of-N scoring. ✅ Result: 5/5 wins vs talc (1.6x–100x+)
  and 5/5 vs the system allocator on Windows x86-64.
- M5 Release: cross-platform CI results, README scoreboard (done),
  API review, publish to crates.io as `allox`. 🟡 in progress

## 8. Module layout

```
src/
  lib.rs        public API, GlobalAlloc impl, docs
  ffi.rs        C ABI exports
  classes.rs    size class table + lookup
  page.rs       PageHeader/LargeHeader, carving, list ops
  cache.rs      ThreadCache (bins, refill, flush)
  heap.rs       GlobalHeap: partial lists, acquire/release, stats atomics
  sys/mod.rs    map/unmap/mutex abstraction
  sys/windows.rs  VirtualAlloc/VirtualFree/SRWLock
  sys/unix.rs     mmap/munmap/pthread_mutex
tests/
  basic.rs  global_alloc.rs  stress.rs  ffi.rs
benches/  alloc.rs (criterion)
fuzz/     alloc_seq.cc
```

## 9. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Subtle UB in unsafe code | Miri + fuzz + conservative design (few invariants, all local to a page) |
| Memory bloat from dead-thread caches | Documented flush API; M4 reclamation pass |
| Lock contention worse than expected | Batch sizes tunable; M4 removes lock from steady state |
| Windows VirtualAlloc 64 KiB granularity waste | Already uniform: we always operate in 64 KiB units |
| Name/API churn before release | v0.x semver; API frozen at 0.1 review |

### 4.6 Zero-init and page lifetime
- Delayed reclamation: a page whose `used` hits zero is parked on its class'
  empty-page list (cap 4/class) and recycled by the next refill; only the
  oldest pages beyond the cap are unmapped. Churn workloads avoid
  map/unmap syscalls entirely.
- Virgin tracking: `FLAG_VIRGIN` means "no block of this page was ever
  allocated-and-freed", so all free blocks are OS-zero *except their first
  word*, which holds the intrusive freelist link. `alloc_zeroed` on virgin
  blocks clears just that word; otherwise it memsets the full block.
  The flag is cleared whenever any block is returned to the page.

Rejected optimization, recorded deliberately: in-place realloc growth into
the adjacent free block requires taking the class lock to inspect the page
free list (it is lock-protected), so every grow pays a lock acquisition to
sometimes avoid a copy - expected net loss; alloc-copy-free stays.
