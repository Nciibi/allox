//! Virtual-memory arena for 64 KiB-granular commits (unix + `std` only).
//!
//! Reservation/commit split, after mimalloc/glibc: reserve one large
//! `PROT_NONE` region once per process (virtual address space only — no page
//! tables, no commit charge), then commit slices on demand with a single
//! `MAP_FIXED` each. A large miss costs 1 VMA write op instead of 3 (mmap +
//! 2 trim unmaps), alignment is structural (every slice is a 64 KiB multiple
//! of a 64 KiB-aligned base), and freed slices recycle through size-local
//! hole stacks with zero syscalls.
//!
//! Design rules (all load-bearing, all tested):
//! - The kernel never places third-party mappings inside our live
//!   reservation, so `MAP_FIXED` inside it cannot clobber anything else.
//!   Commits additionally verify `ret == requested`.
//! - `munmap` is NEVER called inside the reservation (it would punch holes
//!   third parties could claim, breaking the rule above). Overflow returns
//!   are discarded + abandoned instead: virtual stays reserved, physical
//!   drops, nothing reuses them. Bounded by overflow rate; documented.
//! - Every take path re-commits before handing memory out, so contents are
//!   always fresh zeros regardless of discard success — same guarantee as a
//!   fresh `mmap`, which is why `fresh=true` is sound for arena commits.
//! - Discard-then-park ordering (never park-then-discard): a slice is only
//!   discardable under exclusive ownership, i.e. before parking. Parking
//!   first and discarding after unlock raced with reuse and wiped live data
//!   (found the hard way with span cold lists).
//! - Universal fallback: reservation failure, bump exhaustion, commit
//!   failure, and non-unix targets all degrade to the legacy `sys::map`
//!   paths. Callers treat null as "unavailable", never as OOM.
//!
//! Only compiled on unix with `std` (needs an atomic once-guard; the raw
//! syscalls are hand-declared so the lib stays dependency-free). Everywhere
//! else the callers use their legacy paths directly.

use crate::page::{BigMaster, LargeHeader};
use crate::sys::{discard, Mutex};
use core::ptr;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

pub(crate) const ARENA_ALIGN: usize = 64 * 1024;

/// Virtual reservation size. Address space only — costs nothing until
/// committed. Sized well above measured peak virtual across benches.
#[cfg(target_pointer_width = "64")]
const ARENA_SIZE: usize = 16 * 1024 * 1024 * 1024;
/// 32-bit address space can't host gigabytes: small arena, frequent (safe)
/// fallback. The design degrades gracefully by construction.
#[cfg(not(target_pointer_width = "64"))]
const ARENA_SIZE: usize = 512 * 1024 * 1024;

/// Hole-stack slots. Best-fit scans stay L1-resident; overflow discards and
/// abandons (virtual retained, never reused). Sized so large-only variance
/// bursts (32K-1M uniform churn parks thousands of mixed-size holes) fit:
/// 1024 overflowed ~22k/run into abandonment + 16 GiB reservation
/// exhaustion, 2048 still overflowed ~21k, 4096 absorbs with zero
/// abandonment (measured §3 E0-E2). 4096 x 16 B entries = 64 KiB static;
/// scans run only on fresh takes (already past a mutex + before a
/// MAP_FIXED), so scan cost stays well under the syscall it replaces.
const HOLE_SLOTS: usize = 4096;/// Byte cap on parked holes. Bounds dark virtual on churn.
#[cfg(target_pointer_width = "64")]
const HOLE_CAP_BYTES: usize = 4 * 1024 * 1024 * 1024;
#[cfg(not(target_pointer_width = "64"))]
const HOLE_CAP_BYTES: usize = 256 * 1024 * 1024;

const MAP_FIXED: i32 = 0x10; // Linux, macOS, *BSD agree on this value.
const MAP_PRIVATE: i32 = 0x02;
const PROT_READ_WRITE: i32 = 0x03;
const PROT_NONE: i32 = 0x00;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
const MAP_ANONYMOUS: i32 = 0x1000;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
const MAP_ANONYMOUS: i32 = 0x20;

