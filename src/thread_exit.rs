//! Best-effort thread-exit retirement without TLS destructors.
//!
//! DESIGN.md §4.5 explains why the allocator's own TLS carries no
//! destructor. This module adds a single OS key (pthread_key / FlsAlloc)
//! whose destructor retires the exiting thread's cache into a bounded queue.
//! Later allocator slow paths adopt or reclaim queued caches; oversized or
//! overflow caches fall back to the normal blocking flush. Hooks fire only for
//! threads that armed them, so threads that never allocate never pay.
//!
#[cfg(all(feature = "std", any(unix, windows)))]
static FLUSH_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

#[cfg(all(feature = "std", any(unix, windows)))]
fn record_flush() {
    FLUSH_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // One extra relaxed add per exiting thread: negligible next to the
    // flush itself, and it is the only place that can see the cost.
    crate::counters::bump(&crate::counters::VOLUME.exit_flushes, 1);
    #[cfg(all(feature = "telemetry", feature = "std"))]
    {
        let t0 = std::time::Instant::now();
        // SAFETY: called from the OS thread-exit hook (or its synchronous
        // fallback) on the exiting thread, exactly where the destructor
        // used to call it.
        unsafe { crate::tls_retire() };
        crate::counters::bump_ns(
            &crate::counters::TIMING.exit_flush_ns,
            t0.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        );
    }
    #[cfg(not(feature = "telemetry"))]
    // SAFETY: see above.
    unsafe { crate::tls_retire() };
}

pub(crate) fn flush_count() -> u64 {
    #[cfg(all(feature = "std", any(unix, windows)))]
    {
        FLUSH_COUNT.load(core::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(all(feature = "std", any(unix, windows))))]
    {
        0
    }
}

#[cfg(all(feature = "std", unix))]
mod imp {
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, Ordering};

    // pthread_key_t width varies: 32-bitish on Linux/BSDs, 64-bit on Apple.
    #[cfg(target_vendor = "apple")]
    type Key = usize;
    #[cfg(not(target_vendor = "apple"))]
    type Key = u32;

    extern "C" {
        fn pthread_key_create(
            key: *mut Key,
            dtor: Option<unsafe extern "C" fn(*mut c_void)>,
        ) -> i32;
        fn pthread_setspecific(key: Key, value: *const c_void) -> i32;
    }

    static HOOK_KEY: std::sync::OnceLock<Key> = std::sync::OnceLock::new();
    static INSTALLING: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn thread_exit_flush(_value: *mut c_void) {
        if let Some(k) = HOOK_KEY.get() {
            unsafe {
                let _ = pthread_setspecific(*k, core::ptr::null());
            }
        }
        super::record_flush();
    }

    pub(crate) fn ensure_hook() -> bool {
        loop {
            if let Some(k) = HOOK_KEY.get() {
                return unsafe { pthread_setspecific(*k, 1 as *const c_void) == 0 };
            }
            if INSTALLING
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let key = if let Some(k) = HOOK_KEY.get() {
                    Some(*k)
                } else {
                    let mut k: Key = 0;
                    // SAFETY: out-pointer valid for the call; destructor is a plain
                    // extern fn with no allocator interaction beyond the full flush.
                    let r = unsafe { pthread_key_create(&mut k, Some(thread_exit_flush)) };
                    if r == 0 {
                        match HOOK_KEY.set(k) {
                            Ok(()) => Some(k),
                            Err(existing) => Some(existing),
                        }
                    } else {
                        None
                    }
                };
                INSTALLING.store(false, Ordering::Release);
                return match key {
                    Some(k) => unsafe { pthread_setspecific(k, 1 as *const c_void) == 0 },
                    None => false,
                };
            }
            while INSTALLING.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
        }
    }
}

#[cfg(all(feature = "std", windows))]
mod imp {
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, Ordering};

    const FLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;

    extern "system" {
        fn FlsAlloc(lpCallback: Option<unsafe extern "system" fn(*mut c_void)>) -> u32;
        fn FlsSetValue(dwFlsIndex: u32, lpFlsData: *const c_void) -> i32;
    }

    static HOOK_SLOT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    static INSTALLING: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn fls_flush(_value: *mut c_void) {
        if let Some(s) = HOOK_SLOT.get() {
            unsafe {
                let _ = FlsSetValue(*s, core::ptr::null());
            }
        }
        super::record_flush();
    }

    pub(crate) fn ensure_hook() -> bool {
        loop {
            if let Some(s) = HOOK_SLOT.get() {
                return unsafe { FlsSetValue(*s, 1 as *const c_void) == 0 };
            }
            if INSTALLING
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let slot = if let Some(s) = HOOK_SLOT.get() {
                    Some(*s)
                } else {
                    // SAFETY: callback is a plain extern fn; see above.
                    let s = unsafe { FlsAlloc(Some(fls_flush)) };
                    if s != FLS_OUT_OF_INDEXES {
                        match HOOK_SLOT.set(s) {
                            Ok(()) => Some(s),
                            Err(existing) => Some(existing),
                        }
                    } else {
                        None
                    }
                };
                INSTALLING.store(false, Ordering::Release);
                return match slot {
                    Some(s) => unsafe { FlsSetValue(s, 1 as *const c_void) == 0 },
                    None => false,
                };
            }
            while INSTALLING.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
        }
    }
}

#[cfg(all(feature = "std", any(unix, windows)))]
pub(crate) use imp::ensure_hook;

/// Fallback: no OS exit hook (no_std, wasm, other platforms). Caches behave
/// exactly as before; use `flush_current_thread()` explicitly.
#[cfg(not(all(feature = "std", any(unix, windows))))]
pub(crate) fn ensure_hook() -> bool {
    true
}
