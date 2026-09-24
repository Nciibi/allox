# Design: spans for big sizes (past the 65472 block cap)

Status: **IMPLEMENTED 2026-09-23** (plan §4 option 2 / REMAINING_PLAN
large-only option 2). Landed as `BigMaster` + `BigHeap` + arena `BIG_MAP`
side table + cache `bigbins` + dispatch routing (`alloc_impl` /
`dealloc_impl`), with `tests/big_spans.rs`, Kani proofs (P1/P2/P3/P6 +
medium packing in `src/page.rs::kani_proofs`), and CI coverage.

Validation (2026-09-23, Ryzen 5 1600, 2 s × 3 interleaved medians, full
14-workload matrix): **large-only 8T** 9.06M ops/s vs mimalloc 13.51M
(**0.67×**, was 0.05× pre-change), probe `big_maps=1308, unmaps=0,
arena_reuses=1308` — traffic served from arena-backed big spans with zero
syscalls in steady state. Guards all hold: mixed-all 8T 12.1M (in-band),
tight/mixed-small flat, spawn-churn unmaps 0, thread_exit converges,
full suite + telemetry + no_std + release green. Sensitivity: non-arena
targets (`cfg` gated out) route the same sizes through the large path and
pass the identical assertions (`tests/big_spans.rs` covers both shapes).

## 1. Problem

`large-only 8T` (32K–256K uniform variance) runs 0.05–0.18x mimalloc with
±40% run-to-run regime swings. Probes decompose it: ~58k fresh takes/s at
~19% hole reuse, ~47k unmaps/s, ~63–74k transient abandonments. Shard hit
rate under variance is the binding constraint, not mmap cost — our big
objects are individually mmap'd regions cached in exact-fit pools, while
mimalloc carves them from segments with thread-local reuse. Deeper pools
measured flat and were reverted (§4 step 1); sharded holes measured flat
and were reverted; cold-depth helped throughput (+28%) without fixing hit
rate. The remaining structural move: serve big sizes from multi-page
spans with per-thread bins, exactly as medium sizes work today.

## 2. Background: why medium stops at 65472

Medium spans (`SpanMaster` + sub-headers, `src/page.rs`) rest on one
load-bearing invariant: **a block never covers a page base**. The first
64 KiB chunk carries the 64-byte master header; every further chunk carries
a 16-byte sub-header (SUBMAGIC + master pointer) at its base; blocks are
carved per-chunk from the remaining bytes. Lookup is therefore local:
mask to the 64 KiB page, match the magic, follow at most one pointer.
The top medium class is `65536 - 64 = 65472`: the biggest block that still
fits beside headers in one chunk. Any block bigger than a chunk *must*
cover some page base — so the current scheme cannot extend past 65472, and
anything bigger needs a different lookup story. That is this document.

Ground facts (verify against code before implementing; drift kills):
- `SPAN_MASTER_SIZE` = 64, `SPAN_SUB_SIZE` = 16, `PAGE_SIZE` = 65536.
- Medium classes: ~12.5% geometric from 16 KiB to `MAX_MEDIUM_BLOCK` =
  65472; `TARGET_BLOCKS_PER_SPAN` = 8; `MEDIUM_REFILL_BATCH` = 16.
- Medium heap: per-mclass sharded locks, empty 8 spans/class + 2 MiB cap,
  cold 256 slots + 256 MiB cap, discard-under-lock with array-stored
  (base, npages).
- Arena: 16 GiB reservation (512 MiB 32-bit), 64 KiB granular, best-fit
  hole stacks with exact-early-exit, commit = 1 MAP_FIXED, universal
  legacy fallback on null. `contains()` is exact range-in-reservation.
- Dispatch (`dealloc_impl`): large-offset header first (in-bounds always),
  then masked `PAGE_MAGIC`, then `SpanMaster::of` + `contains`, else abort.
  `GlobalAlloc` paths route by contract layout with zero probing reads.
- `GlobalAlloc` routes `align > 16` (MIN_ALIGN) to the large tier always,
  so span classes only ever serve 16-aligned requests.

