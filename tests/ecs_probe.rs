//! Temporary probe: does the ECS growth chain promote once and then stay put?
#![cfg(all(unix, feature = "std"))]

#[test]
fn probe_growth_chain() {
    unsafe {
        let mut size = 65536usize;
        let mut p = allox::malloc(size);
        assert!(!p.is_null());
        eprintln!("init   size={} p={:?} usable={}", size, p, allox::usable_size(p));
        while size < 1_048_576 {
            let nsize = (size * 2).min(1_048_576);
            let np = allox::realloc(p, nsize);
            assert!(!np.is_null());
            eprintln!(
                "grow   {} -> {} same={} np={:?} usable={}",
                size,
                nsize,
                np == p,
                np,
                allox::usable_size(np)
            );
            p = np;
            size = nsize;
        }
        allox::free(p);
    }
}
