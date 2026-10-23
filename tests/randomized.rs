use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

use std::thread;

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

/// A live allocation with a fill-pattern used as an integrity tag.
#[cfg(test)]
mod harness {
    pub struct Slot {
        pub p: *mut u8,
        pub size: usize,
        pub align: usize,
        pub tag: u8,
    }

    impl Slot {
        /// # Safety
        /// `p` must be a live allocation of `size` bytes.
        pub unsafe fn verify(&self) {
            for i in 0..self.size {
                let want = self.pattern(i);
                assert_eq!(
                    *self.p.add(i),
                    want,
                    "corruption at {} of {} (tag {:#x})",
                    i,
                    self.size,
                    self.tag
                );
            }
        }
        pub fn pattern(&self, i: usize) -> u8 {
            self.tag.wrapping_mul(31).wrapping_add(i as u8)
        }
    }
}

use harness::Slot;

unsafe fn alloc_slot(rng: &mut Rng) -> Slot {
    let size = match rng.next() % 5 {
        0 => 1 + (rng.next() % 64) as usize,
        1 => 65 + (rng.next() % 1983) as usize,
        2 => 2048 + (rng.next() % 14336) as usize,
        3 => 16385 + (rng.next() % 48_000) as usize,
        _ => 1 + (rng.next() % 300_000) as usize,
    };
    let align = match rng.next() % 4 {
        0 => 16,
        1 => 64,
        2 => 256,
        _ => 4096,
    };
    let p = allox::aligned_alloc(align, size);
    assert!(!p.is_null(), "OOM {}@{}", size, align);
    let slot = Slot {
        p,
        size,
        align,
        tag: (rng.next() & 0xFF) as u8,
    };
    for i in 0..slot.size {
        *p.add(i) = slot.pattern(i);
    }
    slot
}

