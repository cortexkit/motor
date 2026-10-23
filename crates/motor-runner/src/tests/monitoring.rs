use super::*;

#[test]
fn restart_discards_unpublished_records_but_quarantines_missing_published_records() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    config.disable_warm_builds = true;
    let (request, _) = f.run("true");
    let state = config.state_dir();
    let staging = state.join("job-staging").join(request.job_id.to_string());
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("record.tmp"), b"half a record").unwrap();
    let published = Uuid::now_v7();
    fs::create_dir_all(state.join("jobs").join(published.to_string())).unwrap();
    let runner = Runner::open(config).unwrap();
    assert!(!state.join("job-staging").exists());
    assert!(!runner.core.quarantined.contains_key(&request.job_id));
    assert!(runner.core.quarantined.contains_key(&published));
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .contains_key(&request.job_id));
}

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
    assert_eq!(disk.terminal.unwrap().outcome, Outcome::Exit { code: 101 });
    assert_eq!(disk.monitoring_error, None);
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
        runner.enforce_kills(std::time::Instant::now()).unwrap();
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
        for ms in 0..60_000 {
            runner
                .enforce_kills(instant + Duration::from_millis(ms))
                .unwrap();
        }
        let inner = runner.core.inner.lock().unwrap();
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
        .enforce_kills(instant + Duration::from_secs(120))
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
    let reopened = reopen_with_message(|| Runner::open(config.clone()), "quarantine restart");
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
    runner.prune_refs().unwrap();
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
    let reopened = reopen_with_message(|| Runner::open(config.clone()), "quarantine restart");
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
    fn failed_job_units(&self, _: &Config) -> io::Result<u64> {
        Ok(0)
    }
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

struct EnumeratedUnits {
    listing: String,
    failed: bool,
    pause: Option<Arc<IoPause>>,
}

fn enumerated_quarantine(
    f: &Fixture,
    listing: &str,
    failed: bool,
) -> (Runner, Uuid, Arc<Mutex<EnumeratedUnits>>) {
    enumerated_quarantine_with(f, listing, failed, |_, _| {})
}

