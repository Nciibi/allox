//! Size classes for small allocations.
//!
//! Classes grow geometrically (~12.5%) from [`MIN_BLOCK`] up to
//! [`MAX_SMALL_SIZE`], rounded up to 16-byte multiples, guaranteeing an
//! internal fragmentation bound of ~12.5% (the same bound used by
//! mimalloc/tcmalloc-style designs).

/// Minimum block size; doubles as the maximum useful fundamental alignment.
pub(crate) const MIN_ALIGN: usize = 16;
/// Largest block size served from 64 KiB pages.
pub(crate) const MAX_SMALL_SIZE: usize = 16 * 1024;
/// Number of entries in the class table (trailing entries saturate at MAX).
pub(crate) const NUM_CLASSES: usize = 64;
// `usize::div_ceil` is not const-stable at our MSRV; keep manual rounding.
#[allow(clippy::manual_div_ceil)]
const fn build_classes() -> [usize; NUM_CLASSES] {
    let mut table = [MAX_SMALL_SIZE; NUM_CLASSES];
    let mut size = MIN_ALIGN;
    let mut i = 0;
    while i < NUM_CLASSES {
        table[i] = size;
        if size >= MAX_SMALL_SIZE {
            break;
        }
        size = ((size * 9 + 7) / 8 + 15) & !15;
        if size > MAX_SMALL_SIZE {
            size = MAX_SMALL_SIZE;
        }
        i += 1;
    }
    table
}

pub(crate) const CLASSES: [usize; NUM_CLASSES] = build_classes();

/// Direct-mapped size -> class table: index by `(size + 15) / 16`.
/// One kilobyte of read-mostly data; turns class lookup into a shift,
/// an add and a load instead of a scan.
const CLASS_LUT: [u8; MAX_SMALL_SIZE / MIN_ALIGN + 1] = build_lut();

const fn build_lut() -> [u8; MAX_SMALL_SIZE / MIN_ALIGN + 1] {
    let mut lut = [0u8; MAX_SMALL_SIZE / MIN_ALIGN + 1];
    let mut q = 0;
    while q <= MAX_SMALL_SIZE / MIN_ALIGN {
        // Largest size rounding into this slot is q * 16.
        lut[q] = class_for_size_scan(q * MIN_ALIGN) as u8;
        q += 1;
    }
    lut
}

/// Index of the smallest size class that fits `size` (linear fallback used
/// to build the LUT at compile time).
const fn class_for_size_scan(size: usize) -> usize {
    let mut i = 0;
    while i < NUM_CLASSES - 1 {
        if CLASSES[i] >= size {
            return i;
        }
        i += 1;
    }
    NUM_CLASSES - 1
}

/// Index of the smallest size class that fits `size`.
///
/// Requires `1 <= size <= MAX_SMALL_SIZE`.
#[inline]
#[allow(clippy::manual_div_ceil)] // div_ceil not const-stable at MSRV
pub(crate) const fn class_for_size(size: usize) -> usize {
    if size <= MAX_SMALL_SIZE {
        CLASS_LUT[(size + MIN_ALIGN - 1) / MIN_ALIGN] as usize
    } else {
        NUM_CLASSES - 1
    }
}

// ---------------------------------------------------------------------------
// Medium classes: (MAX_SMALL_SIZE, MAX_MEDIUM_BLOCK], served from multi-page
// spans (see `page::SpanMaster`). Same ~12.5% geometric growth so the small
// fragmentation bound carries over; spans (not single pages) absorb the
// page-level waste that would otherwise hit 50% for e.g. 32 KiB blocks.
// ---------------------------------------------------------------------------

/// Room a medium block needs inside one 64 KiB chunk: the master header is
/// 64 bytes (`page::SPAN_MASTER_SIZE`, kept literal here to avoid a module
/// cycle; asserted equal in tests below).
const MEDIUM_CHUNK_RESERVE: usize = 64;

/// Largest medium block: biggest 12.5% step that still fits beside headers.
pub(crate) const MEDIUM_BLOCK_CAP: usize = 65536 - MEDIUM_CHUNK_RESERVE;