extern "C" {
    fn mmap(
        addr: *mut core::ffi::c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut core::ffi::c_void;
    fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
}

/// (offset bytes from arena start, pages). Values, never intrusive links:
///
/// discarded slices read back as zeros, so any metadata stored inside them
/// would not survive. Offsets (not absolute bases) keep the entries
/// position-independent garbage on reset paths.
const EXACT_BUCKET_MAX: usize = 64;
const COALESCE_MIN_PAGES: usize = 16;
const EMPTY_BUCKET: u16 = u16::MAX;

struct HoleStore {
    len: usize,
    bytes: usize,
    entries: [(usize, usize); HOLE_SLOTS],
    exact: [u16; EXACT_BUCKET_MAX + 1],
    coalesce_dirty: bool,
}

impl HoleStore {
    const fn new() -> Self {
        HoleStore {
            len: 0,
            bytes: 0,
            entries: [(0, 0); HOLE_SLOTS],
            exact: [EMPTY_BUCKET; EXACT_BUCKET_MAX + 1],
            coalesce_dirty: false,
        }
    }

    fn refresh_exact(&mut self, pages: usize) {
        if pages > EXACT_BUCKET_MAX {
            return;
        }
        let mut found = EMPTY_BUCKET;
        let mut i = 0;
        while i < self.len {
            if self.entries[i].1 == pages {
                found = i as u16;
                break;
            }
            i += 1;
        }
        self.exact[pages] = found;
    }

    fn rebuild_exact(&mut self) {
        self.exact = [EMPTY_BUCKET; EXACT_BUCKET_MAX + 1];
        let mut i = 0;
        while i < self.len {
            let pages = self.entries[i].1;
            if pages <= EXACT_BUCKET_MAX && self.exact[pages] == EMPTY_BUCKET {
                self.exact[pages] = i as u16;
            }
            i += 1;
        }
    }

    fn coalesce(&mut self) -> usize {
        if !self.coalesce_dirty || self.len < 2 {
            self.coalesce_dirty = false;
            return 0;
        }
        self.entries[..self.len].sort_unstable_by_key(|entry| entry.0);
        let old_len = self.len;
        let mut write = 0;
        let mut merged_pages = 0;
        for read in 0..old_len {
            let entry = self.entries[read];
            if write != 0 {
                let (previous_off, previous_pages) = self.entries[write - 1];
                if previous_off + previous_pages * ARENA_ALIGN == entry.0 {
                    self.entries[write - 1].1 += entry.1;
                    merged_pages += entry.1;
                    continue;
                }
            }
            self.entries[write] = entry;
            write += 1;
        }
        for index in write..old_len {
            self.entries[index] = (0, 0);
        }
        self.len = write;
        self.coalesce_dirty = false;
        self.rebuild_exact();
        merged_pages
    }
}

pub(crate) struct Arena {
    state: AtomicU8, // 0 = uninit, 1 = ready, 2 = disabled (legacy forever)
    start: AtomicUsize,
    end: AtomicUsize,
    bump: AtomicUsize, // byte offset of the next fresh slice
    init_guard: AtomicU8, // spin-serializes first reservation (0 free, 1 held)
    holes: Mutex<HoleStore>,
    hole_count: AtomicUsize,
    #[cfg(feature = "telemetry")]
    hole_scans: AtomicUsize,
    #[cfg(feature = "telemetry")]
    hole_hits: AtomicUsize,
    #[cfg(feature = "telemetry")]
    hole_exact_hits: AtomicUsize,
    #[cfg(feature = "telemetry")]
    hole_splits: AtomicUsize,
    #[cfg(feature = "telemetry")]
    hole_empty_fastpath: AtomicUsize,
    #[cfg(feature = "telemetry")]
    hole_coalesce_checks: AtomicUsize,
    #[cfg(feature = "telemetry")]
    hole_coalesces: AtomicUsize,
    #[cfg(feature = "telemetry")]
    hole_coalesced_pages: AtomicUsize,
    commits: AtomicUsize,
    reuses: AtomicUsize,
    abandoned: AtomicUsize,
    /// Reservation size for this instance (global uses `ARENA_SIZE`).
    size: usize,
}

impl Arena {
    pub(crate) const fn new() -> Self {
        Self::with_size(ARENA_SIZE)
    }

    pub(crate) const fn with_size(size: usize) -> Self {
        Arena {
            state: AtomicU8::new(0),
            start: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            bump: AtomicUsize::new(0),
            init_guard: AtomicU8::new(0),
            holes: Mutex::new(HoleStore::new()),
            hole_count: AtomicUsize::new(0),
            #[cfg(feature = "telemetry")]
            hole_scans: AtomicUsize::new(0),
            #[cfg(feature = "telemetry")]
            hole_hits: AtomicUsize::new(0),
            #[cfg(feature = "telemetry")]
            hole_exact_hits: AtomicUsize::new(0),
            #[cfg(feature = "telemetry")]
            hole_splits: AtomicUsize::new(0),
            #[cfg(feature = "telemetry")]
            hole_empty_fastpath: AtomicUsize::new(0),
            #[cfg(feature = "telemetry")]
            hole_coalesce_checks: AtomicUsize::new(0),
            #[cfg(feature = "telemetry")]
            hole_coalesces: AtomicUsize::new(0),
            #[cfg(feature = "telemetry")]
            hole_coalesced_pages: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
            reuses: AtomicUsize::new(0),
            abandoned: AtomicUsize::new(0),
            size,
        }
    }

    /// Reserve once per instance. True iff the arena is usable afterwards.
    fn ensure_init(&self) -> bool {
        if self.state.load(Ordering::Acquire) == 1 {
            return true;
        }
        if self.state.load(Ordering::Acquire) == 2 {
            return false;
        }
        // Serialize first reservation; the window is once-per-process.
        while self.init_guard.swap(1, Ordering::Acquire) != 0 {
            core::hint::spin_loop();
        }
        if self.state.load(Ordering::Relaxed) == 0 {
            let total = match self.size.checked_add(ARENA_ALIGN) {
                Some(t) => t,
                None => {
                    self.state.store(2, Ordering::Release);
                    self.init_guard.store(0, Ordering::Release);
                    return false;
                }
            };
            // SAFETY: NULL hint, anonymous/private, valid length. PROT_NONE
            // reserves address space without page tables or commit charge.
            let raw = unsafe {
                mmap(
                    ptr::null_mut(),
                    total,
                    PROT_NONE,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    -1,
                    0,
                ) as usize
            };
            if raw == usize::MAX {
                self.state.store(2, Ordering::Release);
            } else {
                let aligned = (raw + ARENA_ALIGN - 1) & !(ARENA_ALIGN - 1);
                // Trim alignment slack once per process (2 munmaps, ever).
                if aligned != raw {
                    unsafe {
                        munmap(raw as *mut core::ffi::c_void, aligned - raw);
                    }
                }
                let tail = aligned + self.size;
                if tail != raw + total {
                    unsafe {
                        munmap(tail as *mut core::ffi::c_void, raw + total - tail);
                    }
                }
                self.start.store(aligned, Ordering::Relaxed);
                self.end.store(tail, Ordering::Relaxed);
                self.bump.store(0, Ordering::Relaxed);
                self.state.store(1, Ordering::Release);
            }
        }
        self.init_guard.store(0, Ordering::Release);
        self.state.load(Ordering::Acquire) == 1
    }

    /// Commit one slice over `base..base+len` (both 64 KiB-granular, inside
    /// our own reservation). True iff the kernel mapped exactly what we
    /// asked — the `ret == base` check catches ancient kernels that treat
    /// unknown flags as hints instead of failing.
    unsafe fn commit_range(&self, base: usize, len: usize) -> bool {
        // SAFETY: range is exclusively claimed (bump CAS or hole pop under
        // lock) inside our live reservation, so MAP_FIXED cannot clobber
        // anything else. Anonymous/private, fresh zero pages guaranteed.
        let ret = mmap(
            base as *mut core::ffi::c_void,
            len,
            PROT_READ_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
            -1,
            0,
        ) as usize;
        ret == base
    }

    /// Pop a best-fit hole of at least `pages`. Returns the base address with
    /// the entry removed or split, or null. Lock-scoped; caller commits
    /// afterwards.
    fn holes_take(&self, pages: usize) -> *mut u8 {
        if self.hole_count.load(Ordering::Acquire) == 0 {
            #[cfg(feature = "telemetry")]
            self.hole_empty_fastpath.fetch_add(1, Ordering::Relaxed);
            return ptr::null_mut();
        }
        let mut holes = self.holes.lock();
        let mut best: Option<usize> = None;
        #[cfg(feature = "telemetry")]
        let mut exact_hit = false;
        if pages <= EXACT_BUCKET_MAX {
            let mut index = holes.exact[pages] as usize;
            if index >= holes.len || holes.entries[index].1 != pages {
                holes.refresh_exact(pages);
                index = holes.exact[pages] as usize;
            }
            if index < holes.len && holes.entries[index].1 == pages {
                best = Some(index);
                #[cfg(feature = "telemetry")]
                {
                    exact_hit = true;
                }
            }
        }
        #[cfg(feature = "telemetry")]
        let mut scanned = 0usize;
        if best.is_none() {
            for i in 0..holes.len {
                #[cfg(feature = "telemetry")]
                {
                    scanned += 1;
                }
                let (_, p) = holes.entries[i];
                if p == pages {
                    best = Some(i);
                    break;
                }
                if p > pages && best.map_or(true, |b| p < holes.entries[b].1) {
                    best = Some(i);
                }
            }
        }
        #[cfg(feature = "telemetry")]
        self.hole_scans.fetch_add(scanned, Ordering::Relaxed);
        #[cfg(feature = "telemetry")]
        if exact_hit {
            self.hole_exact_hits.fetch_add(1, Ordering::Relaxed);
        }
        match best {
            Some(i) => {
                let (off, p) = holes.entries[i];
                holes.bytes -= pages * ARENA_ALIGN;
                #[cfg(feature = "telemetry")]
                self.hole_hits.fetch_add(1, Ordering::Relaxed);
                if p > pages {
                    holes.entries[i] = (off + pages * ARENA_ALIGN, p - pages);
                    holes.refresh_exact(p);
                    let remainder = p - pages;
                    if remainder <= EXACT_BUCKET_MAX {
                        holes.exact[remainder] = i as u16;
                    }
                    #[cfg(feature = "telemetry")]
                    self.hole_splits.fetch_add(1, Ordering::Relaxed);
                } else {
                    let last = holes.len - 1;
                    holes.entries[i] = holes.entries[last];
                    holes.entries[last] = (0, 0);
                    holes.len = last;
                    holes.refresh_exact(p);
                }
                self.hole_count.store(holes.len, Ordering::Release);
                (self.start.load(Ordering::Relaxed) + off) as *mut u8
            }
            None => {
                self.hole_count.store(holes.len, Ordering::Release);
                ptr::null_mut()
            }
        }
    }

    fn coalesce_holes(&self) {
        let mut holes = self.holes.lock();
        if !holes.coalesce_dirty {
            return;
        }
        #[cfg(feature = "telemetry")]
        self.hole_coalesce_checks.fetch_add(1, Ordering::Relaxed);
        let merged_pages = holes.coalesce();
        #[cfg(feature = "telemetry")]
        if merged_pages > 0 {
            self.hole_coalesces.fetch_add(1, Ordering::Relaxed);
            self.hole_coalesced_pages
                .fetch_add(merged_pages, Ordering::Relaxed);
        }
        #[cfg(not(feature = "telemetry"))]
        let _ = merged_pages;
        self.hole_count.store(holes.len, Ordering::Release);
    }

    unsafe fn commit_hole(&self, pages: usize, len: usize) -> *mut u8 {
        let reuse = self.holes_take(pages);
        if reuse.is_null() {
            return ptr::null_mut();
        }
        if self.commit_range(reuse as usize, len) {
            self.reuses.fetch_add(1, Ordering::Relaxed);
            self.commits.fetch_add(1, Ordering::Relaxed);
            reuse
        } else {
            self.holes_give(reuse, pages);
            ptr::null_mut()
        }
    }

    /// Park a slice for reuse. Discard-then-park is the caller's job (needs
    /// exclusive ownership, which only the caller has pre-lock); see docs.
    /// Overflow discards nothing (caller already did) and abandons the entry:
    /// virtual stays reserved, physical dropped, nothing ever reuses or
    /// unmaps it. Bounded by overflow rate; counted for observability.
    fn holes_give(&self, base: *mut u8, pages: usize) {
        let start = self.start.load(Ordering::Relaxed);
        let off = (base as usize).wrapping_sub(start);
        let bytes = pages * ARENA_ALIGN;
        let mut holes = self.holes.lock();
        if holes.len < HOLE_SLOTS && holes.bytes + bytes <= HOLE_CAP_BYTES {
            let idx = holes.len;
            holes.entries[idx] = (off, pages);
            holes.len = idx + 1;
            holes.coalesce_dirty = holes.len > 1;
            holes.bytes += bytes;
            if pages <= EXACT_BUCKET_MAX {
                holes.exact[pages] = idx as u16;
            }
            self.hole_count.store(holes.len, Ordering::Release);
        } else {
            self.abandoned.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Commit `pages` (64 KiB units, nonzero) and return `(base, fresh)`:
    /// `base` is null when unavailable — reservation failed, bump exhausted,
    /// or the commit itself failed. Null is never OOM-by-itself: callers
    /// fall back to legacy mapping paths. Fresh zeros guaranteed on success.
    ///
    /// `fresh` is true only for bump commits (genuinely new virtual address
    /// space). Hole reuses recommit already-live virtual, so `fresh` is
    /// false for them. Counter contract for callers: count one mapping op
    /// (`MAP_CALLS` + class split) per non-null return — every success is
    /// one kernel mapping either way — but count live virtual
    /// (`MAPPED_PAGES`) only when `fresh` is set. Hole pops, parks, and
    /// abandonments change no counters (virtual stays reserved either way),
    /// which keeps `MAPPED_PAGES` equal to live virtual instead of drifting
    /// as a cumulative-takes counter under churn.
    pub(crate) unsafe fn commit(&self, pages: usize) -> (*mut u8, bool) {
        let len = match pages.checked_mul(ARENA_ALIGN) {
            Some(l) if l > 0 => l,
            _ => return (ptr::null_mut(), false),
        };
        if !self.ensure_init() {
            return (ptr::null_mut(), false);
        }
        let reuse = self.commit_hole(pages, len);
        if !reuse.is_null() {
            return (reuse, false);
        }
        if pages >= COALESCE_MIN_PAGES && self.hole_count.load(Ordering::Acquire) > 1 {
            self.coalesce_holes();
            let reuse = self.commit_hole(pages, len);
            if !reuse.is_null() {
                return (reuse, false);
            }
        }
        // Bump: lock-free CAS claim, commit after (exclusive by construction).
        let start = self.start.load(Ordering::Relaxed);
        loop {
            let off = self.bump.load(Ordering::Relaxed);
            let end = match off.checked_add(len) {
                Some(e) if e <= self.size => e,
                _ => {
                    if pages >= COALESCE_MIN_PAGES
                        && self.hole_count.load(Ordering::Acquire) > 1
                    {
                        self.coalesce_holes();
                    }
                    let reuse = self.commit_hole(pages, len);
                    return (reuse, false);
                }
            };
            match self
                .bump
                .compare_exchange(off, end, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    let base = start + off;
                    if self.commit_range(base, len) {
                        self.commits.fetch_add(1, Ordering::Relaxed);
                        return (base as *mut u8, true);
                    }
                    self.holes_give(base as *mut u8, pages);
                    return (ptr::null_mut(), false);
                }
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Return a slice previously obtained from [`Arena::commit`]. Discards
    /// physical FIRST (exclusive ownership pre-lock — parking first and
    /// discarding after unlock would race a concurrent pop+reuse and wipe
    /// live data), then parks for reuse or abandons on overflow.
    pub(crate) unsafe fn release(&self, base: *mut u8, pages: usize) {
        let len = match pages.checked_mul(ARENA_ALIGN) {
            Some(l) if l > 0 => l,
            _ => {
                debug_assert!(false, "arena release of empty range");
                return;
            }
        };
        discard(base, len);
        self.holes_give(base, pages);
    }

    /// True iff `[base, base+len)` lies fully inside the live reservation.
    /// Legacy (non-arena) mappings can never satisfy this: the kernel never
    /// overlaps them with our existing VMA.
    pub(crate) fn contains(&self, base: *mut u8, len: usize) -> bool {
        if self.state.load(Ordering::Acquire) != 1 {
            return false;
        }
        let start = self.start.load(Ordering::Relaxed);
        let end = self.end.load(Ordering::Relaxed);
        let b = base as usize;
        match b.checked_add(len) {
            Some(top) => start <= b && top <= end && b >= start,
            None => false,
        }
    }

    pub(crate) fn stats(&self) -> (u64, u64, u64) {
        (
            self.commits.load(Ordering::Relaxed) as u64,
            self.reuses.load(Ordering::Relaxed) as u64,
            self.abandoned.load(Ordering::Relaxed) as u64,
        )
    }

    pub(crate) fn hole_stats(&self) -> (u64, u64, u64, u64) {
        #[cfg(feature = "telemetry")]
        {
            (
                self.hole_scans.load(Ordering::Relaxed) as u64,
                self.hole_hits.load(Ordering::Relaxed) as u64,
                self.hole_splits.load(Ordering::Relaxed) as u64,
                self.hole_empty_fastpath.load(Ordering::Relaxed) as u64,
            )
        }
        #[cfg(not(feature = "telemetry"))]
        {
            (0, 0, 0, 0)
        }
    }

    pub(crate) fn coalesce_stats(&self) -> (u64, u64, u64) {
        #[cfg(feature = "telemetry")]
        {
            (
                self.hole_coalesce_checks.load(Ordering::Relaxed) as u64,
                self.hole_coalesces.load(Ordering::Relaxed) as u64,
                self.hole_coalesced_pages.load(Ordering::Relaxed) as u64,
            )
        }
        #[cfg(not(feature = "telemetry"))]
        {
            (0, 0, 0)
        }
    }

    /// Monotonic reservation high-water in bytes (the bump frontier only
    /// advances). Compare against the reservation size to validate headroom.
    pub(crate) fn high_water(&self) -> u64 {
        self.bump.load(Ordering::Relaxed) as u64
    }
}

static ARENA: Arena = Arena::new();

/// Commit `pages` from the process arena: `(base, fresh)` — null base on
/// unavailable (see [`Arena::commit`]). Fresh zeros guaranteed on success.
pub(crate) unsafe fn commit(pages: usize) -> (*mut u8, bool) {
    ARENA.commit(pages)
}

/// Return an arena slice; no-op-safe for any input (misuse still discards,
/// which is always safe, then parks garbage the pop path can never match…
/// callers must only pass arena-owned slices — enforced by [`contains`]).
pub(crate) unsafe fn release(base: *mut u8, pages: usize) {
    ARENA.release(base, pages)
}

/// Membership test for the unmap-vs-return decision.
pub(crate) fn contains(base: *mut u8, len: usize) -> bool {
    ARENA.contains(base, len)
}

/// (commits, hole reuses, abandoned). Hidden observability for tuning.
pub(crate) fn stats() -> (u64, u64, u64) {
    ARENA.stats()
}

pub(crate) fn hole_stats() -> (u64, u64, u64, u64) {
    ARENA.hole_stats()
}

pub(crate) fn coalesce_stats() -> (u64, u64, u64) {
    ARENA.coalesce_stats()
}

/// Reservation high-water in bytes (monotonic bump frontier). Hidden
/// observability for reservation-sizing validation.
pub(crate) fn high_water() -> u64 {
    ARENA.high_water()
}

// ---------------------------------------------------------------------------
// Big-span side table: page-indexed master lookup for big spans, whose data
// chunks carry no headers (blocks cross chunk boundaries), so masking cannot
// locate them. Entry `i` is the master address for arena page `i`, or 0.
//
// The table covers the GLOBAL arena only (offsets are instance-relative, so
// test instances must never touch it — and never do: only BigHeap carve /
// unmap paths write it, and those run solely against the global arena).
// 16 GiB / 64 KiB = 262144 entries x 8 B = 2 MiB static (BSS, faulted on
// touch, bounded by touched regions). Smaller reservations use a prefix;
// out-of-reservation pointers never index it (bounds-checked first).
//
// Lifecycle (writes under the owning class lock unless noted): set for all
// span pages at carve; kept across empty/cold parks (virtual retained, base
// stable, discard never touches this table); cleared for all pages on TRUE
// UNMAP before unmapping. Hole entries are therefore always table-clean:
// holes only come from unmap-fate releases (cleared there) or never-handed
// bump ranges (never set). Reads use Acquire loads with no lock; every hit
// is validated by `BigMaster::contains` before use (fail-closed like all
// magic probes in dispatch).
// ---------------------------------------------------------------------------

/// Slots for the largest possible reservation (64-bit); smaller
/// reservations use a prefix (bounds-checked at every access).
const BIG_MAP_SLOTS: usize = (16 * 1024 * 1024 * 1024) / ARENA_ALIGN;

static BIG_MAP: [AtomicUsize; BIG_MAP_SLOTS] = [const { AtomicUsize::new(0) }; BIG_MAP_SLOTS];

/// Page index of `p` in the global reservation, or `None` outside it
/// (legacy mappings, foreign memory) or before init.
fn big_page_index(p: *mut u8) -> Option<usize> {
    if ARENA.state.load(Ordering::Acquire) != 1 {
        return None;
    }
    let start = ARENA.start.load(Ordering::Relaxed);
    let end = ARENA.end.load(Ordering::Relaxed);
    let b = p as usize;
    if b < start || b >= end {
        return None;
    }
    let idx = (b - start) / ARENA_ALIGN;
    if idx >= BIG_MAP_SLOTS {
        return None;
    }
    Some(idx)
}

const LARGE_TABLE_TAG: usize = 1;
const MEDIUM_TABLE_TAG: usize = 2;

pub(crate) unsafe fn big_table_set(base: *mut u8, pages: u32, master: *mut BigMaster) {
    for i in 0..pages as usize {
        match big_page_index((base as usize + i * ARENA_ALIGN) as *mut u8) {
            Some(idx) => BIG_MAP[idx].store(master as usize, Ordering::Release),
            None => debug_assert!(false, "big table set outside reservation"),
        }
    }
}

pub(crate) unsafe fn large_table_set(
    base: *mut u8,
    pages: u32,
    header: *mut LargeHeader,
) {
    for i in 0..pages as usize {
        match big_page_index((base as usize + i * ARENA_ALIGN) as *mut u8) {
            Some(idx) => BIG_MAP[idx].store(header as usize | LARGE_TABLE_TAG, Ordering::Release),
            None => debug_assert!(false, "large table set outside reservation"),
        }
    }
}

pub(crate) unsafe fn medium_table_set(
    base: *mut u8,
    pages: u32,
    master: *mut crate::page::SpanMaster,
) {
    for i in 0..pages as usize {
        match big_page_index((base as usize + i * ARENA_ALIGN) as *mut u8) {
            Some(idx) => BIG_MAP[idx].store(master as usize | MEDIUM_TABLE_TAG, Ordering::Release),
            None => debug_assert!(false, "medium table set outside reservation"),
        }
    }
}

pub(crate) unsafe fn big_table_clear(base: *mut u8, pages: u32) {
    for i in 0..pages as usize {
        match big_page_index((base as usize + i * ARENA_ALIGN) as *mut u8) {
            Some(idx) => BIG_MAP[idx].store(0, Ordering::Release),
            None => debug_assert!(false, "big table clear outside reservation"),
        }
    }
}

pub(crate) unsafe fn large_table_clear(base: *mut u8, pages: u32) {
    for i in 0..pages as usize {
        match big_page_index((base as usize + i * ARENA_ALIGN) as *mut u8) {
            Some(idx) => BIG_MAP[idx].store(0, Ordering::Release),
            None => debug_assert!(false, "large table clear outside reservation"),
        }
    }
}

pub(crate) unsafe fn medium_table_clear(base: *mut u8, pages: u32) {
    for i in 0..pages as usize {
        match big_page_index((base as usize + i * ARENA_ALIGN) as *mut u8) {
            Some(idx) => BIG_MAP[idx].store(0, Ordering::Release),
            None => debug_assert!(false, "medium table clear outside reservation"),
        }
    }
}

pub(crate) unsafe fn big_table_get(p: *mut u8) -> *mut BigMaster {
    match big_page_index(p) {
        Some(idx) => {
            let raw = BIG_MAP[idx].load(Ordering::Acquire);
            if raw & (LARGE_TABLE_TAG | MEDIUM_TABLE_TAG) != 0 {
                ptr::null_mut()
            } else {
                raw as *mut BigMaster
            }
        }
        None => ptr::null_mut(),
    }
}

pub(crate) unsafe fn large_table_get(p: *mut u8) -> *mut LargeHeader {
    match big_page_index(p) {
        Some(idx) => {
            let raw = BIG_MAP[idx].load(Ordering::Acquire);
            if raw & LARGE_TABLE_TAG == 0 {
                ptr::null_mut()
            } else {
                (raw & !LARGE_TABLE_TAG) as *mut LargeHeader
            }
        }
        None => ptr::null_mut(),
    }
}

pub(crate) unsafe fn medium_table_get(p: *mut u8) -> *mut crate::page::SpanMaster {
    match big_page_index(p) {
        Some(idx) => {
            let raw = BIG_MAP[idx].load(Ordering::Acquire);
            if raw & MEDIUM_TABLE_TAG == 0 {
                ptr::null_mut()
            } else {
                (raw & !MEDIUM_TABLE_TAG) as *mut crate::page::SpanMaster
            }
        }
        None => ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classes::{medium_capacity_for, span_pages_for, MEDIUM_CLASSES};
    use crate::page::{BigMaster, SpanMaster, PAGE_SIZE};

    const MB: usize = 1024 * 1024;

    /// Side-table lifecycle on the global arena (the only arena the table
    /// indexes): set covers every page, get resolves each page to the
    /// master, clear drops all of them, and foreign pointers miss.
    /// Fresh bump slices give disjoint offsets, so parallel tests can't
    /// alias entries.
    // Raw mmap/MAP_FIXED: Miri cannot execute these syscalls.
    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn big_table_set_get_clear() {
        unsafe {
            let (raw, _) = ARENA.commit(4);
            assert!(!raw.is_null());
            let mut master = BigMaster {
                magic: 0,
                prev: ptr::null_mut(),
                next: ptr::null_mut(),
                free_head: ptr::null_mut(),
                free_count: 0,
                used: 0,
                bclass: 0,
                flags: 0,
                npages: 4,
                owner: core::sync::atomic::AtomicU32::new(0),
            };
            let mptr = &mut master as *mut BigMaster;
            big_table_set(raw, 4, mptr);
            for i in 0..4usize {
                let p = (raw as usize + i * ARENA_ALIGN) as *mut u8;
                assert_eq!(big_table_get(p), mptr, "page {}", i);
            }
            big_table_clear(raw, 4);
            for i in 0..4usize {
                let p = (raw as usize + i * ARENA_ALIGN) as *mut u8;
                assert!(big_table_get(p).is_null(), "page {} not cleared", i);
            }
            // Foreign (legacy) mapping misses.
            let guard = super::super::sys::map(ARENA_ALIGN);
            assert!(!guard.is_null());
            assert!(big_table_get(guard).is_null());
            super::super::sys::unmap(guard, ARENA_ALIGN);
            ARENA.release(raw, 4);
        }
    }

    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn hole_count_fast_path_and_counters() {
        let a = Arena::with_size(4 * ARENA_ALIGN);
        assert!(a.holes_take(1).is_null());
        let (base, fresh) = unsafe { a.commit(1) };
        assert!(!base.is_null() && fresh);
        unsafe { a.release(base, 1) };
        assert_eq!(a.hole_count.load(Ordering::Acquire), 1);
        let (reuse, fresh) = unsafe { a.commit(1) };
        assert_eq!(reuse, base);
        assert!(!fresh);
        assert_eq!(a.hole_count.load(Ordering::Acquire), 0);
        unsafe { a.release(reuse, 1) };
        let b = Arena::with_size(4 * ARENA_ALIGN);
        let (large, fresh) = unsafe { b.commit(2) };
        assert!(!large.is_null() && fresh);
        unsafe { b.release(large, 2) };
        assert_eq!(b.hole_count.load(Ordering::Acquire), 1);
        let first = b.holes_take(1);
        assert!(!first.is_null());
        assert_eq!(b.hole_count.load(Ordering::Acquire), 1);
        let (remainder, fresh) = unsafe { b.commit(1) };
        assert!(!remainder.is_null() && !fresh);
        unsafe { b.release(remainder, 1) };
        #[cfg(feature = "telemetry")]
        {
            assert!(b.hole_exact_hits.load(Ordering::Acquire) >= 1);
            let stats = b.hole_stats();
            assert!(stats.0 >= 1);
            assert!(stats.1 >= 2);
            assert!(stats.2 >= 1);
            assert!(stats.3 >= 1);
        }
    }

    #[test]
    fn coalesce_sorts_and_rebuilds_exact_index() {
        let mut holes = HoleStore::new();
        holes.entries[0] = (3 * ARENA_ALIGN, 1);
        holes.entries[1] = (ARENA_ALIGN, 1);
        holes.entries[2] = (2 * ARENA_ALIGN, 1);
        holes.len = 3;
        holes.bytes = 3 * ARENA_ALIGN;
        holes.coalesce_dirty = true;

        assert_eq!(holes.coalesce(), 2);
        assert_eq!(holes.len, 1);
        assert_eq!(holes.entries[0], (ARENA_ALIGN, 3));
        assert_eq!(holes.entries[1], (0, 0));
        assert_eq!(holes.exact[3], 0);
        assert_eq!(holes.exact[1], EMPTY_BUCKET);
        assert!(!holes.coalesce_dirty);
    }

    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn medium_table_resolves_cross_page_blocks() {
        unsafe {
            let block = MEDIUM_CLASSES[0];
            let pages = span_pages_for(block);
            let (raw, _) = ARENA.commit(pages);
            assert!(!raw.is_null());
            let span = raw.cast::<SpanMaster>();
            (*span).init(0, pages as u32);
            medium_table_set(raw, pages as u32, span);
            let expected = medium_capacity_for(block, pages);
            assert_eq!((*span).free_count as usize, expected);
            let mut current = (*span).free_head;
            let mut count = 0usize;
            let mut crossed = false;
            while !current.is_null() {
                assert_eq!(SpanMaster::of(current), span);
                assert!(current as usize + block <= raw as usize + pages * PAGE_SIZE);
                if (current as usize) / PAGE_SIZE != (current as usize + block - 1) / PAGE_SIZE {
                    crossed = true;
                }
                count += 1;
                current = *current.cast::<*mut u8>();
            }
            assert_eq!(count, expected);
            assert!(crossed);
            medium_table_clear(raw, pages as u32);
            assert!(medium_table_get(raw).is_null());
            ARENA.release(raw, pages);
        }
    }

    // Raw mmap/MAP_FIXED: Miri cannot execute these syscalls.
    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn commits_are_64k_aligned_and_zeroed() {
        let a = Arena::with_size(64 * MB);
        for pages in [1usize, 3, 17, 129] {
            let b = unsafe { a.commit(pages).0 };
            assert!(!b.is_null(), "commit {} pages", pages);
            assert_eq!(b as usize % ARENA_ALIGN, 0);
            assert!(a.contains(b, pages * ARENA_ALIGN));
            // Fresh zeros guaranteed: dirty it, release, recommit, re-read.
            unsafe {
                core::ptr::write_bytes(b, 0xAB, pages * ARENA_ALIGN);
                a.release(b, pages);
                let b2 = a.commit(pages).0;
                assert!(!b2.is_null());
                for i in 0..pages * ARENA_ALIGN {
                    assert_eq!(*b2.add(i), 0, "stale byte at {}", i);
                }
                a.release(b2, pages);
            }
        }
    }

    // Raw mmap/MAP_FIXED: Miri cannot execute these syscalls.
    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn exhaustion_reuses_coalesced_holes() {
        // 256 KiB arena = 4 pages: exact-supply then graceful nulls.
        let a = Arena::with_size(4 * ARENA_ALIGN);
        let mut bases = Vec::new();
        for _ in 0..4 {
            let b = unsafe { a.commit(1).0 };
            assert!(!b.is_null());
            bases.push(b);
        }
        assert!(unsafe { a.commit(1).0 }.is_null());
        assert!(unsafe { a.commit(64).0 }.is_null());
        // Exact-size holes serve exact requests (no syscalls beyond commit).
        let first = bases[0];
        for b in bases.drain(..) {
            unsafe { a.release(b, 1) };
        }
        for _ in 0..4 {
            let b = unsafe { a.commit(1).0 };
            assert!(!b.is_null(), "exact hole reuse");
            unsafe { a.release(b, 1) };
        }
        let (coalesced, fresh) = unsafe { a.commit(4) };
        assert_eq!(coalesced, first);
        assert!(!fresh);
        assert_eq!(a.hole_count.load(Ordering::Acquire), 0);
        #[cfg(feature = "telemetry")]
        assert_eq!(a.coalesce_stats(), (1, 1, 3));
        unsafe { a.release(coalesced, 4) };
    }

    // Raw mmap/MAP_FIXED: Miri cannot execute these syscalls.
    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn hole_reuse_avoids_new_commits() {
        let a = Arena::with_size(64 * MB);
        let mut bases = Vec::new();
        for _ in 0..8 {
            bases.push(unsafe { a.commit(2).0 });
        }
        let (c0, _, _) = a.stats();
        assert_eq!(c0, 8);
        for b in bases.drain(..) {
            unsafe { a.release(b, 2) };
        }
        for _ in 0..8 {
            let b = unsafe { a.commit(2).0 };
            assert!(!b.is_null());
            unsafe { a.release(b, 2) };
        }
        let (c1, r1, _) = a.stats();
        assert_eq!(c1, c0 + 8, "every reuse still commits (fresh zeros)");
        assert_eq!(r1, 8, "all served from holes, none from bump");
    }

    // Raw mmap/MAP_FIXED: Miri cannot execute these syscalls.
    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn commit_reports_fresh_only_for_new_virtual() {
        // Live-virtual accounting depends on this: bump commits are new
        // address space (callers count MAPPED_PAGES), hole reuses recommit
        // virtual that is already counted (callers must not count again,
        // since releases never decrement).
        let a = Arena::with_size(64 * MB);
        let (b1, fresh1) = unsafe { a.commit(2) };
        assert!(!b1.is_null() && fresh1, "bump commit is new virtual");
        unsafe { a.release(b1, 2) };
        let (b2, fresh2) = unsafe { a.commit(2) };
        assert!(!b2.is_null() && !fresh2, "hole reuse is already-counted virtual");
        unsafe { a.release(b2, 2) };
    }

    // Raw mmap/MAP_FIXED: Miri cannot execute these syscalls.
    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn split_remainder_stays_usable() {
        let a = Arena::with_size(64 * MB);
        let big = unsafe { a.commit(16).0 };
        assert!(!big.is_null());
        unsafe { a.release(big, 16) };
        // Best-fit takes the 16-page hole for a 1-page request...
        let small = unsafe { a.commit(1).0 };
        assert!(!small.is_null());
        // ...and the 15-page remainder must serve a later request.
        let rest = unsafe { a.commit(15).0 };
        assert!(!rest.is_null(), "split remainder lost");
        assert_eq!(
            rest as usize,
            small as usize + ARENA_ALIGN,
            "oversized hole was not split at the requested boundary"
        );
        unsafe {
            a.release(small, 1);
            a.release(rest, 15);
        }
    }

    // Raw mmap/MAP_FIXED: Miri cannot execute these syscalls.
    #[cfg_attr(miri, ignore = "raw mmap not available under Miri")]
    #[test]
    fn commits_never_clobber_neighbors() {
        // Guard mapping via the legacy path, then churn the arena around it.
        let a = Arena::with_size(64 * MB);
        let guard = unsafe { super::super::sys::map(ARENA_ALIGN) };
        assert!(!guard.is_null());
        unsafe { core::ptr::write_bytes(guard, 0x5A, ARENA_ALIGN) };
        let mut v = Vec::new();
        for i in 0..64usize {
            let pages = 1 + (i * 7919) % 9;
            let b = unsafe { a.commit(pages).0 };
            assert!(!b.is_null());
            unsafe { core::ptr::write_bytes(b, i as u8, pages * ARENA_ALIGN) };
            v.push((b, pages));
        }
        for (b, pages) in v.drain(..) {
            unsafe { a.release(b, pages) };
        }
        for i in 0..ARENA_ALIGN {
            assert_eq!(unsafe { *guard.add(i) }, 0x5A, "clobbered at {}", i);
        }
        unsafe { super::super::sys::unmap(guard, ARENA_ALIGN) };
    }
}
