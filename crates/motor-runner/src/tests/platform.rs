use super::*;

fn platform_runner(f: &Fixture, kind: BackendKind) -> Runner {
    let mut config = f.config();
    if kind == BackendKind::WindowsVm {
        config = windows_instance::config(f);
    }
    #[cfg(unix)]
    if kind == BackendKind::MacosVm {
        let path = f.scratch.join("macos.toml");
        let mut text = String::from(
            "image = '/image'\nhelper = '/helper'\ntoolchain_version = 'sealed rustc'\n",
        );
        for tool in ["cargo", "rustc", "bun", "git"] {
            text.push_str(&format!(
                "[tools.{tool}]\npath = '/tools/{tool}'\nsha256 = '{}'\n",
                "0".repeat(64)
            ));
        }
        fs::write(&path, text).unwrap();
        config.vm_config = Some(path);
        config.free_space_probe = Arc::new(|_| Ok(100 << 30));
    }
    config.backend = kind;
    // Inject only the portable backend: these tests stop at admission, before
    // any Windows command could be handed to the host executor.
    Runner::open_with_backend(config, Box::new(backend::HostBackend)).unwrap()
}

fn assert_platform_terminal(runner: &Runner, id: Uuid, requested: &str, served: &str) {
    let job: Job = read_json(&runner.record_path(id)).unwrap();
    assert_eq!(job.state, JobState::Tombstone);
    assert!(job.started_ms.is_none());
    assert!(job.process.is_none());
    assert!(job.setup_ms.is_none());
    let terminal = job.terminal.unwrap();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::PlatformUnsupported,
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    let detail = terminal.refusal_detail().unwrap();
    assert!(detail.contains(requested), "{detail}");
    assert!(detail.contains(served), "{detail}");
}

#[test]
fn served_platform_is_derived_from_backend_kind() {
    let f = Fixture::new();
    let mut config = f.config();
    assert_eq!(config.served_platform(), Platform::Linux);
    config.backend = BackendKind::Vm;
    assert_eq!(config.served_platform(), Platform::Linux);
    config.backend = BackendKind::WindowsVm;
    assert_eq!(config.served_platform(), Platform::Windows);
    config.backend = BackendKind::Host;
    assert_eq!(config.served_platform(), Platform::Linux);
    config.backend = BackendKind::MacosVm;
    assert_eq!(config.served_platform(), Platform::Macos);
    assert_eq!(
        "macos-vm".parse::<BackendKind>().unwrap(),
        BackendKind::MacosVm
    );
    assert_eq!(
        "windows-vm".parse::<BackendKind>().unwrap(),
        BackendKind::WindowsVm
    );
}

#[test]
fn windows_backend_selection_requires_a_readable_vm_config() {
    let f = Fixture::new();
    let mut config = f.config();
    config.backend = BackendKind::WindowsVm;
    let error = Runner::open(config.clone()).err().unwrap();
    assert!(error.to_string().contains("--vm-config"), "{error}");
    config.vm_config = Some(f.scratch.join("vm.toml"));
    let error = Runner::open(config).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!f.config().state_dir().exists());
}

#[test]
fn platform_refusal_precedes_ordinary_admission_checks() {
    let f = Fixture::new();
    let runner = platform_runner(&f, BackendKind::Host);
    let (mut request, _) = f.run("true");
    request.platform = Some(Platform::Windows);
    request.weight_hint = Some(u32::MAX);
    let mut inner = runner.core.inner.lock().unwrap();
    for obstruction in ["cpu", "drain", "deletion", "workspace", "cwd", "dependency"] {
        let mut request = request.clone();
        request.weight_hint = None;
        match obstruction {
            "cpu" => request.weight_hint = Some(u32::MAX),
            "drain" => {
                inner.drain_deadline = Some(std::time::Instant::now() + Duration::from_secs(60))
            }
            "deletion" => {
                inner.dropping.insert(request.workspace_key.clone());
            }
            "workspace" => request.workspace_key = "relative".into(),
            "cwd" => request.cwd = "/outside-workspace".into(),
            "dependency" => request.git_dependencies.push(GitDependency {
                url: "invalid".into(),
                commits: vec!["invalid".into()],
                bundle: None,
            }),
            _ => unreachable!(),
        }
        assert_eq!(
            runner
                .run_refusal(&inner, &request, &Default::default())
                .unwrap()
                .0,
            RefusalReason::PlatformUnsupported,
            "{obstruction} must not mask platform refusal",
        );
        inner.drain_deadline = None;
        inner.dropping.clear();
    }
    drop(inner);
    for obstruction in ["queue", "disk"] {
        let f = Fixture::new();
        let mut config = f.config();
        if obstruction == "queue" {
            config.max_queue_depth = 0;
        } else {
            config.free_space_floor_bytes = u64::MAX;
            config.free_space_probe = Arc::new(|_| Ok(0));
        }
        let runner = Runner::open(config).unwrap();
        let (mut request, _) = f.run("true");
        request.platform = Some(Platform::Windows);
        assert!(
            matches!(
                runner.accept(request.clone()).unwrap(),
                Admission::Refused(RefusalReason::PlatformUnsupported)
            ),
            "{obstruction} must not mask platform refusal"
        );
        assert_platform_terminal(&runner, request.job_id, "windows", "linux");
    }
}

