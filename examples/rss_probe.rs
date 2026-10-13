use allox::Allox;

#[global_allocator]
static GLOBAL: Allox = Allox;

#[link(name = "psapi")]
extern "system" {
    fn GetProcessMemoryInfo(
        p: *mut core::ffi::c_void,
        c: *mut Counters,
        cb: u32,
    ) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
}

#[repr(C)]
struct Counters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    a: usize,
    b: usize,
    c2: usize,
    d: usize,
    e: usize,
    f: usize,
}

fn rss_mib() -> f64 {
    let sz = core::mem::size_of::<Counters>();
    let mut c = Counters {
        cb: sz as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        a: 0,
        b: 0,
        c2: 0,
        d: 0,
        e: 0,
        f: 0,
    };
    unsafe {
        GetProcessMemoryInfo(GetCurrentProcess(), &mut c, sz as u32);
    }
    c.working_set_size as f64 / (1024.0 * 1024.0)
}

fn main() {
    let block = 2 * 1024 * 1024;
    let n = 512; // 1 GiB

    let baseline = rss_mib();
    let mut live: Vec<*mut u8> = Vec::with_capacity(n);
    for _ in 0..n {
        let p = unsafe { allox::malloc(block) };
        assert!(!p.is_null());
        unsafe {
            for i in (0..block).step_by(4096) {
                *p.add(i) = 1;
            }
        }
        live.push(p);
    }
    let held = rss_mib();
    let held_mapped = allox::stats().mapped_pages;

    for p in live.drain(..) {
        unsafe { allox::free(p) };
    }
    drop(live);
    allox::flush_current_thread();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let after = rss_mib();
    let after_mapped = allox::stats().mapped_pages;

    println!("baseline          : {baseline:8.1} MiB resident");
    println!("holding 1 GiB     : {held:8.1} MiB resident  ({held_mapped} pages mapped)");
    println!("after free + purge: {after:8.1} MiB resident  ({after_mapped} pages mapped)");
    println!(
        "released          : {:8.1} MiB  ({:.1}% of the peak)",
        held - after,
        100.0 * (held - after) / (held - baseline).max(1.0)
    );
}