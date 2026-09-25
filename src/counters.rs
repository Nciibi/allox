//! Diagnostic counters, split by what they cost to produce.
//!
//! **Volume counters** (counts and bytes) are only ever touched on paths
//! that already take a lock, map, unmap or copy, so they are always
//! compiled and always updated: the incremental cost is one relaxed
//! `fetch_add` on a cold cache line. They are read through the hidden
//! [`crate::__diagnostics::volume`] so the benchmark harness can sample
//! them without enabling the `telemetry` feature.
//!
//! **Timing counters** (nanoseconds) need a clock read, which costs more
//! than most of the operations being measured, so they are compiled only
//! with the `telemetry` feature and read through
//! `telemetry::timing()`. A production build with the feature off pays
//! nothing and has no counter statics at all.
//!
//! Every counter is monotonic and process-wide. They are diagnostics, not
//! accounting: correctness never reads them, so a lost race under relaxed
//! ordering can only skew a tuning number, never behaviour.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        /// Always-compiled volume counters. Field order is part of the
        /// hidden `__diagnostics` ABI; append, never reorder.
        #[allow(missing_docs)]
        pub struct VolumeCounters {
            $(pub $name: AtomicU64,)*
        }

        impl VolumeCounters {
            const fn new() -> Self {
                VolumeCounters { $($name: AtomicU64::new(0),)* }
            }

            /// Flat snapshot, in declaration order.
            pub fn snapshot(&self) -> [u64; VOLUME_COUNT] {
                [
                    $(self.$name.load(Relaxed),)*
                ]
            }
        }

        pub static VOLUME: VolumeCounters = VolumeCounters::new();
    };
}

counters! {
    // --- thread-cache traffic (all on slow paths) ---
    // Heap-mutex acquisitions (every tier, every slow path).
    heap_lock_acquisitions,
    // Batches pulled from the global heap into a thread cache, small tier.
    small_refills,
    // Blocks delivered by those small refills.
    small_refill_blocks,
    // Same, medium tier.
    medium_refills,
    medium_refill_blocks,
    // Same, big tier.
    big_refills,
    big_refill_blocks,
    // Cache-budget trim passes.
    trims,
    // Cache-to-heap flush calls.
    flushes,
    // Blocks returned to the heap by those flushes.
    flush_blocks,
    // Ownership probes on the free path (drift gate open only).
    owner_probes,
    // Frees of blocks owned by another thread (drift gate open only).
    remote_frees,
    // Thread caches retired through the exit queue.
    retired_caches,
    // Retired caches adopted directly by a worker with an empty cache.
    adopted_caches,
    // Thread-exit hook invocations (each one retires or flushes a cache).
    exit_flushes,

    // --- copy / zero work ---
    // `realloc` calls that returned a different pointer.
    realloc_relocations,
    // Bytes memmove'd by a relocating `realloc`.
    realloc_copy_bytes,
    // Relocations served by the big-block growth promotion.
    realloc_promotions,
    // `alloc_zeroed`/`calloc` calls that had to zero memory in software.
    zeroed_calls,
    // Bytes zeroed in software by those calls.
    zeroed_bytes,

    // --- OS-facing work ---
    // `madvise`/`VirtualAlloc` discard calls (physical page drop).
    purge_calls,
    // Bytes passed to discard.
    purge_bytes,
    // Large regions served by the legacy mmap path because the arena was
    // unavailable or exhausted.
    arena_fallbacks,
    // Large regions parked in the arena hole store.
    arena_parks,
}

/// Number of volume counters. Kept in step with [`VOLUME_FIELDS`] by the
/// assertion below, so appending a counter without a reader fails here.
pub const VOLUME_COUNT: usize = 24;

/// Nanosecond timings. `telemetry` feature only: a clock read costs more
/// than most operations here, so production builds without the feature
/// neither compile nor update them.
#[cfg(feature = "telemetry")]
mod timing_impl {
    #[cfg(feature = "std")]
    use core::sync::atomic::AtomicU64;

