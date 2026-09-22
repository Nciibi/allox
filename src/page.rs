//! Page structures: 64 KiB OS-mapped pages holding blocks of one size class,
//! multi-page spans for medium blocks, plus the header layout used for large
//! (directly mapped) regions.

use crate::classes::{MEDIUM_CLASSES, NUM_MEDIUM};
use crate::classes::CLASSES;
use core::ptr;

pub(crate) const PAGE_SHIFT: u32 = 16;
pub(crate) const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub(crate) const PAGE_MASK: usize = PAGE_SIZE - 1;

/// Marks memory at a 64 KiB boundary as an allocator-managed small page.
pub(crate) const PAGE_MAGIC: u64 = 0xA110_CCA7_E5A1_1E5D;
/// Marks the base of a medium multi-page span (first 64 KiB unit).
pub(crate) const SPAN_MAGIC: u64 = 0xA110_CCA7_5EED_5A11;
/// Marks a non-first 64 KiB unit of a medium span; +8 holds the master ptr.
pub(crate) const SPAN_SUBMAGIC: u64 = 0xA110_CCA7_5EED_5A12;
/// Marks memory at a 64 KiB boundary as a large, directly mapped region.
pub(crate) const LARGE_MAGIC: u64 = 0x00B1_0C5A_6E0F_F1CE;

/// Flag: page is currently linked into its size class' partial list.
pub(crate) const FLAG_IN_PARTIAL: u16 = 1;
/// Flag: no block of this page was ever allocated-and-freed since the page
/// was carved, so every free block is still OS-zero. Cleared the moment any
/// block is returned to the page.
pub(crate) const FLAG_VIRGIN: u16 = 2;

// magic + prev + next + free_head + free_count/used/class/flags = 40 bytes,
// padded by align(16) to 48.
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
// npages/pad = 56 bytes, padded by align(16) to 64.
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
    pub(crate) _pad: u32,
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
        self.magic = SPAN_MAGIC;
        self.prev = ptr::null_mut();
        self.next = ptr::null_mut();
        self.free_head = head;
        self.free_count = count;
        self.used = 0;
        self.mclass = mclass as u16;
        self.flags = FLAG_VIRGIN;
        self.npages = npages;
        self._pad = 0;
    }

    /// Byte size of the whole span mapping (for unmap).
    #[inline]
    pub(crate) unsafe fn mapped_bytes(&self) -> usize {
        self.npages as usize * PAGE_SIZE
    }
}

#[repr(C, align(16))]
pub(crate) struct LargeHeader {    pub(crate) magic: u64,
    pub(crate) mapped_size: usize,
    /// True base of the OS mapping (may differ from the page this header
    /// appears in, when alignment pushed the user pointer across a boundary).
    pub(crate) base: *mut u8,
}

pub(crate) const LARGE_HEADER_SIZE: usize = 32; // padded to keep user ptr 16-aligned

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
