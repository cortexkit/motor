use super::*;
use motor_lease::metrics::{Tenant, TenantState};
use serde_json::json;

fn scratch() -> PathBuf {
    let path = cortexkit_test_support::scratch_root()
        .join(format!("monitor-{}", motor_protocol::Uuid::now_v7()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn accepted_record() -> HostMetrics {
    HostMetrics {
        at_ms: 1_800_000_000_000,
        cpu: Cpu {
            threads: 32,
            busy_pct: 41,
            load1: Some(12.4),
        },
        mem: Memory {
            total_mib: 128000,
            avail_mib: 22000,
        },
        disks: vec![Disk {
            mount: "/".into(),
            total_gib: 900,
            free_gib: 310,
        }],
        tenants: vec![Tenant {
            vm: "vm-1".into(),
            state: TenantState::Ready,
            vcpus: 30,
            mem_mib: 106496,
            slots_total: 90,
            slots_used: 41,
            jobs_running: 12,
            recovery: None,
            detail: None,
        }],
        runners: vec![Runner {
            name: "direct".into(),
            state: RunnerState::Ready,
            slots_total: 16,
            slots_used: 3,
            queued: 0,
            oldest_wait_s: 0,
            detail: None,
        }],
    }
}

#[test]
fn monitor_signing_bytes_match_cloud_vector() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/host-metrics.json"
    ))
    .unwrap();
    let hex = vector["seed_hex"].as_str().unwrap();
    let bytes: Vec<_> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let key = SigningKey::from_bytes(&bytes.try_into().unwrap());
    for (index, load1) in [(0, Some(12.4)), (5, None)] {
        let mut record = accepted_record();
        record.cpu.load1 = load1;
        let case = &vector["cases"][index];
        let request = &case["request"];
        let timestamp = request["headers"]["CK-Timestamp"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let counter = request["headers"]["CK-Counter"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let (path, body, headers) =
            signed(&record, "metrics-vector", &key, counter, timestamp).unwrap();
        assert_eq!(
            motor_lease::host_message_bytes("POST", &path, &body, timestamp, counter),
            case["signed_string"].as_str().unwrap().as_bytes()
        );
        assert_eq!(std::str::from_utf8(&body).unwrap(), request["body"]);
        assert_eq!(headers.signature, request["headers"]["CK-Signature"]);
    }
}

#[test]
fn monitor_settings_are_opt_in_and_defaults_are_not_serialized() {
    assert_eq!(
        serde_json::to_value(Settings::default()).unwrap(),
        json!({})
    );
    let mut config = Config::new(scratch(), "metrics");
    assert!(!config.metrics.enabled);
    start(&config).unwrap();
    assert!(!config.state_dir().exists());
    assert!(run(&config).is_err());
    config.metrics.enabled = true;
    assert!(config.metrics.check().is_err());
    config.metrics.host_id = Some("monitor-1".into());
    config.metrics.endpoint = Some("https://runners.cortexkit.io".into());
    config.metrics.check().unwrap();
    config.metrics.endpoint = Some("http://runners.cortexkit.io".into());
    assert!(config.metrics.check().is_err());
    config.metrics.key_file = Some("../job-key".into());
    assert!(public_key(&config).is_err());
    config.metrics.key_file = None;
    config.instance = "..".into();
    assert!(public_key(&config).is_err());
}

#[test]
fn monitor_load_absence_is_null_not_a_fake_zero_or_missing_key() {
    let mut record = accepted_record();
    record.cpu.load1 = None;
    let body = record.encode(record.at_ms).unwrap();
    assert!(std::str::from_utf8(&body)
        .unwrap()
        .contains("\"load1\":null"));
    let mut value = serde_json::to_value(&record).unwrap();
    value["cpu"].as_object_mut().unwrap().remove("load1");
    assert!(serde_json::from_value::<HostMetrics>(value).is_err());
}

#[test]
fn monitor_identity_is_stable_and_counter_is_durable_exclusive_and_fail_closed() {
    let config = Config::new(scratch(), "metrics");
    let first = public_key(&config).unwrap();
    assert_eq!(first.len(), 64);
    assert_eq!(public_key(&config).unwrap(), first);
    let (key, journal) = identity_paths(&config).unwrap();
    assert_eq!(std::fs::metadata(key).unwrap().len(), 32);
    let mut counter = identity::Counter::open(&journal).unwrap();
    assert!(identity::Counter::open(&journal).is_err());
    assert_eq!(counter.reserve(100).unwrap(), 100_000);
    assert_eq!(counter.reserve(99).unwrap(), 100_001);
    drop(counter);
    let mut counter = identity::Counter::open(&journal).unwrap();
    assert_eq!(counter.reserve(1).unwrap(), 100_002);
    drop(counter);
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(&[0])
        .unwrap();
    assert!(identity::Counter::open(&journal).is_err());
}

#[cfg(unix)]
#[test]
fn monitor_identity_refuses_symlinks_and_readable_or_linked_keys() {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    let config = Config::new(scratch(), "metrics");
    public_key(&config).unwrap();
    let (key, _) = identity_paths(&config).unwrap();
    assert_eq!(
        std::fs::metadata(key.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(std::fs::metadata(&key).unwrap().mode() & 0o777, 0o600);
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(public_key(&config).is_err());
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let saved = key.with_extension("saved");
    std::fs::rename(&key, &saved).unwrap();
    symlink(&saved, &key).unwrap();
    assert!(public_key(&config).is_err());
    std::fs::remove_file(&key).unwrap();
    std::fs::hard_link(&saved, &key).unwrap();
    assert!(public_key(&config).is_err());
    std::fs::remove_file(&key).unwrap();
    std::fs::rename(&saved, &key).unwrap();
    let root = key.parent().unwrap();
    let moved = root.with_extension("saved");
    std::fs::rename(root, &moved).unwrap();
    symlink(&moved, root).unwrap();
    assert!(public_key(&config).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn monitor_linux_counters_exclude_guest_cpu_and_use_available_memory() {
    let (total, idle, threads) =
        sample::cpu_ticks("cpu 10 20 30 40 50 60 70 80 999 999\ncpu0 0\ncpu1 0\n").unwrap();
    assert_eq!((total, idle, threads), (360, 90, 2));
    let mem =
        sample::memory("MemTotal: 8192 kB\nMemFree: 1024 kB\nMemAvailable: 3072 kB\n").unwrap();
    assert_eq!((mem.total_mib, mem.avail_mib), (8, 3));
    assert!(sample::cpu_ticks("").is_err());
    assert!(sample::memory("MemFree: 1000 kB").is_err());
    assert!(sample::Sampler::default()
        .sample(unix_ms())
        .unwrap()
        .is_none());
}

#[test]
fn monitor_runner_mapping_exports_only_contract_counters_and_fixed_errors() {
    let status = motor_protocol::StatusReply::new(4, vec![], true, vec![], "rustc");
    let health: motor_protocol::RunnerHealth = serde_json::from_value(json!({
        "runner_version": "test", "since_ms": 0, "window_minutes": 60, "free_bytes": 123,
        "queue_depth": 4, "running": [], "counts": motor_protocol::HealthCounts::default(),
        "permits_cap": 12, "permits_held": 5, "oldest_wait_s": 71
    }))
    .unwrap();
    let mut reply = motor_protocol::RunnerStatusReply {
        status,
        disk: None,
        health: Some(Box::new(health)),
    };
    let record = runner_record(&reply).unwrap();
    assert_eq!(record.slots_total, 12);
    assert_eq!(record.slots_used, 5);
    assert_eq!(record.queued, 4);
    assert_eq!(record.oldest_wait_s, 71);
    assert_eq!(record.state, RunnerState::Ready);
    let body = serde_json::to_value(record).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 7);
    reply.status.server_reachable = false;
    assert_eq!(
        runner_record(&reply).unwrap().detail.as_deref(),
        Some("runner_not_ready")
    );
    reply.health.as_mut().unwrap().oldest_wait_s = None;
    assert!(runner_record(&reply).is_err());
}

#[test]
fn monitor_sender_uses_metrics_only_and_burns_refused_counters_without_retry() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let key = SigningKey::from_bytes(&[7; 32]);
    let public = key.verifying_key();
    let server = std::thread::spawn(move || {
        let mut auth = motor_lease::HostAuthenticator::default();
        auth.register("monitor-1".into(), public);
        let mut counters = vec![];
        for status in [400, 401, 204] {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let end = loop {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break request.len();
                }
                assert!(request.len() < 8192);
            };
            let headers = String::from_utf8(request[..end].to_vec()).unwrap();
            assert!(headers.starts_with("POST /v1/hosts/monitor-1/metrics HTTP/1.1\r\n"));
            let field = |name: &str| {
                headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                            .map(|(_, value)| value.trim().to_owned())
                    })
                    .unwrap()
            };
            let length: usize = field("content-length").parse().unwrap();
            let mut body = vec![0; length];
            socket.read_exact(&mut body).unwrap();
            let signed = HostHeaders {
                host: field("ck-host"),
                timestamp: field("ck-timestamp").parse().unwrap(),
                counter: field("ck-counter").parse().unwrap(),
                signature: field("ck-signature"),
            };
            auth.verify(
                "POST",
                "/v1/hosts/monitor-1/metrics",
                &body,
                &signed,
                signed.timestamp,
            )
            .unwrap();
            counters.push(signed.counter);
            let body = match status {
                400 => r#"{"error":"bad_request","field":"cpu.load1"}"#,
                401 => r#"{"error":"host_auth_failed"}"#,
                _ => "",
            };
            write!(
                socket,
                "HTTP/1.1 {status} reply\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
        counters
    });
    let config = Config::new(scratch(), "sender");
    public_key(&config).unwrap();
    let (_, journal) = identity_paths(&config).unwrap();
    let mut counter = identity::Counter::open(&journal).unwrap();
    let settings = Settings {
        endpoint: Some(endpoint),
        host_id: Some("monitor-1".into()),
        ..Settings::default()
    };
    let client = client().unwrap();
    let mut record = accepted_record();
    record.at_ms = unix_ms();
    assert!(send(&client, &settings, &key, &mut counter, &record).is_err());
    assert!(send(&client, &settings, &key, &mut counter, &record).is_err());
    send(&client, &settings, &key, &mut counter, &record).unwrap();
    let counters = server.join().unwrap();
    assert_eq!(counters.len(), 3);
    assert!(counters.windows(2).all(|pair| pair[1] > pair[0]));
}

#[test]
fn monitor_sender_timeout_includes_response_body_and_does_not_retry() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (release, wait) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut buffer = [0; 8192];
        assert!(socket.read(&mut buffer).unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 200\r\n\r\n{")
            .unwrap();
        wait.recv_timeout(Duration::from_secs(10)).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    });
    let config = Config::new(scratch(), "timeout");
    public_key(&config).unwrap();
    let (_, journal) = identity_paths(&config).unwrap();
    let mut counter = identity::Counter::open(&journal).unwrap();
    let settings = Settings {
        endpoint: Some(endpoint),
        host_id: Some("monitor-1".into()),
        ..Settings::default()
    };
    let mut record = accepted_record();
    record.at_ms = unix_ms();
    let started = std::time::Instant::now();
    assert!(send(
        &client().unwrap(),
        &settings,
        &SigningKey::from_bytes(&[7; 32]),
        &mut counter,
        &record
    )
    .is_err());
    assert!(started.elapsed() >= TIMEOUT);
    release.send(()).unwrap();
    server.join().unwrap();
}
