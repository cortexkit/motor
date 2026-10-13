use super::*;
use std::os::unix::process::ExitStatusExt;
use std::time::Instant;

struct CancellationBackend {
    events: Arc<Mutex<Vec<&'static str>>>,
    collect: Arc<Hold>,
    exited: Arc<AtomicBool>,
}

impl Backend for CancellationBackend {
    fn materialize(&self, _: &Config, _: &Trees, _: &Setup) -> io::Result<Prepared> {
        unreachable!("the test inserts an already-running job")
    }

    fn launch(
        &self,
        _: &Config,
        _: &Trees,
        _: &Setup,
        _: &Prepared,
        _: u32,
    ) -> io::Result<backend::Launch> {
        unreachable!("the test inserts an already-running job")
    }

    fn cancel(&self, _: &Config, _: &Job, hard: bool) -> io::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(if hard { "kill" } else { "cancel" });
        Ok(())
    }

    fn unit_alive(
        &self,
        _: &Config,
        _: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        // Host jobs without systemd units also have unknown liveness after exit.
        Ok(self.exited.load(Ordering::SeqCst).then_some(false))
    }

    fn collect_changes(&self, _: &Trees, _: &Job) -> io::Result<Option<Vec<String>>> {
        self.events.lock().unwrap().push("collect_changes");
        self.collect.hook()("collect_changes");
        Ok(Some(Vec::new()))
    }

    fn reconcile(&self, _: &Config, _: &[Job]) -> io::Result<Vec<Uuid>> {
        Ok(Vec::new())
    }
}

struct CancellationFixture {
    runner: Runner,
    fixture: Fixture,
    id: Uuid,
    events: Arc<Mutex<Vec<&'static str>>>,
    collect: Arc<Hold>,
    retry_at: Instant,
    writes: Arc<AtomicU64>,
    exited: Arc<AtomicBool>,
}

impl CancellationFixture {
    fn new() -> Self {
        let fixture = Fixture::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let collect = Hold::new("collect_changes");
        let exited = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicU64::new(0));
        let mut config = fixture.config();
        let recorded_writes = writes.clone();
        config.before_job_write = Some(Arc::new(move |_| {
            recorded_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        let runner = Runner::open_with_backend(
            config,
            Box::new(CancellationBackend {
                events: events.clone(),
                collect: collect.clone(),
                exited: exited.clone(),
            }),
        )
        .unwrap();
        let (request, _) = fixture.run("true");
        let id = request.job_id;
        let mut job = Job::new(id, Some(request), runner.now(), 1, 0);
        job.state = JobState::Running;
        job.started_ms = Some(runner.now());
        job.process = Some(platform::ProcessIdentity {
            pid: 0,
            start: 0,
            unit: None,
        });
        // Only explicit calls using this injected instant can retry the job;
        // the background scheduler cannot consume a retry during the test.
        let retry_at = Instant::now() + Duration::from_secs(3600);
        {
            let mut inner = runner.core.inner.lock().unwrap();
            inner.jobs.insert(id, job);
            inner.kill_retries.insert(id, (0, retry_at));
        }
        Self {
            runner,
            fixture,
            id,
            events,
            collect,
            retry_at,
            writes,
            exited,
        }
    }

    fn retry(&self, instant: Instant) {
        self.runner.enforce_kills(instant).unwrap();
    }
}

#[test]
fn cancelled_unit_scheduler_and_real_exit_finalize_once_with_real_status() {
    let f = CancellationFixture::new();
    let id = f.id;
    {
        let mut inner = f.runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&id).unwrap();
        job.kill = Some((Killed::Cancel, f.runner.now()));
        job.durable_output = true;
    }
    let output = f
        .runner
        .core
        .trees
        .state
        .join("sandboxes")
        .join(id.to_string())
        .join("output");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("stdout"), b"").unwrap();
    fs::write(output.join("stderr"), b"").unwrap();
    fs::write(output.join("exit"), b"0\n").unwrap();
    f.exited.store(true, Ordering::SeqCst);
    f.collect.arm();
    let release = Release(f.collect.clone());
    // The scheduler observes the exited unit first, then pauses its synthetic
    // finalization while the normal monitor reads the retained actual exit status.
    f.retry(f.retry_at);
    assert!(
        poll(Duration::from_secs(10), || f.collect.held()),
        "scheduler did not reach collect_changes"
    );
    f.runner.follow_unit(id).unwrap();
    f.runner.cancel(id).unwrap();
    f.retry(f.retry_at + Duration::from_secs(10));
    assert!(
        f.runner.core.inner.lock().unwrap().jobs[&id]
            .terminal
            .is_none(),
        "the competing finish must not persist while the owner is held"
    );
    let collections = f
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| **event == "collect_changes")
        .count();
    drop(release);
    assert!(poll(Duration::from_secs(10), || {
        f.runner.core.inner.lock().unwrap().jobs[&id]
            .terminal
            .is_some()
    }));
    let persisted: Job = read_json(&f.runner.record_path(id)).unwrap();
    f.runner.cancel(id).unwrap();
    f.retry(f.retry_at + Duration::from_secs(20));
    assert_eq!(collections, 1, "only the finalization owner may collect");
    assert_eq!(
        persisted.terminal.unwrap().outcome,
        Outcome::Exit { code: 0 }
    );
    assert!(persisted.monitoring_error.is_none());
    assert_eq!(f.writes.load(Ordering::SeqCst), 1, "one terminal write");
    assert_eq!(*f.events.lock().unwrap(), ["collect_changes"]);
    let inner = f.runner.core.inner.lock().unwrap();
    assert!(!inner.finalizing.contains(&id));
    assert!(!inner.kill_retries.contains_key(&id));
    assert!(!inner.exit_statuses.contains_key(&id));
}