fn enumerated_quarantine_with(
    f: &Fixture,
    listing: &str,
    failed: bool,
    configure: impl FnOnce(&mut Config, Uuid),
) -> (Runner, Uuid, Arc<Mutex<EnumeratedUnits>>) {
    use std::os::unix::process::ExitStatusExt;
    let mut config = f.config();
    // A forced refresh bypasses throttling, not a busy probe. These fixtures
    // control each observation, so only explicit ticks may refresh the fence.
    config.manual_tick = true;
    let id = Uuid::now_v7();
    let record = config
        .state_dir()
        .join("jobs")
        .join(id.to_string())
        .join("record.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(record, b"{broken job}").unwrap();
    configure(&mut config, id);
    let state = Arc::new(Mutex::new(EnumeratedUnits {
        listing: listing.to_string(),
        failed,
        pause: None,
    }));
    config.systemd_command = Some({
        let state = state.clone();
        Arc::new(move |args| {
            assert_eq!(args[0], "list-units");
            assert!(args.contains(&"--all"));
            assert_eq!(args.last(), Some(&"motor-job-*"));
            let (listing, failed, pause) = {
                let mut state = state.lock().unwrap();
                (state.listing.clone(), state.failed, state.pause.take())
            };
            if let Some(pause) = pause {
                pause.block();
            }
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(if failed { 1 << 8 } else { 0 }),
                stdout: listing.into_bytes(),
                stderr: if failed {
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

fn quarantine_disk_fixture(
    f: &Fixture,
    owner: &str,
) -> (Runner, Uuid, Arc<Mutex<EnumeratedUnits>>, PathBuf) {
    let trash = f.config().state_dir().join("trash/evict-me");
    let reclaim = trash.clone();
    let (runner, id, state) = enumerated_quarantine_with(f, "", false, |config, id| {
        config.disable_warm_builds = true;
        config.free_space_floor_bytes = 100;
        config.free_space_probe = Arc::new(move |_| Ok(if reclaim.exists() { 10 } else { 200 }));
        let (mut request, _) = f.run("true");
        request.job_id = id;
        let marker = QuarantinedJob {
            id,
            request: (owner == "request").then_some(request),
            process: (owner == "process").then_some(platform::ProcessIdentity {
                pid: 1,
                start: 1,
                unit: Some(Uuid::now_v7()),
            }),
            observed_ms: 1,
        };
        atomic_json_private(
            &config
                .state_dir()
                .join("quarantine")
                .join(format!("{id}.json")),
            &marker,
        )
        .unwrap();
        atomic_json_private(
            &config
                .state_dir()
                .join("jobs")
                .join(id.to_string())
                .join("record.json"),
            &serde_json::json!({"quarantined_job": id}),
        )
        .unwrap();
    });
    fs::create_dir_all(&trash).unwrap();
    fs::write(trash.join("discardable"), b"cache garbage").unwrap();
    (runner, id, state, trash)
}

#[test]
fn empty_placeholder_quarantine_does_not_turn_low_disk_into_permanent_full_disk() {
    let f = Fixture::new();
    let (runner, id, _, trash) = quarantine_disk_fixture(&f, "none");
    assert_eq!(
        runner
            .runner_status()
            .health
            .unwrap()
            .cleanup_blocked_by_quarantine,
        vec![id]
    );
    let logs = QuarantineLogs::default();
    tracing::subscriber::with_default(logs.clone(), || {
        assert!(runner.ensure_disk_space().unwrap().is_none());
    });
    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        output
            .matches("disk cleanup proceeding past ownerless quarantine")
            .count(),
        1,
        "{output}"
    );
    assert!(
        output.contains("WARN") && output.contains(&id.to_string()),
        "{output}"
    );
    assert!(
        !trash.exists(),
        "an ownerless placeholder must not block eviction"
    );
    let health = runner.runner_status().health.unwrap();
    assert!(health.cleanup_blocked_by_quarantine.is_empty());
    let mut legacy = serde_json::to_value(&health).unwrap();
    assert!(legacy.get("cleanup_blocked_by_quarantine").is_none());
    legacy
        .as_object_mut()
        .unwrap()
        .remove("cleanup_blocked_by_quarantine");
    assert!(serde_json::from_value::<RunnerHealth>(legacy)
        .unwrap()
        .cleanup_blocked_by_quarantine
        .is_empty());
    assert!(runner
        .config()
        .state_dir()
        .join("quarantine")
        .join(format!("{id}.json"))
        .exists());
    assert_eq!(runner.job_status(id).unwrap().state, "quarantined");
    assert!(runner.replay(id, 0).is_err());
    assert!(runner.ensure_reconciled().is_err());
    let (mut request, _) = f.run("true");
    request.job_id = id;
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Duplicate
    ));
}

#[test]
fn recovered_quarantine_owner_keeps_disk_cleanup_and_health_fenced() {
    for owner in ["request", "process"] {
        let f = Fixture::new();
        let (runner, id, _, trash) = quarantine_disk_fixture(&f, owner);
        assert!(runner.ensure_disk_space().unwrap().is_some());
        assert!(trash.exists());
        let health = serde_json::to_value(runner.runner_status().health.unwrap()).unwrap();
        assert_eq!(
            health["cleanup_blocked_by_quarantine"],
            serde_json::json!([id])
        );
    }
}

#[test]
fn ownerless_quarantine_requires_fresh_complete_idle_inventory_and_no_sandbox() {
    for blocker in [
        "failed",
        "malformed",
        "unknown",
        "busy",
        "launching",
        "sandbox",
        "sandbox-error",
    ] {
        let f = Fixture::new();
        let (runner, id, state, trash) = quarantine_disk_fixture(&f, "none");
        match blocker {
            "failed" => state.lock().unwrap().failed = true,
            "malformed" => {
                state.lock().unwrap().listing = "motor-job-broken.service loaded\n".into()
            }
            "unknown" => {
                state.lock().unwrap().listing = format!(
                    "{} loaded active running command\n",
                    backend::host_unit_name(Uuid::now_v7())
                )
            }
            "launching" => {
                runner
                    .core
                    .inner
                    .lock()
                    .unwrap()
                    .launching
                    .insert(Uuid::now_v7());
            }
            "sandbox" => {
                fs::create_dir_all(
                    runner
                        .config()
                        .state_dir()
                        .join("sandboxes")
                        .join(id.to_string()),
                )
                .unwrap();
            }
            "sandbox-error" => {
                let path = runner.config().state_dir().join("sandboxes");
                if path.exists() {
                    fs::remove_dir_all(&path).unwrap();
                }
                fs::write(path, b"not a directory").unwrap();
            }
            _ => {}
        }
        let busy = (blocker == "busy").then(|| runner.core.quarantine_probe.lock().unwrap());
        assert!(runner.ensure_disk_space().unwrap().is_some(), "{blocker}");
        assert!(trash.exists(), "{blocker}");
        assert_eq!(
            runner
                .runner_status()
                .health
                .unwrap()
                .cleanup_blocked_by_quarantine,
            vec![id]
        );
        drop(busy);
        runner.core.inner.lock().unwrap().launching.clear();
    }
}

#[derive(Clone, Default)]
struct QuarantineLogs(Arc<Mutex<Vec<u8>>>);

impl tracing::Subscriber for QuarantineLogs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut bytes = self.0.lock().unwrap();
        write!(bytes, "{} ", event.metadata().level()).unwrap();
        event.record(&mut QuarantineFields(&mut bytes));
        writeln!(bytes).unwrap();
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

struct QuarantineFields<'a>(&'a mut Vec<u8>);
impl tracing::field::Visit for QuarantineFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        write!(self.0, "{}={value:?} ", field.name()).unwrap();
    }
}

#[test]
fn quarantine_cleanup_errors_name_ids_and_are_limited_to_once_per_five_minutes() {
    let f = Fixture::new();
    let (runner, id, _, trash) = quarantine_disk_fixture(&f, "request");
    let logs = QuarantineLogs::default();
    tracing::subscriber::with_default(logs.clone(), || {
        fs::remove_dir_all(&trash).unwrap();
        assert!(runner.ensure_disk_space().unwrap().is_none());
        assert!(
            logs.0.lock().unwrap().is_empty(),
            "no eviction was requested"
        );
        fs::create_dir_all(&trash).unwrap();
        for advance in [0, 0, 299_999, 1] {
            f.clock.fetch_add(advance, Ordering::SeqCst);
            assert!(runner.ensure_disk_space().unwrap().is_some());
        }
    });
    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        output
            .matches("runner disk cleanup blocked by quarantine")
            .count(),
        2,
        "{output}"
    );
    assert_eq!(output.matches(&id.to_string()).count(), 2, "{output}");
    assert_eq!(output.matches("ERROR").count(), 2, "{output}");
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

/// A job being launched has a live unit before its record names it. While
/// such a launch is in flight, an unmatched unit may be its own, so it does
/// not close the admission fence; once no launch is in flight, it does.
#[test]
fn quarantine_does_not_fence_on_the_unit_of_a_job_being_launched() {
    let f = Fixture::new();
    let (runner, _, state) = enumerated_quarantine(&f, "", false);
    assert!(runner.ensure_admission_ownership().is_ok());
    let launching = Uuid::now_v7();
    let name = backend::host_unit_name(Uuid::now_v7());
    state.lock().unwrap().listing = format!("{name} loaded activating start waiting\n");
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .launching
        .insert(launching);
    runner.refresh_quarantine_admission(true);
    assert!(
        runner.ensure_admission_ownership().is_ok(),
        "a launching job's unit closed the admission fence"
    );
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .launching
        .remove(&launching);
    runner.refresh_quarantine_admission(true);
    assert!(runner
        .ensure_admission_ownership()
        .unwrap_err()
        .to_string()
        .contains(&name));
}

/// The deferral counts: with more unmatched units than launches in flight,
/// at least one unit belongs to no launch, and the fence closes.
#[test]
fn quarantine_fences_on_more_unmatched_units_than_launches() {
    let f = Fixture::new();
    let (runner, _, state) = enumerated_quarantine(&f, "", false);
    let first = backend::host_unit_name(Uuid::now_v7());
    let second = backend::host_unit_name(Uuid::now_v7());
    state.lock().unwrap().listing = format!(
        "{first} loaded activating start waiting\n{second} loaded activating start waiting\n"
    );
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .launching
        .insert(Uuid::now_v7());
    runner.refresh_quarantine_admission(true);
    assert!(
        runner.ensure_admission_ownership().is_err(),
        "two unmatched units were taken for one launch"
    );
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
    state.lock().unwrap().listing = format!("{name} loaded inactive dead exited\n");
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
    state.lock().unwrap().listing = format!(
        "{} loaded active running command\n",
        backend::host_unit_name(launch)
    );
    runner.refresh_quarantine_admission(true);
    assert!(runner.ensure_admission_ownership().is_ok());
    let wrong = backend::host_unit_name(job_id);
    state.lock().unwrap().listing = format!("{wrong} loaded active running command\n");
    runner.refresh_quarantine_admission(true);
    assert!(runner
        .ensure_admission_ownership()
        .unwrap_err()
        .to_string()
        .contains(&wrong));
    runner.core.inner.lock().unwrap().jobs.remove(&job_id);
}

#[test]
fn quarantine_refresh_serializes_enumeration_and_publication() {
    let f = Fixture::new();
    let (runner, _, state) = enumerated_quarantine(&f, "", false);
    let (pause, entered) = IoPause::new();
    let release = ReleaseIo(pause.clone());
    state.lock().unwrap().pause = Some(pause);
    *runner.core.quarantine_probe.lock().unwrap() = None;
    let older = std::thread::spawn({
        let runner = runner.clone();
        move || runner.tick()
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(runner.core.quarantine_probe.try_lock().is_err());
    let unknown = backend::host_unit_name(Uuid::now_v7());
    state.lock().unwrap().listing = format!("{unknown} loaded active running command\n");

    // The older enumeration owns the probe through publication. A concurrent
    // forced call skips it rather than observing the changed listing out of order.
    runner.refresh_quarantine_admission(true);
    assert!(runner.ensure_admission_ownership().is_ok());
    drop(release);
    older.join().unwrap().unwrap();
    assert!(runner.ensure_admission_ownership().is_ok());

    runner.refresh_quarantine_admission(true);
    assert!(runner
        .ensure_admission_ownership()
        .unwrap_err()
        .to_string()
        .contains(&unknown));
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

pub(super) struct IoPause {
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    released: Mutex<bool>,
    ready: std::sync::Condvar,
}

impl IoPause {
    pub(super) fn new() -> (Arc<Self>, std::sync::mpsc::Receiver<()>) {
        let (send, recv) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                entered: Mutex::new(Some(send)),
                released: Mutex::new(false),
                ready: std::sync::Condvar::new(),
            }),
            recv,
        )
    }
    pub(super) fn block(&self) {
        if let Some(send) = self.entered.lock().unwrap().take() {
            send.send(()).unwrap();
        }
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.ready.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.ready.notify_all();
    }
}

pub(super) struct ReleaseIo(pub(super) Arc<IoPause>);
impl Drop for ReleaseIo {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn responsive_controls(
    runner: &Runner,
    request: Run,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<bool>) {
    let runner = runner.clone();
    let (send, recv) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = runner.status();
        let admitted = matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Accepted(_)
        );
        runner.cancel(request.job_id).unwrap();
        send.send(admitted).unwrap();
    });
    (thread, recv)
}

