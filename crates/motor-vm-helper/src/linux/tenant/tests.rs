use super::*;
use std::io::BufRead;
use std::os::unix::net::UnixListener;

#[test]
fn network_admission_escalates_when_fence_and_vm_containment_both_fail() {
    let config = serde_json::from_str(include_str!("../../../fixtures/helper.json")).unwrap();
    let mut tenants = LinuxTenants::new(&config, PathBuf::new());
    let escalated = std::cell::Cell::new(false);
    let error = tenants
        .check_network_with(
            || Err(Error("uplink invalid".into())),
            |_| Ok(Vec::new()),
            |_| Err(Error("NFT unavailable".into())),
            |_| Err(Error("VMM kill failed".into())),
            |message| {
                assert!(message.contains("VMM kill failed"));
                escalated.set(true);
            },
        )
        .unwrap_err();
    assert!(
        escalated.get(),
        "production must terminate instead of serving with uncontained VMMs"
    );
    assert_eq!(error.case, StartRefusalCase::Uplink);
}

#[test]
fn network_admission_fences_live_taps_when_registry_is_unreadable() {
    let config = serde_json::from_str(include_str!("../../../fixtures/helper.json")).unwrap();
    let mut tenants = LinuxTenants::new(&config, PathBuf::new());
    let existing = serde_json::json!({"nftables":[
        {"set":{"name":"tenant_taps","type":"ifname","elem":["mtap7"]}},
        {"set":{"name":"closed_taps","type":"ifname","elem":[]}}
    ]});
    let mut writes = Vec::new();
    let error = tenants
        .check_network_with(
            || Err(Error("configured uplink is down".into())),
            |_| Err(Error("registry decode failed".into())),
            |records| {
                assert!(records.is_empty());
                tenant_network::fence_policy(records, Some(&existing), None, |text| {
                    writes.push(text.to_owned());
                    Ok(())
                })
            },
            |_| panic!("successful gate containment must not stop VMs"),
            |_| panic!("successful fencing must not terminate the helper"),
        )
        .unwrap_err();
    assert_eq!(error.case, StartRefusalCase::Uplink);
    assert!(error.message.contains("registry decode failed"));
    assert_eq!(writes.len(), 1);
    assert!(writes[0].contains("closed_taps { \"mtap7\" }"));
}

#[test]
fn network_admission_fence_failure_stops_vms_without_revalidating() {
    let config = serde_json::from_str(include_str!("../../../fixtures/helper.json")).unwrap();
    let mut tenants = LinuxTenants::new(&config, PathBuf::new());
    let stopped = std::cell::Cell::new(false);
    let error = tenants
        .check_network_with(
            || Err(Error("uplink validation failed".into())),
            |_| Ok(vec![crate::tenant::tests::registry("a")]),
            |_| Err(Error("nft write denied".into())),
            |_| {
                stopped.set(true);
                Ok(())
            },
            |_| panic!("successful VM containment must not terminate the helper"),
        )
        .unwrap_err();
    assert!(stopped.get());
    assert!(error.message.contains("nft write denied"));
    tenants
        .check_network_with(
            || Ok(()),
            |_| panic!("no registry read on valid admission"),
            |_| panic!("must not fence valid admission"),
            |_| panic!("must not stop valid admission"),
            |_| panic!("must not terminate valid admission"),
        )
        .unwrap();
}

#[test]
fn oom_protection_verifies_identity_write_and_readback() {
    let scratch = cortexkit_test_support::ScratchDir::new("vmm-oom");
    std::fs::write(scratch.join("comm"), "firecracker\n").unwrap();
    std::fs::write(scratch.join("oom_score_adj"), "0\n").unwrap();
    protect_vmm_at(scratch.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(scratch.join("oom_score_adj")).unwrap(),
        "-500"
    );
    std::fs::write(scratch.join("comm"), "jailer\n").unwrap();
    assert_eq!(
        protect_vmm_at(scratch.path()).unwrap_err().case,
        StartRefusalCase::Oom
    );
    let error = protect_vmm_with(
        |name| {
            Ok(if name == "comm" {
                "firecracker\n"
            } else {
                "0\n"
            }
            .into())
        },
        |value| {
            assert_eq!(value, "-500");
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.case, StartRefusalCase::Oom);
    assert!(error.message.contains("expected -500"));
}

#[test]
fn both_snapshot_load_paths_apply_oom_protection_before_the_api_request() {
    for prefix in ["base", "suspend"] {
        let scratch = cortexkit_test_support::ScratchDir::new("guarded-load");
        std::fs::write(scratch.join("comm"), "firecracker\n").unwrap();
        std::fs::write(scratch.join("oom_score_adj"), "0\n").unwrap();
        let listener = UnixListener::bind(scratch.join("api.sock")).unwrap();
        let score = scratch.join("oom_score_adj");
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "PUT /snapshot/load HTTP/1.1\r\n");
            assert_eq!(std::fs::read_to_string(score).unwrap(), "-500");
            let mut len = None;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    len = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut bytes = vec![0; len.unwrap()];
            reader.read_exact(&mut bytes).unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["snapshot_path"], format!("/state/{prefix}.vmstate"));
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        guarded_snapshot_load(scratch.path(), prefix, "mtap1", || {
            protect_vmm_at(scratch.path())
        })
        .unwrap();
        thread.join().unwrap();
    }
}

