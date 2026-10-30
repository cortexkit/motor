use super::*;
use std::sync::mpsc;

fn finished_pending(runner: &Runner) -> Uuid {
    let id = Uuid::now_v7();
    let now = runner.now();
    let mut job = Job::new(id, None, now, 0, 1);
    job.started_ms = Some(now - 100);
    job.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(id),
    });
    job.durable_output = true;
    job.memory_accounting_pending = true;
    job.execution_ended_ms = Some(now);
    job.terminal_ended(Outcome::Exit { code: 0 }, now);
    runner.tenant_events().unwrap().admit(id).unwrap();
    runner.save(&job).unwrap();
    runner.core.inner.lock().unwrap().jobs.insert(id, job);
    assert!(pending(runner)
        .iter()
        .all(|event| event.event != TenantEventKind::Terminal));
    id
}

fn report(runner: &Runner, id: Uuid) -> Vec<u8> {
    let unit = runner.core.backend.unit_name(runner.config(), id);
    format!("Running as unit: {unit}\nFinished with result: success\nCPU time consumed: 95ms\nMemory peak: 4.2M\n").into_bytes()
}

struct HeldReader {
    entered: Option<mpsc::Sender<()>>,
    release: mpsc::Receiver<Vec<u8>>,
    bytes: io::Cursor<Vec<u8>>,
}

impl Read for HeldReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            self.bytes = io::Cursor::new(self.release.recv().unwrap());
        }
        self.bytes.read(buf)
    }
}

struct ReleaseReader(Option<mpsc::Sender<Vec<u8>>>);

impl ReleaseReader {
    fn send(&mut self, report: Vec<u8>) {
        self.0.take().unwrap().send(report).unwrap();
    }
}

impl Drop for ReleaseReader {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(Vec::new());
        }
    }
}

fn held_reader(runner: &Runner, id: Uuid) -> (ReleaseReader, std::thread::JoinHandle<()>) {
    let (entered, wait) = mpsc::channel();
    let (release, read) = mpsc::channel();
    let release = ReleaseReader(Some(release));
    let thread = runner.spawn_accounting_reader(
        id,
        Box::new(HeldReader {
            entered: Some(entered),
            release: read,
            bytes: io::Cursor::new(Vec::new()),
        }),
    );
    wait.recv_timeout(Duration::from_secs(10)).unwrap();
    (release, thread)
}

#[test]
fn held_open_accounting_reader_settles_at_deadline_and_ignores_late_peak() {
    let f = Fixture::new();
    let runner = open(&f);
    let id = finished_pending(&runner);
    let finished = runner.now();
    let terminal = runner.core.inner.lock().unwrap().jobs[&id]
        .terminal
        .clone()
        .unwrap();
    let (mut release, thread) = held_reader(&runner, id);

    // Assert the public bound literally, rather than deriving both the clock
    // advance and the expectation from the implementation's constant.
    f.clock.store(finished + 29_999, Ordering::SeqCst);
    runner.tick().unwrap();
    assert!(pending(&runner).is_empty());
    assert!(runner.core.inner.lock().unwrap().jobs[&id].memory_accounting_pending);
    f.clock.store(finished + 30_000, Ordering::SeqCst);
    runner.tick().unwrap();
    let events = pending(&runner);
    assert_eq!(
        events.len(),
        1,
        "a missing EOF must not hold the host lease"
    );
    assert_eq!(events[0].event, TenantEventKind::Terminal);
    assert_eq!(events[0].mem_peak_mib, None);
    assert!(!runner.core.inner.lock().unwrap().jobs[&id].memory_accounting_pending);
    assert_eq!(
        runner
            .core
            .health
            .lock()
            .unwrap()
            .counts(runner.now())
            .memory_peak_unavailable,
        1
    );

    release.send(report(&runner, id));
    thread.join().unwrap();
    assert!(
        pending(&runner).is_empty(),
        "a late peak must not re-emit Terminal"
    );
    let disk: Job = read_json(&runner.record_path(id)).unwrap();
    assert_eq!(disk.mem_peak_mib, None);
    assert_eq!(disk.reservation_use.unwrap().cpu_time_ns, 95_000_000);
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&id]
            .terminal
            .as_ref(),
        Some(&terminal)
    );
    assert_eq!(
        runner
            .core
            .health
            .lock()
            .unwrap()
            .counts(runner.now())
            .memory_peak_unavailable,
        1
    );
}

#[test]
fn held_open_accounting_reader_does_not_retain_core_or_instance_lock() {
    let f = Fixture::new();
    let runner = open(&f);
    let id = finished_pending(&runner);
    let (release, thread) = held_reader(&runner, id);
    let weak = Arc::downgrade(&runner.core);
    drop(runner);
    assert!(weak.upgrade().is_none(), "blocked accounting retained Core");
    // Reopening also proves that the old Core released its exclusive file lock
    // while the original reader is still blocked, not just after EOF.
    let reopened = open(&f);
    assert_eq!(pending(&reopened).len(), 1);
    drop(reopened);
    drop(release);
    thread.join().unwrap();
}

