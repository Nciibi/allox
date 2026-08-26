//! OS virtual memory primitives and a dependency-free mutex.
//!
//! Everything here avoids heap allocation so the allocator can never
//! recursively re-enter itself through `GlobalAlloc`.

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(unix)]
pub(crate) mod unix;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
pub(crate) use windows::{map, unmap};
#[cfg(unix)]
pub(crate) use unix::{map, unmap};

/// Spin-then-yield mutex.
///
/// The allocator only takes this on slow paths (page refill/flush), and those
/// are batched (>=32 blocks per acquisition), so spinning briefly before
/// yielding is sufficient. Unlike `std::sync::Mutex` this can never allocate,
/// which would risk unbounded recursion inside `GlobalAlloc`.
pub(crate) struct Mutex<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}

pub(crate) struct MutexGuard<'a, T: 'a> {
    mutex: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub(crate) const fn new(value: T) -> Self {
        Mutex {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        let mut spins = 0u32;
        loop {
            if !self.locked.swap(true, Ordering::Acquire) {
                return MutexGuard { mutex: self };
            }
            while self.locked.load(Ordering::Relaxed) && spins < 64 {
                core::hint::spin_loop();
                spins += 1;
            }
            if self.locked.load(Ordering::Relaxed) {
                std::thread::yield_now();
                spins = 0;
            }
        }
    }
}

impl<T> core::ops::Deref for MutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.mutex.value.get().cast::<T>().read_ref()
    }
}
impl<T> core::ops::DerefMut for MutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        self.mutex.value.get().cast::<T>().read_mut()
    }
}
impl<T> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
    }
}
