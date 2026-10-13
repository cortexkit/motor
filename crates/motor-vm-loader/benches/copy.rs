//! CPU copy baseline; actual UFFD latency must also be measured on the deployed host.
use motor_vm_loader::{Layout, Region, PAGE_SIZE};
use std::{hint::black_box, time::Instant};
fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args().any(|a| a == "--uffd") {
        let duration =
            motor_vm_loader::linux::benchmark_copies(2048).expect("UFFD benchmark prerequisites");
        println!(
            "2 MiB UFFDIO_COPY + byte verification + discard: 2048 iterations, {:.1} GiB/s",
            4.0 / duration.as_secs_f64()
        );
        return;
    }
    let source = vec![0x5au8; PAGE_SIZE as usize];
    let mut destination = vec![0u8; PAGE_SIZE as usize];
    let layout = Layout::new(
        vec![Region {
            base_host_virt_addr: PAGE_SIZE,
            offset: 0,
            size: PAGE_SIZE,
            page_size: PAGE_SIZE,
        }],
        PAGE_SIZE,
    )
    .unwrap();
    let start = Instant::now();
    for _ in 0..2048 {
        let copy = layout.locate(black_box(PAGE_SIZE + 17)).unwrap();
        destination[..copy.length as usize].copy_from_slice(black_box(&source));
        black_box(&destination);
    }
    println!(
        "2 MiB layout + copy: 2048 iterations, {:.1} GiB/s",
        4.0 / start.elapsed().as_secs_f64()
    );
}