#[test]
#[cfg(feature = "linux-service-tests")]
fn isolated_tenant_load_paths_verify_firecracker_oom_score() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let tenant = std::env::var("MOTOR_VM_TEST_TENANT").expect("set MOTOR_VM_TEST_TENANT to a stopped disposable tenant with saved snapshots and a matching size base");
    for unit in ["motor-host.service", "motor-vm-helper.service"] {
        let output = Command::new("systemctl")
            .args(["show", "-p", "ActiveState", "--value", unit])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "inactive",
            "{unit} must be stopped on the dedicated fixture host"
        );
    }
    let config: Config =
        serde_json::from_reader(File::open("/etc/motor-vm-helper.json").unwrap()).unwrap();
    let groups = super::super::cgroup::Cgroups::discover().unwrap();
    groups.enable().unwrap();
    let mut backend = LinuxTenants::new(&config, groups.root);
    let r = backend.registry(&tenant).unwrap();
    let group = backend.groups.join(format!("tenant-{}", r.tap_index));
    assert!(!backend.vm_present(&tenant).unwrap());
    assert!(!group.exists());
    assert!(!backend.was_running(&r).unwrap());
    let generation = std::fs::read(&r.generation).unwrap();
    let saved: Vec<_> = ["suspend.vmstate", "suspend.memory", "key.json"]
        .iter()
        .map(|file| {
            (
                r.suspend.join(file),
                std::fs::metadata(r.suspend.join(file)).unwrap(),
            )
        })
        .collect();
    for warm in [false, true] {
        backend.check_start(&tenant).unwrap();
        let loaded = backend.load(&tenant, &r, warm);
        let observed = if loaded.is_ok() {
            let pid = backend.running[&tenant].child.id();
            Some((
                std::fs::read_to_string(format!("/proc/{pid}/comm")),
                std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj")),
            ))
        } else {
            None
        };
        backend.stop(&tenant).unwrap();
        backend.clear_running(&r).unwrap();
        loaded.unwrap();
        let (comm, oom) = observed.unwrap();
        assert_eq!(comm.unwrap(), "firecracker\n");
        assert_eq!(oom.unwrap().trim(), "-500");
        assert!(!backend.vm_present(&tenant).unwrap());
        assert!(!group.exists());
        assert!(!backend.was_running(&r).unwrap());
    }
    assert_eq!(std::fs::read(&r.generation).unwrap(), generation);
    use std::os::unix::fs::MetadataExt;
    for (path, before) in saved {
        let after = std::fs::metadata(path).unwrap();
        assert_eq!(
            (after.ino(), after.len(), after.mtime(), after.mtime_nsec()),
            (
                before.ino(),
                before.len(),
                before.mtime(),
                before.mtime_nsec()
            )
        );
    }
}

#[test]
fn snapshot_deadline_scales_with_memory_and_reserves_helper_cleanup_budget() {
    assert_eq!(snapshot_create_timeout(1024), Duration::from_secs(60));
    assert_eq!(snapshot_create_timeout(28 * 1024), Duration::from_secs(478));
    assert_eq!(snapshot_create_timeout(30 * 1024), Duration::from_secs(480));
    assert_eq!(snapshot_create_timeout(u64::MAX), Duration::from_secs(480));
}

#[test]
fn firecracker_create_timeout_names_the_endpoint_and_uncertain_writer() {
    let scratch = cortexkit_test_support::ScratchDir::new("create-timeout");
    let listener = UnixListener::bind(scratch.join("api.sock")).unwrap();
    let (done, wait) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        assert_eq!(request, "PUT /snapshot/create HTTP/1.1\r\n");
        wait.recv().unwrap();
    });
    let mut uncertain = false;
    let error =
        create_snapshot(scratch.path(), Duration::from_millis(20), &mut uncertain).unwrap_err();
    let text = error.to_string();
    assert!(
        text.starts_with(crate::tenant::SNAPSHOT_CREATE_UNCERTAIN),
        "{text}"
    );
    assert!(
        text.contains("PUT /snapshot/create step=read_status request_started=true"),
        "{text}"
    );
    assert!(
        text.contains("api_timeout") && text.contains("timeout_ms=20"),
        "{text}"
    );
    done.send(()).unwrap();
    thread.join().unwrap();
    assert!(uncertain, "a sent create must quarantine the VM on timeout");

    let error = create_snapshot(
        &scratch.join("missing"),
        Duration::from_millis(20),
        &mut uncertain,
    )
    .unwrap_err();
    assert!(!uncertain, "a connection failure did not start a writer");
    assert!(error.0.starts_with("suspend_create_not_sent"));
    assert!(error
        .0
        .contains("step=open_jail_root request_started=false"));
}

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