#[test]
fn real_exit_owner_ignores_competing_synthetic_finish() {
    let f = CancellationFixture::new();
    let id = f.id;
    f.collect.arm();
    let release = Release(f.collect.clone());
    let runner = f.runner.clone();
    let monitor = std::thread::spawn(move || {
        runner.finish(id, Ok(std::process::ExitStatus::from_raw(7 << 8)))
    });
    assert!(poll(Duration::from_secs(10), || f.collect.held()));
    // A competing synthetic finalizer must not scan or persist another terminal
    // after the real-status monitor has claimed finalization.
    f.runner
        .finish(id, Err(io::Error::other("cancelled unit already exited")))
        .unwrap();
    assert_eq!(*f.events.lock().unwrap(), ["collect_changes"]);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    drop(release);
    monitor.join().unwrap().unwrap();
    let persisted: Job = read_json(&f.runner.record_path(id)).unwrap();
    assert_eq!(
        persisted.terminal.unwrap().outcome,
        Outcome::Exit { code: 7 }
    );
    assert!(persisted.monitoring_error.is_none());
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
}

#[test]
fn cancelled_exited_unit_without_real_status_still_finalizes() {
    let f = CancellationFixture::new();
    let id = f.id;
    f.runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&id)
        .unwrap()
        .kill = Some((Killed::Cancel, f.runner.now()));
    f.exited.store(true, Ordering::SeqCst);
    f.retry(f.retry_at);
    assert!(poll(Duration::from_secs(10), || {
        f.runner.core.inner.lock().unwrap().jobs[&id]
            .terminal
            .is_some()
    }));
    let persisted: Job = read_json(&f.runner.record_path(id)).unwrap();
    assert_eq!(persisted.terminal.unwrap().outcome, Outcome::OutcomeUnknown);
    assert_eq!(
        persisted.monitoring_error.as_deref(),
        Some("cancelled unit already exited")
    );
    assert_eq!(*f.events.lock().unwrap(), ["collect_changes"]);
    let inner = f.runner.core.inner.lock().unwrap();
    assert!(!inner.finalizing.contains(&id));
    assert!(!inner.kill_retries.contains_key(&id));
}