    /// Cumulative wall time, in nanoseconds, spent in instrumented
    /// regions. Divide by the matching call count in
    /// [`crate::__diagnostics::volume`] for a mean.
    ///
    /// Without `std` there is no clock, so every field stays zero.
    #[derive(Clone, Copy, Debug, Default)]
    #[allow(missing_docs)]
    pub struct TimingCounters {
        pub lock_wait_ns: u64,
        pub purge_ns: u64,
        pub exit_flush_ns: u64,
    }

    #[cfg(feature = "std")]
    pub struct TimingAtomics {
        pub lock_wait_ns: AtomicU64,
        pub purge_ns: AtomicU64,
        pub exit_flush_ns: AtomicU64,
    }

    #[cfg(feature = "std")]
    impl TimingAtomics {
        const fn new() -> Self {
            TimingAtomics {
                lock_wait_ns: AtomicU64::new(0),
                purge_ns: AtomicU64::new(0),
                exit_flush_ns: AtomicU64::new(0),
            }
        }

        pub fn snapshot(&self) -> TimingCounters {
            use core::sync::atomic::Ordering::Relaxed;
            TimingCounters {
                lock_wait_ns: self.lock_wait_ns.load(Relaxed),
                purge_ns: self.purge_ns.load(Relaxed),
                exit_flush_ns: self.exit_flush_ns.load(Relaxed),
            }
        }
    }

    #[cfg(feature = "std")]
    pub static TIMING: TimingAtomics = TimingAtomics::new();

    /// Read the timings. Only with `std`: without a clock there is nothing
    /// to read, and `crate::counters::timing_snapshot` returns zeros.
    #[cfg(feature = "std")]
    pub fn snapshot() -> TimingCounters {
        TIMING.snapshot()
    }
}

/// Timing-counter accessors. `pub(crate)` so the telemetry module can build
/// its public snapshot without exposing the atomics.
#[cfg(all(feature = "telemetry", feature = "std"))]
pub(crate) use timing_impl::snapshot as timing_snapshot;
/// Without `std` there is no clock, so every timing reads zero.
#[cfg(all(feature = "telemetry", not(feature = "std")))]
pub(crate) fn timing_snapshot() -> TimingCounters {
    TimingCounters::default()
}
#[cfg(all(feature = "telemetry", feature = "std"))]
pub(crate) use timing_impl::TIMING;
#[cfg(feature = "telemetry")]
#[allow(unused_imports)]
pub(crate) use timing_impl::TimingCounters;

/// Named-field view of the always-on volume counters, for consumers that
/// want more than a positional array.
#[derive(Clone, Copy, Debug)]
pub struct Volume {
    /// Heap-mutex acquisitions (every tier, every slow path).
    pub heap_lock_acquisitions: u64,
    /// Batches pulled from the global heap into a thread cache, small tier.
    pub small_refills: u64,
    pub small_refill_blocks: u64,
    pub medium_refills: u64,
    pub medium_refill_blocks: u64,
    pub big_refills: u64,
    pub big_refill_blocks: u64,
    pub trims: u64,
    pub flushes: u64,
    pub flush_blocks: u64,
    pub owner_probes: u64,
    pub remote_frees: u64,
    pub retired_caches: u64,
    pub adopted_caches: u64,
    /// Thread-exit hook invocations (each one retires or flushes a cache).
    pub exit_flushes: u64,
    pub realloc_relocations: u64,
    pub realloc_copy_bytes: u64,
    pub realloc_promotions: u64,
    pub zeroed_calls: u64,
    pub zeroed_bytes: u64,
    pub purge_calls: u64,
    pub purge_bytes: u64,
    pub arena_fallbacks: u64,
    pub arena_parks: u64,
}

