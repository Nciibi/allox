use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

use std::thread;
use std::time::Instant;

/// Deterministic xorshift PRNG — no external dependencies.
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

const OPS_PER_THREAD: usize = 150_000;

#[test]
fn randomized_stress_multithreaded() {
    let start = Instant::now();
    let handles: Vec<_> = (0..6)
        .map(|t| {
            thread::spawn(move || {
                let mut rng = Rng(0x9E3779B97F4A7C15 ^ (t as u64 + 1));
                let mut live: Vec<(*mut u8, usize, u8)> = Vec::with_capacity(4096);
                for op in 0..OPS_PER_THREAD {
                    let roll = rng.next() % 100;
                    if roll < 55 || live.is_empty() {
                        let size =
                            match rng.next() % 4 {
                                0 => 1 + (rng.next() % 128) as usize,
                                1 => 129 + (rng.next() % 3968) as usize,
                                2 => 4097 + (rng.next() % 12288) as usize,
                                _ => 16385 + (rng.next() % 100_000) as usize,
                            };
                        let tag = (rng.next() & 0xFF) as u8;
                        let p = unsafe { allox::malloc(size) };
                        assert!(!p.is_null(), "OOM at {} bytes", size);
                        unsafe {
                            core::ptr::write_bytes(p, tag, size);
                            *p.add(size - 1) = tag.wrapping_add(1);
                        }
                        live.push((p, size, tag));
                    } else {
                        let idx = (rng.next() as usize) % live.len();
                        let (p, size, tag) = live.swap_remove(idx);
                        unsafe {
                            assert_eq!(*p.add(size - 1), tag.wrapping_add(1), "corruption");
                            allox::free(p);
                        }
                    }
                    if op % 50_000 == 0 && !live.is_empty() {
                        // verify a sample
                        let idx = (rng.next() as usize) % live.len();
                        let (p, size, tag) = live[idx];
                        unsafe {
                            assert_eq!(*p.add(size - 1), tag.wrapping_add(1));
                        }
                    }
                }
                let leaked = live.len();
                for (p, _, _) in live {
                    unsafe { allox::free(p) };
                }
                leaked
            })
        })
        .collect();

    let total_leaked: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(total_leaked, 0);

    // After everything is freed and flushed, mapped page count should be small.
    allox::flush_current_thread();
    let s = allox::stats();
    println!(
        "stress done in {:?}: {} pages mapped",
        start.elapsed(),
        s.mapped_pages
    );
}

#[test]
fn stats_are_sane() {
    let before = allox::stats();
    let p = unsafe { allox::malloc(1 << 20) }; // large path
    assert!(!p.is_null());
    let mid = allox::stats();
    assert!(mid.mapped_pages > before.mapped_pages);
    unsafe { allox::free(p) };
}
