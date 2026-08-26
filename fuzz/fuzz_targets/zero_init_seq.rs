#![no_main]

//! calloc zero-init invariant under heavy recycling: every byte returned by
//! calloc must be zero, no matter what was in the recycled memory before.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut live: Vec<(*mut u8, usize)> = Vec::new();
    let mut i = 0usize;
    for chunk in data.chunks(4) {
        if chunk.len() < 3 {
            break;
        }
        let n = ((chunk[0] as usize) << 8 | chunk[1] as usize) % 8192 + 1;
        match chunk[2] % 3 {
            0 => unsafe {
                // dirty a fresh block then free it
                let p = allox::malloc(n);
                assert!(!p.is_null());
                core::ptr::write_bytes(p, 0xA5, n);
                live.push((p, n));
                if live.len() > 256 {
                    let (p, _) = live.swap_remove(0);
                    allox::free(p);
                }
            },
            1 => {
                if !live.is_empty() {
                    let idx = chunk[0] as usize % live.len();
                    let (p, _) = live.swap_remove(idx);
                    unsafe { allox::free(p) };
                }
            }
            _ => {
                // calloc must be fully zero even from heavily recycled memory
                let p = allox::calloc(1, n.max(1));
                assert!(!p.is_null());
                unsafe {
                    for b in 0..n {
                        assert_eq!(*p.add(b), 0, "calloc dirtied at {} (iter {})", b, i);
                        *p.add(b) = 0x5A;
                    }
                    live.push((p, n));
                }
                i += 1;
            }
        }
    }
    for (p, _) in live {
        unsafe { allox::free(p) };
    }
});