#[test]
fn slow_output_persistence_leaves_unrelated_control_operations_responsive() {
    for index_write in [false, true] {
        let f = Fixture::new();
        let (pause, entered) = IoPause::new();
        let release = ReleaseIo(pause.clone());
        let armed = Arc::new(AtomicBool::new(false));
        let mut config = f.config();
        config.before_job_write = Some({
            let armed = armed.clone();
            Arc::new(move |path| {
                let filename = path.file_name().unwrap().to_string_lossy();
                let selected = if index_write {
                    filename == "record.json"
                } else {
                    filename.starts_with("chunk-")
                };
                if selected && armed.swap(false, Ordering::SeqCst) {
                    pause.block();
                }
                Ok(())
            })
        });
        let runner = Runner::open(config).unwrap();
        let id = Uuid::now_v7();
        runner.cancel(id).unwrap();
        armed.store(true, Ordering::SeqCst);
        let writer = std::thread::spawn({
            let runner = runner.clone();
            move || runner.output(id, OutputStream::Stdout, b"durable output")
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (request, _) = f.run("true");
        let (control, reply) = responsive_controls(&runner, request);
        let responsive = reply.recv_timeout(Duration::from_secs(1));
        drop(release);
        writer.join().unwrap().unwrap();
        control.join().unwrap();
        assert_eq!(
            responsive.ok(),
            Some(true),
            "output storage stalled unrelated controls (index_write={index_write})"
        );
        assert!(
            matches!(runner.replay(id, 0).unwrap().first(), Some(Frame::Output(o)) if o.bytes.0 == b"durable output")
        );
    }
}

#[test]
fn slow_replay_reads_leave_unrelated_control_operations_responsive() {
    let f = Fixture::new();
    let (pause, entered) = IoPause::new();
    let release = ReleaseIo(pause.clone());
    let mut config = f.config();
    config.before_replay_read = Some(Arc::new(move |_| {
        pause.block();
        Ok(())
    }));
    let runner = Runner::open(config).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    runner
        .output(id, OutputStream::Stdout, b"replayed output")
        .unwrap();
    let reader = std::thread::spawn({
        let runner = runner.clone();
        move || runner.replay(id, 0)
    });
    entered.recv().unwrap();
    let (request, _) = f.run("true");
    let (control, reply) = responsive_controls(&runner, request);
    // Complete every control before releasing storage. Host scheduling delay
    // is not evidence that the controls waited for the replay read.
    assert!(
        reply.recv().unwrap(),
        "replay storage stalled unrelated controls"
    );
    assert!(!reader.is_finished(), "replay must still be held");
    control.join().unwrap();
    drop(release);
    let frames = reader.join().unwrap().unwrap();
    assert!(matches!(frames.first(), Some(Frame::Output(o)) if o.bytes.0 == b"replayed output"));
}

fn synthetic_running(f: &Fixture, runner: &Runner) -> Uuid {
    let (request, _) = f.run("true");
    let mut job = Job::new(request.job_id, Some(request), runner.now(), 1, 2);
    job.state = JobState::Running;
    job.started_ms = Some(runner.now());
    runner.save(&job).unwrap();
    let id = job.id;
    runner.core.inner.lock().unwrap().jobs.insert(id, job);
    id
}

#[test]
fn replay_pages_keep_inclusive_contiguous_cursors_and_defer_terminal() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    for sequence in 0..70u8 {
        runner
            .output(id, OutputStream::Stdout, &[sequence; 16384])
            .unwrap();
    }
    let first = runner.replay(id, 0).unwrap();
    assert_eq!(first.len(), 64);
    assert!(!first
        .iter()
        .any(|frame| matches!(frame, Frame::Terminal(_))));
    for (sequence, frame) in first.iter().enumerate() {
        assert!(
            matches!(frame, Frame::Output(output) if output.seq == sequence as u64 && output.bytes.0 == [sequence as u8; 16384])
        );
    }
    let last = runner.replay(id, 64).unwrap();
    assert_eq!(last.len(), 7);
    assert!(matches!(last.first(), Some(Frame::Output(output)) if output.seq == 64));
    assert!(matches!(last.last(), Some(Frame::Terminal(_))));
    assert!(
        matches!(runner.replay(id, 7).unwrap().first(), Some(Frame::Output(output)) if output.seq == 7)
    );
}

#[test]
fn replay_leases_pin_only_the_page_during_eviction_and_keep_terminal_snapshot() {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new();
    let (pause, entered) = IoPause::new();
    let release = ReleaseIo(pause.clone());
    let mut config = f.config();
    config.before_replay_read = Some(Arc::new(move |_| {
        pause.block();
        Ok(())
    }));
    let runner = Runner::open(config).unwrap();
    let id = synthetic_running(&f, &runner);
    let megabyte = vec![b'x'; 1024 * 1024];
    runner.output(id, OutputStream::Stdout, &megabyte).unwrap();
    let reader = std::thread::spawn({
        let runner = runner.clone();
        move || runner.replay(id, 0)
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..128 {
        runner.output(id, OutputStream::Stdout, &megabyte).unwrap();
    }
    runner
        .finish(id, Ok(std::process::ExitStatus::from_raw(0)))
        .unwrap();
    let directory = runner.record_path(id).parent().unwrap().to_owned();
    assert!(directory.join("chunk-0").exists());
    let disk: u64 = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("chunk-"))
        .map(|entry| entry.metadata().unwrap().len())
        .sum();
    assert_eq!(
        disk,
        65 * 1024 * 1024,
        "only this page, not all evicted history, remains leased"
    );
    drop(release);
    let frames = reader.join().unwrap().unwrap();
    assert_eq!(
        frames.len(),
        1,
        "the pre-terminal snapshot must not acquire a later terminal"
    );
    assert!(
        matches!(frames.first(), Some(Frame::Output(output)) if output.seq == 0 && output.bytes.0 == megabyte)
    );
    assert!(!directory.join("chunk-0").exists());
    let latest = runner.replay(id, 0).unwrap();
    assert!(
        matches!(latest.first(), Some(Frame::Output(output)) if output.seq == 65 && output.truncated_before_seq == Some(65))
    );
}

#[test]
fn replay_lease_defers_expiry_until_page_read_finishes() {
    let f = Fixture::new();
    let (pause, entered) = IoPause::new();
    let release = ReleaseIo(pause.clone());
    let mut config = f.config();
    config.before_replay_read = Some(Arc::new(move |_| {
        pause.block();
        Ok(())
    }));
    let runner = Runner::open(config).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    runner
        .output(id, OutputStream::Stdout, b"retained")
        .unwrap();
    let reader = std::thread::spawn({
        let runner = runner.clone();
        move || runner.replay(id, 0)
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    f.clock
        .store(runner.now() + RETENTION_MS + 1000, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(runner.record_path(id).exists());
    drop(release);
    let frames = reader.join().unwrap().unwrap();
    assert!(matches!(frames.first(), Some(Frame::Output(output)) if output.bytes.0 == b"retained"));
    runner.maintenance().unwrap();
    assert!(!runner.record_path(id).exists());
}

#[test]
fn unknown_id_publication_fences_admission_without_holding_state_lock() {
    let f = Fixture::new();
    let id = Uuid::now_v7();
    let (pause, entered) = IoPause::new();
    let release = ReleaseIo(pause.clone());
    let mut config = f.config();
    config.before_job_write = Some(Arc::new(move |path| {
        if path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            == id.to_string()
        {
            pause.block();
        }
        Ok(())
    }));
    let runner = Runner::open(config.clone()).unwrap();
    let writer = std::thread::spawn({
        let runner = runner.clone();
        move || runner.replay(id, 0)
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let (mut late, _) = f.run("true");
    late.job_id = id;
    let (other, _) = f.run("true");
    let (send, reply) = std::sync::mpsc::channel();
    let control = std::thread::spawn({
        let runner = runner.clone();
        let late = late.clone();
        move || {
            let duplicate = matches!(runner.accept(late).unwrap(), Admission::Duplicate);
            let _ = runner.status();
            let admitted = matches!(
                runner.accept(other.clone()).unwrap(),
                Admission::Accepted(_)
            );
            runner.cancel(other.job_id).unwrap();
            send.send(duplicate && admitted).unwrap();
        }
    });
    let responsive = reply.recv_timeout(Duration::from_secs(1));
    drop(release);
    assert!(
        matches!(writer.join().unwrap().unwrap().last(), Some(Frame::Terminal(terminal)) if terminal.ran == Some(Ran::None))
    );
    control.join().unwrap();
    assert_eq!(responsive.ok(), Some(true));
    drop(runner);
    let runner = reopen(config);
    assert!(matches!(runner.accept(late).unwrap(), Admission::Duplicate));
}

#[test]
fn failed_unknown_id_publication_releases_handoff_without_a_no_start_proof() {
    let f = Fixture::new();
    let armed = Arc::new(AtomicBool::new(true));
    let mut config = f.config();
    config.before_job_write = Some(Arc::new(move |_| {
        if armed.swap(false, Ordering::SeqCst) {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        } else {
            Ok(())
        }
    }));
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    assert!(runner.replay(request.job_id, 0).is_err());
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    runner.cancel(request.job_id).unwrap();
}

#[test]
fn failed_output_index_keeps_committed_cursor_and_restart_discards_incomplete_tail() {
    let f = Fixture::new();
    let armed = Arc::new(AtomicBool::new(false));
    let mut config = f.config();
    config.before_job_write = Some({
        let armed = armed.clone();
        Arc::new(move |path| {
            if path.file_name().unwrap() == "record.json" && armed.swap(false, Ordering::SeqCst) {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        })
    });
    let runner = Runner::open(config.clone()).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    armed.store(true, Ordering::SeqCst);
    assert!(runner
        .output_at(id, OutputStream::Stdout, b"uncommitted", Some(11))
        .is_err());
    let job: Job = read_json(&runner.record_path(id)).unwrap();
    assert_eq!(job.next_seq, 0);
    assert_eq!(job.stdout_offset, 0);
    let visible_sequence = runner.core.inner.lock().unwrap().jobs[&id].next_seq;
    assert_eq!(visible_sequence, 0);
    assert!(matches!(
        runner.replay(id, 0).unwrap().as_slice(),
        [Frame::Terminal(_)]
    ));
    let directory = runner.record_path(id).parent().unwrap().to_owned();
    fs::write(directory.join("chunk-99"), b"partial tail").unwrap();
    drop(runner);
    let runner = reopen(config);
    assert!(!directory.join("chunk-0").exists());
    assert!(!directory.join("chunk-99").exists());
    runner
        .output_at(id, OutputStream::Stdout, b"committed", Some(9))
        .unwrap();
    assert!(
        matches!(runner.replay(id, 0).unwrap().first(), Some(Frame::Output(output)) if output.seq == 0 && output.bytes.0 == b"committed")
    );
    let legacy: Job = serde_json::from_reader(File::open(runner.record_path(id)).unwrap()).unwrap();
    assert_eq!(legacy.next_seq, 1);
    assert_eq!(legacy.stdout_offset, 9);
}

#[test]
fn terminal_commit_waits_for_pending_output_index_publication() {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new();
    let (pause, entered) = IoPause::new();
    let release = ReleaseIo(pause.clone());
    let mut config = f.config();
    config.before_job_write = Some(Arc::new(move |path| {
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("chunk-")
        {
            pause.block();
        }
        Ok(())
    }));
    let runner = Runner::open(config).unwrap();
    let id = synthetic_running(&f, &runner);
    let writer = std::thread::spawn({
        let runner = runner.clone();
        move || runner.output(id, OutputStream::Stdout, b"last output")
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let finisher = std::thread::spawn({
        let runner = runner.clone();
        move || runner.finish(id, Ok(std::process::ExitStatus::from_raw(0)))
    });
    assert!(poll(Duration::from_secs(5), || runner
        .core
        .inner
        .lock()
        .unwrap()
        .finalizing
        .contains(&id)));
    let terminal_visible = runner.core.inner.lock().unwrap().jobs[&id]
        .terminal
        .is_some();
    assert!(!terminal_visible);
    drop(release);
    writer.join().unwrap().unwrap();
    finisher.join().unwrap().unwrap();
    let replay = runner.replay(id, 0).unwrap();
    assert!(
        matches!(replay.as_slice(), [Frame::Output(output), Frame::Terminal(terminal)] if output.seq == 0 && output.bytes.0 == b"last output" && terminal.outcome == Outcome::Exit { code: 0 })
    );
}

#[test]
fn output_splits_large_appends_at_page_boundaries_with_committed_offsets() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    let bytes = vec![b'a'; 2 * 1024 * 1024 + 19];
    runner
        .output_at(
            id,
            OutputStream::Stdout,
            &bytes,
            Some(2 * 1024 * 1024 + 219),
        )
        .unwrap();
    for sequence in 0..2 {
        let page = runner.replay(id, sequence).unwrap();
        assert!(
            matches!(page.as_slice(), [Frame::Output(output)] if output.seq == sequence && output.bytes.0.len() == 1024 * 1024)
        );
    }
    let tail = runner.replay(id, 2).unwrap();
    assert!(
        matches!(tail.as_slice(), [Frame::Output(output), Frame::Terminal(_)] if output.seq == 2 && output.bytes.0 == [b'a'; 19])
    );
    let legacy: Job = serde_json::from_reader(File::open(runner.record_path(id)).unwrap()).unwrap();
    assert_eq!(legacy.next_seq, 3);
    assert_eq!(legacy.stdout_offset, 2 * 1024 * 1024 + 219);
    assert!(legacy
        .chunks
        .iter()
        .all(|chunk| chunk.length <= 1024 * 1024));
}

#[test]
fn replay_rejects_truncated_or_oversized_committed_chunk_files_without_leaking_leases() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    runner.output(id, OutputStream::Stdout, b"good").unwrap();
    let path = runner.record_path(id).parent().unwrap().join("chunk-0");
    for bytes in [b"bad".as_slice(), b"good-extra".as_slice()] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            runner.replay(id, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let readers = runner.job_log(id).retention.lock().unwrap().readers;
        assert_eq!(readers, 0);
    }
    fs::write(&path, b"good").unwrap();
    assert!(
        matches!(runner.replay(id, 0).unwrap().first(), Some(Frame::Output(output)) if output.bytes.0 == b"good")
    );
}

#[test]
fn slow_ref_pruning_leaves_unrelated_control_operations_responsive() {
    let f = Fixture::new();
    let (pause, entered) = IoPause::new();
    let release = ReleaseIo(pause.clone());
    let armed = Arc::new(AtomicBool::new(false));
    let mut config = f.config();
    // Disable background ticks so the runner.maintenance() call below, not the
    // scheduler, enters the one-shot pruning hold.
    config.manual_tick = true;
    config.long_work = Some({
        let armed = armed.clone();
        Arc::new(move |label| {
            if label == "prune_refs_git" && armed.swap(false, Ordering::SeqCst) {
                pause.block();
            }
        })
    });
    let runner = Runner::open(config).unwrap();
    let mirror = runner.core.trees.mirror(&f.root);
    fs::create_dir_all(&mirror).unwrap();
    git(&mirror, &["init", "--bare"]);
    git(&mirror, &["fetch", text(&f.main), "HEAD"]);
    let commit = git(&mirror, &["rev-parse", "FETCH_HEAD"]);
    git(&mirror, &["update-ref", "refs/snapshots/stale", &commit]);
    armed.store(true, Ordering::SeqCst);
    let pruner = std::thread::spawn({
        let runner = runner.clone();
        move || runner.maintenance()
    });
    entered.recv().unwrap();
    let (request, _) = f.run("true");
    let (control, reply) = responsive_controls(&runner, request);
    // The Git step remains paused until all controls complete, irrespective of
    // how long the host takes to schedule their worker.
    assert!(
        reply.recv().unwrap(),
        "ref pruning stalled unrelated controls"
    );
    assert!(!pruner.is_finished(), "ref pruning must still be held");
    control.join().unwrap();
    drop(release);
    pruner.join().unwrap().unwrap();
    assert!(git(
        &mirror,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/snapshots/stale"
        ]
    )
    .is_empty());
}

#[test]
fn ref_pruning_cannot_delete_a_pin_published_after_its_snapshot() {
    let f = Fixture::new();
    let (pause, entered) = IoPause::new();
    let release = ReleaseIo(pause.clone());
    let armed = Arc::new(AtomicBool::new(false));
    let (waiting_send, waiting) = std::sync::mpsc::channel();
    let (pin_send, pin_entered) = std::sync::mpsc::channel();
    let mut config = f.config();
    config.long_work = Some({
        let armed = armed.clone();
        Arc::new(move |label| {
            if label == "prune_refs_git" && armed.swap(false, Ordering::SeqCst) {
                pause.block();
            }
            if label == "pin_refs_wait" {
                waiting_send.send(()).unwrap();
            }
            if label == "pin_refs_git" {
                pin_send.send(()).unwrap();
            }
        })
    });
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    let spool = f.spool(&runner, &request, &bytes);
    runner
        .core
        .trees
        .fetch(
            &spool.directory,
            &request.bundles,
            &request.repository_root,
            &request.snapshot_commit,
            &request.integration_tip,
            &request.siblings,
        )
        .unwrap();
    runner
        .core
        .trees
        .pin(
            &request.repository_root,
            &request.snapshot_commit,
            &request.integration_tip,
            &request.siblings,
        )
        .unwrap();
    let prepare = Prepare {
        transfer_id: Uuid::now_v7(),
        workspace_key: request.workspace_key.clone(),
        repository_root: request.repository_root.clone(),
        base_commit: request.snapshot_commit.clone(),
        integration_tip: request.integration_tip.clone(),
        siblings: request.siblings.clone(),
        bundles: request.bundles.clone(),
        git_dependencies: vec![],
        workspace_kind: None,
    };
    armed.store(true, Ordering::SeqCst);
    let pruner = std::thread::spawn({
        let runner = runner.clone();
        move || runner.maintenance()
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let pinner = std::thread::spawn({
        let runner = runner.clone();
        let directory = spool.directory.clone();
        let prepare = prepare.clone();
        move || runner.fetch_prepare(&prepare, &directory)
    });
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let premature_pin = pin_entered.recv_timeout(Duration::from_secs(1)).is_ok();
    drop(release);
    pruner.join().unwrap().unwrap();
    pinner.join().unwrap().unwrap();
    assert!(
        !premature_pin,
        "pinning must wait outside shared state until the older prune completes"
    );
    let mirror = runner.core.trees.mirror(&request.repository_root);
    assert_eq!(
        git(
            &mirror,
            &[
                "rev-parse",
                &format!("refs/snapshots/{}", request.snapshot_commit)
            ]
        ),
        request.snapshot_commit
    );
    assert_eq!(
        git(
            &mirror,
            &[
                "rev-parse",
                &format!("refs/tips/{}", request.integration_tip)
            ]
        ),
        request.integration_tip
    );
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .transferred
        .contains_key(&prepare.transfer_id));
    runner.maintenance().unwrap();
    assert_eq!(
        git(
            &mirror,
            &[
                "rev-parse",
                &format!("refs/snapshots/{}", request.snapshot_commit)
            ]
        ),
        request.snapshot_commit
    );
}

#[test]
fn ref_pruning_batches_stale_deletions_without_dropping_held_refs() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (request, bytes) = f.run("true");
    let spool = f.spool(&runner, &request, &bytes);
    runner
        .core
        .trees
        .fetch(
            &spool.directory,
            &request.bundles,
            &request.repository_root,
            &request.snapshot_commit,
            &request.integration_tip,
            &request.siblings,
        )
        .unwrap();
    runner
        .core
        .trees
        .pin(
            &request.repository_root,
            &request.snapshot_commit,
            &request.integration_tip,
            &request.siblings,
        )
        .unwrap();
    runner.accept(request.clone()).unwrap();
    let mirror = runner.core.trees.mirror(&request.repository_root);
    for index in 0..128 {
        git(
            &mirror,
            &[
                "update-ref",
                &format!("refs/snapshots/stale-{index}"),
                &request.snapshot_commit,
            ],
        );
    }
    runner.prune_refs().unwrap();
    let remaining = git(
        &mirror,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/snapshots",
            "refs/tips",
        ],
    );
    assert_eq!(remaining.lines().count(), 2);
    assert!(remaining
        .lines()
        .any(|reference| reference == format!("refs/snapshots/{}", request.snapshot_commit)));
    assert!(remaining
        .lines()
        .any(|reference| reference == format!("refs/tips/{}", request.integration_tip)));
    runner.cancel(request.job_id).unwrap();
}

#[test]
fn boat_oom_killed_init_without_exit_is_definite_and_explained() {
    let f = Fixture::new();
    let mut config = f.config();
    config.boat_profile = true;
    config.boat_memory_max_bytes = Some(12_500_000_000);
    config.systemd_command = Some(Arc::new(|args| {
        use std::os::unix::process::ExitStatusExt;
        let text = match args {
            ["show", "--property=LoadState", "--property=ActiveState", _] => "LoadState=loaded\nActiveState=failed\n",
            ["show", "--property=ExecMainCode", "--property=ExecMainStatus", _] => "ExecMainCode=0\nExecMainStatus=0\n",
            ["show", "--property=Result", _] => "Result=oom-kill\n",
            ["show", "--property=ControlGroup,MemoryPeak,MemoryMax,Result,ExecMainCode,ExecMainStatus", _] => "Result=oom-kill\nMemoryPeak=12500000000\nMemoryMax=12500000000\nExecMainCode=0\nExecMainStatus=0\n",
            ["reset-failed", _] | ["list-units", ..] => "",
            _ => panic!("unexpected systemd command: {args:?}"),
        };
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: text.as_bytes().to_vec(),
            stderr: vec![],
        })
    }));
    let runner = Runner::open(config).unwrap();
    let id = running_record(&f, &runner);
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&id)
        .unwrap()
        .process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(Uuid::now_v7()),
    });
    let directory = runner
        .config()
        .state_dir()
        .join("sandboxes")
        .join(id.to_string())
        .join("output");
    fs::create_dir_all(&directory).unwrap();
    for name in ["stdout", "stderr", "exit"] {
        fs::write(directory.join(name), "").unwrap();
    }
    runner.follow_unit(id).unwrap();
    let job: Job = read_json(&runner.record_path(id)).unwrap();
    let terminal = job.terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::Signal { signal: 9 });
    assert_eq!(terminal.ran, Some(Ran::Remote));
    let detail = terminal.refusal_detail().unwrap();
    for expected in [
        "rule=out_of_memory",
        "out of memory",
        "memory.peak=12500000000",
        "memory.max=12500000000",
        "memory.max_hit=true",
        "ExecMainCode=0",
        "ExecMainStatus=0",
        "workspace[free_bytes=",
        "free_inodes=",
        "runner_state[free_bytes=",
    ] {
        assert!(detail.contains(expected), "missing {expected}: {detail}");
    }
    assert!(detail.len() <= REFUSAL_DETAIL_MAX_BYTES);
    let decoded: TerminalRecord =
        serde_json::from_value(serde_json::to_value(&terminal).unwrap()).unwrap();
    assert_eq!(decoded, terminal);
}

