//! WASM smoke test: compiled to wasm32-unknown-unknown and executed by a
//! host runtime (see scripts/wasm_smoke.mjs).
//!
//! Exports return 0 on success, non-zero failure codes.

// Binaries need a main; the JS host drives the exported function instead.
fn main() {}

use std::alloc::Layout;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Churn allocations through the global allocator inside linear memory.
#[no_mangle]
pub extern "C" fn allox_wasm_smoke() -> i32 {
    let mut rng = Rng(0x57A5_4D53); // WASMS
    let mut live: Vec<(*mut u8, usize)> = Vec::with_capacity(1024);

    for op in 0..50_000u32 {
        match op % 3 {
            0 | 1 => {
                let size = 1 + (rng.next() as usize % 8000);
                let p = unsafe { allox::malloc(size) };
                if p.is_null() {
                    return 1;
                }
                unsafe {
                    core::ptr::write_bytes(p, (op & 0xFF) as u8, size);
                    if *p.add(size - 1) != (op & 0xFF) as u8 {
                        return 2;
                    }
                }
                live.push((p, size));
            }
            _ => {
                if live.is_empty() {
                    continue;
                }
                let idx = (rng.next() as usize) % live.len();
                let (p, _) = live.swap_remove(idx);
                unsafe { allox::free(p) };
            }
        }
    }

    // calloc must be zeroed.
    for n in [1usize, 64, 1000, 20_000] {
        let p = unsafe { allox::calloc(n, 1) };
        if p.is_null() {
            return 3;
        }
        unsafe {
            for i in 0..n {
                if *p.add(i) != 0 {
                    return 4;
                }
            }
            allox::free(p);
        }
    }

    // Over-aligned allocations.
    for align in [32usize, 4096] {
        let p = unsafe { allox::aligned_alloc(align, 500) };
        if p.is_null() || (p as usize) % align != 0 {
            return 5;
        }
        unsafe { allox::free(p) };
    }

    // Large allocation.
    let big = unsafe { allox::malloc(200_001) };
    if big.is_null() {
        return 6;
    }
    unsafe {
        *big.add(200_000) = 42;
        if *big.add(200_000) != 42 {
            return 7;
        }
        allox::free(big);
    }

    let leaked = live.len();
    for (p, _) in live {
        unsafe { allox::free(p) };
    }
    if leaked > 0 {
        // Silence unused-mut style lints while keeping the count meaningful.
        let _ = core::mem::size_of::<usize>();
    }
    0
}
