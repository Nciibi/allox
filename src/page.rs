//! Page structures: 64 KiB OS-mapped pages holding blocks of one size class,
//! multi-page spans for medium blocks, plus the header layout used for large
//! (directly mapped) regions.

use crate::classes::{medium_capacity_for, medium_capacity_legacy_for, MEDIUM_CLASSES, NUM_MEDIUM};
use crate::classes::CLASSES;
#[cfg(all(unix, feature = "std"))]
use crate::classes::{BIG_CLASSES, NUM_BIG};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

pub(crate) const PAGE_SHIFT: u32 = 16;
pub(crate) const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub(crate) const PAGE_MASK: usize = PAGE_SIZE - 1;

/// Marks memory at a 64 KiB boundary as an allocator-managed small page.
pub(crate) const PAGE_MAGIC: u64 = 0xA110_CCA7_E5A1_1E5D;
/// Marks the base of a medium multi-page span (first 64 KiB unit).
pub(crate) const SPAN_MAGIC: u64 = 0xA110_CCA7_5EED_5A11;
/// Marks a non-first 64 KiB unit of a medium span; +8 holds the master ptr.
pub(crate) const SPAN_SUBMAGIC: u64 = 0xA110_CCA7_5EED_5A12;
/// Marks the base of a big multi-page span (first 64 KiB unit). Data chunks
/// (non-first units) carry NO headers — blocks run contiguously across
/// chunk boundaries — so lookup goes through the arena side table, never
/// through masking (see DESIGN_SPANS_BIG.md).
#[cfg(all(unix, feature = "std"))]
pub(crate) const BIGMAGIC: u64 = 0xA110_CCA7_B16_5A13;
/// Marks memory at a 64 KiB boundary as a large, directly mapped region.
pub(crate) const LARGE_MAGIC: u64 = 0x00B1_0C5A_6E0F_F1CE;

/// Flag: page is currently linked into its size class' partial list.
pub(crate) const FLAG_IN_PARTIAL: u16 = 1;
/// Flag: no block of this page was ever allocated-and-freed since the page
/// was carved, so every free block is still OS-zero. Cleared the moment any
/// block is returned to the page.
pub(crate) const FLAG_VIRGIN: u16 = 2;

// magic + prev + next + free_head + free_count/used/class/flags + owner = 44
// bytes, padded by align(16) to 48 (HEADER_SIZE unchanged).
#[repr(C, align(16))]
pub(crate) struct PageHeader {
    pub(crate) magic: u64,
    pub(crate) prev: *mut PageHeader,
    pub(crate) next: *mut PageHeader,
    pub(crate) free_head: *mut u8,
    pub(crate) free_count: u16,
    /// Blocks held outside this page's own free list (live or thread-cached).
    pub(crate) used: u16,
    pub(crate) class: u16,
    pub(crate) flags: u16,
    /// Heuristic owner thread-id (0 = unowned). Set when a thread refills
    /// from this page; used only to detect remote frees under cache pressure
    /// (drift cap) — never for correctness. Concurrent stores are last-writer-wins.
    pub(crate) owner: AtomicU32,
}

pub(crate) const HEADER_SIZE: usize = core::mem::size_of::<PageHeader>();

impl PageHeader {
    /// The page header owning `p`, found by masking address bits.
    #[inline]
    pub(crate) unsafe fn of(p: *mut u8) -> *mut PageHeader {
        (p as usize & !PAGE_MASK) as *mut PageHeader
    }

