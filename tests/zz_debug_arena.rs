//! TEMPORARY debug harness (to be deleted): replicates thread_exit churn with
//! per-generation allocator counters to chase an intermittent mapped_pages jump.

use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

fn churn_no_flush() {
    let mut small = Vec::with_capacity(3000);
    for _ in 0..3000 {
        let p = unsafe { allox::malloc(4096) };
        assert!(!p.is_null());
        unsafe {
            *p = 0xAB;
        }
        small.push(p);
    }
    let mut medium = Vec::with_capacity(200);
    for _ in 0..200 {
        let p = unsafe { allox::malloc(32768) };
        assert!(!p.is_null());
        unsafe {
            *p = 0xCD;
        }
        medium.push(p);
    }
    let mut large = Vec::with_capacity(20);
    for _ in 0..20 {
        let p = unsafe { allox::malloc(524288) };
        assert!(!p.is_null());
        unsafe {
            *p = 0xEF;
        }
        large.push(p);
    }
    for p in small {
        unsafe { allox::free(p) };
    }
    for p in medium {
        unsafe { allox::free(p) };
    }
    for p in large {
        unsafe { allox::free(p) };
    }
}

fn one_generation(workers: usize) {
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(churn_no_flush)
                .unwrap()
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    allox::flush_current_thread();
}

#[test]
fn zz_debug_arena_growth() {
    allox::flush_current_thread();
    let mut prev = allox::stats();
    let mut prev_split = allox::__debug_map_split();
    for g in 0..5 {
        one_generation(4);
        let s = allox::stats();
        let split = allox::__debug_map_split();
        eprintln!(
            "gen {}: mapped={} (+{}) maps={} (+{}) unmaps={} (+{}) split_d=(spm+{} spunm+{} smm+{} smunm+{} ac+{} ar+{})",
            g,
            s.mapped_pages,
            s.mapped_pages.saturating_sub(prev.mapped_pages),
            s.map_calls,
            s.map_calls.saturating_sub(prev.map_calls),
            s.unmap_calls,
            s.unmap_calls.saturating_sub(prev.unmap_calls),
            split.0.saturating_sub(prev_split.0),
            split.1.saturating_sub(prev_split.1),
            split.2.saturating_sub(prev_split.2),
            split.3.saturating_sub(prev_split.3),
            split.4.saturating_sub(prev_split.4),
            split.5.saturating_sub(prev_split.5),
        );
        prev = s;
        prev_split = split;
    }
}