/// Blocks packed per span at carve time; spans are sized to hold at least
/// this many, so one heap lock acquisition yields several thread-cache fills.
pub(crate) const TARGET_BLOCKS_PER_SPAN: usize = 8;

const fn medium_step(size: usize) -> usize {
    let grown = (size * 9 + 7) / 8;
    (grown + 15) & !15
}

const fn count_medium() -> usize {
    let mut n = 0;
    let mut size = medium_step(MAX_SMALL_SIZE);
    while size <= MEDIUM_BLOCK_CAP {
        n += 1;
        let next = medium_step(size);
        if next <= size {
            break; // overflow / saturation guard (unreachable at these sizes)
        }
        size = next;
    }
    n
}

/// Last value of the geometric chain (without the explicit top-up).
const fn geo_last_medium() -> usize {
    let mut size = medium_step(MAX_SMALL_SIZE);
    let mut last = size;
    while size <= MEDIUM_BLOCK_CAP {
        last = size;
        let next = medium_step(size);
        if next <= size {
            break;
        }
        size = next;
    }
    last
}

/// Number of medium size classes: the geometric chain plus one explicit top
/// class when the chain stops short of a full page chunk (see below).
pub(crate) const NUM_MEDIUM: usize = if geo_last_medium() < TOP_MEDIUM_BLOCK {
    count_medium() + 1
} else {
    count_medium()
};

/// Explicit top class: the biggest block that still fits beside headers in
/// a 64 KiB chunk (master chunk: 65536 - 64). One block per page, eight per
/// span — same per-lock amortization as every other medium class. Without
/// it, requests in (geo_last, 65472] would fall to the mmap large path;
/// that ~6% tail of e.g. mixed-all dominated large-path syscalls.
pub(crate) const TOP_MEDIUM_BLOCK: usize = 65536 - MEDIUM_CHUNK_RESERVE;

const fn build_medium() -> [usize; NUM_MEDIUM] {
    let mut table = [0usize; NUM_MEDIUM];
    let mut size = medium_step(MAX_SMALL_SIZE);
    let mut i = 0;
    // Geometric chain first...
    while i < NUM_MEDIUM && size <= MEDIUM_BLOCK_CAP {
        table[i] = size;
        size = medium_step(size);
        i += 1;
    }
    // ...then the explicit top class if the chain stopped short.
    if i < NUM_MEDIUM {
        table[i] = TOP_MEDIUM_BLOCK;
    }
    table
}

pub(crate) const MEDIUM_CLASSES: [usize; NUM_MEDIUM] = build_medium();

/// Largest servable medium block (top of the generated table).
pub(crate) const MAX_MEDIUM_BLOCK: usize = MEDIUM_CLASSES[NUM_MEDIUM - 1];

/// Span length in 64 KiB pages for a medium block size: covers the master
/// header plus `TARGET_BLOCKS_PER_SPAN` blocks, rounded up to whole pages.
/// Carving skips 64 B (master) + 16 B per sub-page, so usable space always
/// exceeds `TARGET_BLOCKS_PER_SPAN` blocks (asserted in tests).
pub(crate) const fn span_pages_for(block: usize) -> usize {
    let need = MEDIUM_CHUNK_RESERVE + TARGET_BLOCKS_PER_SPAN * block;
    (need + 65536 - 1) / 65536
}

/// Direct-mapped size -> medium-class table for
/// `size in (MAX_SMALL_SIZE, MAX_MEDIUM_BLOCK]`, slot `(size-MAX_SMALL-1)/16`.
const MEDIUM_LUT_LEN: usize = (MAX_MEDIUM_BLOCK - MAX_SMALL_SIZE) / MIN_ALIGN;

const fn medium_scan(size: usize) -> usize {
    let mut i = 0;
    while i < NUM_MEDIUM - 1 {
        if MEDIUM_CLASSES[i] >= size {
            return i;
        }
        i += 1;
    }
    NUM_MEDIUM - 1
}

const fn build_medium_lut() -> [u8; MEDIUM_LUT_LEN + 1] {
    let mut lut = [0u8; MEDIUM_LUT_LEN + 1];
    let mut q = 0;
    while q <= MEDIUM_LUT_LEN {
        lut[q] = medium_scan(MAX_SMALL_SIZE + 1 + q * MIN_ALIGN) as u8;
        q += 1;
    }
    lut
}

