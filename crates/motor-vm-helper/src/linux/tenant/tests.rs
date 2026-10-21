use super::*;
use std::os::unix::net::UnixListener;

#[test]
fn hosted_connect_line_reaches_firecracker_in_one_write() {
    struct FirstReadMuxer {
        command: Vec<u8>,
        rejected: bool,
        reply: std::io::Cursor<Vec<u8>>,
    }
    impl Write for FirstReadMuxer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.rejected {
                return Err(std::io::Error::from_raw_os_error(libc::EPIPE));
            }
            // Model Firecracker reading as soon as each socket write completes,
            // rather than letting a buffered line reader hide split writes.
            self.rejected = bytes != self.command;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Read for FirstReadMuxer {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if self.rejected {
                return Ok(0);
            }
            self.reply.read(bytes)
        }
    }
    for port in 52..=54 {
        let mut muxer = FirstReadMuxer {
            command: format!("CONNECT {port}\n").into_bytes(),
            rejected: false,
            reply: std::io::Cursor::new(b"OK 123\nFRAME".to_vec()),
        };
        relay_handshake(&mut muxer, port)
            .expect("Firecracker must receive the complete CONNECT command before parsing it");
        assert!(!muxer.rejected);
        let mut frame = [0; 5];
        muxer.read_exact(&mut frame).unwrap();
        assert_eq!(&frame, b"FRAME");
    }
}

#[test]
fn hosted_relay_refusals_name_peer_port_and_handshake_step() {
    let scratch = cortexkit_test_support::ScratchDir::new("relay-refusal");
    let listener = UnixListener::bind(scratch.join("vsock.sock")).unwrap();
    let thread = std::thread::spawn(move || {
        for reply in [b"NO\n".as_slice(), b"".as_slice()] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "CONNECT 52\n");
            stream.write_all(reply).unwrap();
        }
    });
    for detail in ["invalid acknowledgement", "failed to fill whole buffer"] {
        let error = connect_relay(scratch.path(), 52).unwrap_err().to_string();
        assert!(error.contains(&format!("peer={}/vsock.sock", scratch.path().display())));
        assert!(error.contains("guest_port=52 step=handshake: read_CONNECT_ack:"));
        assert!(error.contains(detail), "{error}");
    }
    thread.join().unwrap();

    let (mut stream, peer) = UnixStream::pair().unwrap();
    drop(peer);
    let error = relay_handshake(&mut stream, 52).unwrap_err().to_string();
    assert!(error.starts_with("write_CONNECT: "), "{error}");
    assert!(error.contains("Broken pipe"), "{error}");
}

#[test]
fn hosted_jail_caps_use_ceiling_tasks_and_memory_headroom_not_job_memory() {
    let limits = cgroup_limits(&crate::tenant::tests::registry("a"), "0".into()).unwrap();
    assert_eq!(
        limits,
        [
            ("cpuset.cpus", "0-1".into()),
            ("cpuset.mems", "0".into()),
            ("cpu.max", "100000 100000".into()),
            ("memory.max", "19864223744".into()),
            ("pids.max", "66".into()),
            ("io.weight", "default 100".into())
        ]
    );
}

#[test]
fn hosted_real_relay_handshakes_only_the_three_ports_without_eating_frames() {
    let root =
        cortexkit_test_support::scratch_root().join(format!("tenant-relay-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = UnixListener::bind(root.join("vsock.sock")).unwrap();
    let thread = std::thread::spawn(move || {
        let mut ports = vec![];
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            ports.push(line);
            stream.write_all(b"OK 123\nFRAME").unwrap();
        }
        ports
    });
    for port in [0, 51, 55, u32::MAX] {
        assert!(connect_relay(&root, port).is_err());
    }
    for port in 52..=54 {
        let mut stream = UnixStream::from(connect_relay(&root, port).unwrap());
        let mut frame = [0; 5];
        stream.read_exact(&mut frame).unwrap();
        assert_eq!(&frame, b"FRAME");
    }
    assert_eq!(
        thread.join().unwrap(),
        ["CONNECT 52\n", "CONNECT 53\n", "CONNECT 54\n"]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hosted_firecracker_bodies_match_the_published_1_17_schema() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../script/tenant-vm-spike");
    let requests = json!([
        ["PUT", "/snapshot/load", load_body("base", "mtap1")],
        ["PUT", "/snapshot/load", load_body("suspend", "mtap2")],
        ["PUT", "/snapshot/create", snapshot_body()],
        ["PATCH", "/drives/tenant", drive_body()],
        ["PATCH", "/vm", json!({"state":"Paused"})],
        ["PATCH", "/vm", json!({"state":"Resumed"})]
    ]);
    let mut child=Command::new("python3").arg("-c").arg("import sys,json; sys.path.insert(0,sys.argv[1]); from api_schema import SCHEMA; rows=json.load(sys.stdin); [SCHEMA.validate(*row) for row in rows]; print(len(rows), 'published-schema checks passed')")
        .arg(path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(requests.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hosted_running_marker_is_written_whole_and_removal_tolerates_absence() {
    let scratch = cortexkit_test_support::ScratchDir::new("running-marker");
    let marker = scratch.join("running.json");
    write_running_marker(&marker).unwrap();
    let value: Value = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    assert!(value["at_ms"].as_u64().unwrap() > 0);
    assert!(!scratch.join("running.pending").exists());
    remove_running_marker(&marker).unwrap();
    assert!(!marker.exists());
    // A clean stop of a VM that never wrote a marker must still succeed.
    remove_running_marker(&marker).unwrap();
}