#[test]
fn boat_disk_full_replay_keeps_real_exit_and_explanation() {
    let f = Fixture::new();
    let mut config = f.config();
    config.boat_profile = true;
    config.boat_memory_max_bytes = Some(12_500_000_000);
    config.free_space_floor_bytes = u64::MAX;
    // Admission has headroom; the end-of-job statvfs snapshot is below this floor.
    config.free_space_probe = Arc::new(|_| Ok(u64::MAX));
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
    let id = running_record(&f, &runner);
    let directory = runner
        .config()
        .state_dir()
        .join("sandboxes")
        .join(id.to_string())
        .join("output");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("stdout"), "all suites passed\n").unwrap();
    fs::write(directory.join("stderr"), "").unwrap();
    fs::write(directory.join("exit"), "101\n                            ").unwrap();
    runner.monitor_unit(id);
    let job: Job = read_json(&runner.record_path(id)).unwrap();
    let terminal = job.terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::Exit { code: 101 });
    assert_eq!(terminal.ran, Some(Ran::Remote));
    let detail = terminal.refusal_detail().unwrap();
    assert!(detail.contains("rule=disk_full"), "{detail}");
    assert!(detail.contains("disk full: the job ran"), "{detail}");
    assert!(
        detail.contains("withdrawal=disk_below_floor volume=runner_state"),
        "{detail}"
    );
    assert!(detail.contains("free_inodes="), "{detail}");
    assert!(detail.len() <= REFUSAL_DETAIL_MAX_BYTES);
}

#[test]
fn cancel_after_durable_unit_exit_recovers_command_status() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = running_record(&f, &runner);
    let directory = runner
        .config()
        .state_dir()
        .join("sandboxes")
        .join(id.to_string())
        .join("output");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("exit"), "23\n").unwrap();
    runner.cancel(id).unwrap();
    let job: Job = read_json(&runner.record_path(id)).unwrap();
    let terminal = job.terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert_eq!(terminal.killed, Some(Killed::Cancel));
}