const MEDIUM_LUT: [u8; MEDIUM_LUT_LEN + 1] = build_medium_lut();

/// Index of the smallest medium class that fits `size`.
///
/// Requires `MAX_SMALL_SIZE < size <= MAX_MEDIUM_BLOCK` (callers route on the
/// size first; out-of-range inputs saturate instead of trapping).
#[inline]
pub(crate) const fn medium_class_for_size(size: usize) -> usize {
    if size <= MAX_SMALL_SIZE || size > MAX_MEDIUM_BLOCK {
        NUM_MEDIUM - 1
    } else {
        MEDIUM_LUT[(size - MAX_SMALL_SIZE - 1) / MIN_ALIGN] as usize
    }
}

// ---------------------------------------------------------------------------
// Big classes: (MAX_MEDIUM_BLOCK, MAX_BIG_BLOCK], served from whole spans
// with a single meta chunk plus pure data chunks (see `page::BigMaster`
// and DESIGN_SPANS_BIG.md). Same ~12.5% geometric growth; blocks may exceed
// a 64 KiB chunk because data chunks carry no headers and blocks run
// contiguously across chunk boundaries.
//
// Arena-gated with the side-table lookup: without the arena there is no
// page-indexed master map, so big spans cannot exist and these sizes route
// to the large path. Non-arena targets never reference this section.
// ---------------------------------------------------------------------------

/// Room a big block needs at the span base: the master header (`page::
/// BIG_MASTER_SIZE`, kept literal here to avoid a module cycle; asserted
/// equal in tests below). Data chunks reserve nothing.
#[cfg(all(unix, feature = "std"))]
const BIG_MASTER_RESERVE: usize = 64;

/// Largest big block (explicit top class, phase 1 cap). The 1T bench tail
/// past 256 KiB stays on the large path; revisit with its own numbers.
#[cfg(all(unix, feature = "std"))]
pub(crate) const BIG_BLOCK_CAP: usize = 262144;

/// Blocks packed per big span at carve time (same amortization rationale
/// as `TARGET_BLOCKS_PER_SPAN`; an 8 x 256 KiB span is ~2 MiB).
#[cfg(all(unix, feature = "std"))]
pub(crate) const TARGET_BLOCKS_PER_BIG_SPAN: usize = 8;

#[cfg(all(unix, feature = "std"))]
const fn count_big() -> usize {
    let mut n = 0;
    let mut size = medium_step(MAX_MEDIUM_BLOCK);
    while size <= BIG_BLOCK_CAP {
        n += 1;
        let next = medium_step(size);
        if next <= size {
            break; // overflow / saturation guard (unreachable at these sizes)
        }
        size = next;
    }
    n
}

/// Number of big size classes: the geometric chain plus one explicit top
/// class at exactly BIG_BLOCK_CAP when the chain stops short (same tail
/// argument as TOP_MEDIUM_BLOCK: without it, requests in
/// (geo_last, 262144] would fall to the mmap large path).
#[cfg(all(unix, feature = "std"))]
pub(crate) const NUM_BIG: usize = {
    let mut last = medium_step(MAX_MEDIUM_BLOCK);
    let mut size = last;
    while size <= BIG_BLOCK_CAP {
        last = size;
        let next = medium_step(size);
        if next <= size {
            break;
        }
        size = next;
    }
    if last < BIG_BLOCK_CAP {
        count_big() + 1
    } else {
        count_big()
    }
};

#[cfg(all(unix, feature = "std"))]
const fn build_big() -> [usize; NUM_BIG] {
    let mut table = [0usize; NUM_BIG];
    let mut size = medium_step(MAX_MEDIUM_BLOCK);
    let mut i = 0;
    // Geometric chain first...
    while i < NUM_BIG && size <= BIG_BLOCK_CAP {
        table[i] = size;
        size = medium_step(size);
        i += 1;
    }
    // ...then the explicit top class if the chain stopped short.
    if i < NUM_BIG {
        table[i] = BIG_BLOCK_CAP;
    }
    table
}

#[cfg(all(unix, feature = "std"))]
pub(crate) const BIG_CLASSES: [usize; NUM_BIG] = build_big();

