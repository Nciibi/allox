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

use crate::sys::{discard, Mutex};
use crate::heap::{MAPPED_PAGES, MAP_CALLS};
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
/// abandons (virtual retained, never reused).
const HOLE_SLOTS: usize = 1024;
/// Byte cap on parked holes. Bounds dark virtual on churn.
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
struct HoleStore {
    len: usize,
    bytes: usize,
    entries: [(usize, usize); HOLE_SLOTS],
}

impl HoleStore {
    const fn new() -> Self {
        HoleStore {
            len: 0,
            bytes: 0,
            entries: [(0, 0); HOLE_SLOTS],
        }
    }
}

pub(crate) struct Arena {
    state: AtomicU8, // 0 = uninit, 1 = ready, 2 = disabled (legacy forever)
    start: AtomicUsize,
    end: AtomicUsize,
    bump: AtomicUsize, // byte offset of the next fresh slice
    init_guard: AtomicU8, // spin-serializes first reservation (0 free, 1 held)
    holes: Mutex<HoleStore>,
    commits: AtomicUsize,
    reuses: AtomicUsize,
    abandoned: AtomicUsize,
}

impl Arena {
    pub(crate) const fn new() -> Self {
        Arena {
            state: AtomicU8::new(0),
            start: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            bump: AtomicUsize::new(0),
            init_guard: AtomicU8::new(0),
            holes: Mutex::new(HoleStore::new()),
            commits: AtomicUsize::new(0),
            reuses: AtomicUsize::new(0),
            abandoned: AtomicUsize::new(0),
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
            let total = match ARENA_SIZE.checked_add(ARENA_ALIGN) {
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
                let tail = aligned + ARENA_SIZE;
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
    /// the entry removed, or null. Lock-scoped; caller commits afterwards.
    fn holes_take(&self, pages: usize) -> *mut u8 {
        let mut holes = self.holes.lock();
        let mut best: Option<usize> = None;
        for i in 0..holes.len {
            let (_, p) = holes.entries[i];
            if p >= pages && best.map_or(true, |b| p < holes.entries[b].1) {
                best = Some(i);
            }
        }
        match best {
            Some(i) => {
                let last = holes.len - 1;
                let (off, p) = holes.entries[i];
                holes.entries[i] = holes.entries[last];
                holes.entries[last] = (0, 0);
                holes.len = last;
                holes.bytes -= p * ARENA_ALIGN;
                (self.start.load(Ordering::Relaxed) + off) as *mut u8
            }
            None => ptr::null_mut(),
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
            holes.bytes += bytes;
        } else {
            self.abandoned.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Commit `pages` (64 KiB units, nonzero) and return the fresh-zeroed
    /// base, or null when unavailable — reservation failed, bump exhausted,
    /// or the commit itself failed. Null is never OOM-by-itself: callers
    /// fall back to legacy mapping paths.
    pub(crate) unsafe fn commit(&self, pages: usize) -> *mut u8 {
        let len = match pages.checked_mul(ARENA_ALIGN) {
            Some(l) if l > 0 => l,
            _ => return ptr::null_mut(),
        };
        if !self.ensure_init() {
            return ptr::null_mut();
        }
        // Best-fit hole first: zero syscalls beyond the commit itself.
        let reuse = self.holes_take(pages);
        if !reuse.is_null() {
            if self.commit_range(reuse as usize, len) {
                self.reuses.fetch_add(1, Ordering::Relaxed);
                self.commits.fetch_add(1, Ordering::Relaxed);
                return reuse;
            }
            self.holes_give(reuse, pages);
            return ptr::null_mut();
        }
        // Bump: lock-free CAS claim, commit after (exclusive by construction).
        let start = self.start.load(Ordering::Relaxed);
        loop {
            let off = self.bump.load(Ordering::Relaxed);
            let end = match off.checked_add(len) {
                Some(e) if e <= ARENA_SIZE => e,
                _ => return ptr::null_mut(), // exhausted: legacy fallback
            };
            match self
                .bump
                .compare_exchange(off, end, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    let base = start + off;
                    if self.commit_range(base, len) {
                        self.commits.fetch_add(1, Ordering::Relaxed);
                        return base as *mut u8;
                    }
                    self.holes_give(base as *mut u8, pages);
                    return ptr::null_mut();
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
}

static ARENA: Arena = Arena::new();

/// Commit `pages` from the process arena; null on unavailable (see
/// [`Arena::commit`]). Fresh zeros guaranteed on success.
pub(crate) unsafe fn commit(pages: usize) -> *mut u8 {
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
