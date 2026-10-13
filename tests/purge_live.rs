//! The purge path must actually be *reached*, not merely be correct.
//!
//! Every other purge test in this crate verifies the contract: a discarded
//! region still reads as zero, a recycled large region is still readable.
//! Those are the right invariants, but they are satisfied *trivially* by a
//! purge that never happens. Nothing in the suite would notice if the
//! cold-retention threshold drifted upward, or if the call site were dropped:
//! the allocator would silently stop returning memory to the OS while every
//! test stayed green.
//!
//! `discard_counts_calls_and_bytes` in `lib.rs` calls `sys::discard`
//! directly. That proves the backend increments its counters. It does not
//! prove the allocator ever gets there.
//!
//! So this test drives ordinary allocation churn through the public API and
//! requires the purge counter to move. It lives alone in its own binary
//! because `purge_calls` is a process-global monotonic counter: in a shared
//! binary a neighbouring test's purges would satisfy the assertion for us,
//! and the test would stop testing anything.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

#[test]
fn ordinary_churn_reaches_the_purge_path() {
    let before = allox::__diagnostics::volume().purge_calls;
    let before_bytes = allox::__diagnostics::volume().purge_bytes;

    // Bounded, and sized to cross the cold-retention ladder: enough 64 KiB
    // spans that the sharded caches must evict rather than grow.
    let block = 64 * 1024;
    for _ in 0..400 {
        let mut live: Vec<*mut u8> = Vec::with_capacity(256);
        for _ in 0..256 {
            let p = unsafe { allox::malloc(block) };
            assert!(!p.is_null(), "OOM at {block}");
            live.push(p);
        }
        for p in live {
            unsafe { allox::free(p) };
        }
    }
    allox::flush_current_thread();

    let after = allox::__diagnostics::volume();
    assert!(
        after.purge_calls > before,
        "churn never reached a purge: purge_calls stayed at {before}. \
         The cold-retention ladder is no longer discarding anything."
    );
    assert!(
        after.purge_bytes > before_bytes,
        "purges reported zero bytes: the backend is being called with an \
         empty extent"
    );
}
