use super::*;

fn running_record(f: &Fixture, runner: &Runner) -> Uuid {
    let (request, _) = f.run("true");
    runner.accept(request.clone()).unwrap();
    let mut inner = runner.core.inner.lock().unwrap();
    let job = inner.jobs.get_mut(&request.job_id).unwrap();
    job.state = JobState::Running;
    job.started_ms = Some(runner.now());
    job.durable_output = true;
    runner.save(job).unwrap();
    request.job_id
}

#[tokio::test]
async fn monitor_output_enospc_retries_terminal_and_drain_completes() {
    let f = Fixture::new();
    let armed = Arc::new(AtomicBool::new(false));
    let allow_record = Arc::new(AtomicBool::new(false));
    struct AllowWrites(Arc<AtomicBool>);
    impl Drop for AllowWrites {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let _allow_on_panic = AllowWrites(allow_record.clone());
    let attempts = Arc::new(AtomicU64::new(0));
    let mut config = f.config();
    config.before_job_write = Some({
        let (armed, allow_record, attempts) =
            (armed.clone(), allow_record.clone(), attempts.clone());
        Arc::new(move |path| {
            if armed.load(Ordering::SeqCst) {
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("chunk-")
                {
                    return Err(io::Error::from_raw_os_error(libc::ENOSPC));
                }
                if !allow_record.load(Ordering::SeqCst) {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    return Err(io::Error::from_raw_os_error(libc::ENOSPC));
                }
            }
            Ok(())
        })
    });
    let runner = Runner::open(config).unwrap();
    let id = running_record(&f, &runner);
    let directory = runner
        .core
        .trees
        .state
        .join("sandboxes")
        .join(id.to_string())
        .join("output");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("stdout"), "lost output\n").unwrap();
    fs::write(directory.join("stderr"), "").unwrap();
    fs::write(directory.join("exit"), "101\n").unwrap();
    armed.store(true, Ordering::SeqCst);
    runner.begin_drain(Duration::from_secs(30), false);
    let monitor = {
        let runner = runner.clone();
        std::thread::spawn(move || runner.monitor_unit(id))
    };
    wait_for(|| attempts.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(
        runner.drain_result(),
        None,
        "a memory-only terminal is not a drain barrier"
    );
    assert_eq!(runner.job_status(id).unwrap().state, "running");
    let disk: Job = read_json(&runner.record_path(id)).unwrap();
    assert!(disk.terminal.is_none());
    // Another job can still use the state lock during this job's write backoff.
    let other = Uuid::now_v7();
    allow_record.store(true, Ordering::SeqCst);
    runner.cancel(other).unwrap();
    wait_for(|| runner.drain_result() == Some(false)).await;
    monitor.join().unwrap();
    let disk: Job = read_json(&runner.record_path(id)).unwrap();
    assert_eq!(disk.terminal.unwrap().outcome, Outcome::OutcomeUnknown);
    assert_eq!(
        disk.monitoring_error.as_deref(),
        Some(
            io::Error::from_raw_os_error(libc::ENOSPC)
                .to_string()
                .as_str()
        )
    );
}

#[tokio::test]
async fn cancel_after_monitor_lost_an_exited_process_finalizes_record() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = running_record(&f, &runner);
    runner.cancel(id).unwrap();
    let disk: Job = read_json(&runner.record_path(id)).unwrap();
    let terminal = disk.terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    assert_eq!(terminal.killed, Some(Killed::Cancel));
}

fn systemd_runner(f: &Fixture, state: &str) -> (Runner, Arc<Mutex<Vec<Vec<String>>>>) {
    systemd_runner_with_state(f, Arc::new(Mutex::new(state.to_string())))
}

/// Like `systemd_runner`, but the reported unit state can change during the test.
fn systemd_runner_with_state(
    f: &Fixture,
    state: Arc<Mutex<String>>,
) -> (Runner, Arc<Mutex<Vec<Vec<String>>>>) {
    use std::os::unix::process::ExitStatusExt;
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut config = f.config();
    config.systemd_command = Some({
        let commands = commands.clone();
        Arc::new(move |args| {
            commands
                .lock()
                .unwrap()
                .push(args.iter().map(|s| s.to_string()).collect());
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: if args[0] == "show" {
                    state.lock().unwrap().as_bytes().to_vec()
                } else {
                    Vec::new()
                },
                stderr: Vec::new(),
            })
        })
    });
    (Runner::open(config).unwrap(), commands)
}

