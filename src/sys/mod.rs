//! OS virtual memory primitives and a dependency-free mutex.
//!
//! Everything here avoids heap allocation so the allocator can never
//! recursively re-enter itself through `GlobalAlloc`.

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(target_family = "wasm")]
pub(crate) mod wasm;
#[cfg(windows)]
pub(crate) mod windows;

use core::cell::UnsafeCell;

#[cfg(all(unix, feature = "std"))]
pub(crate) use unix::{discard, map, map_any, unmap, RawMutex};
#[cfg(all(unix, not(feature = "std")))]
pub(crate) use unix::{discard, map, map_any, unmap};
#[cfg(all(not(windows), not(unix), target_family = "wasm"))]
pub(crate) use wasm::{discard, map, unmap};
#[cfg(all(not(windows), not(unix), target_family = "wasm"))]
pub(crate) use wasm::map as map_any;
#[cfg(all(not(windows), not(unix), not(target_family = "wasm")))]
compile_error!("allox has no memory backend for this target");
#[cfg(windows)]
pub(crate) use windows::{discard, map, unmap, RawMutex};
/// Windows VirtualAlloc is already single-syscall and 64 KiB-aligned.
#[cfg(windows)]
pub(crate) use windows::map as map_any;

/// Fallback spin mutex: Windows has SRWLock, hosted unix has pthread above;
/// everything else (no_std targets, wasm) spins. Only ever taken on batched
/// slow paths, and it can never allocate.
#[cfg(not(any(windows, all(unix, feature = "std"))))]
pub(crate) use spin_raw::RawMutex;
#[cfg(not(any(windows, all(unix, feature = "std"))))]
mod spin_raw {
    use core::sync::atomic::{AtomicBool, Ordering};

    pub(crate) struct RawMutex {
        locked: AtomicBool,
    }

    impl RawMutex {
        pub(crate) const fn new() -> Self {
            RawMutex {
                locked: AtomicBool::new(false),
            }
        }

        #[inline]
        pub(crate) fn lock(&self) {
            let mut spins = 0u32;
            loop {
                if !self.locked.swap(true, Ordering::Acquire) {
                    return;
                }
                while self.locked.load(Ordering::Relaxed) && spins < 64 {
                    core::hint::spin_loop();
                    spins += 1;
                }
                if self.locked.load(Ordering::Relaxed) {
                    #[cfg(feature = "std")]
                    std::thread::yield_now();
                    #[cfg(not(feature = "std"))]
                    {
                        // No OS to schedule us out; keep spinning.
                        core::hint::spin_loop();
                        core::hint::spin_loop();
                        core::hint::spin_loop();
                        core::hint::spin_loop();
                    }
                    spins = 0;
                }
            }
        }

        #[inline]
        pub(crate) fn unlock(&self) {
            self.locked.store(false, Ordering::Release);
        }
    }
}

/// Mutex built on a platform raw mutex.
///
/// - Windows: SRWLock — contended threads park in the kernel.
/// - Other platforms: adaptive spin (bounded spinning, then `yield_now` where
///   an OS exists). The allocator only takes this on batched slow paths, so
///   spinning briefly is acceptable there; none of it can allocate, which is
///   the property that actually matters inside `GlobalAlloc`.
pub(crate) struct Mutex<T> {
    raw: RawMutex,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}

pub(crate) struct MutexGuard<'a, T: 'a> {
    mutex: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub(crate) const fn new(value: T) -> Self {
        Mutex {
            raw: RawMutex::new(),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        // Lock-wait timing needs a clock read, so it is telemetry-only.
        //
        // There is deliberately no always-on acquisition counter here: this
        // is the one place where a shared atomic cannot be batched (the
        // cache is not in hand), and one global counter incremented by all
        // ~64 class locks is a single contended cache line on every slow
        // path. `flushes`/`*_refills` in `__diagnostics::volume()` proxy the
        // lock traffic instead.
        #[cfg(all(feature = "telemetry", feature = "std"))]
        let t0 = std::time::Instant::now();
        self.raw.lock();
        #[cfg(all(feature = "telemetry", feature = "std"))]
        crate::counters::bump_ns(
            &crate::counters::TIMING.lock_wait_ns,
            t0.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        );
        MutexGuard { mutex: self }
    }
}

impl<T> core::ops::Deref for MutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}
impl<T> core::ops::DerefMut for MutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}
impl<T> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.raw.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::Mutex;

    #[test]
    fn mutex_excludes_and_releases() {
        static M: Mutex<u32> = Mutex::new(0);
        {
            let mut g = M.lock();
            *g += 1;
        }
        assert_eq!(*M.lock(), 1);
    }

    /// Contended slow-path hammering: must stay correct and complete (this
    /// is the shape heap slow paths take; parking backends must not lose
    /// wakeups under it).
    #[cfg(feature = "std")]
    #[test]
    fn mutex_survives_contention() {
        static M: Mutex<u64> = Mutex::new(0);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..10_000 {
                        *M.lock() += 1;
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*M.lock(), 80_000);
    }
}