## 3. Requirements

1. Serve `(65472, 262144]` (phase 1; 256K–1M stays large-path — §7) with
   per-thread bins + sharded heap + cold retention, mirroring medium.
2. `free(p)` with NO layout (C ABI) must locate the owning span from the
   bare pointer without faulting and without false positives.
3. Fresh commits are virgin-zero; carving dirties only freelist-link words
   (the virgin invariant: `calloc` keeps its link-word-only fast path).
4. Blocks are contiguous user memory (non-negotiable C semantics).
5. Counter/telemetry integration: fresh-take vs reuse accounting follows
   the `fresh`-flag contract; new hidden split counters (tuning depends
   on them — the established lesson).
6. Universal fallback: arena exhaustion, non-arena targets → today's
   large path, gracefully, reversibly.
7. No measurable regression on any non-big workload (revert bar).

## 4. Options analysis

**A. Per-chunk in-band headers (REJECTED).** Reserve 16 B at every 64 KiB
base and carve big blocks around them. Breaks requirement 4: a 100 KiB
block cannot be contiguous while skipping reserved words mid-span.
Fundamental, not fixable.

**B. Size-aligned spans + class inference (REJECTED).** Align each span to
a multiple of its size; locate by masking with the class size. Requires
knowing the class at free time — available on layout-routed paths, NOT
on `free(p)`. Splits the allocator into two lookup regimes by entry
point; the C path would need a second mechanism anyway.

**C. Arena-only big spans + side table (RECOMMENDED, §5).** All metadata
lives in the first chunk; data chunks carry NO headers (blocks cross
chunk boundaries freely — requirement 4 holds). Lookup for non-first
chunks goes through a page-indexed side table covering exactly the arena
reservation. Big spans exist ONLY in the arena; exhaustion/non-arena
targets use today's large path (requirement 6 falls out naturally).

**D. Legacy big spans via over-map + masking (REJECTED).** Reintroduces
the over-map + 2-trim-munmap tax per miss that the arena was built to
kill (§1 measured it). No.

## 5. Recommended design (C)

### 5.1 Size classes and span sizing

New `BIG_CLASSES` table in `src/classes.rs`: ~12.5% geometric steps from
the first step past 65472 up to and including 262144 (about 12 classes;
generate, don't hand-list). `BIG_REFILL_BATCH` = 4 (4 × 256 KiB = 1 MiB
per refill keeps thread-cache budgets sane; measure 2/4/8 during
implementation). Span length per class covers the master header plus
`TARGET_BIG_BLOCKS_PER_SPAN` = 8 blocks, rounded up to whole pages
(same formula shape as `span_pages_for`; an 8×256 KiB span is ~2 MiB —
retention caps in §5.4 are sized for this).

Out of scope for phase 1: 256K–1M (stays large-path; revisit with its own
numbers after phase 1 validates).

### 5.2 Span layout: one meta chunk + pure data chunks

```
chunk 0 (64 KiB): [BigMaster: magic + prev + next + free_head +
                   free_count/used + bclass/flags + npages/pad = 64 B]
                  [blocks carved contiguously...           ]
chunk 1..N      : [...blocks continue across boundaries...  ]
```

- Blocks start at `base + 64`, packed contiguously (16-aligned; align ≤ 16
  guaranteed by dispatch), running to `base + npages * 64 KiB`.
- NO headers in chunks 1..N — blocks freely cover chunk bases.
- New magic `BIGMAGIC` (distinct from `PAGE_MAGIC`, `SPAN_MAGIC`,
  `SPAN_SUBMAGIC`, `LARGE_MAGIC`); new struct `BigMaster` mirroring
  `SpanMaster` (own `contains()`: magic + bclass range + npages > 0 +
  pointer strictly inside extent). Do NOT extend `SpanMaster`: its
  invariants (sub-headers, per-chunk carving) were hard-won; shared
  structs would couple both. Duplication (~100 lines) is the documented
  price, same rule as the BigHeap decision in §5.5.
