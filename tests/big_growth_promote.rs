//! Growable-extent promotion for packed big blocks.
//!
//! A big block crossing size classes used to relocate on every step of a
//! doubling chain (packed spans cannot grow in place). The first cross-class
//! growth now relocates once into a large region that reserves slack up to
//! the big cap, so the rest of the chain runs in place with no copy. These
//! tests pin both halves of that contract: content is always preserved, and
//! once growth settles in place it never relocates again.

use allox::malloc;
use std::alloc::{GlobalAlloc, Layout};

const BIG_TOP: usize = 1_048_576;

fn fill(p: *mut u8, size: usize) {
    unsafe {
        for i in 0..size {
            *p.add(i) = (i % 251) as u8;
        }
    }
}

fn check(p: *mut u8, size: usize) {
    unsafe {
        for i in 0..size {
            assert_eq!(*p.add(i), (i % 251) as u8, "byte {}", i);
        }
    }
}

/// Walk the ECS growth chain (64 KiB doubled to the 1 MiB cap), verifying
/// content at every step and that in-place growth is sticky.
#[test]
fn growth_chain_preserves_content_and_settles_in_place() {
    unsafe {
        let mut size = 65536usize;
        let mut p = malloc(size);
        assert!(!p.is_null());
        fill(p, size);

        let mut settled = false;
        while size < BIG_TOP {
            let nsize = (size * 2).min(BIG_TOP);
            let np = allox::realloc(p, nsize);
            assert!(!np.is_null(), "grow {} -> {}", size, nsize);
            // Whatever happened (relocate or grow), the old prefix survives.
            check(np, size);
            if np == p {
                // Once a growth fits in place, every later growth must too:
                // the reserve only ever grows, so it can never start moving
                // again.
                settled = true;
            } else {
                assert!(!settled, "relocated after settling in place at {}", nsize);
            }
            for i in size..nsize {
                *np.add(i) = (i % 251) as u8;
            }
            p = np;
            size = nsize;
        }
        check(p, BIG_TOP);
        allox::free(p);
    }
}

/// The global-allocator entry point takes the same promotion path as the
/// free function, and keeps alignment + content across the chain.
#[test]
fn global_realloc_growth_chain() {
    unsafe {
        let a = allox::Allox;
        let mut size = 65536usize;
        let mut layout = Layout::from_size_align(size, 16).unwrap();
        let mut p = a.alloc(layout);
        assert!(!p.is_null());
        fill(p, size);

        let mut settled = false;
        while size < BIG_TOP {
            let nsize = (size * 2).min(BIG_TOP);
            let np = a.realloc(p, layout, nsize);
            assert!(!np.is_null(), "grow {} -> {}", size, nsize);
            assert_eq!(np as usize % 16, 0, "alignment preserved");
            check(np, size);
            if np == p {
                settled = true;
            } else {
                assert!(!settled, "relocated after settling in place at {}", nsize);
            }
            for i in size..nsize {
                *np.add(i) = (i % 251) as u8;
            }
            p = np;
            size = nsize;
            layout = Layout::from_size_align(size, 16).unwrap();
        }
        check(p, BIG_TOP);
        a.dealloc(p, layout);
    }
}

/// Cross-class growth with a shrink back down must round-trip content and
/// leave a freeable pointer.
#[test]
fn growth_then_shrink_round_trips() {
    unsafe {
        let a = allox::Allox;
        let mut size = 65536usize;
        let layout = Layout::from_size_align(size, 16).unwrap();
        let mut p = a.alloc(layout);
        assert!(!p.is_null());
        fill(p, size);

        for _ in 0..4 {
            let nsize = (size * 2).min(BIG_TOP);
            let np = a.realloc(p, layout, nsize);
            assert!(!np.is_null());
            check(np, size);
            for i in size..nsize {
                *np.add(i) = (i % 251) as u8;
            }
            p = np;
            size = nsize;
            if size == BIG_TOP {
                break;
            }
        }
        // Shrink hard; the prefix must survive and the result must be freeable.
        let sp = a.realloc(p, Layout::from_size_align(size, 16).unwrap(), 70000);
        assert!(!sp.is_null());
        check(sp, 70000);
        a.dealloc(sp, Layout::from_size_align(70000, 16).unwrap());
    }
}
