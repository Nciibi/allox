//! Page structures: 64 KiB OS-mapped pages holding blocks of one size class,
//! plus the header layout used for large (directly mapped) regions.

use crate::classes::CLASSES;
use core::ptr;

pub(crate) const PAGE_SHIFT: u32 = 16;
pub(crate) const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub(crate) const PAGE_MASK: usize = PAGE_SIZE - 1;

/// Marks memory at a 64 KiB boundary as an allocator-managed small page.
pub(crate) const PAGE_MAGIC: u64 = 0xA110_CCA7_E5A11E_5Du64;
/// Marks memory at a 64 KiB boundary as a large, directly mapped region.
pub(crate) const LARGE_MAGIC: u64 = 0xB10C_5A6E_0FF1CEu64;

/// Flag: page is currently linked into its size class' partial list.
pub(crate) const FLAG_IN_PARTIAL: u16 = 1;

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
        ((p as usize & !PAGE_MASK) as *mut PageHeader)
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
        self.flags = 0;
    }
}

#[repr(C, align(16))]
pub(crate) struct LargeHeader {
    pub(crate) magic: u64,
    pub(crate) mapped_size: usize,
}

pub(crate) const LARGE_HEADER_SIZE: usize = 32; // padded to keep user ptr 16-aligned

#[inline]
pub(crate) fn align_up(v: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (v + align - 1) & !(align - 1)
}