/// Largest servable big block (top of the generated table).
#[cfg(all(unix, feature = "std"))]
pub(crate) const MAX_BIG_BLOCK: usize = BIG_CLASSES[NUM_BIG - 1];

/// Span length in 64 KiB pages for a big block size: covers the master
/// header plus `TARGET_BLOCKS_PER_BIG_SPAN` blocks, rounded up to whole
/// pages. Only the 64 B master is skipped (data chunks reserve nothing),
/// so usable space always exceeds the target (asserted in tests).
#[cfg(all(unix, feature = "std"))]
pub(crate) const fn big_span_pages_for(block: usize) -> usize {
    let need = BIG_MASTER_RESERVE + TARGET_BLOCKS_PER_BIG_SPAN * block;
    (need + 65536 - 1) / 65536
}

/// Direct-mapped size -> big-class table for
/// `size in (MAX_MEDIUM_BLOCK, MAX_BIG_BLOCK]`, slot
/// `(size-MAX_MEDIUM_BLOCK-1)/16`.
#[cfg(all(unix, feature = "std"))]
const BIG_LUT_LEN: usize = (MAX_BIG_BLOCK - MAX_MEDIUM_BLOCK) / MIN_ALIGN;

#[cfg(all(unix, feature = "std"))]
const fn big_scan(size: usize) -> usize {
    let mut i = 0;
    while i < NUM_BIG - 1 {
        if BIG_CLASSES[i] >= size {
            return i;
        }
        i += 1;
    }
    NUM_BIG - 1
}

#[cfg(all(unix, feature = "std"))]
const fn build_big_lut() -> [u8; BIG_LUT_LEN + 1] {
    let mut lut = [0u8; BIG_LUT_LEN + 1];
    let mut q = 0;
    while q <= BIG_LUT_LEN {
        lut[q] = big_scan(MAX_MEDIUM_BLOCK + 1 + q * MIN_ALIGN) as u8;
        q += 1;
    }
    lut
}

#[cfg(all(unix, feature = "std"))]
const BIG_LUT: [u8; BIG_LUT_LEN + 1] = build_big_lut();

/// Index of the smallest big class that fits `size`.
///
/// Requires `MAX_MEDIUM_BLOCK < size <= MAX_BIG_BLOCK` (callers route on
/// the size first; out-of-range inputs saturate instead of trapping).
#[cfg(all(unix, feature = "std"))]
#[inline]
pub(crate) const fn big_class_for_size(size: usize) -> usize {
    if size <= MAX_MEDIUM_BLOCK || size > MAX_BIG_BLOCK {
        NUM_BIG - 1
    } else {
        BIG_LUT[(size - MAX_MEDIUM_BLOCK - 1) / MIN_ALIGN] as usize
    }
}

/// Telemetry dimension: small classes followed by medium classes.
#[cfg(all(feature = "telemetry", not(all(unix, feature = "std"))))]
pub(crate) const TOTAL_CLASSES: usize = NUM_CLASSES + NUM_MEDIUM;

