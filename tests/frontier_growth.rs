#![cfg(all(unix, feature = "std"))]

use allox::Allox;
use std::alloc::{GlobalAlloc, Layout};

#[global_allocator]
static GLOBAL: Allox = Allox;

#[test]
fn large_realloc_grows_at_arena_frontier() {
    unsafe {
        let before = allox::__debug_arena_detail().1;
        let p = allox::malloc(8 * 1024 * 1024);
        assert!(!p.is_null());
        let after = allox::__debug_arena_detail().1;
        if after <= before {
            allox::free(p);
            return;
        }

        for i in [0usize, 8 * 1024 * 1024 - 1] {
            *p.add(i) = (i % 251) as u8;
        }
        let q = allox::realloc(p, 16 * 1024 * 1024);
        assert_eq!(q, p);
        assert!(*q == 0);
        assert_eq!(*q.add(8 * 1024 * 1024 - 1), ((8 * 1024 * 1024 - 1) % 251) as u8);
        assert!(allox::usable_size(q) >= 16 * 1024 * 1024);
        allox::free(q);
    }
}
