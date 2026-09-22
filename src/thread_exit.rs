//! Best-effort thread-exit flush without TLS destructors.
//!
//! DESIGN.md §4.5 explains why the allocator's own TLS carries no
//! destructor. This module adds the missing half: a single OS key whose
//! destructor does nothing but [`ThreadCache::try_flush_all`] — try-locks
//! and unmaps only, never blocking — so dead threads stop pinning their
//! caches behind them. Anything unreleasable is abandoned, exactly like
//! today's always-leak; success is the common case (exiting threads race
//! with almost nothing).
//!
//! Hooks fire only for threads that armed them (allocator slow paths set a
//! per-thread flag and a nonzero key value; OS destructors ignore threads
//! with no value). Threads that never allocate never pay, non-allocator
//! threads are untouched, and the main thread on return-from-main keeps
//! today's behavior (process exit reclaims everything).
//!
//! Only active with `std` on unix/Windows. Elsewhere `ensure_hook` is a
//! no-op and caches behave exactly as before.

#[cfg(all(feature = "std", unix))]
mod imp {
    use core::ffi::c_void;

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

    static HOOK_KEY: std::sync::OnceLock<Option<Key>> = std::sync::OnceLock::new();

    unsafe extern "C" fn thread_exit_flush(_value: *mut c_void) {
        // Blocking flush: at thread exit no allocator locks are held (all
        // critical sections are scoped and user-code-free), so waiting on a
        // class lock can only stall behind another bounded critical section
        // — never deadlock. pthread-key destructors hold no locks that heap
        // locks could cycle with. Try-only flushing was measured to abandon
        // ~everything when several threads exit at once (thundering herd on
        // try_lock); blocking serializes the herd and actually reclaims.
        // Panic-free by construction (bounded loops, atomics, syscalls only).
        eprintln!("[allox-debug] thread-exit flush firing");
        crate::tls_flush_full();
    }

    pub(crate) fn ensure_hook() {
        let key = HOOK_KEY.get_or_init(|| {
            let mut k: Key = 0;
            // SAFETY: out-pointer valid for the call; destructor is a plain
            // extern fn with no allocator interaction beyond try-flush.
            let r = unsafe { pthread_key_create(&mut k, Some(thread_exit_flush)) };
            eprintln!("[allox-debug] pthread_key_create -> {} key {:?}", r, k);
            if r == 0 {
                Some(k)
            } else {
                None
            }
        });
        if let Some(k) = *key {
            // Nonzero value arms the destructor for this thread. Userspace
            // TCB write, no syscall; failure just skips this thread.
            unsafe {
                let _ = pthread_setspecific(k, 1 as *const c_void);
            }
        }
    }
}

#[cfg(all(feature = "std", windows))]
mod imp {
    use core::ffi::c_void;

    const FLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;

    extern "system" {
        fn FlsAlloc(lpCallback: Option<unsafe extern "system" fn(*mut c_void)>) -> u32;
        fn FlsSetValue(dwFlsIndex: u32, lpFlsData: *const c_void) -> i32;
    }

    static HOOK_SLOT: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();

    unsafe extern "system" fn fls_flush(_value: *mut c_void) {
        // Blocking is safe here too: SRWLock never touches the loader lock,
        // and allocator critical sections never touch it either, so no wait
        // cycle exists even though Fls callbacks run during thread teardown.
        // Panic-free by construction (see above).
        crate::tls_flush_full();
    }

    pub(crate) fn ensure_hook() {
        let slot = HOOK_SLOT.get_or_init(|| {
            // SAFETY: callback is a plain extern fn; see above.
            let s = unsafe { FlsAlloc(Some(fls_flush)) };
            if s != FLS_OUT_OF_INDEXES {
                Some(s)
            } else {
                None
            }
        });
        if let Some(s) = *slot {
            // Nonzero value arms the callback for this thread.
            unsafe {
                let _ = FlsSetValue(s, 1 as *const c_void);
            }
        }
    }
}

#[cfg(all(feature = "std", any(unix, windows)))]
pub(crate) use imp::ensure_hook;

/// Fallback: no OS exit hook (no_std, wasm, other platforms). Caches behave
/// exactly as before; use `flush_current_thread()` explicitly.
#[cfg(not(all(feature = "std", any(unix, windows))))]
pub(crate) fn ensure_hook() {}