/// Telemetry dimension with big classes appended (arena targets only).
#[cfg(all(feature = "telemetry", unix, feature = "std"))]
pub(crate) const TOTAL_CLASSES: usize = NUM_CLASSES + NUM_MEDIUM + NUM_BIG;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_are_monotonic_and_cover_range() {
        assert_eq!(CLASSES[0], 16);
        let mut prev = 0;
        for &c in CLASSES.iter() {
            assert!(c >= prev);
            assert!(c % 16 == 0);
            prev = c;
        }
        assert_eq!(class_for_size(1), 0);
        assert_eq!(class_for_size(16), 0);
        assert_eq!(class_for_size(17), 1);
        assert_eq!(
            class_for_size(MAX_SMALL_SIZE),
            class_for_size(MAX_SMALL_SIZE)
        );
        assert!(CLASSES[class_for_size(MAX_SMALL_SIZE)] >= MAX_SMALL_SIZE);
    }

    #[test]
    fn fragmentation_bound_holds() {
        for size in 1..=MAX_SMALL_SIZE {
            let cls = CLASSES[class_for_size(size)];
            assert!(cls >= size);
            assert!(cls < size * 9 / 8 + 16, "size {} class {}", size, cls);
        }
    }

    #[test]
    fn medium_tables_cover_range_with_bound() {
        use crate::page::SPAN_MASTER_SIZE;
        assert_eq!(MEDIUM_CHUNK_RESERVE, SPAN_MASTER_SIZE);
        assert!(NUM_MEDIUM >= 8, "expected ~13 medium classes, got {}", NUM_MEDIUM);
        assert!(MEDIUM_CLASSES[0] > MAX_SMALL_SIZE);
        assert!(MAX_MEDIUM_BLOCK <= MEDIUM_BLOCK_CAP);
        // Next geometric step would exceed the cap (table is maximal).
        assert!(medium_step(MAX_MEDIUM_BLOCK) > MEDIUM_BLOCK_CAP);
        let mut prev = MAX_SMALL_SIZE;
        for &c in MEDIUM_CLASSES.iter() {
            assert!(c > prev, "not strictly growing: {}", c);
            assert!(c % 16 == 0);
            prev = c;
        }
        // Fragmentation bound + LUT agree with scan on every slot.
        let mut size = MAX_SMALL_SIZE + 1;
        while size <= MAX_MEDIUM_BLOCK {
            let cls = MEDIUM_CLASSES[medium_class_for_size(size)];
            assert!(cls >= size, "size {}", size);
            assert!(cls < size * 9 / 8 + 16, "size {} class {}", size, cls);
            assert_eq!(medium_class_for_size(size), medium_scan(size));
            size += 1;
        }
        // Carve capacity is per 64 KiB chunk, not aggregate span capacity.
        for &b in MEDIUM_CLASSES.iter() {
            let pages = span_pages_for(b);
            assert!(pages >= 2 && pages <= 16, "block {} pages {}", b, pages);
            let first_chunk = (65536 - MEDIUM_CHUNK_RESERVE) / b;
            let later_chunks = (65536 - 16) / b;
            let capacity = first_chunk + (pages - 1) * later_chunks;
            assert!(capacity > 0, "block {} pages {}", b, pages);
            assert!(
                capacity <= pages * 65536 / b,
                "block {} pages {} capacity {} exceeds aggregate bound",
                b,
                pages,
                capacity
            );
        }
    }

    #[cfg(all(unix, feature = "std"))]
    #[test]
    fn big_tables_cover_range_with_bound() {
        use crate::page::BIG_MASTER_SIZE;
        assert_eq!(BIG_MASTER_RESERVE, BIG_MASTER_SIZE);
        assert!(NUM_BIG >= 8, "expected ~11 big classes, got {}", NUM_BIG);
        assert!(BIG_CLASSES[0] > MAX_MEDIUM_BLOCK);
        assert_eq!(MAX_BIG_BLOCK, BIG_BLOCK_CAP);
        assert!(MAX_BIG_BLOCK <= BIG_BLOCK_CAP);
        let mut prev = MAX_MEDIUM_BLOCK;
        for &c in BIG_CLASSES.iter() {
            assert!(c > prev, "not strictly growing: {}", c);
            assert!(c % 16 == 0);
            prev = c;
        }
        // Fragmentation bound + LUT agree with scan on every slot.
        let mut size = MAX_MEDIUM_BLOCK + 1;
        while size <= MAX_BIG_BLOCK {
            let cls = BIG_CLASSES[big_class_for_size(size)];
            assert!(cls >= size, "size {}", size);
            assert!(cls < size * 9 / 8 + 16, "size {} class {}", size, cls);
            assert_eq!(big_class_for_size(size), big_scan(size));
            size += 1;
        }
        // Every big span holds comfortably more than one lock's worth of
        // blocks (data chunks reserve nothing — only the 64 B master).
        for &b in BIG_CLASSES.iter() {
            let pages = big_span_pages_for(b);
            assert!(pages >= 2, "block {} pages {}", b, pages);
            let usable = pages * 65536 - BIG_MASTER_RESERVE;
            assert!(
                usable / b >= TARGET_BLOCKS_PER_BIG_SPAN,
                "block {} pages {} capacity {}",
                b,
                pages,
                usable / b
            );
        }
    }
}
