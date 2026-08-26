//! OS virtual memory primitives and a dependency-free mutex.
//!
//! Everything here avoids heap allocation so the allocator can never
//! recursively re-enter itself through `GlobalAlloc`.

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(target_family = "wasm")]
pub(crate) mod wasm;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
pub(crate) use unix::{map, unmap};
#[cfg(windows)]
pub(crate) use windows::{map, unmap};
#[cfg(target_family = "wasm")]
pub(crate) use wasm::{map, unmap};

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
        self.raw.lock();
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

// ---------------------------------------------------------------------------
// RawMutex backends for non-Windows platforms (Windows has SRWLock above).
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
pub(crate) struct RawMutex {
    locked: AtomicBool,
}

#[cfg(not(windows))]
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