#[test]
fn oom_result_records_detail_and_preserves_the_exit_code() {
    use std::os::unix::process::ExitStatusExt;

    for (vcpus, limit, advice) in [
        (
            1,
            "16",
            " Request more vCPUs, for example linux,8c, to raise the limit to 28 GiB.",
        ),
        (
            2,
            "16",
            " Request more vCPUs, for example linux,8c, to raise the limit to 28 GiB.",
        ),
        (
            4,
            "16",
            " Request more vCPUs, for example linux,8c, to raise the limit to 28 GiB.",
        ),
        (
            5,
            "17.5",
            " Request more vCPUs, for example linux,10c, to raise the limit to 35 GiB.",
        ),
        (
            8,
            "28",
            " Request more vCPUs, for example linux,16c, to raise the limit to 56 GiB.",
        ),
        (
            16,
            "56",
            " Request more vCPUs, for example linux,30c, to raise the limit to 105 GiB.",
        ),
        (
            29,
            "101.5",
            " Request more vCPUs, for example linux,30c, to raise the limit to 105 GiB.",
        ),
        (
            30,
            "105",
            " The job is already at the maximum of 30 vCPUs (105 GiB).",
        ),
    ] {
        for (result, code, oom) in [("oom-kill", 143, true), ("success", 0, false)] {
            let f = Fixture::new();
            let unit_id = Uuid::now_v7();
            let mut config = f.config();
            config.systemd_command = Some(Arc::new(move |args| {
                use std::os::unix::process::ExitStatusExt;
                let stdout = match args {
                    ["show", "--property=Result", unit] => {
                        assert_eq!(*unit, crate::backend::host_unit_name(unit_id));
                        format!("Result={result}\n").into_bytes()
                    }
                    ["reset-failed", unit] => {
                        assert_eq!(*unit, crate::backend::host_unit_name(unit_id));
                        Vec::new()
                    }
                    ["list-units", ..] => Vec::new(),
                    _ => panic!("unexpected systemd command: {args:?}"),
                };
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout,
                    stderr: Vec::new(),
                })
            }));
            let runner = Runner::open(config).unwrap();
            let id = running_record(&f, &runner);
            {
                let mut inner = runner.core.inner.lock().unwrap();
                let job = inner.jobs.get_mut(&id).unwrap();
                job.process = Some(platform::ProcessIdentity {
                    pid: 0,
                    start: 0,
                    unit: Some(unit_id),
                });
                job.weight = vcpus;
                runner.save(job).unwrap();
            }

            runner
                .finish(id, Ok(std::process::ExitStatus::from_raw(code << 8)))
                .unwrap();

            let job: Job = read_json(&runner.record_path(id)).unwrap();
            assert_eq!(job.terminal.unwrap().outcome, Outcome::Exit { code });
            let detail = format!(
            "the job's memory limit of {limit} GiB was reached; the kernel killed a process and the job was stopped.{advice}"
        );
            assert_eq!(job.memory_limit_detail, oom.then_some(detail.clone()));
            assert_eq!(
                runner.runner_status().health.unwrap().counts.oom_stops,
                if oom { 1 } else { 0 }
            );
            let output = job_output(&runner, id);
            assert_eq!(output.contains(&detail), oom, "{output}");
        }
    }
}

fn cancelled_unit(f: &Fixture, runner: &Runner) -> Uuid {
    let id = running_record(f, runner);
    let mut inner = runner.core.inner.lock().unwrap();
    let job = inner.jobs.get_mut(&id).unwrap();
    job.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(Uuid::now_v7()),
    });
    job.kill = Some((Killed::Cancel, runner.now() - 5000));
    runner.save(job).unwrap();
    id
}