/// Read the always-on volume counters by name.
pub fn volume() -> Volume {
    let v = &VOLUME;
    Volume {
        heap_lock_acquisitions: v.heap_lock_acquisitions.load(Relaxed),
        small_refills: v.small_refills.load(Relaxed),
        small_refill_blocks: v.small_refill_blocks.load(Relaxed),
        medium_refills: v.medium_refills.load(Relaxed),
        medium_refill_blocks: v.medium_refill_blocks.load(Relaxed),
        big_refills: v.big_refills.load(Relaxed),
        big_refill_blocks: v.big_refill_blocks.load(Relaxed),
        trims: v.trims.load(Relaxed),
        flushes: v.flushes.load(Relaxed),
        flush_blocks: v.flush_blocks.load(Relaxed),
        owner_probes: v.owner_probes.load(Relaxed),
        remote_frees: v.remote_frees.load(Relaxed),
        retired_caches: v.retired_caches.load(Relaxed),
        adopted_caches: v.adopted_caches.load(Relaxed),
        exit_flushes: v.exit_flushes.load(Relaxed),
        realloc_relocations: v.realloc_relocations.load(Relaxed),
        realloc_copy_bytes: v.realloc_copy_bytes.load(Relaxed),
        realloc_promotions: v.realloc_promotions.load(Relaxed),
        zeroed_calls: v.zeroed_calls.load(Relaxed),
        zeroed_bytes: v.zeroed_bytes.load(Relaxed),
        purge_calls: v.purge_calls.load(Relaxed),
        purge_bytes: v.purge_bytes.load(Relaxed),
        arena_fallbacks: v.arena_fallbacks.load(Relaxed),
        arena_parks: v.arena_parks.load(Relaxed),
    }
}

/// Positional snapshot, cheapest to read in a benchmark loop.
pub fn volume_raw() -> [u64; VOLUME_COUNT] {
    VOLUME.snapshot()
}

/// Field names in [`volume_raw`] order, so a consumer can label columns.
pub const VOLUME_FIELDS: [&str; VOLUME_COUNT] = [
    "heap_lock_acquisitions",
    "small_refills",
    "small_refill_blocks",
    "medium_refills",
    "medium_refill_blocks",
    "big_refills",
    "big_refill_blocks",
    "trims",
    "flushes",
    "flush_blocks",
    "owner_probes",
    "remote_frees",
    "retired_caches",
    "adopted_caches",
    "exit_flushes",
    "realloc_relocations",
    "realloc_copy_bytes",
    "realloc_promotions",
    "zeroed_calls",
    "zeroed_bytes",
    "purge_calls",
    "purge_bytes",
    "arena_fallbacks",
    "arena_parks",
];

const _: () = assert!(VOLUME_FIELDS.len() == VOLUME_COUNT);

/// Increment a volume counter. `#[inline]` and a single relaxed add; the
/// counter is only reached on paths that already do far more work.
#[inline(always)]
pub(crate) fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Relaxed);
}

/// Read a volume counter (unit tests and the bench probe).
#[cfg(test)]
#[inline]
pub(crate) fn get(counter: &AtomicU64) -> u64 {
    counter.load(Relaxed)
}

/// Add nanoseconds to a timing counter. `telemetry` + `std` only: the
/// callers read a clock, which `no_std` has no reason to pull in.
#[cfg(all(feature = "telemetry", feature = "std"))]
#[inline]
pub(crate) fn bump_ns(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_counters_start_clean_and_bump() {
        // Fresh statics: this test binary has not run the allocator yet, but
        // be tolerant — just prove the mapping is wired both ways.
        let before = get(&VOLUME.trims);
        bump(&VOLUME.trims, 1);
        assert_eq!(get(&VOLUME.trims), before + 1);
        bump(&VOLUME.trims, 1);
    }

    #[test]
    fn raw_and_named_snapshots_agree() {
        let raw = volume_raw();
        let named = volume();
        assert_eq!(raw.len(), VOLUME_FIELDS.len());
        assert_eq!(raw[VOLUME_FIELDS.iter().position(|f| *f == "trims").unwrap()], named.trims);
        assert_eq!(
            raw[VOLUME_FIELDS
                .iter()
                .position(|f| *f == "realloc_copy_bytes")
                .unwrap()],
            named.realloc_copy_bytes
        );
    }
}
