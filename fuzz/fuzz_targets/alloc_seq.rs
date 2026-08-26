#![no_main]

//! Randomized allocation sequences: exercises small/large paths, alignment,
/// realloc growth/shrink, and verifies data integrity via fill tags.

use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;

#[derive(Debug)]
enum Op {
    Alloc { size: u16, align: u8, tag: u8 },
    Free { idx: u8 },
    Realloc { idx: u8, new_size: u16 },
    Verify { idx: u8 },
}

fn gen_ops(data: &[u8]) -> Vec<Op> {
    let mut u = Unstructured::new(data);
    let mut ops = Vec::new();
    while u.data.len() >= 5 && ops.len() < 4096 {
        let kind = u.data[0] % 6;
        let a = u.data[1] as u16 | ((u.data[2] as u16) << 8);
        let b = u.data[3];
        let c = u.data[4];
        u.data = &u.data[5..];
        ops.push(match kind {
            0 | 1 => Op::Alloc {
                size: (a % 300_000).max(1),
                align: match b % 3 {
                    0 => 16,
                    1 => 64,
                    _ => 4096,
                },
                tag: c,
            },
            2 => Op::Free { idx: b },
            3 => Op::Realloc {
                idx: b,
                new_size: (a % 300_000).max(1),
            },
            4 => Op::Alloc {
                size: (a % 64).max(1),
                align: 16,
                tag: c,
            },
            _ => Op::Verify { idx: b },
        });
    }
    ops
}

struct Slot {
    p: *mut u8,
    size: usize,
    tag: u8,
}

impl Slot {
    unsafe fn pattern(&self, i: usize) -> u8 {
        self.tag.wrapping_mul(31).wrapping_add(i as u8)
    }
    unsafe fn verify(&self) {
        for i in 0..self.size {
            assert_eq!(*self.p.add(i), self.pattern(i), "corruption at {}", i);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut live: Vec<Slot> = Vec::new();
    for op in gen_ops(data) {
        match op {
            Op::Alloc { size, align, tag } => unsafe {
                let p = allox::aligned_alloc(align, size as usize);
                assert!(!p.is_null(), "OOM {}@{}", size, align);
                let s = Slot {
                    p,
                    size: size as usize,
                    tag,
                };
                for i in 0..s.size {
                    *p.add(i) = s.pattern(i);
                }
                live.push(s);
            },
            Op::Free { idx } => {
                if !live.is_empty() {
                    let s = live.swap_remove(idx as usize % live.len());
                    unsafe {
                        s.verify();
                        allox::free(s.p);
                    }
                }
            }
            Op::Realloc { idx, new_size } => {
                if !live.is_empty() {
                    let mut s = live.swap_remove(idx as usize % live.len());
                    unsafe {
                        s.verify();
                        let np = allox::realloc(s.p, new_size as usize);
                        assert!(!np.is_null());
                        let check = Slot {
                            p: np,
                            size: s.size.min(new_size as usize),
                            tag: s.tag,
                        };
                        check.verify();
                        for i in s.size..new_size as usize {
                            *np.add(i) = s.pattern(i);
                        }
                        s.p = np;
                        s.size = new_size as usize;
                        live.push(s);
                    }
                }
            }
            Op::Verify { idx } => {
                if !live.is_empty() {
                    unsafe { live[idx as usize % live.len()].verify() };
                }
            }
        }
    }
    for s in &live {
        unsafe {
            s.verify();
            allox::free(s.p);
        }
    }
});