#[tokio::test]
async fn recovery_of_gone_unit_never_kills_and_finalizes() {
    for state in [
        "LoadState=not-found\n",
        "LoadState=loaded\nActiveState=inactive\nSubState=dead\n",
        "LoadState=loaded\nActiveState=failed\nSubState=failed\n",
    ] {
        let f = Fixture::new();
        let (runner, commands) = systemd_runner(&f, state);
        let id = cancelled_unit(&f, &runner);
        runner
            .enforce_kills(
                &mut runner.core.inner.lock().unwrap(),
                std::time::Instant::now(),
            )
            .unwrap();
        let result = terminal(&runner, id).await;
        assert_eq!(result.outcome, Outcome::OutcomeUnknown);
        assert_eq!(result.killed, Some(Killed::Cancel));
        assert!(
            commands
                .lock()
                .unwrap()
                .iter()
                .all(|args| args[0] == "show"),
            "gone unit received a stop/kill"
        );
    }
}

#[test]
fn recovery_of_stuck_active_unit_bounds_kills_with_backoff() {
    let f = Fixture::new();
    let state = Arc::new(Mutex::new(
        "LoadState=loaded\nActiveState=active\nSubState=running\n".to_string(),
    ));
    let (runner, commands) = systemd_runner_with_state(&f, state.clone());
    let id = cancelled_unit(&f, &runner);
    let instant = std::time::Instant::now();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        for ms in 0..60_000 {
            runner
                .enforce_kills(&mut inner, instant + Duration::from_millis(ms))
                .unwrap();
        }
        assert!(
            inner.jobs[&id].terminal.is_none(),
            "never release a live unit's workspace"
        );
        let commands = commands.lock().unwrap();
        let kills: Vec<_> = commands.iter().filter(|args| args[0] == "kill").collect();
        assert_eq!(kills.len(), 3);
        assert!(
            commands.len() < 25,
            "inspection also needs backoff: {} commands",
            commands.len()
        );
    }
    // The fake unit never exits on its own. A runner thread can outlive this
    // handle, and the fixture then waits for every job to end; let the unit
    // stop so the cancelled job can finish instead of hanging teardown.
    *state.lock().unwrap() = "LoadState=loaded\nActiveState=inactive\nSubState=dead\n".into();
    runner
        .enforce_kills(
            &mut runner.core.inner.lock().unwrap(),
            instant + Duration::from_secs(120),
        )
        .unwrap();
}

#[tokio::test]
async fn cancel_recorded_after_unit_exit_finalizes_without_signalling() {
    let f = Fixture::new();
    let (runner, commands) = systemd_runner(
        &f,
        "LoadState=loaded\nActiveState=inactive\nSubState=dead\n",
    );
    let id = cancelled_unit(&f, &runner);
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&id)
        .unwrap()
        .kill = None;
    runner.cancel(id).unwrap();
    assert_eq!(terminal(&runner, id).await.outcome, Outcome::OutcomeUnknown);
    assert!(commands
        .lock()
        .unwrap()
        .iter()
        .all(|args| args[0] == "show"));
}

#[tokio::test]
async fn pipe_output_failure_drains_verbose_child_and_releases_workspace() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    config.before_job_write = Some(Arc::new(|path| {
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("chunk-")
        {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        } else {
            Ok(())
        }
    }));
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("head -c 2097152 /dev/zero; printf completed > pipe-completed");
    f.submit(&runner, &request, &bytes);
    let result = terminal(&runner, request.job_id).await;
    assert_eq!(result.outcome, Outcome::OutcomeUnknown);
    assert!(Path::new(&f.key).join("pipe-completed").exists());
    let inner = runner.core.inner.lock().unwrap();
    assert!(!Runner::occupied(&inner, &f.key));
    assert!(inner.jobs[&request.job_id]
        .monitoring_error
        .as_ref()
        .unwrap()
        .contains("space"));
}

