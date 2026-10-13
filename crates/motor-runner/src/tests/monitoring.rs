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
