use allox::Allox;

/// Exercise Allox as the process-wide allocator: everything in this test
/// binary (including std internals) allocates through it.
#[global_allocator]
static GLOBAL: Allox = Allox;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::thread;

#[test]
fn std_collections_work() {
    let mut map: HashMap<String, Vec<u8>> = HashMap::new();
    for i in 0..10_000u32 {
        map.insert(format!("key-{}", i), vec![i as u8; (i as usize % 512) + 1]);
    }
    for (k, v) in &map {
        assert_eq!(v.len(), k.rsplit('-').next().unwrap().parse::<u32>().unwrap() as usize % 512 + 1);
    }
    let mut btree = BTreeMap::new();
    for i in 0..5_000u32 {
        btree.insert(i, format!("value-{}", i));
    }
    assert_eq!(btree[&4999], "value-4999");
    let mut dq = VecDeque::new();
    for i in 0..100_000u32 {
        dq.push_back(i);
    }
    assert_eq!(dq.pop_front(), Some(0));
}

#[test]
fn threads_allocate_concurrently() {
    let handles: Vec<_> = (0..8)
        .map(|t| {
            thread::spawn(move || {
                let mut acc = 0usize;
                for i in 0..20_000u32 {
                    let size = ((t * 31 + i * 17) as usize % 8000) + 1;
                    let v = vec![7u8; size];
                    acc += v[size - 1] as usize + v.len();
                    let s = format!("{}-{}", t, i);
                    acc += s.len();
                }
                acc
            })
        })
        .collect();
    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0);
}

#[test]
fn cross_thread_send_and_drop() {
    let handle = thread::spawn(|| {
        let data: Vec<Vec<u64>> = (0..500).map(|i| vec![i as u64; 300]).collect();
        data
    });
    let data = handle.join().unwrap();
    drop(data);
    let more: Vec<Box<[u8]>> = (0..1000).map(|i| vec![0u8; i % 2048 + 1].into_boxed_slice()).collect();
    assert_eq!(more.len(), 1000);
}
