use super::*;

fn platform_runner(f: &Fixture, kind: BackendKind) -> Runner {
    let config = platform_config(f, kind);
    // Inject only the portable backend: these tests stop at admission, before
    // any Windows command could be handed to the host executor.
    Runner::open_with_backend(config, Box::new(backend::HostBackend)).unwrap()
}

fn platform_config(f: &Fixture, kind: BackendKind) -> Config {
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
    config
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
            return_chunked_max_bytes: None,
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
            br#"{"type":"hello","protocol_version":1,"started":true,"return_paths":true,"return_chunked":true,"platforms":["windows"]}"#
                .as_slice()
        }
        (BackendKind::MacosVm, true) => br#"{"type":"hello","protocol_version":1,"started":true,"return_paths":true,"return_chunked":true,"platforms":["macos"]}"#.as_slice(),
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
            "network",
            "network_unsupported",
            RefusalReason::NetworkUnsupported,
        ),
    ] {
        let mut request = macos_request(&f);
        match feature {
            "weight" => request.weight_hint = Some(5),
            _ => request.network = Some(Network::Outbound),
        }
        assert!(
            matches!(runner.accept(request.clone()).unwrap(),Admission::Refused(r) if r==reason)
        );
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        assert_eq!(job.terminal.unwrap().refusal_detail(), Some(detail));
        assert!(job.started_ms.is_none());
    }
    for chunked in [false, true] {
        let mut request = macos_request(&f);
        request.return_paths = vec![format!("{}/file", request.workspace_key)];
        request.return_chunked = chunked;
        assert!(matches!(
            runner.accept(request).unwrap(),
            Admission::Accepted(_)
        ));
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
        // Exercise production's default rather than the library fixture opt-in.
        config.development_allow_host_backend = false;
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
fn macos_terminal_without_a_scan_is_unknown_and_queued_cancel_is_prompt() {
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

#[cfg(unix)]
struct MacosTerminalBackend;

#[cfg(unix)]
impl Backend for MacosTerminalBackend {
    fn materialize(&self, _: &Config, _: &Trees, _: &Setup) -> io::Result<Prepared> {
        unreachable!("terminal tests do not launch a VM")
    }
    fn launch(
        &self,
        _: &Config,
        _: &Trees,
        _: &Setup,
        _: &Prepared,
        _: u32,
    ) -> io::Result<backend::Launch> {
        unreachable!("terminal tests do not launch a VM")
    }
    fn cancel(&self, config: &Config, job: &Job, hard: bool) -> io::Result<()> {
        crate::macos::MacosBackend.cancel(config, job, hard)
    }
    fn unit_alive(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        crate::macos::MacosBackend.unit_alive(config, process)
    }
    fn collect_changes(
        &self,
        config: &Config,
        trees: &Trees,
        job: &Job,
    ) -> io::Result<Option<Vec<String>>> {
        crate::macos::MacosBackend.collect_changes(config, trees, job)
    }
    fn reconcile(&self, _: &Config, _: &[Job]) -> io::Result<Vec<Uuid>> {
        Ok(vec![])
    }
}

#[cfg(unix)]
fn macos_terminal_after_workspace_scan(failed_scan: bool) -> TerminalRecord {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new();
    let mut config = platform_config(&f, BackendKind::MacosVm);
    config.manual_tick = true;
    let runner = Runner::open_with_backend(config, Box::new(MacosTerminalBackend)).unwrap();
    let request = macos_request(&f);
    runner.accept(request.clone()).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&request.job_id).unwrap();
        job.state = JobState::Running;
        job.started_ms = Some(runner.now());
        job.durable_output = true;
        runner.save(job).unwrap();
    }
    let mirror = runner
        .core
        .trees
        .ensure_mirror(&request.repository_root)
        .unwrap();
    git(&mirror, &["fetch", text(&f.main), "HEAD:refs/heads/main"]);
    let workspace = runner
        .config()
        .state_root
        .join("jobs")
        .join(request.job_id.to_string())
        .join("Users/motor/project");
    fs::create_dir_all(workspace.parent().unwrap()).unwrap();
    git(
        &f.main,
        &["clone", "--no-local", text(&f.main), text(&workspace)],
    );
    fs::write(workspace.join("source"), "job edit\n").unwrap();
    fs::write(workspace.join("new"), "job addition\n").unwrap();
    if failed_scan {
        fs::remove_dir_all(&workspace).unwrap();
    }
    crate::macos::capture_finished_workspace(
        runner.config(),
        &runner.core.trees,
        &request,
        workspace,
    );
    runner
        .finish_execution(
            request.job_id,
            Ok(backend::ExecutionOutcome::Unix(
                std::process::ExitStatus::from_raw(0),
            )),
            false,
        )
        .unwrap();
    let disk: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    let terminal = disk.terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
    assert_eq!(terminal.ran, Some(Ran::Remote));
    terminal
}