- Reuse `npages: u32` width (spans to 2 MiB+ are small counts).

### 5.3 Lookup: side table over the arena range

- `BIG_MAP: [AtomicUsize; 16 GiB / 64 KiB = 262144]` = 2 MiB static,
  entry `i` = master address for arena page `i`, 0 = none. Index:
  `(p - arena_start) >> 16`, bounds-checked against the live reservation
  (never touch it for outside pointers — legacy mappings can sit
  anywhere).
- Writes happen only under the owning class lock: set at carve, rewrite
  at re-carve (same base, same entry), cleared (zeroed) on TRUE UNMAP
  only. Cold/empty parks keep entries (virtual retained, base stable).
  Reads on the free path use `Acquire` loads, no lock (the heap lock for
  the release comes after dispatch, as today).
- Stale-entry safety: an entry is read, then `contains(p)` validates
  (magic + class + range). A torn/stale read fails closed to abort, same
  as today's magic-collision philosophy. ABA across unmap→remap cycles
  is impossible: unmap clears under lock, and no live pointer into an
  unmapped span exists (used==0 pinned it).
- Legacy (non-arena) big spans DO NOT EXIST: big takes only consult the
  arena; null falls back to `map_large_region` + `LargeHeader` exactly as
  today. Non-arena targets (Windows/macOS pass-through? no — macOS has
  arena; Windows has no arena module): gate big classes on arena
  availability — without it, sizes past 65472 route to large (LUT-level
  gating, build-time cfg like the existing medium tables). 32-bit: same
  gating (512 MiB arena); fallback tests cover the shape.
- Dispatch order becomes: large-offset header → small magic →
  `SpanMaster::of` + contains → **side-table + `BigMaster::contains`** →
  abort. The new branch costs two loads + one compare on paths that
  already missed everything else; small/medium hot paths gain exactly
  one predictable branch. `GlobalAlloc` layout-routed free skips
  straight to the big path for big sizes (zero probing reads, as today).

### 5.4 Heap and caches: mirror, don't refactor

- New `BigHeap` mirroring `MediumHeap` (own per-bclass locks, own
  empty/cold arrays, same discard-under-lock + array-stored-values
  discipline — the cold-discard race fix applies from day one).
- Caps scaled for big blocks: cold slots/depth sized so the byte cap
  (not slots) binds, per the cold-slots lesson (§4: slots must not bind
  below byte caps); empty retention byte-scaled as medium's
  `MAX_EMPTY_SPAN_BYTES_PER_CLASS` is. Propose starting values in
  implementation, tuned by bench, with RSS assertions (per-class cold
  bytes × classes must stay sane against the hundreds-of-MiB rule).
- Thread-cache bins: extend the medium-bin arrays (or parallel big-bin
  arrays) with `mvirgin`-style tracking resized to the new dimension;
  refill splits first/rest identically. Budget accounting unchanged.
- Big allocations use a per-class active span as the first cache tier. A
  same-span refill keeps the detached chain in the active list, avoiding the
  bin head push/pop path; mixed-span refills remain in the ordinary bin.
  Frees whose pointers fall inside the active span return directly to it, and
  trim/flush release the active chain back to `BigHeap` before returning
  blocks to the bin. The active span carries the same virgin accounting and
  is bounded by the existing cache byte budget.
- Telemetry `per_class` dimension grows by `NUM_BIG` (pre-1.0
  acceptable, CHANGELOG-noted pattern).

### 5.5 Counters

- Fresh-take vs reuse follows the existing `fresh`-flag contract
  (`MAPPED_PAGES` only on genuinely new virtual). New hidden counters
  (e.g. `BIG_MAP_CALLS`/`BIG_UNMAP_CALLS`) rather than reusing `SPAN_*`:
  tuning needs the split. `__debug_map_split` documents itself unstable
  ("may change or vanish") — extend the tuple and update the bench
  destructure in the same change.

## 6. Carving proofs (must hold before code review)