#[test]
fn persistent_accounting_save_failure_is_bounded_and_settles_null() {
    let f = Fixture::new();
    let fail = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicU64::new(0));
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    let failing = fail.clone();
    let writes = attempts.clone();
    config.before_job_write = Some(Arc::new(move |_| {
        if failing.load(Ordering::SeqCst) {
            writes.fetch_add(1, Ordering::SeqCst);
            return Err(io::Error::other("record storage unavailable"));
        }
        Ok(())
    }));
    let runner = Runner::open(config).unwrap();
    let id = finished_pending(&runner);
    let report = report(&runner, id);
    fail.store(true, Ordering::SeqCst);
    let (done, wait) = mpsc::channel();
    let worker = runner.clone();
    let thread = std::thread::spawn(move || {
        worker.collect_accounting(id, &report[..]);
        done.send(()).unwrap();
    });
    let result = wait.recv_timeout(Duration::from_secs(2));
    let failed_writes = attempts.load(Ordering::SeqCst);
    // Release failures before joining even if an unbounded retry regresses, so
    // the test can fail without leaving a retry worker running indefinitely.
    fail.store(false, Ordering::SeqCst);
    thread.join().unwrap();
    assert!(
        result.is_ok(),
        "accounting kept retrying a permanent save failure"
    );
    // One CPU write, three peak writes, one best-effort null record write.
    assert_eq!(failed_writes, 5);
    let events = pending(&runner);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, TenantEventKind::Terminal);
    assert_eq!(events[0].mem_peak_mib, None);
    assert!(!runner.core.inner.lock().unwrap().jobs[&id].memory_accounting_pending);
    f.clock.fetch_add(30_000, Ordering::SeqCst);
    runner.tick().unwrap();
    assert!(pending(&runner).is_empty());
    assert_eq!(
        runner
            .core
            .health
            .lock()
            .unwrap()
            .counts(runner.now())
            .memory_peak_unavailable,
        1
    );
}

#[test]
fn restart_resolves_terminal_with_pending_accounting_without_a_reader() {
    let f = Fixture::new();
    let runner = open(&f);
    let id = finished_pending(&runner);
    let path = runner.record_path(id);
    let job = runner.core.inner.lock().unwrap().jobs[&id].clone();
    assert!(job.memory_accounting_pending);
    let mut persisted = serde_json::to_value(&job).unwrap();
    // The pending flag is process-local and normally omitted. Also tolerate a
    // record explicitly containing it: no reader survives a daemon restart.
    assert!(persisted.get("memory_accounting_pending").is_none());
    persisted["memory_accounting_pending"] = true.into();
    atomic_json(&path, &persisted).unwrap();
    drop(runner);

    let reopened = open(&f);
    let recovered = reopened.core.inner.lock().unwrap().jobs[&id].clone();
    assert!(!recovered.memory_accounting_pending);
    assert_eq!(recovered.mem_peak_mib, None);
    let events = pending(&reopened);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, TenantEventKind::Terminal);
    assert_eq!(events[0].mem_peak_mib, None);
    drop(reopened);
    let reopened = open(&f);
    assert!(pending(&reopened).is_empty(), "restart re-emitted Terminal");
}

#[test]
fn null_settlement_retries_only_event_durability_after_log_failure() {
    let f = Fixture::new();
    let runner = open(&f);
    let id = finished_pending(&runner);
    let events_path = runner.core.trees.state.join("tenant-events.json");
    fs::remove_file(&events_path).unwrap();
    fs::create_dir(&events_path).unwrap();
    f.clock.fetch_add(30_000, Ordering::SeqCst);
    runner.tick().unwrap();
    assert!(!runner.core.inner.lock().unwrap().jobs[&id].memory_accounting_pending);
    assert!(pending(&runner).is_empty());
    assert_eq!(
        runner.core.inner.lock().unwrap().memory_event_retries[&id],
        runner.now() + 1000
    );

    fs::remove_dir(&events_path).unwrap();
    f.clock.fetch_add(999, Ordering::SeqCst);
    runner.tick().unwrap();
    assert!(pending(&runner).is_empty());
    f.clock.fetch_add(1, Ordering::SeqCst);
    runner.tick().unwrap();
    let events = pending(&runner);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, TenantEventKind::Terminal);
    assert_eq!(events[0].mem_peak_mib, None);
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .memory_event_retries
        .is_empty());
    runner.tick().unwrap();
    assert!(pending(&runner).is_empty());
    assert_eq!(
        runner
            .core
            .health
            .lock()
            .unwrap()
            .counts(runner.now())
            .memory_peak_unavailable,
        1
    );
}