#[tokio::test]
async fn corrupt_job_record_is_quarantined_without_losing_identity_or_safe_history() {
    let f = Fixture::new();
    let config = f.config();
    let (request, _) = f.run("true");
    let directory = config
        .state_dir()
        .join("jobs")
        .join(request.job_id.to_string());
    fs::create_dir_all(&directory).unwrap();
    let mut corrupt =
        serde_json::to_value(Job::new(request.job_id, Some(request.clone()), 1, 1, 2)).unwrap();
    corrupt["state"] = serde_json::json!("damaged_state");
    let original = serde_json::to_vec(&corrupt).unwrap();
    fs::write(directory.join("record.json"), &original).unwrap();
    fs::create_dir_all(Path::new(&f.key)).unwrap();
    fs::write(Path::new(&f.key).join("owned"), b"possibly live").unwrap();
    let mut catalog = Catalog::default();
    catalog.workspaces.insert(
        f.key.clone(),
        Workspace {
            used_ms: 0,
            ..Workspace::default()
        },
    );
    atomic_json(&config.state_dir().join("catalog.json"), &catalog).unwrap();
    let safe_id = Uuid::now_v7();
    let mut safe = Job::new(safe_id, None, f.clock.load(Ordering::SeqCst), 2, 0);
    safe.terminal(Outcome::Exit { code: 0 }, f.clock.load(Ordering::SeqCst));
    atomic_json(
        &config
            .state_dir()
            .join("jobs")
            .join(safe_id.to_string())
            .join("record.json"),
        &safe,
    )
    .unwrap();

    let runner = Runner::open(config.clone())
        .expect("one damaged record must not prevent inspection of healthy jobs");
    assert_eq!(
        runner.job_status(request.job_id).unwrap().state,
        "quarantined"
    );
    assert!(
        matches!(runner.replay(safe_id, 0).unwrap().last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::Exit { code: 0 })
    );
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Duplicate
    ));
    for result in [
        runner.cancel(request.job_id),
        runner.refuse(
            request.job_id,
            RefusalReason::TransferInterrupted,
            "late upload",
        ),
    ] {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
    assert_eq!(
        runner.replay(request.job_id, 0).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        runner.withdraw(request.job_id).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let mut later = request.clone();
    later.job_id = Uuid::now_v7();
    later.workspace_key = f
        .scratch
        .join("remote/worktrees/unrelated")
        .to_string_lossy()
        .into();
    later.cwd = later.workspace_key.clone();
    assert!(matches!(
        runner.accept(later).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    assert!(runner.allow_prepare().is_err());
    assert!(runner.drop_workspace(&f.key).await.is_err());
    runner.maintenance().unwrap();
    assert!(Path::new(&f.key).join("owned").exists());
    assert_eq!(fs::read(directory.join("record.json")).unwrap(), original);
    let marker: serde_json::Value = read_json(
        &config
            .state_dir()
            .join("quarantine")
            .join(format!("{}.json", request.job_id)),
    )
    .unwrap();
    assert_eq!(marker["request"], serde_json::to_value(&request).unwrap());
    // Keep the unreadable original: an older runner must still refuse startup,
    // rather than ignoring the sidecar and releasing possibly-live ownership.
    assert!(
        serde_json::from_reader::<_, Job>(File::open(directory.join("record.json")).unwrap())
            .is_err()
    );
    drop(runner);
    let mut reopened = None;
    assert!(poll(Duration::from_secs(5), || {
        match Runner::open(config.clone()) {
            Ok(runner) => {
                reopened = Some(runner);
                true
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("quarantine restart failed: {error}"),
        }
    }));
    let reopened = reopened.unwrap();
    assert_eq!(
        reopened.job_status(request.job_id).unwrap().state,
        "quarantined"
    );
    assert_eq!(
        read_json::<serde_json::Value>(
            &config
                .state_dir()
                .join("quarantine")
                .join(format!("{}.json", request.job_id))
        )
        .unwrap(),
        marker
    );
    assert!(Path::new(&f.key).join("owned").exists());
}

#[test]
fn unreadable_job_ownership_fences_cleanup_and_admission() {
    let f = Fixture::new();
    let mut config = f.config();
    let id = Uuid::now_v7();
    let directory = config.state_dir().join("jobs").join(id.to_string());
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("record.json"), b"{\"request\":").unwrap();
    use std::os::unix::fs::PermissionsExt;
    // A durable fence must not require writes into the damaged job directory.
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();
    let scratch = config.writer_scratch_dir();
    fs::create_dir_all(&scratch).unwrap();
    fs::write(scratch.join("owned"), b"possibly live writer").unwrap();
    let spools = config.state_dir().join("spools");
    fs::create_dir_all(&spools).unwrap();
    fs::write(spools.join("owned"), b"retained upload").unwrap();
    let garbage = config.state_dir().join("trash/unknown-owner");
    fs::create_dir_all(&garbage).unwrap();
    fs::write(garbage.join("owned"), b"unverified garbage").unwrap();
    config.free_space_floor_bytes = 100;
    config.free_space_probe = Arc::new(|_| Ok(0));
    let runner = Runner::open(config).expect("unreadable ownership must be inspectable but fenced");
    assert_eq!(runner.job_status(id).unwrap().state, "quarantined");
    assert!(runner.ensure_disk_space().is_err());
    let mirror = runner.core.trees.mirror(&f.root);
    fs::create_dir_all(&mirror).unwrap();
    git(&mirror, &["init", "--bare"]);
    git(&mirror, &["fetch", text(&f.main), "HEAD"]);
    let tip = git(&mirror, &["rev-parse", "FETCH_HEAD"]);
    let reference = format!("refs/snapshots/{tip}");
    git(&mirror, &["update-ref", &reference, &tip]);
    runner
        .prune_refs(&runner.core.inner.lock().unwrap())
        .unwrap();
    assert_eq!(git(&mirror, &["rev-parse", &reference]), tip);
    assert!(scratch.join("owned").exists());
    assert!(spools.join("owned").exists());
    assert!(garbage.join("owned").exists());
    let marker: serde_json::Value = read_json(
        &runner
            .config()
            .state_dir()
            .join("quarantine")
            .join(format!("{id}.json")),
    )
    .unwrap();
    assert!(marker["request"].is_null());
    let (request, _) = f.run("true");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
}

#[test]
fn failed_expired_job_deletion_does_not_abort_later_maintenance() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let now = runner.now();
    let bad = Uuid::from_u128(1);
    let good = Uuid::from_u128(2);
    for id in [bad, good] {
        let mut job = Job::new(id, None, now, 0, 0);
        job.terminal(Outcome::Exit { code: 0 }, now);
        runner.save(&job).unwrap();
        runner.core.inner.lock().unwrap().jobs.insert(id, job);
    }
    let bad_directory = runner.record_path(bad).parent().unwrap().to_owned();
    fs::remove_dir_all(&bad_directory).unwrap();
    fs::write(&bad_directory, b"cannot remove a file as a directory").unwrap();
    fs::create_dir_all(Path::new(&f.key)).unwrap();
    runner.core.inner.lock().unwrap().catalog.workspaces.insert(
        f.key.clone(),
        Workspace {
            used_ms: now,
            ..Workspace::default()
        },
    );
    f.clock.store(now + RETENTION_MS + 1, Ordering::SeqCst);
    runner
        .maintenance()
        .expect("one failed expiry must not abort the pass");
    assert!(runner.core.inner.lock().unwrap().jobs.contains_key(&bad));
    assert!(!runner.core.inner.lock().unwrap().jobs.contains_key(&good));
    assert!(!runner.record_path(good).parent().unwrap().exists());
    assert!(!Path::new(&f.key).exists());
    fs::remove_file(&bad_directory).unwrap();
    fs::create_dir_all(&bad_directory).unwrap();
    runner.maintenance().unwrap();
    assert!(!runner.core.inner.lock().unwrap().jobs.contains_key(&bad));
}

#[test]
fn corrupt_catalog_fails_closed_without_replacing_ownership() {
    let f = Fixture::new();
    let config = f.config();
    fs::create_dir_all(config.state_dir()).unwrap();
    let path = config.state_dir().join("catalog.json");
    fs::write(&path, b"{broken catalog}").unwrap();
    assert!(Runner::open(config).is_err());
    assert_eq!(fs::read(path).unwrap(), b"{broken catalog}");
}

#[test]
fn missing_job_record_retains_a_durable_fence_across_repeated_startup() {
    let f = Fixture::new();
    let config = f.config();
    let (request, _) = f.run("true");
    let directory = config
        .state_dir()
        .join("jobs")
        .join(request.job_id.to_string());
    fs::create_dir_all(&directory).unwrap();
    let runner = Runner::open(config.clone()).unwrap();
    assert_eq!(
        runner.job_status(request.job_id).unwrap().state,
        "quarantined"
    );
    let marker_path = config
        .state_dir()
        .join("quarantine")
        .join(format!("{}.json", request.job_id));
    let marker = fs::read(&marker_path).unwrap();
    let placeholder = fs::read(directory.join("record.json")).unwrap();
    assert!(serde_json::from_slice::<Job>(&placeholder).is_err());
    drop(runner);
    fs::remove_dir_all(&directory).unwrap();
    let mut reopened = None;
    assert!(poll(Duration::from_secs(5), || {
        match Runner::open(config.clone()) {
            Ok(runner) => {
                reopened = Some(runner);
                true
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("quarantine restart failed: {error}"),
        }
    }));
    let reopened = reopened.unwrap();
    assert_eq!(
        reopened.job_status(request.job_id).unwrap().state,
        "quarantined"
    );
    assert!(matches!(
        reopened.accept(request).unwrap(),
        Admission::Duplicate
    ));
    assert_eq!(fs::read(marker_path).unwrap(), marker);
    assert_eq!(
        fs::read(directory.join("record.json")).unwrap(),
        placeholder
    );
}

#[test]
fn failed_workspace_expiry_preserves_catalog_and_continues_maintenance() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    fs::create_dir_all(&f.key).unwrap();
    let now = runner.now();
    let transfer = Uuid::now_v7();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.catalog.workspaces.insert(
            f.key.clone(),
            Workspace {
                used_ms: now,
                ..Workspace::default()
            },
        );
        inner.catalog.transferred.insert(
            transfer,
            RetainedTransfer {
                repository: f.root.clone(),
                snapshot: String::new(),
                tip: String::new(),
                siblings: vec![],
                finished_ms: now,
            },
        );
    }
    fs::write(runner.trash(), b"block workspace retirement").unwrap();
    f.clock.store(now + RETENTION_MS + 1, Ordering::SeqCst);
    runner
        .maintenance()
        .expect("a failed workspace retirement must not stop transfer expiry");
    {
        let inner = runner.core.inner.lock().unwrap();
        assert!(inner.catalog.workspaces.contains_key(&f.key));
        assert!(!inner.catalog.transferred.contains_key(&transfer));
    }
    assert!(Path::new(&f.key).exists());
    fs::remove_file(runner.trash()).unwrap();
    runner.maintenance().unwrap();
    assert!(!Path::new(&f.key).exists());
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(&f.key));
}

