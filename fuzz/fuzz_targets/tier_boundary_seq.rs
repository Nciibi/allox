#![no_main]

//! Sequences that deliberately cross the small / medium / big / large tier
//! boundaries (16 KiB, 65472, 262144, 524288) with mixed alignment, verifying
//! contents and usable_size routing. Complements `alloc_seq`, which samples
//! sizes broadly but rarely hits exact class edges.

use libfuzzer_sys::fuzz_target;

/// Exact tier edges plus neighbors: n-1, n, n+1 for each boundary.
const EDGES: &[usize] = &[
    16, 4096, 16384, 16385, 32768, 65471, 65472, 65473, 131072, 262143, 262144, 262145, 524288,
    524289,
];

fn pick(data: &[u8], i: usize) -> usize {
    if data.is_empty() {
        return EDGES[0];
    }
    let b = data[i % data.len()] as usize;
    // 70% exact edges (biased by b), 30% arbitrary nearby size.
    if b % 10 < 7 {
        EDGES[b % EDGES.len()]
    } else {
        (b * 257 + i) % 300_000 + 1
    }
}

fuzz_target!(|data: &[u8]| {
    let mut live: Vec<(*mut u8, usize, u8)> = Vec::new();
    for (i, chunk) in data.chunks(4).enumerate() {
        if chunk.len() < 3 {
            break;
        }
        let size = pick(chunk, i);
        let align = match chunk[1] % 4 {
            0 => 16,
            1 => 64,
            2 => 4096,
            _ => 65536,
        };
        match chunk[0] % 5 {
            0 | 1 => unsafe {
                let p = allox::aligned_alloc(align, size);
                assert!(!p.is_null(), "OOM size={} align={}", size, align);
                assert_eq!(p as usize % align, 0, "misaligned");
                // usable_size must be >= requested and hit the right tier's
                // rounded class (exact class value varies; just bound it).
                let us = allox::usable_size(p);
                assert!(us >= size, "usable_size {} < {}", us, size);
                let tag = chunk[2];
                for b in 0..size {
                    *p.add(b) = tag.wrapping_add(b as u8);
                }
                live.push((p, size, tag));
                if live.len() > 128 {
                    let (p, size, tag) = live.swap_remove(0);
                    for b in 0..size {
                        assert_eq!(
                            *p.add(b),
                            tag.wrapping_add(b as u8),
                            "corruption before free"
                        );
                    }
                    allox::free(p);
                }
            },
            2 => {
                if !live.is_empty() {
                    let idx = chunk[1] as usize % live.len();
                    let (p, size, tag) = live.swap_remove(idx);
                    unsafe {
                        for b in 0..size {
                            assert_eq!(*p.add(b), tag.wrapping_add(b as u8));
                        }
                        allox::free(p);
                    }
                }
            }
            3 => {
                if !live.is_empty() {
                    let idx = chunk[1] as usize % live.len();
                    let (p, old_size, tag) = live.swap_remove(idx);
                    let new_size = pick(chunk, i + 1);
                    unsafe {
                        for b in 0..old_size {
                            assert_eq!(*p.add(b), tag.wrapping_add(b as u8), "pre-realloc");
                        }
                        let np = allox::realloc(p, new_size);
                        assert!(!np.is_null());
                        let check = old_size.min(new_size);
                        for b in 0..check {
                            assert_eq!(*np.add(b), tag.wrapping_add(b as u8), "post-realloc");
                        }
                        for b in check..new_size {
                            *np.add(b) = tag.wrapping_add(b as u8);
                        }
                        live.push((np, new_size, tag));
                    }
                }
            }
            _ => {
                // calloc across a boundary: zero-init must hold.
                let n = size;
                unsafe {
                    let p = allox::calloc(1, n);
                    assert!(!p.is_null());
                    for b in 0..n {
                        assert_eq!(*p.add(b), 0, "calloc dirty at {} size {}", b, n);
                    }
                    allox::free(p);
                }
            }
        }
    }
    for (p, size, tag) in live {
        unsafe {
            for b in 0..size {
                assert_eq!(*p.add(b), tag.wrapping_add(b as u8));
            }
            allox::free(p);
        }
    }
});
