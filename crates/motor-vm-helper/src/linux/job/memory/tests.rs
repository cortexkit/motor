use super::*;
use std::io::BufRead;
use std::os::unix::{fs::symlink, net::UnixListener};

// Synthetic, not an operator capture. RAM flags follow Firecracker v1.17.0
// src/vmm/src/vstate/memory.rs:884-960,261-296; the read-only shared pmem
// mapping follows src/vmm/src/devices/virtio/pmem/device.rs:181-255.
const SYNTHETIC: &str = include_str!("synthetic-firecracker-1.17.smaps");

// A real capture: /proc/<pid>/smaps of the Firecracker 1.17.0 process of a
// production-booted job VM (3072 MiB guest RAM, 2 vCPUs, the toolchain image
// on pmem) while a job ran, on a Linux 7.0 hosted server.
const CAPTURED: &str = include_str!("captured-firecracker-1.17.smaps");

#[test]
fn captured_smaps_counts_one_guest_ram_mapping_not_pmem_heap_or_memfd() {
    // Exactly one MAP_NORESERVE anonymous private mapping of the configured RAM;
    // the 32 GiB shared toolchain mapping, heap and memfd are not guest RAM.
    assert_eq!(guest_resident_bytes(CAPTURED, 3072).unwrap(), 782_636 * 1024);
    assert!(guest_resident_bytes(CAPTURED, 3071).is_err());
    assert!(guest_resident_bytes(CAPTURED, 4096).is_err());
}

#[test]
fn synthetic_smaps_counts_only_guest_ram_never_resident_pmem_or_heap() {
    assert_eq!(
        guest_resident_bytes(SYNTHETIC, 256).unwrap(),
        128 * 1024 * 1024
    );
    let false_pmem = SYNTHETIC.replace("900000 kB", "1048576 kB");
    assert_eq!(
        guest_resident_bytes(&false_pmem, 256).unwrap(),
        128 * 1024 * 1024
    );
}

#[test]
fn ambiguous_missing_or_malformed_ram_is_an_error_not_zero_or_process_rss() {
    for text in [
        String::new(),
        SYNTHETIC.replace(" nr ", " "),
        SYNTHETIC.replace("Rss:               98304 kB\n", ""),
        SYNTHETIC.replace("98304 kB", "not-a-number kB"),
        SYNTHETIC.replace("98304 kB", "98304 MB"),
        SYNTHETIC.replace("98304 kB", "200000 kB"),
        SYNTHETIC.replace("Size:             131072 kB", "Size:             131071 kB"),
        SYNTHETIC.replace("me ac sd", "me nr sd"),
    ] {
        assert!(guest_resident_bytes(&text, 256).is_err(), "{text}");
    }
    assert!(guest_resident_bytes(SYNTHETIC, 255).is_err());
    assert!(guest_resident_bytes(SYNTHETIC, u64::MAX).is_err());
    let empty_ram = SYNTHETIC
        .replace("98304 kB", "0 kB")
        .replace("32768 kB", "0 kB");
    assert_eq!(guest_resident_bytes(&empty_ram, 256).unwrap(), 0);
}

#[test]
fn pidfd_and_proc_directory_remain_pinned_after_process_exit() {
    let mut child = Command::new("sleep").arg("60").spawn().unwrap();
    let pin = Process::pin(child.id()).unwrap();
    assert!(!pin.smaps().unwrap().is_empty());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(pin.alive().is_err());
    assert!(pin.smaps().is_err());
}

#[test]
fn api_socket_refuses_symlinks_and_wrong_owner_and_patches_only_the_pinned_socket() {
    let scratch = cortexkit_test_support::ScratchDir::new("balloon-api-socket");
    let root = scratch.path();
    let listener = UnixListener::bind(root.join("real.sock")).unwrap();
    symlink("real.sock", root.join("api.sock")).unwrap();
    assert!(api_socket(root, unsafe { libc::geteuid() }).is_err());
    std::fs::remove_file(root.join("api.sock")).unwrap();
    std::fs::rename(root.join("real.sock"), root.join("api.sock")).unwrap();
    assert!(api_socket(root, unsafe { libc::geteuid() } + 1).is_err());
    let mut stream = api_socket(root, unsafe { libc::geteuid() }).unwrap();
    std::fs::rename(root.join("api.sock"), root.join("old.sock")).unwrap();
    let replacement = UnixListener::bind(root.join("api.sock")).unwrap();
    replacement.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(connection.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "PATCH /balloon HTTP/1.1\r\n");
        let mut length = 0;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"amount_mib":37})
        );
        connection
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .unwrap();
    });
    patch_balloon(&mut stream, 37).unwrap();
    server.join().unwrap();
    assert!(replacement.accept().is_err());
}