struct EnumeratingBackend;

impl Backend for EnumeratingBackend {
    fn live_units(&self, config: &Config) -> io::Result<Option<BTreeSet<String>>> {
        backend::live_job_units(config).map(Some)
    }
    fn materialize(&self, _: &Config, _: &Trees, _: &Setup) -> io::Result<Prepared> {
        unreachable!("these tests only admit requests")
    }
    fn launch(
        &self,
        _: &Config,
        _: &Trees,
        _: &Setup,
        _: &Prepared,
        _: u32,
    ) -> io::Result<backend::Launch> {
        unreachable!("these tests only admit requests")
    }
    fn cancel(&self, _: &Config, _: &Job, _: bool) -> io::Result<()> {
        Ok(())
    }
    fn unit_alive(
        &self,
        _: &Config,
        _: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        Ok(None)
    }
    fn collect_changes(&self, _: &Trees, _: &Job) -> io::Result<Option<Vec<String>>> {
        Ok(None)
    }
    fn reconcile(&self, _: &Config, _: &[Job]) -> io::Result<Vec<Uuid>> {
        Ok(vec![])
    }
}

fn enumerated_quarantine(
    f: &Fixture,
    listing: &str,
    failed: bool,
) -> (Runner, Uuid, Arc<Mutex<(String, bool)>>) {
    use std::os::unix::process::ExitStatusExt;
    let mut config = f.config();
    let id = Uuid::now_v7();
    let record = config
        .state_dir()
        .join("jobs")
        .join(id.to_string())
        .join("record.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(record, b"{broken job}").unwrap();
    let state = Arc::new(Mutex::new((listing.to_string(), failed)));
    config.systemd_command = Some({
        let state = state.clone();
        Arc::new(move |args| {
            assert_eq!(args[0], "list-units");
            assert!(args.contains(&"--all"));
            assert_eq!(args.last(), Some(&"motor-job-*"));
            let state = state.lock().unwrap();
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(if state.1 { 1 << 8 } else { 0 }),
                stdout: state.0.as_bytes().to_vec(),
                stderr: if state.1 {
                    b"enumeration unavailable".to_vec()
                } else {
                    vec![]
                },
            })
        })
    });
    (
        Runner::open_with_backend(config, Box::new(EnumeratingBackend)).unwrap(),
        id,
        state,
    )
}

