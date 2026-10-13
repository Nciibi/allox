// TEMPORARY diagnostic - not for commit.
// Tight single-size-class calloc/write/free loop. No benchmark-harness
// overhead, so per-allocator cost is visible directly.
use std::alloc::{GlobalAlloc, Layout};
use std::time::Instant;

#[global_allocator]
static ALLOX: allox::Allox = allox::Allox;

fn run2<A: GlobalAlloc>(name: &str, a: &A, iters: u64, zeroed: bool) {
    let l = Layout::from_size_align(128, 16).unwrap();
    for _ in 0..10_000 {
        unsafe {
            let p = a.alloc_zeroed(l);
            a.dealloc(p, l);
        }
    }
    let t = Instant::now();
    for _ in 0..iters {
        unsafe {
            let p = if zeroed { a.alloc_zeroed(l) } else { a.alloc(l) };
            *p = 1;
            a.dealloc(p, l);
        }
    }
    let el = t.elapsed();
    let ns = el.as_nanos() as f64 / iters as f64;
    let tag = if zeroed { "calloc" } else { "malloc" };
    println!("  {name:<12} {tag:<7} {ns:8.2} ns/op");
}

struct Mima;
unsafe impl GlobalAlloc for Mima {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        mimalloc::MiMalloc.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        mimalloc::MiMalloc.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        mimalloc::MiMalloc.realloc(p, l, n)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        mimalloc::MiMalloc.alloc_zeroed(l)
    }
}

fn main() {
    let iters = 20_000_000u64;
    println!("128B, single thread, {iters} iters, best of 3 rounds");
    for round in 0..2 {
        println!(" round {round}");
        run2("allox", &ALLOX, iters, true);
        run2("allox", &ALLOX, iters, false);
        run2("mimalloc", &Mima, iters, true);
        run2("mimalloc", &Mima, iters, false);
        run2("system", &std::alloc::System, iters, true);
        run2("system", &std::alloc::System, iters, false);
    }
}