    /// Carve a freshly mapped page into a full free list of `class`-sized
    /// blocks. The page is born empty of users (`used == 0`).
    pub(crate) unsafe fn init(&mut self, class: usize) {
        let block_size = CLASSES[class];
        let base = self as *mut _ as usize;
        let start = base + HEADER_SIZE;
        let count = (PAGE_SIZE - HEADER_SIZE) / block_size;
        let mut head: *mut u8 = ptr::null_mut();
        let mut i = count;
        while i > 0 {
            i -= 1;
            let b = (start + i * block_size) as *mut u8;
            *(b.cast::<*mut u8>()) = head;
            head = b;
        }
        self.magic = PAGE_MAGIC;
        self.prev = ptr::null_mut();
        self.next = ptr::null_mut();
        self.free_head = head;
        self.free_count = count as u16;
        self.used = 0;
        self.class = class as u16;
        self.flags = FLAG_VIRGIN;
        self.owner.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Medium spans: contiguous runs of 64 KiB pages carved into blocks of one
// medium class (16 KiB, 64 KiB]. The first page carries a SpanMaster; every
// further page carries a 16-byte sub-header (SUBMAGIC + master pointer) at
// its base. Blocks are sliced per page-chunk so none straddles a page base
// or covers a header — which is why medium blocks cap at 65472 bytes.
// Pointer -> master: mask to the 64 KiB page, match the magic, follow one
// pointer for sub-pages. The small-page fast path (`PAGE_MAGIC` check first)
// is unaffected.
// ---------------------------------------------------------------------------

/// Sub-header size at the base of each non-first span page.
pub(crate) const SPAN_SUB_SIZE: usize = 16;

// magic + prev + next + free_head + free_count/used + mclass/flags +
// npages/owner = 56 bytes, padded by align(16) to 64.
#[repr(C, align(16))]
pub(crate) struct SpanMaster {
    pub(crate) magic: u64,
    pub(crate) prev: *mut SpanMaster,
    pub(crate) next: *mut SpanMaster,
    pub(crate) free_head: *mut u8,
    pub(crate) free_count: u32,
    /// Blocks held outside this span's own free list (live or thread-cached).
    pub(crate) used: u32,
    pub(crate) mclass: u16,
    pub(crate) flags: u16,
    /// Span length in 64 KiB pages (master page included).
    pub(crate) npages: u32,
    /// Heuristic owner thread-id (0 = unowned); drift-cap only, see
    /// [`PageHeader::owner`].
    pub(crate) owner: AtomicU32,
}

pub(crate) const SPAN_MASTER_SIZE: usize = core::mem::size_of::<SpanMaster>();

impl SpanMaster {
    /// The master owning `p`: mask to the 64 KiB page, then follow at most
    /// one sub-header pointer. Returns null when `p` is not inside a span
    /// (large region, foreign memory); the small-page check runs first in
    /// all dispatch paths so this never misclassifies small blocks.
    #[inline]
    pub(crate) unsafe fn of(p: *mut u8) -> *mut SpanMaster {
        let base = p as usize & !PAGE_MASK;
        if *(base as *const u64) == SPAN_MAGIC {
            return base as *mut SpanMaster;
        }
        if *(base as *const u64) == SPAN_SUBMAGIC {
            return *(base.wrapping_add(8) as *const *mut SpanMaster);
        }
        ptr::null_mut()
    }

    /// Carve freshly mapped `npages` pages (base 64 KiB-aligned) into a full
    /// free list of medium-`mclass` blocks. The span is born with no users.
    pub(crate) unsafe fn init(&mut self, mclass: usize, npages: u32) {
        debug_assert!(mclass < NUM_MEDIUM);
        let block_size = MEDIUM_CLASSES[mclass];
        let base = self as *mut _ as usize;
        // Sub-headers on every non-first page, before carving blocks around
        // them: a block never covers a page base.
        let mut i = 1u32;
        while i < npages {
            let sub = (base + i as usize * PAGE_SIZE) as *mut u64;
            *sub = SPAN_SUBMAGIC;
            *((sub as *mut u8).add(8).cast::<*mut SpanMaster>()) = self;
            i += 1;
        }
        let mut head: *mut u8 = ptr::null_mut();
        let mut count = 0u32;
        let mut page = 0u32;
        while page < npages {
            let chunk = base + page as usize * PAGE_SIZE;
            let (start, end) = if page == 0 {
                (chunk + SPAN_MASTER_SIZE, chunk + PAGE_SIZE)
            } else {
                (chunk + SPAN_SUB_SIZE, chunk + PAGE_SIZE)
            };
            let mut b = start;
            while b + block_size <= end {
                *(b as *mut *mut u8) = head;
                head = b as *mut u8;
                count += 1;
                b += block_size;
            }
            page += 1;
        }
        debug_assert!(count > 0);
        debug_assert_eq!(
            count as usize,
            medium_capacity_for(block_size, npages as usize)
        );
        self.magic = SPAN_MAGIC;
        self.prev = ptr::null_mut();
        self.next = ptr::null_mut();
        self.free_head = head;
        self.free_count = count;
        self.used = 0;
        self.mclass = mclass as u16;
        self.flags = FLAG_VIRGIN;
        self.npages = npages;
        self.owner.store(0, Ordering::Relaxed);
    }

    /// Byte size of the whole span mapping (for unmap).
    #[inline]
    pub(crate) unsafe fn mapped_bytes(&self) -> usize {
        self.npages as usize * PAGE_SIZE
    }

    /// Ownership check: master magic intact, class in range, and `p` inside
    /// the span extent. Guards the free path against (astronomically rare)
    /// magic collisions with user data — collisions fail safe to the large
    /// check / corrupt-pointer abort instead of heap corruption.
    #[inline]
    pub(crate) unsafe fn contains(&self, p: *mut u8) -> bool {
        self.magic == SPAN_MAGIC
            && (self.mclass as usize) < NUM_MEDIUM
            && self.npages > 0
            && (p as usize) > (self as *const _ as usize)
            && (p as usize) < (self as *const _ as usize) + self.mapped_bytes()
    }
}

// ---------------------------------------------------------------------------
// Big spans: contiguous runs of 64 KiB pages carved into blocks of one big
// class (65472 B, 262144 B]. Layout is one meta chunk (BigMaster, 64 B)
// plus pure data chunks: blocks are carved contiguously from base + 64 and
// freely cross chunk boundaries, because data chunks carry no headers.
// Lookup therefore cannot mask (a masked base inside a data chunk is user
// data); it goes through the arena page-indexed side table instead.
// Pointer -> master: table[(p - arena_start) >> 16], validated by
// contains() below. See DESIGN_SPANS_BIG.md.
// ---------------------------------------------------------------------------

// magic + prev + next + free_head + free_count/used + bclass/flags +
// npages/owner = 56 bytes, padded by align(16) to 64 (same as SpanMaster).
#[cfg(all(unix, feature = "std"))]
#[repr(C, align(16))]
pub(crate) struct BigMaster {
    pub(crate) magic: u64,
    pub(crate) prev: *mut BigMaster,
    pub(crate) next: *mut BigMaster,
    pub(crate) free_head: *mut u8,
    pub(crate) free_count: u32,
    /// Blocks held outside this span's own free list (live or thread-cached).
    pub(crate) used: u32,
    pub(crate) bclass: u16,
    pub(crate) flags: u16,
    /// Span length in 64 KiB pages (meta chunk included).
    pub(crate) npages: u32,
    /// Heuristic owner thread-id (0 = unowned); drift-cap only, see
    /// [`PageHeader::owner`].
    pub(crate) owner: AtomicU32,
}

#[cfg(all(unix, feature = "std"))]
pub(crate) const BIG_MASTER_SIZE: usize = core::mem::size_of::<BigMaster>();

#[cfg(all(unix, feature = "std"))]
impl BigMaster {
    /// Carve freshly mapped `npages` pages (base 64 KiB-aligned) into a full
    /// free list of big-`bclass` blocks, packed contiguously from just past
    /// the master header — including across chunk boundaries, which hold no
    /// headers. The span is born with no users.
    pub(crate) unsafe fn init(&mut self, bclass: usize, npages: u32) {
        debug_assert!(bclass < NUM_BIG);
        let block_size = BIG_CLASSES[bclass];
        let base = self as *mut _ as usize;
        let end = base + npages as usize * PAGE_SIZE;
        let mut head: *mut u8 = ptr::null_mut();
        let mut count = 0u32;
        let mut b = base + BIG_MASTER_SIZE;
        while b + block_size <= end {
            *(b as *mut *mut u8) = head;
            head = b as *mut u8;
            count += 1;
            b += block_size;
        }
        debug_assert!(count > 0);
        self.magic = BIGMAGIC;
        self.prev = ptr::null_mut();
        self.next = ptr::null_mut();
        self.free_head = head;
        self.free_count = count;
        self.used = 0;
        self.bclass = bclass as u16;
        self.flags = FLAG_VIRGIN;
        self.npages = npages;
        self.owner.store(0, Ordering::Relaxed);
    }

    /// Byte size of the whole span mapping (for unmap).
    #[inline]
    pub(crate) unsafe fn mapped_bytes(&self) -> usize {
        self.npages as usize * PAGE_SIZE
    }

    /// Ownership check: master magic intact, class in range, and `p` inside
    /// the span extent. Same fail-closed role as `SpanMaster::contains` for
    /// stale side-table reads and magic collisions with user data.
    #[inline]
    pub(crate) unsafe fn contains(&self, p: *mut u8) -> bool {
        self.magic == BIGMAGIC
            && (self.bclass as usize) < NUM_BIG
            && self.npages > 0
            && (p as usize) > (self as *const _ as usize)
            && (p as usize) < (self as *const _ as usize) + self.mapped_bytes()
    }
}

#[repr(C, align(16))]
pub(crate) struct LargeHeader {
    pub(crate) magic: u64,
    pub(crate) mapped_size: usize,
    pub(crate) base: *mut u8,
    pub(crate) requested_size: usize,
    pub(crate) registry_next: *mut LargeHeader,
}

pub(crate) const LARGE_HEADER_SIZE: usize = core::mem::size_of::<LargeHeader>();

#[inline]
pub(crate) fn align_up(v: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (v + align - 1) & !(align - 1)
}

// Intrusive free-list ops: a free block's first word holds the next pointer.

#[inline]
pub(crate) unsafe fn push_block(head: &mut *mut u8, b: *mut u8) {
    *b.cast::<*mut u8>() = *head;
    *head = b;
}

#[inline]
pub(crate) unsafe fn pop_block(head: &mut *mut u8) -> Option<*mut u8> {
    let b = *head;
    if b.is_null() {
        None
    } else {
        *head = *b.cast::<*mut u8>();
        Some(b)
    }
}

#[cfg(all(test, unix, feature = "std"))]
mod tests {
    use super::*;
    use crate::classes::{
        big_span_pages_for, medium_capacity_for, span_pages_for, BIG_CLASSES, MEDIUM_CLASSES,
        MIN_ALIGN, NUM_BIG, NUM_MEDIUM,
    };

    /// 64 KiB-aligned multi-page buffer without OS mmap (Miri-safe).
    /// Caller must free with the same layout.
    unsafe fn aligned_pages(pages: usize) -> (*mut u8, core::alloc::Layout) {
        let layout = core::alloc::Layout::from_size_align(pages * PAGE_SIZE, PAGE_SIZE)
            .expect("layout");
        let p = std::alloc::alloc(layout);
        assert!(!p.is_null(), "alloc {} pages", pages);
        // Freshness not guaranteed by std::alloc; zero so virgin asserts hold.
        core::ptr::write_bytes(p, 0, pages * PAGE_SIZE);
        (p, layout)
    }

    unsafe fn aligned_free(p: *mut u8, layout: core::alloc::Layout) {
        std::alloc::dealloc(p, layout);
    }

    /// Carve audit per big class on 64 KiB-aligned buffers: magic + virgin
    /// flags, exact capacity formula, every block 16-aligned inside the
    /// extent, packed at exactly one-block stride (contiguous carve — P1/P2/
    /// P3), and containment of sample pointers (P5). Uses std::alloc so the
    /// whole check runs under Miri (no raw mmap).
    #[test]
    fn big_carve_packs_contiguously() {
        for bclass in [0usize, NUM_BIG / 2, NUM_BIG - 1] {
            let block = BIG_CLASSES[bclass];
            let pages = big_span_pages_for(block);
            let (raw, layout) = unsafe { aligned_pages(pages) };
            let span = raw.cast::<BigMaster>();
            unsafe { (*span).init(bclass, pages as u32) };
            assert_eq!(unsafe { (*span).magic }, BIGMAGIC);
            assert_eq!(unsafe { (*span).bclass } as usize, bclass);
            assert_eq!(unsafe { (*span).npages } as usize, pages);
            assert!(unsafe { (*span).flags } & FLAG_VIRGIN != 0);
            let capacity = (pages * PAGE_SIZE - BIG_MASTER_SIZE) / block;
            assert_eq!(unsafe { (*span).free_count } as usize, capacity);
            // Walk the chain: stride must equal block size exactly.
            let mut addrs = Vec::new();
            let mut cur = unsafe { (*span).free_head };
            while !cur.is_null() {
                addrs.push(cur as usize);
                cur = unsafe { *cur.cast::<*mut u8>() };
            }
            assert_eq!(addrs.len(), capacity, "bclass {}", bclass);
            addrs.sort_unstable();
            let base = raw as usize;
            for (k, a) in addrs.iter().enumerate() {
                assert_eq!(*a % 16, 0, "unaligned block {}", k);
                assert!(
                    *a >= base + BIG_MASTER_SIZE && *a + block <= base + pages * PAGE_SIZE,
                    "block {} outside extent",
                    k
                );
                if k > 0 {
                    assert_eq!(
                        *a - addrs[k - 1],
                        block,
                        "non-contiguous carve at {} (bclass {})",
                        k,
                        bclass
                    );
                }
            }
            // contains() agrees on interior pointers, rejects the edges.
            let mid = unsafe { (*span).free_head };
            assert!(unsafe { (*span).contains(mid) });
            assert!(!unsafe { (*span).contains(raw) });
            unsafe { aligned_free(raw, layout) };
        }
    }

    /// BigMaster::contains fail-closed contract: wrong magic, out-of-range
    /// class, zero npages, and pointers outside the extent all reject.
    #[test]
    fn big_contains_is_fail_closed() {
        let (raw, layout) = unsafe { aligned_pages(2) };
        let span = raw.cast::<BigMaster>();
        unsafe {
            (*span).init(0, 2);
            assert!((*span).contains(raw.add(BIG_MASTER_SIZE)));
            assert!((*span).contains(raw.add(2 * PAGE_SIZE - 16)));

            let good_magic = (*span).magic;
            let good_class = (*span).bclass;
            let good_npages = (*span).npages;

            (*span).magic = 0;
            assert!(!(*span).contains(raw.add(BIG_MASTER_SIZE)));
            (*span).magic = good_magic;

            (*span).bclass = u16::MAX;
            assert!(!(*span).contains(raw.add(BIG_MASTER_SIZE)));
            (*span).bclass = good_class;

            (*span).npages = 0;
            assert!(!(*span).contains(raw.add(BIG_MASTER_SIZE)));
            (*span).npages = good_npages;

            // Pointer at/before master and past the extent reject.
            assert!(!(*span).contains(raw));
            assert!(!(*span).contains(raw.add(2 * PAGE_SIZE)));
        }
        unsafe { aligned_free(raw, layout) };
    }

    /// Medium span: init carves sub-headers, SpanMaster::of resolves master
    /// and sub pages, contains rejects edges. Miri-safe (std::alloc).
    #[test]
    fn medium_span_of_and_contains() {
        let (raw, layout) = unsafe { aligned_pages(3) };
        let span = raw.cast::<SpanMaster>();
        // Class 0 medium: smallest medium block.
        let mclass = 0usize;
        unsafe {
            (*span).init(mclass, 3);
            assert_eq!((*span).magic, SPAN_MAGIC);
            assert_eq!((*span).npages, 3);
            assert!((*span).flags & FLAG_VIRGIN != 0);

            // Master page resolves to itself.
            assert_eq!(SpanMaster::of(raw), span);
            // Sub pages resolve via sub-header pointer.
            for page in 1..3usize {
                let sub = raw.add(page * PAGE_SIZE);
                assert_eq!(SpanMaster::of(sub), span, "sub page {}", page);
            }
            // Interior data pointer in first page (past master header).
            let interior = raw.add(SPAN_MASTER_SIZE + 16);
            assert!((*span).contains(interior));
            // Edges reject.
            assert!(!(*span).contains(raw));
            assert!(!(*span).contains(raw.add(3 * PAGE_SIZE)));

            // of() on non-span memory (zeroed) returns null.
            let (f2, l2) = aligned_pages(1);
            assert!(SpanMaster::of(f2).is_null());
            aligned_free(f2, l2);
        }
        unsafe { aligned_free(raw, layout) };
    }

    #[test]
    fn medium_carve_matches_capacity_for_every_class() {
        for mclass in 0..NUM_MEDIUM {
            let block = MEDIUM_CLASSES[mclass];
            let pages = span_pages_for(block);
            let (raw, layout) = unsafe { aligned_pages(pages) };
            let span = raw.cast::<SpanMaster>();
            unsafe { (*span).init(mclass, pages as u32) };
            let expected = medium_capacity_for(block, pages);
            assert_eq!(unsafe { (*span).free_count } as usize, expected);

            let mut addresses = Vec::with_capacity(expected);
            let mut current = unsafe { (*span).free_head };
            while !current.is_null() {
                let address = current as usize;
                assert_eq!(address % MIN_ALIGN, 0, "mclass {} address {}", mclass, address);
                assert_eq!(unsafe { SpanMaster::of(current) }, span);
                assert!(unsafe { (*span).contains(current) });
                addresses.push(address);
                current = unsafe { *current.cast::<*mut u8>() };
            }
            assert_eq!(addresses.len(), expected, "mclass {}", mclass);
            addresses.sort_unstable();
            let base = raw as usize;
            for (index, address) in addresses.iter().copied().enumerate() {
                let page = (address - base) / PAGE_SIZE;
                let start = if page == 0 { SPAN_MASTER_SIZE } else { SPAN_SUB_SIZE };
                let page_start = base + page * PAGE_SIZE;
                assert!(address >= page_start + start, "mclass {}", mclass);
                assert!(address + block <= page_start + PAGE_SIZE, "mclass {}", mclass);
                if index > 0 {
                    let previous = addresses[index - 1];
                    let previous_page = (previous - base) / PAGE_SIZE;
                    if page == previous_page {
                        assert_eq!(address - previous, block, "mclass {}", mclass);
                    }
                }
            }
            unsafe {
                for page in 1..pages {
                    let page_base = raw.add(page * PAGE_SIZE);
                    assert_eq!(*page_base.cast::<u64>(), SPAN_SUBMAGIC);
                    assert_eq!(*page_base.add(8).cast::<*mut SpanMaster>(), span);
                }
            }
            unsafe { aligned_free(raw, layout) };
        }
    }

    #[test]
    fn owner_updates_are_atomic() {
        let (raw, layout) = unsafe { aligned_pages(1) };
        let page = raw.cast::<PageHeader>();
        unsafe { (*page).init(0) };
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(core::mem::size_of::<PageHeader>(), 48);
            assert_eq!(core::mem::size_of::<SpanMaster>(), 64);
        }
        struct SharedPage(*mut PageHeader);
        unsafe impl Send for SharedPage {}
        unsafe impl Sync for SharedPage {}
        std::thread::scope(|scope| {
            for seed in 0..2u32 {
                let shared = SharedPage(page);
                scope.spawn(move || {
                    let shared = shared;
                    for i in 0..10_000u32 {
                        unsafe {
                            (*shared.0).owner.store(seed + i, Ordering::Relaxed);
                            let _ = (*shared.0).owner.load(Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        unsafe { aligned_free(raw, layout) };
    }
}

// ---------------------------------------------------------------------------
// Kani proofs — DESIGN_SPANS_BIG.md §6 carving properties P1–P6 (and the
// equivalent medium-span packing invariants). Pure arithmetic: no mmap, so
// these run under `cargo kani` in CI (nightly + kani-verifier).
// P4 (virgin) and P5 (lookup totality) are state/pointer properties covered
// by the runtime unit tests above (`big_carve_packs_contiguously`,
// `medium_span_of_and_contains`); the proofs here discharge the static
// sizing/alignment bounds that unit tests only sample at fixed classes.
// ---------------------------------------------------------------------------
#[cfg(all(kani, unix, feature = "std"))]
mod kani_proofs {
    use super::*;

    /// Nondeterministic big-class block size within the documented range
    /// (65473..=262144, 16-aligned), plus a nondeterministic page count.
    fn any_big_block_and_pages() -> (usize, usize) {
        let block: usize = kani::any();
        // BIG_CLASSES: 65472 exclusive lower edge in design notes; actual
        // min is the first class >65472 (65488 or similar). Use the full
        // design window: >65472, <=262144, 16-aligned.
        kani::assume(block > 65_472 && block <= 262_144 && block % 16 == 0);
        let pages: usize = kani::any();
        kani::assume(pages >= 1 && pages <= 16);
        (block, pages)
    }

    /// P2 + P6 (sizing): last carved block end stays inside the mapping.
    /// `count = (pages*PAGE_SIZE - BIG_MASTER_SIZE) / block` implies
    /// `BIG_MASTER_SIZE + count*block <= pages*PAGE_SIZE` by div properties;
    /// prove it directly so underflow/overflow in the formula is caught.
    #[kani::proof]
    fn p2_last_block_within_mapping() {
        let (block, pages) = any_big_block_and_pages();
        let total = pages * PAGE_SIZE;
        kani::assume(total >= BIG_MASTER_SIZE);
        let usable = total - BIG_MASTER_SIZE;
        let count = usable / block;
        let last_end = BIG_MASTER_SIZE + count * block;
        assert!(last_end <= total, "P2: last block past mapping end");
        // P6 stronger form: every write target `base + BIG_MASTER_SIZE + k*block`
        // for k in 0..count is < total (carve loop condition `b + block <= end`).
        let first = BIG_MASTER_SIZE;
        assert!(first + count * block <= total, "P6: carve extent overflow");
    }

    /// P1 (contiguity) + P3 (alignment): block k lives at
    /// `BIG_MASTER_SIZE + k*block`, stride == block, 16-aligned when base
    /// is 64 KiB-aligned and both offsets are 16-multiples.
    #[kani::proof]
    fn p1_contiguity_and_p3_alignment() {
        let (block, pages) = any_big_block_and_pages();
        let total = pages * PAGE_SIZE;
        kani::assume(total >= BIG_MASTER_SIZE);
        // Structural: base 64 KiB-aligned; master size and block 16-aligned.
        kani::assume(BIG_MASTER_SIZE % 16 == 0);
        kani::assume(block % 16 == 0);
        let count = (total - BIG_MASTER_SIZE) / block;
        // Nondeterministic index into the freelist.
        let k: usize = kani::any();
        kani::assume(k < count);
        let addr = BIG_MASTER_SIZE + k * block;
        assert!(addr % 16 == 0, "P3: block not 16-aligned");
        assert!(addr + block <= total, "P2/P6: block past end");
        if k > 0 {
            let prev = BIG_MASTER_SIZE + (k - 1) * block;
            assert!(
                addr - prev == block,
                "P1: non-contiguous stride at k={}"
            );
        }
    }

    /// Medium-span equivalent of P2: per-page carve never overruns the page
    /// (`b + block_size <= end` with `end = chunk + PAGE_SIZE`).
    #[kani::proof]
    fn medium_page_carve_stays_in_page() {
        let block: usize = kani::any();
        // Medium window: (16384, 65472], 16-aligned.
        kani::assume(block > 16_384 && block <= 65_472 && block % 16 == 0);
        let chunk_off: usize = kani::any();
        kani::assume(chunk_off < PAGE_SIZE);
        // Header sizes are 16-multiples (SpanMaster 64, sub 16).
        let start = if chunk_off == 0 {
            SPAN_MASTER_SIZE
        } else {
            SPAN_SUB_SIZE
        };
        kani::assume(start < PAGE_SIZE);
        let end = PAGE_SIZE;
        // First block position in-page (relative).
        let b0 = start;
        if b0 + block <= end {
            assert!(b0 + block <= end, "P2 medium: first block in page");
            assert!(b0 % 16 == 0, "P3 medium: start 16-aligned");
        }
        // Stride stays in-page for any valid k.
        let count = end.saturating_sub(start) / block;
        let k: usize = kani::any();
        kani::assume(k < count);
        let addr = start + k * block;
        assert!(addr + block <= end, "P6 medium: block past page end");
    }
}
