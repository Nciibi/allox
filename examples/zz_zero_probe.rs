//! TEMPORARY zero-size probe (delete after).
use std::alloc::{GlobalAlloc, Layout};

fn main() {
    let mode = std::env::var("MODE").unwrap();
    unsafe {
        match mode.as_str() {
            "free0" => {
                let p = allox::malloc(0);
                eprintln!("malloc(0) = {:?}", p);
                allox::free(p);
                eprintln!("free survived");
            }
            "realloc0" => {
                let p = allox::malloc(0);
                eprintln!("malloc(0) = {:?}", p);
                let q = allox::realloc(p, 8);
                eprintln!("realloc(p, 8) = {:?}", q);
                if !q.is_null() {
                    *q = 0xAB;
                    *q.add(7) = 0xCD;
                    eprintln!("write survived");
                    allox::free(q);
                    eprintln!("free survived");
                }
            }
            "global" => {
                let a = allox::Allox;
                let l0 = Layout::from_size_align(0, 16).unwrap();
                let p = a.alloc(l0);
                eprintln!("alloc(0) = {:?}", p);
                let q = a.realloc(p, l0, 8);
                eprintln!("realloc(p, 0->8) = {:?}", q);
                if q == p {
                    eprintln!("BUG: returned dangling identity");
                } else if !q.is_null() {
                    eprintln!("OK: fresh pointer");
                    a.dealloc(q, Layout::from_size_align(8, 16).unwrap());
                }
            }
            "usable0" => {
                let p = allox::malloc(0);
                eprintln!("malloc(0) = {:?}", p);
                let u = allox::usable_size(p);
                eprintln!("usable_size = {} survived", u);
            }
            _ => panic!("MODE"),
        }
    }
}