#[test]
fn platform_refusals_are_durable_no_start_terminals() {
    for (kind, served, requests) in [
        (
            BackendKind::Host,
            "linux",
            vec![
                Some(Platform::Windows),
                Some(Platform::Unknown("plan9".into())),
            ],
        ),
        (
            BackendKind::WindowsVm,
            "windows",
            vec![
                None,
                Some(Platform::Linux),
                Some(Platform::Unknown("plan9".into())),
            ],
        ),
    ] {
        let f = Fixture::new();
        let runner = platform_runner(&f, kind);
        for platform in requests {
            let requested = String::from(platform.clone().unwrap_or(Platform::Linux));
            let (mut request, _) = f.run("touch never-started");
            request.platform = platform;
            assert!(matches!(
                runner.accept(request.clone()).unwrap(),
                Admission::Refused(RefusalReason::PlatformUnsupported)
            ));
            assert_platform_terminal(&runner, request.job_id, &requested, served);
            let replay = runner.replay(request.job_id, 0).unwrap();
            assert!(!replay
                .iter()
                .any(|frame| matches!(frame, Frame::Started { .. })));
            assert!(matches!(replay.last(), Some(Frame::Terminal(_))));
            assert!(matches!(
                runner.accept(request.clone()).unwrap(),
                Admission::Duplicate
            ));
            assert!(!Path::new(&request.workspace_key)
                .join("never-started")
                .exists());
        }
    }
}

async fn platform_connect(socket: &Path, kind: BackendKind, started: bool) -> UnixStream {
    use tokio::io::AsyncReadExt;

    let mut stream = UnixStream::connect(socket).await.unwrap();
    write_frame_async(
        &mut stream,
        &Frame::Hello {
            return_paths: false,
            return_chunked: false,
            sibling_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        },
    )
    .await
    .unwrap();
    let expected = match (kind, started) {
        (BackendKind::Host, false) => {
            br#"{"type":"hello","protocol_version":1,"return_paths":true,"return_chunked":true}"#.as_slice()
        }
        (BackendKind::Host, true) => {
            br#"{"type":"hello","protocol_version":1,"started":true,"return_paths":true,"return_chunked":true}"#
                .as_slice()
        }
        (BackendKind::WindowsVm, true) => {
            br#"{"type":"hello","protocol_version":1,"started":true,"platforms":["windows"]}"#
                .as_slice()
        }
        (BackendKind::MacosVm, true) => br#"{"type":"hello","protocol_version":1,"started":true,"platforms":["macos"]}"#.as_slice(),
        _ => unreachable!(),
    };
    // The session's writer sends a heartbeat on its first tick, which can land
    // before the hello reply; clients skip heartbeats, so read past them and
    // compare the exact bytes of the first other frame.
    let actual = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut length = [0; 4];
            stream.read_exact(&mut length).await.unwrap();
            let mut payload = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut payload).await.unwrap();
            if !matches!(
                serde_json::from_slice::<Frame>(&payload),
                Ok(Frame::Heartbeat { .. })
            ) {
                return payload;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(actual, expected);
    stream
}