P1 (contiguity): blocks are `[base+64 + k*B, base+64 + (k+1)*B)` for
16-aligned B; data chunks carry no headers, so no block covers metadata.
P2 (containment): last block end `base+64 + count*B <= base+npages*64K`
by the span-sizing formula (assert in unit tests per class, as medium's
`usable / b >= TARGET` test does).
P3 (alignment): base is 64 KiB-aligned (arena structural), 64 and B are
16-multiples, so every block is 16-aligned.
P4 (virgin): fresh commits are zero pages; carving writes only freelist
link words; `FLAG_VIRGIN` stays set exactly as medium spans (cold
re-carves clear it; `calloc` keeps the link-word fast path).
P5 (lookup totality): every live big-block pointer is either in chunk 0
(found by magic at its masked base... note: chunk-0 masking reads
BIGMAGIC — the `of()` fast path CAN serve chunk 0 directly) or in a
data chunk (found via the side table); every table hit is validated by
`contains()` before use.
P6 (no clobber): carving writes stay within `[base, base+npages*64K)`
(canary test: legacy guard mapping beside churn, as arena's
`commits_never_clobber_neighbors` does).

## 7. Debug validators and test plan

- `BigMaster::contains` (magic + class range + npages + strict-inside,
  mirroring span/medium validators).
- Unit tests (isolated arenas, as arena's tests are): alignment+zeroing
  per class; capacity formula per class; table consistency (every mapped
  page indexes its master; legacy addresses index out-of-range → miss);
  exhaustion fallback (tiny arena → legacy large path, correct);
  no-clobber canary; double-free detection adaptation (free-list walk,
  as small/medium validators do).
- Miri: side-table atomics are Miri-clean; carve/reuse tests run on
  isolated instances like the existing arena tests (raw `mmap` paths
  stay `cfg_attr(miri, ignore)` where already gated — no new Miri-blind
  unsafe beyond the established pattern).
- Fuzz: alloc/free/realloc sequences crossing small/medium/big/large
  boundaries (extend existing targets).

## 8. Bench validation and revert rules

- Primary: large-only 8T hit rate (fresh-take rate, unmaps/s, big-split
  counters) — must improve vs pre-change with 5-run medians (regime
  noise drowns singles; established §4 protocol).
- Guards (any violation reverts): mixed-all 1T/8T flat-or-better,
  tight/mixed-small flat, spawn-churn unmaps ~0, thread_exit convergence,
  RSS within caps, full suite + telemetry + no_std + release green.
- Sensitivity: disable-big-spans must fail the new tests (fallback-path
  coverage) and move the 8T number back.

## 9. Open questions (resolved during implementation 2026-09-23)

1. Exact class top for phase 1: **262144** (as designed). The 1T bench
   tail past 256 KiB stays on the large path; measured — tail is a
   minority of large-only 1T ops and arena hole reuse already absorbs it
   cheaply (probe `a67298` reuses, 0 unmaps). Revisit only with its own
   numbers if the 1T large-only gap (0.40× mimalloc) becomes a priority.
2. `BIG_REFILL_BATCH`: **4** shipped (measure-don't-assume noted in
   `heap.rs`; 4 × up to 256 KiB ≈ 1 MiB per refill, budget-sane). Tune
   2/8 only if large-only 8T refill shows up in a future profile.
3. Cold/empty byte caps: **shipped at medium-scaled starts**
   (empty 8 MiB/class, cold 256 MiB/class 64-bit / 16 MiB 32-bit, 256
   slots). Bench shows RSS sane (large-only 8T peak ~5.2 GiB VmHWM with
   hi8048MiB reservation — within the 16 GiB reservation + caps rule);
   no further tuning without an RSS-regression signal.
4. Side-table 2 MiB static: **kept as designed** (BSS, faulted on touch,
   bounded by touched regions). No 4 KiB-granular alternative needed.
5. 32-bit arena: **covered by CI `bit32` job** (build + test on i686);
   big spans are `cfg(unix, feature="std")` and reuse the same
   exhaustion-fallback shape as medium (null → large path). Local box
   has no i686 std — CI is the verification path (same as Miri/Kani).