/// Randomized churn: alloc / free / realloc / verify across many threads and
/// all code paths (small classes, large regions, over-aligned regions).
#[test]
fn randomized_churn_all_paths_multithreaded() {
    // Bound the live set: allocs outnumber frees, so an uncapped run grows to
    // thousands of slots per thread (hundreds of MB each) and exhausts a
    // 32-bit address space before the iteration count is reached. The cap
    // keeps the same path coverage with a bounded working set.
    const LIVE_CAP: usize = 512;
    let handles: Vec<_> = (0..6)
        .map(|t| {
            thread::spawn(move || {
                let mut rng = Rng((t as u64 + 7).wrapping_mul(0x2545F4914F6CDD1D));
                let mut live: Vec<Slot> = Vec::with_capacity(LIVE_CAP);
                let mut reallocs = 0usize;
                for _ in 0..25_000 {
                    match rng.next() % 10 {
                        0..=4 => {
                            let s = unsafe { alloc_slot(&mut rng) };
                            live.push(s);
                            if live.len() > LIVE_CAP {
                                let idx = (rng.next() as usize) % live.len();
                                let s = live.swap_remove(idx);
                                unsafe {
                                    s.verify();
                                    allox::free(s.p);
                                }
                            }
                        }
                        5..=6 if !live.is_empty() => {
                            let idx = (rng.next() as usize) % live.len();
                            let s = live.swap_remove(idx);
                            unsafe {
                                s.verify();
                                allox::free(s.p);
                            }
                        }
                        7..=8 if !live.is_empty() => {
                            let idx = (rng.next() as usize) % live.len();
                            let old_size = live[idx].size;
                            if old_size < 400_000 {
                                let new_size = old_size + 1 + (rng.next() % 4096) as usize;
                                let mut s = live.swap_remove(idx);
                                unsafe {
                                    s.verify();
                                    let np = allox::realloc(s.p, new_size);
                                    assert!(!np.is_null());
                                    // preserved prefix must still match
                                    let check = Slot {
                                        p: np,
                                        size: old_size,
                                        align: s.align,
                                        tag: s.tag,
                                    };
                                    check.verify();
                                    for i in old_size..new_size {
                                        *np.add(i) = s.tag.wrapping_mul(31).wrapping_add(i as u8);
                                    }
                                    s.p = np;
                                    s.size = new_size;
                                }
                                live.push(s);
                                reallocs += 1;
                            } else {
                                let keep = live.swap_remove(idx);
                                live.push(keep);
                            }
                        }
                        _ if !live.is_empty() => {
                            // verify a random survivor without touching it
                            let idx = (rng.next() as usize) % live.len();
                            unsafe { live[idx].verify() };
                        }
                        _ => {}
                    }
                }
                for s in live.iter() {
                    unsafe {
                        s.verify();
                        allox::free(s.p);
                    }
                }
                assert!(reallocs > 1000, "test should exercise realloc heavily");
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread must not abort");
    }

    allox::flush_current_thread();
    let stats = allox::stats();
    println!("post-flush mapped pages: {}", stats.live_mappings);
}

/// calloc must zero even when memory is recycled from previous frees.
#[test]
fn calloc_zero_after_recycle() {
    let handles: Vec<_> = (0..4)
        .map(|_| {
            thread::spawn(|| {
                let mut rng = Rng(0xC0FFEE);
                for round in 0..2000 {
                    let n = 1 + (rng.next() % 8000) as usize;
                    let p = unsafe { allox::calloc(n, 1) };
                    assert!(!p.is_null());
                    // dirty it
                    unsafe { core::ptr::write_bytes(p, 0xFF, n) };
                    unsafe { allox::free(p) };

                    let n2 = 1 + (rng.next() % 8000) as usize;
                    let q = unsafe { allox::calloc(n2, 2) };
                    assert!(!q.is_null());
                    for i in 0..n2 * 2 {
                        assert_eq!(unsafe { *q.add(i) }, 0, "round {} offset {}", round, i);
                    }
                    unsafe { allox::free(q) };
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

/// `GlobalAlloc::alloc_zeroed` under concurrent churn, with alignments large
/// enough to force the large path in every size tier. Complements the
/// deterministic tier coverage in `discard_contract.rs` with the interleaving
/// that only a randomized multithreaded run produces.
#[test]
fn randomized_zeroed_alignment_multithreaded() {
    use std::alloc::{alloc_zeroed, dealloc, Layout};

    let handles: Vec<_> = (0..4)
        .map(|t| {
            thread::spawn(move || {
                let mut rng = Rng((t as u64 + 11).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                let mut live: Vec<(*mut u8, Layout)> = Vec::new();
                for _ in 0..800 {
                    match rng.next() % 3 {
                        0 | 1 => {
                            let size = match rng.next() % 4 {
                                0 => 1 + (rng.next() % 1024) as usize,
                                1 => 16_385 + (rng.next() % 8_000) as usize,
                                2 => 65_473 + (rng.next() % 140_000) as usize,
                                _ => 1 + (rng.next() % 250_000) as usize,
                            };
                            let align = [16usize, 64, 4096, 65_536][(rng.next() % 4) as usize];
                            let layout = Layout::from_size_align(size, align).unwrap();
                            let p = unsafe { alloc_zeroed(layout) };
                            assert!(!p.is_null(), "OOM {size}@{align}");
                            assert_eq!(
                                p as usize % align,
                                0,
                                "alloc_zeroed {size}@{align} misaligned"
                            );
                            for i in 0..size {
                                assert_eq!(
                                    unsafe { *p.add(i) },
                                    0,
                                    "alloc_zeroed {size}@{align} dirty at {i}"
                                );
                            }
                            live.push((p, layout));
                            if live.len() > 96 {
                                let (p, layout) = live.swap_remove(0);
                                unsafe { dealloc(p, layout) };
                            }
                        }
                        _ => {
                            if !live.is_empty() {
                                let idx = (rng.next() as usize) % live.len();
                                let (p, layout) = live.swap_remove(idx);
                                unsafe { dealloc(p, layout) };
                            }
                        }
                    }
                }
                for (p, layout) in live {
                    unsafe { dealloc(p, layout) };
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
