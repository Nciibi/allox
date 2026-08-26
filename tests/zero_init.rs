//! Focused repro: single-threaded class-0 churn; every calloc must be zero.

fn main() {
    unsafe {
        let mut ptrs: Vec<*mut u8> = Vec::new();
        for round in 0..200_000usize {
            match round % 3 {
                0 => {
                    ptrs.push(allox::malloc(8));
                }
                1 => {
                    let p = allox::calloc(1, 8);
                    assert!(!p.is_null());
                    for i in 0..8 {
                        assert_eq!(*p.add(i), 0, "round {} off {}", round, i);
                    }
                    ptrs.push(p);
                }
                _ => {
                    if !ptrs.is_empty() {
                        allox::free(ptrs.swap_remove(ptrs.len() - 1));
                    }
                }
            }
            if ptrs.len() > 5000 {
                allox::free(ptrs.swap_remove(0));
            }
        }
        println!("no corruption");
    }
}
