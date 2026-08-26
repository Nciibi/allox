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
pub(crate) const fn class_for_size(size: usize) -> usize {
    if size <= MAX_SMALL_SIZE {
        CLASS_LUT[(size + MIN_ALIGN - 1) / MIN_ALIGN] as usize
    } else {
        NUM_CLASSES - 1
    }
}

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