#[tokio::test]
async fn platform_wire_echo_and_linux_hello_preserve_the_served_platform() {
    for (kind, requested, served, started) in [
        (BackendKind::Host, None, Platform::Linux, false),
        (
            BackendKind::Host,
            Some(Platform::Linux),
            Platform::Linux,
            true,
        ),
        (
            BackendKind::WindowsVm,
            Some(Platform::Windows),
            Platform::Windows,
            true,
        ),
    ] {
        let f = Fixture::new();
        let runner = platform_runner(&f, kind);
        let (socket, server) = listener(&f, &runner);
        let mut stream = platform_connect(&socket, kind, started).await;
        let (mut request, _) = f.run("true");
        request.platform = requested;
        write_frame_async(&mut stream, &Frame::Run(request.clone()))
            .await
            .unwrap();
        let Frame::Accepted(accepted) = read_next(&mut stream).await else {
            panic!("expected acceptance");
        };
        assert_eq!(accepted.platform(), Some(&served));
        assert_eq!(accepted.job_id, request.job_id);
        drop(stream);
        server.abort();
    }
}

#[tokio::test]
async fn platform_wire_refusal_names_requested_and_served_platforms() {
    for (kind, served, platforms) in [
        (
            BackendKind::Host,
            "linux",
            vec![
                Some(Platform::Windows),
                Some(Platform::Unknown("plan9".into())),
            ],
        ),
        (
            BackendKind::WindowsVm,
            "windows",
            vec![
                None,
                Some(Platform::Linux),
                Some(Platform::Unknown("plan9".into())),
            ],
        ),
    ] {
        let f = Fixture::new();
        let runner = platform_runner(&f, kind);
        let (socket, server) = listener(&f, &runner);
        for platform in platforms {
            let mut stream = platform_connect(&socket, kind, true).await;
            let requested = String::from(platform.clone().unwrap_or(Platform::Linux));
            let (mut request, _) = f.run("touch never-started");
            request.platform = platform;
            write_frame_async(&mut stream, &Frame::Run(request.clone()))
                .await
                .unwrap();
            let Frame::Refusal(refusal) = read_next(&mut stream).await else {
                panic!("expected refusal before upload");
            };
            assert_eq!(refusal.reason, RefusalReason::PlatformUnsupported);
            assert_eq!(refusal.job_id, Some(request.job_id));
            let detail = refusal.detail.unwrap();
            assert!(detail.contains(&requested), "{detail}");
            assert!(detail.contains(served), "{detail}");
            assert_platform_terminal(&runner, request.job_id, &requested, served);
        }
        server.abort();
    }
}

#[cfg(unix)]
#[test]
fn macos_path_refusal_is_durable_before_platform_or_workspace_setup() {
    let f = Fixture::new();
    let runner = platform_runner(&f, BackendKind::Host);
    for invalid in [
        "relative",
        "/Users/a/../r",
        "/Users/./r",
        "/Users//r",
        "/Users/a/r/",
        "/outside/r",
        "/Users/a/r b",
    ] {
        let (mut request, _) = f.run("touch must-not-run");
        request.platform = Some(Platform::Macos);
        request.workspace_key = "/Users/motor/r".into();
        request.repository_root = "/Users/motor/r".into();
        request.siblings.clear();
        request.cwd = invalid.into();
        let id = request.job_id;
        assert!(matches!(
            runner.accept(request).unwrap(),
            Admission::Refused(RefusalReason::RequestRejected)
        ));
        let job: Job = read_json(&runner.record_path(id)).unwrap();
        assert!(job.started_ms.is_none());
        assert!(job.process.is_none());
        let terminal = job.terminal.unwrap();
        assert_eq!(terminal.ran, Some(Ran::None));
        assert!(terminal
            .refusal_detail()
            .unwrap()
            .contains("macos_path_invalid"));
    }
}