#[test]
fn quarantine_without_unaccounted_units_allows_unrelated_admission() {
    let f = Fixture::new();
    let (runner, quarantined, _) = enumerated_quarantine(&f, "", false);
    let (mut request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    runner.cancel(request.job_id).unwrap();
    request.job_id = quarantined;
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Duplicate
    ));
    assert!(runner.allow_prepare().is_ok());
    assert_eq!(runner.job_status(quarantined).unwrap().state, "quarantined");
    assert!(runner.ensure_reconciled().is_err());
}

#[test]
fn quarantine_unaccounted_unit_fences_until_it_exits() {
    let f = Fixture::new();
    let launch = Uuid::now_v7();
    let name = backend::host_unit_name(launch);
    let (runner, quarantined, state) = enumerated_quarantine(
        &f,
        &format!("{name} loaded activating start waiting\n"),
        false,
    );
    assert_ne!(quarantined, launch);
    assert!(runner
        .ensure_admission_ownership()
        .unwrap_err()
        .to_string()
        .contains(&name));
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    state.lock().unwrap().0 = format!("{name} loaded inactive dead exited\n");
    runner.refresh_quarantine_admission(true);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    runner.cancel(request.job_id).unwrap();
    assert_eq!(runner.job_status(quarantined).unwrap().state, "quarantined");
}

