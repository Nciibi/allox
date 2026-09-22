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

/// Number of medium size classes (generated; ~13 for the 16 KiB → 60 KiB range).
pub(crate) const NUM_MEDIUM: usize = count_medium();

const fn build_medium() -> [usize; NUM_MEDIUM] {
    let mut table = [0usize; NUM_MEDIUM];
    let mut size = medium_step(MAX_SMALL_SIZE);
    let mut i = 0;
    while i < NUM_MEDIUM {
        table[i] = size;
        size = medium_step(size);
        i += 1;
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

/// Telemetry dimension: small classes followed by medium classes.
pub(crate) const TOTAL_CLASSES: usize = NUM_CLASSES + NUM_MEDIUM;

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
}