#[cfg(unix)]
fn macos_request(f: &Fixture) -> Run {
    let (mut request, _) = f.run("true");
    request.platform = Some(Platform::Macos);
    request.repository_root = "/Users/motor/project".into();
    request.workspace_key = request.repository_root.clone();
    request.cwd = request.workspace_key.clone();
    request.siblings.clear();
    request.bundles.truncate(1);
    request.bundles[0].repository = request.repository_root.clone();
    request
}
#[cfg(unix)]
#[test]
fn macos_capacity_roots_and_feature_refusals_are_backend_specific() {
    let f = Fixture::new();
    let runner = platform_runner(&f, BackendKind::MacosVm);
    assert_eq!(runner.config().max_weight, 8);
    assert_eq!(
        runner.config().workspace_roots,
        vec![runner.config().state_root.clone()]
    );
    assert_eq!(
        runner.config().owned_workspace_roots,
        runner.config().workspace_roots
    );
    assert_eq!(runner.config().free_space_floor_bytes, 20 << 30);
    assert_eq!(runner.config().free_space_eviction_bytes, Some(40 << 30));
    assert!(!runner.warm_builds_enabled("arbitrary untrusted integration tip"));
    assert!(!runner.config().writer_enabled());
    for hint in 1..=4 {
        let mut request = macos_request(&f);
        request.weight_hint = Some(hint);
        assert!(matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Accepted(_)
        ));
        assert_eq!(
            read_json::<Job>(&runner.record_path(request.job_id))
                .unwrap()
                .weight,
            4
        );
    }
    for (feature, detail, reason) in [
        ("weight", "macos_vcpu_limit", RefusalReason::RequestRejected),
        (
            "paths",
            "return_paths_unsupported",
            RefusalReason::RequestRejected,
        ),
        (
            "chunks",
            "return_chunked_unsupported",
            RefusalReason::RequestRejected,
        ),
        (
            "network",
            "network_unsupported",
            RefusalReason::NetworkUnsupported,
        ),
    ] {
        let mut request = macos_request(&f);
        match feature {
            "weight" => request.weight_hint = Some(5),
            "paths" => request.return_paths = vec![format!("{}/file", request.workspace_key)],
            "chunks" => request.return_chunked = true,
            _ => request.network = Some(Network::Outbound),
        }
        assert!(
            matches!(runner.accept(request.clone()).unwrap(),Admission::Refused(r) if r==reason)
        );
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        assert_eq!(job.terminal.unwrap().refusal_detail(), Some(detail));
        assert!(job.started_ms.is_none());
    }
}
#[cfg(unix)]
#[tokio::test]
async fn macos_hello_advertises_only_macos_and_accepted_echoes_it() {
    let f = Fixture::new();
    let runner = platform_runner(&f, BackendKind::MacosVm);
    let (socket, server) = listener(&f, &runner);
    let mut stream = platform_connect(&socket, BackendKind::MacosVm, true).await;
    let request = macos_request(&f);
    write_frame_async(&mut stream, &Frame::Run(request.clone()))
        .await
        .unwrap();
    let Frame::Accepted(accepted) = read_next(&mut stream).await else {
        panic!("expected macOS acceptance")
    };
    assert_eq!(accepted.platform(), Some(&Platform::Macos));
    assert_eq!(accepted.job_id, request.job_id);
    server.abort();
}

#[cfg(target_os = "macos")]
#[test]
fn macos_backend_guard_requires_explicit_development_override() {
    for kind in [BackendKind::Host, BackendKind::Vm, BackendKind::WindowsVm] {
        let mut config = Config::new("unused", "guard");
        config.backend = kind;
        assert!(config
            .configure_backend()
            .unwrap_err()
            .to_string()
            .contains("macOS hosts require --backend macos-vm"));
    }
    let mut config = Config::new("unused", "guard");
    config.development_allow_host_backend = true;
    config.configure_backend().unwrap();
}

#[cfg(unix)]
#[test]
fn macos_terminal_never_claims_workspace_changes_and_queued_cancel_is_prompt() {
    let f = Fixture::new();
    let runner = platform_runner(&f, BackendKind::MacosVm);
    let request = macos_request(&f);
    let mut job = Job::new(request.job_id, Some(request.clone()), 1, 1, 4);
    job.started_ms = Some(2);
    job.terminal(Outcome::Exit { code: 7 }, 3);
    let terminal = job.terminal.unwrap();
    assert!(terminal.workspace_changes.is_none());
    assert_eq!(
        serde_json::to_value(&terminal)
            .unwrap()
            .get("workspace_changes"),
        Some(&serde_json::Value::Null)
    );
    runner.accept(request.clone()).unwrap();
    let now = std::time::Instant::now();
    runner.cancel(request.job_id).unwrap();
    assert!(now.elapsed() < Duration::from_secs(2));
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_eq!(job.terminal.unwrap().outcome, Outcome::Cancelled);
    assert!(job.started_ms.is_none());
}