#[test]
fn cancellation_retries_stop_before_collecting_changes() {
    let f = CancellationFixture::new();
    f.runner.cancel(f.id).unwrap();
    f.collect.arm();
    let release = Release(f.collect.clone());
    let runner = f.runner.clone();
    let id = f.id;
    let monitor =
        std::thread::spawn(move || runner.finish(id, Ok(std::process::ExitStatus::from_raw(0))));
    let held = poll(Duration::from_secs(10), || f.collect.held());
    if held {
        // The monitor has entered collect_changes but has not persisted a
        // terminal yet. Force a due retry into that unlocked interval.
        assert!(f.runner.core.inner.lock().unwrap().jobs[&id]
            .terminal
            .is_none());
        f.retry(f.retry_at);
    }
    drop(release);
    monitor.join().unwrap().unwrap();
    assert!(held, "monitor did not reach collect_changes");
    assert!(f.runner.core.inner.lock().unwrap().jobs[&id]
        .terminal
        .is_some());
    {
        let inner = f.runner.core.inner.lock().unwrap();
        assert!(!inner.kill_retries.contains_key(&id));
        assert!(!inner.finalizing.contains(&id));
    }
    f.retry(f.retry_at + Duration::from_secs(10));
    f.runner.cancel(id).unwrap();
    assert_eq!(
        *f.events.lock().unwrap(),
        ["cancel", "collect_changes"],
        "neither finalization nor a persisted terminal may be followed by a backend cancel"
    );
}

#[test]
fn cancel_during_change_collection_does_not_reach_backend() {
    let f = CancellationFixture::new();
    f.collect.arm();
    let release = Release(f.collect.clone());
    let runner = f.runner.clone();
    let id = f.id;
    let monitor =
        std::thread::spawn(move || runner.finish(id, Ok(std::process::ExitStatus::from_raw(0))));
    let held = poll(Duration::from_secs(10), || f.collect.held());
    if held {
        f.runner.cancel(id).unwrap();
    }
    drop(release);
    monitor.join().unwrap().unwrap();
    assert!(held, "monitor did not reach collect_changes");
    f.runner.cancel(id).unwrap();
    assert_eq!(*f.events.lock().unwrap(), ["collect_changes"]);
}

#[test]
fn cancellation_retries_keep_running_job_backoff_and_attempt_limit() {
    let f = CancellationFixture::new();
    f.runner.cancel(f.id).unwrap();
    f.retry(f.retry_at);
    assert_eq!(*f.events.lock().unwrap(), ["cancel", "cancel"]);
    f.retry(f.retry_at + Duration::from_secs(1));
    assert_eq!(*f.events.lock().unwrap(), ["cancel", "cancel"]);
    f.fixture.clock.fetch_add(2000, Ordering::SeqCst);
    for (seconds, attempts, next) in [(2, 1, 3), (3, 2, 5), (5, 3, 9)] {
        let instant = f.retry_at + Duration::from_secs(seconds);
        f.retry(instant);
        assert_eq!(
            f.runner.core.inner.lock().unwrap().kill_retries[&f.id],
            (attempts, f.retry_at + Duration::from_secs(next))
        );
        f.retry(f.retry_at + Duration::from_secs(next) - Duration::from_nanos(1));
        assert_eq!(f.events.lock().unwrap().len(), 2 + attempts as usize);
    }
    f.retry(f.retry_at + Duration::from_secs(9));
    assert_eq!(
        f.runner.core.inner.lock().unwrap().kill_retries[&f.id],
        (3, f.retry_at + Duration::from_secs(14))
    );
    f.retry(f.retry_at + Duration::from_secs(14));
    assert_eq!(
        *f.events.lock().unwrap(),
        ["cancel", "cancel", "kill", "kill", "kill"]
    );
}
