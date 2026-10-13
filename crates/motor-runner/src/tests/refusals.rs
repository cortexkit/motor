use super::*;

#[derive(Clone, Default)]
struct LogBytes(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn logs(f: impl FnOnce()) -> String {
    let bytes = LogBytes::default();
    tracing::subscriber::with_default(
        motor_protocol::logging::LineSubscriber::new("[runner]", bytes.clone()),
        f,
    );
    let captured = bytes.0.lock().unwrap().clone();
    String::from_utf8(captured).unwrap()
}

fn assert_refusal(log: &str, id: Uuid, reason: &str, error: &str) {
    let lines: Vec<_> = log
        .lines()
        .filter(|line| line.contains("job refused_before_start"))
        .collect();
    assert_eq!(lines.len(), 1, "one no-start refusal line expected: {log}");
    let line = lines[0];
    assert!(line.contains(&format!("job_id={id}")), "{line}");
    assert!(line.contains(&format!("reason={reason}")), "{line}");
    assert!(line.contains("error=") && line.contains(error), "{line}");
}

#[test]
fn admission_during_eviction_logs_the_previously_silent_tombstone() {
    let f = Fixture::new();
    let (request, _) = f.run("true");
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let mut c = f.config();
    c.free_space_floor_bytes = 100;
    c.free_space_probe = Arc::new(|_| Ok(150));
    let removing = entered.clone();
    let resume = release.clone();
    c.long_work = Some(Arc::new(move |label| {
        if label == "disk_remove" {
            removing.wait();
            resume.wait();
        }
    }));
    let runner = Runner::open(c).unwrap();
    fs::create_dir_all(&request.workspace_key).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .catalog
            .workspaces
            .insert(request.workspace_key.clone(), Workspace::default());
    }
    let evictor = runner.clone();
    let eviction = std::thread::spawn(move || evictor.ensure_disk_space());
    entered.wait();
    let log = logs(|| {
        assert!(matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Refused(RefusalReason::WorkspaceSetupFailed)
        ));
    });
    release.wait();
    assert!(eviction.join().unwrap().unwrap().is_none());
    assert_refusal(
        &log,
        request.job_id,
        "WorkspaceSetupFailed",
        "deletion in progress",
    );
    assert!(log.contains(&request.workspace_key));
    assert!(!log.contains("job accepted"));
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_eq!(job.state, JobState::Tombstone);
    assert!(job.started_ms.is_none());
    assert!(job.process.is_none());
    assert!(job.setup_ms.is_none());
}

#[test]
fn refusal_logs_preserve_errors_from_transfer_fetch_setup_restart_and_attach() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (request, _) = f.run("true");
    runner.accept(request.clone()).unwrap();
    let log = logs(|| {
        runner
            .refuse(
                request.job_id,
                RefusalReason::BundleRejected,
                io::Error::other("bad chunk digest"),
            )
            .unwrap()
    });
    assert_refusal(&log, request.job_id, "BundleRejected", "bad chunk digest");
    assert!(logs(|| runner
        .refuse(request.job_id, RefusalReason::BundleRejected, "duplicate")
        .unwrap())
    .is_empty());

    let (fetch, _) = f.run("true");
    runner.accept(fetch.clone()).unwrap();
    let missing = f.scratch.join("missing-upload");
    let log = logs(|| runner.fetch_run(&fetch, &missing, true).unwrap());
    assert_refusal(&log, fetch.job_id, "BundleRejected", "error=");
    assert!(
        log.contains("could not open") && log.contains(text(&missing)),
        "{log}"
    );

    let (setup, _) = f.run("true");
    runner.accept(setup.clone()).unwrap();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&setup.job_id)
        .unwrap()
        .state = JobState::Queued;
    let log = logs(|| {
        runner
            .launch(
                Setup {
                    id: setup.job_id,
                    cpus: vec![1, 17],
                    request: setup.clone(),
                    base: false,
                    recorded: BTreeMap::new(),
                },
                Err(io::Error::other("sandbox ownership denied")),
            )
            .unwrap()
    });
    assert_refusal(&log, setup.job_id, "Unknown", "sandbox ownership denied");

    let unknown = Uuid::now_v7();
    let log = logs(|| {
        runner.replay(unknown, 0).unwrap();
    });
    assert_refusal(
        &log,
        unknown,
        "TransferInterrupted",
        "no recorded admission",
    );
    assert!(logs(|| {
        runner.replay(unknown, 0).unwrap();
    })
    .is_empty());

    let (restart, _) = f.run("true");
    runner.accept(restart.clone()).unwrap();
    drop(runner);
    let log = logs(|| {
        let _runner = reopen(f.config());
    });
    assert_refusal(&log, restart.job_id, "Unknown", "runner restarted");
}

#[test]
fn policy_and_withdrawal_refusals_log_their_diagnostics() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, _) = f.run("true");
    request.cwd = "/outside-workspace".into();
    let log = logs(|| {
        runner.accept(request.clone()).unwrap();
    });
    assert_refusal(
        &log,
        request.job_id,
        "WorkspaceKeyRejected",
        "/outside-workspace",
    );

    let (request, _) = f.run("true");
    runner.accept(request.clone()).unwrap();
    let log = logs(|| {
        runner
            .withdraw_for(
                request.job_id,
                Some(RefusalReason::Unknown("transfer_stalled".into())),
            )
            .unwrap();
    });
    assert_refusal(&log, request.job_id, "Unknown", "transfer_stalled");

    runner.begin_drain(Duration::from_secs(10), false);
    let (request, _) = f.run("true");
    let log = logs(|| {
        runner.accept(request.clone()).unwrap();
    });
    assert_refusal(&log, request.job_id, "RunnerDraining", "runner is draining");

    let f = Fixture::new();
    let mut c = f.config();
    c.free_space_floor_bytes = 100;
    c.free_space_probe = Arc::new(|_| Ok(7));
    let runner = Runner::open(c).unwrap();
    let (request, _) = f.run("true");
    let log = logs(|| {
        runner.accept(request.clone()).unwrap();
    });
    assert_refusal(
        &log,
        request.job_id,
        "RunnerDiskFull",
        "free_bytes=7 floor_bytes=100",
    );
}
