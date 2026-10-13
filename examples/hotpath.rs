// TEMPORARY diagnostic - not for commit.
// Tight single-size-class calloc/write/free loop. No benchmark-harness
// overhead, so per-allocator cost is visible directly.
use std::alloc::{GlobalAlloc, Layout};
use std::time::Instant;

#[global_allocator]
static ALLOX: allox::Allox = allox::Allox;

fn run<A: GlobalAlloc>(name: &str, a: &A, iters: u64) {
    let l = Layout::from_size_align(128, 16).unwrap();
    // warm
    for _ in 0..10_000 {
        unsafe {
            let p = a.alloc_zeroed(l);
            a.dealloc(p, l);
        }
    }
    let t = Instant::now();
    for _ in 0..iters {
        unsafe {
            let p = a.alloc_zeroed(l);
            *p = 1;
            a.dealloc(p, l);
        }
    }
    let el = t.elapsed();
    let ns = el.as_nanos() as f64 / iters as f64;
    println!("  {name:<12} {ns:8.2} ns/op   {:.2} M ops/s", iters as f64 / (el.as_secs_f64() * 1e6));
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
    println!("tight 128B calloc+dirty+free, single thread, {iters} iters");
    for round in 0..3 {
        println!(" round {round}");
        run("allox", &ALLOX, iters);
        run("mimalloc", &Mima, iters);
        run("system", &std::alloc::System, iters);
    }
}