#[test]
fn quarantine_matches_readable_process_launch_units_not_job_ids() {
    let f = Fixture::new();
    let (runner, _, state) = enumerated_quarantine(&f, "", false);
    let job_id = Uuid::now_v7();
    let launch = Uuid::now_v7();
    let mut job = Job::new(job_id, None, runner.now(), 1, 1);
    job.state = JobState::Running;
    job.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(launch),
    });
    runner.core.inner.lock().unwrap().jobs.insert(job_id, job);
    state.lock().unwrap().0 = format!(
        "{} loaded active running command\n",
        backend::host_unit_name(launch)
    );
    runner.refresh_quarantine_admission(true);
    assert!(runner.ensure_admission_ownership().is_ok());
    let wrong = backend::host_unit_name(job_id);
    state.lock().unwrap().0 = format!("{wrong} loaded active running command\n");
    runner.refresh_quarantine_admission(true);
    assert!(runner
        .ensure_admission_ownership()
        .unwrap_err()
        .to_string()
        .contains(&wrong));
    runner.core.inner.lock().unwrap().jobs.remove(&job_id);
}

#[test]
fn quarantine_failed_or_malformed_enumeration_cannot_release_the_fence() {
    for (listing, failed) in [
        ("", true),
        ("motor-job-broken.service loaded\n", false),
        ("unrelated.service loaded active running\n", false),
    ] {
        let f = Fixture::new();
        let (runner, _, _) = enumerated_quarantine(&f, listing, failed);
        assert!(runner
            .ensure_admission_ownership()
            .unwrap_err()
            .to_string()
            .contains("enumeration failed"));
        let (request, _) = f.run("true");
        assert!(matches!(
            runner.accept(request).unwrap(),
            Admission::Refused(RefusalReason::WorkspaceSetupFailed)
        ));
    }
}

#[test]
fn portable_host_backend_cannot_prove_quarantined_execution_is_idle() {
    let f = Fixture::new();
    assert!(backend::HostBackend
        .live_units(&f.config())
        .unwrap()
        .is_none());
}
