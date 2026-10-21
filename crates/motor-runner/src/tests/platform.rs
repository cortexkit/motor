use super::*;

fn platform_runner(f: &Fixture, kind: BackendKind) -> Runner {
    let mut config = f.config();
    if kind == BackendKind::WindowsVm {
        config = windows_instance::config(f);
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
            runner.run_refusal(&inner, &request).unwrap().0,
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
            br#"{"type":"hello","protocol_version":1,"return_paths":true}"#.as_slice()
        }
        (BackendKind::Host, true) => {
            br#"{"type":"hello","protocol_version":1,"started":true,"return_paths":true}"#
                .as_slice()
        }
        (BackendKind::WindowsVm, true) => {
            br#"{"type":"hello","protocol_version":1,"started":true,"platforms":["windows"]}"#
                .as_slice()
        }
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