#[cfg(unix)]
#[test]
fn macos_terminal_reports_workspace_changes_after_tree_cleanup() {
    let terminal = macos_terminal_after_workspace_scan(false);
    let mut changes = terminal.workspace_changes.unwrap();
    changes.sort();
    assert_eq!(changes, ["new", "source"]);
}

#[cfg(unix)]
#[test]
fn macos_terminal_omits_workspace_changes_when_scan_fails() {
    let terminal = macos_terminal_after_workspace_scan(true);
    assert!(terminal.workspace_changes.is_none());
}

#[cfg(unix)]
#[test]
fn macos_returns_replay_restart_and_expire_after_staging_deletion() {
    use std::os::unix::process::ExitStatusExt;
    for chunked in [false, true] {
        let f = Fixture::new();
        let mut config = platform_config(&f, BackendKind::MacosVm);
        config.manual_tick = true;
        let runner =
            Runner::open_with_backend(config.clone(), Box::new(MacosTerminalBackend)).unwrap();
        let mut request = macos_request(&f);
        request.return_chunked = chunked;
        request.return_paths = vec![format!("{}/artifact", request.workspace_key)];
        runner.accept(request.clone()).unwrap();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&request.job_id).unwrap();
            job.state = JobState::Running;
            job.started_ms = Some(runner.now());
            job.durable_output = true;
            runner.save(job).unwrap();
        }
        runner
            .output(request.job_id, OutputStream::Stdout, b"output")
            .unwrap();
        let workspace = config
            .state_root
            .join("jobs")
            .join(request.job_id.to_string())
            .join("Users/motor/project");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("artifact"), b"x").unwrap();
        crate::macos::capture_finished_workspace(
            runner.config(),
            &runner.core.trees,
            &request,
            workspace.clone(),
        );
        assert!(!workspace.exists());
        assert!(runner
            .config()
            .macos_runtime
            .take_returns(Uuid::now_v7())
            .is_none());
        runner
            .finish_execution(
                request.job_id,
                Ok(backend::ExecutionOutcome::Unix(
                    std::process::ExitStatus::from_raw(23 << 8),
                )),
                false,
            )
            .unwrap();
        assert!(
            runner
                .config()
                .macos_runtime
                .take_returns(request.job_id)
                .is_none(),
            "completion consumed once"
        );
        let frames = runner.replay(request.job_id, 0).unwrap();
        assert!(matches!(&frames[0], Frame::Output(_)));
        assert!(
            matches!(frames.last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::Exit { code: 23 })
        );
        if chunked {
            let (files, manifest, seqs) = super::return_paths::assemble(&frames, &request);
            assert_eq!(files, vec![Some(b"x".to_vec())]);
            assert_eq!(seqs, vec![1]);
            assert_eq!(manifest.seq, 2);
        } else {
            assert!(
                matches!(&frames[1], Frame::ReturnResult(result) if result.seq == 1 && result.returned_files.as_ref().unwrap()[0].data == "eA==")
            );
        }
        drop(runner);
        let runner = Runner::open_with_backend(config, Box::new(MacosTerminalBackend)).unwrap();
        assert_eq!(runner.replay(request.job_id, 0).unwrap(), frames);
        if chunked {
            let (finished, returned) = {
                let inner = runner.core.inner.lock().unwrap();
                let job = &inner.jobs[&request.job_id];
                (job.finished_ms.unwrap(), job.returned.clone().unwrap())
            };
            let directory = runner
                .record_path(request.job_id)
                .parent()
                .unwrap()
                .join(returned.directory);
            f.clock
                .store(finished + RETURN_RETENTION_MS, Ordering::SeqCst);
            runner.maintenance().unwrap();
            assert!(!directory.exists());
            let expired = runner.replay(request.job_id, 0).unwrap();
            let (files, manifest, _) = super::return_paths::assemble(&expired, &request);
            assert_eq!(files, vec![None]);
            assert_eq!(
                manifest.files[0].cause(),
                Some(&ReturnPathsFailureCause::Expired)
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn macos_kill_after_stop_waits_for_capture_handoff_without_orphan_spool() {
    use std::os::unix::process::ExitStatusExt;
    for deadline in [false, true] {
        let f = Fixture::new();
        let mut config = platform_config(&f, BackendKind::MacosVm);
        config.manual_tick = true;
        let runner = Runner::open_with_backend(config, Box::new(MacosTerminalBackend)).unwrap();
        let mut request = macos_request(&f);
        request.return_chunked = true;
        request.return_paths = vec![format!("{}/artifact", request.workspace_key)];
        request.timeout = deadline.then_some(1);
        runner.accept(request.clone()).unwrap();
        let process = platform::ProcessIdentity {
            pid: 0,
            start: 0,
            unit: Some(request.job_id),
        };
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&request.job_id).unwrap();
            job.state = JobState::Running;
            job.started_ms = Some(runner.now());
            job.process = Some(process);
            runner.save(job).unwrap();
        }
        let mirror = runner
            .core
            .trees
            .ensure_mirror(&request.repository_root)
            .unwrap();
        git(&mirror, &["fetch", text(&f.main), "HEAD:refs/heads/main"]);
        let workspace = runner
            .config()
            .state_root
            .join("jobs")
            .join(request.job_id.to_string())
            .join("Users/motor/project");
        fs::create_dir_all(workspace.parent().unwrap()).unwrap();
        git(
            &f.main,
            &["clone", "--no-local", text(&f.main), text(&workspace)],
        );
        fs::write(workspace.join("source"), "job edit\n").unwrap();
        fs::write(workspace.join("artifact"), b"x").unwrap();
        runner
            .output(request.job_id, OutputStream::Stdout, b"output")
            .unwrap();

        crate::macos::capture_finished_workspace_after_stop(
            runner.config(),
            &runner.core.trees,
            &request,
            workspace.clone(),
            || {
                if deadline {
                    f.clock.fetch_add(1000, Ordering::SeqCst);
                    runner.enforce_kills(std::time::Instant::now()).unwrap();
                } else {
                    runner.cancel(request.job_id).unwrap();
                    runner.cancel(request.job_id).unwrap();
                    runner.enforce_kills(std::time::Instant::now()).unwrap();
                }
                assert_eq!(
                    runner.unit_alive(Some(process)).unwrap(),
                    Some(true),
                    "a stopped VM still owns pending captures"
                );
                let inner = runner.core.inner.lock().unwrap();
                assert!(inner.jobs[&request.job_id].kill.is_some());
                assert!(inner.jobs[&request.job_id].terminal.is_none());
                assert!(!inner.finalizing.contains(&request.job_id));
            },
        );
        assert!(!workspace.exists());
        assert_eq!(runner.unit_alive(Some(process)).unwrap(), Some(false));
        runner
            .finish_execution(
                request.job_id,
                Ok(backend::ExecutionOutcome::Unix(
                    std::process::ExitStatus::from_raw(0),
                )),
                false,
            )
            .unwrap();
        let frames = runner.replay(request.job_id, 0).unwrap();
        let (files, _, _) = super::return_paths::assemble(&frames, &request);
        assert_eq!(files, vec![Some(b"x".to_vec())]);
        let Frame::Terminal(terminal) = frames.last().unwrap() else {
            unreachable!()
        };
        let mut changes = terminal.workspace_changes.clone().unwrap();
        changes.sort();
        assert_eq!(changes, ["artifact", "source"]);
        let disk: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        let directory = disk.returned.unwrap().directory;
        let mut spools: Vec<_> = fs::read_dir(runner.record_path(request.job_id).parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
            .filter(|name| name.starts_with(RETURNED_DIRECTORY_PREFIX))
            .collect();
        spools.sort();
        assert_eq!(
            spools,
            vec![directory],
            "every spool belongs to the finished record"
        );
        assert!(runner
            .config()
            .macos_runtime
            .take_returns(request.job_id)
            .is_none());
    }
}
