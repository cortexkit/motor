use super::*;
use std::os::unix::process::ExitStatusExt;

/// Pauses whichever long step its `Hold` is labelled for: the unit query or
/// stop command of this backend, or any runner `long_work` step or job record
/// save the fixture routes to it.
struct HeldBackend {
    hold: Arc<Hold>,
    /// Unit stops ("stop") and change collections ("collect"), in order.
    events: Arc<Mutex<Vec<(&'static str, Uuid)>>>,
    /// Process start times, standing in for units, of the jobs stopped so
    /// far. A stopped job's unit has exited, so teardown can finish it.
    stopped: Mutex<BTreeSet<u64>>,
}

impl Backend for HeldBackend {
    fn materialize(&self, _: &Config, _: &Trees, _: &Setup) -> io::Result<Prepared> {
        unreachable!("the test inserts its jobs directly")
    }

    fn launch(
        &self,
        _: &Config,
        _: &Trees,
        _: &Setup,
        _: &Prepared,
        _: u32,
    ) -> io::Result<backend::Launch> {
        unreachable!("the test inserts its jobs directly")
    }

    fn cancel(&self, _: &Config, job: &Job, _: bool) -> io::Result<()> {
        self.hold.hook()("stop_unit");
        self.events.lock().unwrap().push(("stop", job.id));
        if let Some(process) = job.process {
            self.stopped.lock().unwrap().insert(process.start);
        }
        Ok(())
    }

    fn unit_alive(
        &self,
        _: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        self.hold.hook()("unit_alive");
        // Host processes without a unit have unknown liveness until stopped.
        let stopped = process.is_some_and(|p| self.stopped.lock().unwrap().contains(&p.start));
        Ok(stopped.then_some(false))
    }

    fn collect_changes(&self, _: &Trees, job: &Job) -> io::Result<Option<Vec<String>>> {
        self.events.lock().unwrap().push(("collect", job.id));
        Ok(Some(Vec::new()))
    }

    fn reconcile(&self, _: &Config, _: &[Job]) -> io::Result<Vec<Uuid>> {
        Ok(Vec::new())
    }
}

struct LockFixture {
    runner: Runner,
    fixture: Fixture,
    hold: Arc<Hold>,
    events: Arc<Mutex<Vec<(&'static str, Uuid)>>>,
    /// The job record whose next save pauses on the `record_save` hold.
    held_record: Arc<Mutex<Option<PathBuf>>>,
    /// Kill retries scheduled here are only due for explicit
    /// `enforce_kills` calls, never for the background scheduler.
    retry_at: std::time::Instant,
}

impl LockFixture {
    fn new(label: &'static str) -> Self {
        let fixture = Fixture::new();
        let hold = Hold::new(label);
        let events = Arc::new(Mutex::new(Vec::new()));
        let held_record = Arc::new(Mutex::new(None::<PathBuf>));
        let mut config = fixture.config();
        config.long_work = Some(hold.hook());
        config.before_job_write = Some({
            let hold = hold.clone();
            let held_record = held_record.clone();
            Arc::new(move |path| {
                if held_record.lock().unwrap().as_deref() == Some(path) {
                    hold.hook()("record_save");
                }
                Ok(())
            })
        });
        let runner = Runner::open_with_backend(
            config,
            Box::new(HeldBackend {
                hold: hold.clone(),
                events: events.clone(),
                stopped: Mutex::default(),
            }),
        )
        .unwrap();
        Self {
            runner,
            fixture,
            hold,
            events,
            held_record,
            retry_at: std::time::Instant::now() + Duration::from_secs(3600),
        }
    }

    /// A running job with no unit, as on a host without systemd.
    fn running(&self) -> Uuid {
        let (request, _) = self.fixture.run("true");
        let id = request.job_id;
        let now = self.runner.now();
        let mut job = Job::new(id, Some(request), now, 1, 0);
        job.state = JobState::Running;
        job.started_ms = Some(now);
        // Pid 0 keeps teardown from signalling a real process; the start
        // time tells the fake backend's jobs apart.
        static STARTS: AtomicU64 = AtomicU64::new(1);
        job.process = Some(platform::ProcessIdentity {
            pid: 0,
            start: STARTS.fetch_add(1, Ordering::Relaxed),
            unit: None,
        });
        let mut inner = self.runner.core.inner.lock().unwrap();
        inner.jobs.insert(id, job);
        inner.kill_retries.insert(id, (0, self.retry_at));
        id
    }

    /// A finished job whose record and a file of retained output are on disk.
    fn finished(&self, returned: bool) -> Uuid {
        let (request, _) = self.fixture.run("true");
        let id = request.job_id;
        let now = self.runner.now();
        let mut job = Job::new(id, Some(request), now, 1, 0);
        job.terminal(Outcome::Exit { code: 0 }, now);
        let directory = self.runner.record_path(id).parent().unwrap().to_owned();
        fs::create_dir_all(directory.join("returned")).unwrap();
        fs::write(directory.join("chunk-0"), b"output").unwrap();
        fs::write(directory.join("returned/chunk-0"), b"returned").unwrap();
        if returned {
            job.returned = Some(ReturnedFiles {
                directory: "returned".into(),
                chunks: Vec::new(),
                manifest: ReturnManifest::new(id, 0, Vec::new(), 0, 0),
                expired: false,
            });
        }
        self.runner.save(&job).unwrap();
        self.runner.core.inner.lock().unwrap().jobs.insert(id, job);
        id
    }

    /// Run `work` on its own thread until it, or the background scheduler
    /// doing the same work, reaches the hold. Arm the hold first, before the
    /// change that makes the work due, so the scheduler cannot finish it
    /// unheld in between.
    fn hold_in(&self, work: impl FnOnce(Runner) + Send + 'static) -> HeldWork {
        let runner = self.runner.clone();
        let held = HeldWork {
            hold: self.hold.clone(),
            worker: Some(std::thread::spawn(move || work(runner))),
        };
        assert!(
            poll(Duration::from_secs(10), || self.hold.held()),
            "the work never reached the {:?} hold",
            self.hold.label
        );
        held
    }

    /// A cancel of `id` completes, unit stop included, while the hold is held.
    fn assert_cancel_completes(&self, id: Uuid) {
        let runner = self.runner.clone();
        let cancel = std::thread::spawn(move || runner.cancel(id));
        assert!(
            poll(Duration::from_secs(10), || cancel.is_finished()),
            "the cancel waited for the held {:?} step",
            self.hold.label
        );
        cancel.join().unwrap().unwrap();
        assert!(self.events.lock().unwrap().contains(&("stop", id)));
        assert!(self.hold.held(), "the held step is still in progress");
    }
}

impl Drop for LockFixture {
    fn drop(&mut self) {
        // Make every kill retry due now, so the scheduler passes of the
        // fixture's teardown find each stopped job's unit exited and finish it.
        if let Ok(mut inner) = self.runner.core.inner.lock() {
            let now = std::time::Instant::now();
            for retry in inner.kill_retries.values_mut() {
                *retry = (0, now);
            }
        }
    }
}

/// Releases the hold when dropped and waits for the held work to end, so the
/// fixture's teardown never races a thread still using the runner.
struct HeldWork {
    hold: Arc<Hold>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HeldWork {
    fn drop(&mut self) {
        self.hold.release();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// A deadline or cancel retry spawns systemctl to query and to stop its unit.
/// On a loaded host each spawn takes long enough that holding the state lock
/// across them delays every other job's cancel.
#[test]
fn cancel_proceeds_while_a_kill_retry_waits_for_systemctl() {
    for label in ["unit_alive", "stop_unit"] {
        let f = LockFixture::new(label);
        let killed = f.running();
        let other = f.running();
        f.runner
            .core
            .inner
            .lock()
            .unwrap()
            .jobs
            .get_mut(&killed)
            .unwrap()
            .kill = Some((Killed::Cancel, f.runner.now()));
        let retry_at = f.retry_at;
        f.hold.arm();
        let _release = f.hold_in(move |runner| runner.enforce_kills(retry_at).unwrap());
        f.assert_cancel_completes(other);
    }
}

/// The same for a cancel that is itself waiting for its unit stop.
#[test]
fn cancel_proceeds_while_another_cancel_waits_for_systemctl() {
    for label in ["unit_alive", "stop_unit"] {
        let f = LockFixture::new(label);
        let first = f.running();
        let second = f.running();
        f.hold.arm();
        let _release = f.hold_in(move |runner| runner.cancel(first).unwrap());
        f.assert_cancel_completes(second);
    }
}

/// A cancel queries and stops its unit without the state lock. The job's
/// monitor must not claim finalization in between: once finalization is
/// claimed, the exited job's unit is never signalled.
#[test]
fn finalization_waits_for_a_cancel_signalling_without_the_state_lock() {
    let f = LockFixture::new("unit_alive");
    let id = f.running();
    f.hold.arm();
    let held = f.hold_in(move |runner| runner.cancel(id).unwrap());
    let runner = f.runner.clone();
    let monitor =
        std::thread::spawn(move || runner.finish(id, Ok(std::process::ExitStatus::from_raw(0))));
    // A finalization that did not wait for the cancel would claim the job and
    // collect its changes in this window, before the cancel's stop.
    poll(Duration::from_millis(500), || {
        f.events
            .lock()
            .unwrap()
            .iter()
            .any(|(event, _)| *event == "collect")
    });
    drop(held);
    monitor.join().unwrap().unwrap();
    assert_eq!(*f.events.lock().unwrap(), [("stop", id), ("collect", id)]);
}

/// A job expired after `RETENTION_MS` takes its output and returned files
/// with it; deleting them can take seconds on a busy disk.
#[test]
fn cancel_proceeds_while_maintenance_deletes_an_expired_job() {
    let f = LockFixture::new("expired_job_remove");
    let expired = f.finished(false);
    let running = f.running();
    let directory = f.runner.record_path(expired).parent().unwrap().to_owned();
    f.hold.arm();
    // Expiry counts from the later of the job's end and its ID's mint time.
    let finished = f.runner.core.inner.lock().unwrap().jobs[&expired]
        .finished_ms
        .unwrap();
    f.fixture.clock.store(
        finished.max(mint_ms(expired)) + RETENTION_MS,
        Ordering::SeqCst,
    );
    let release = f.hold_in(|runner| runner.maintenance().unwrap());
    f.assert_cancel_completes(running);
    // The job record was removed from the state and its directory moved
    // aside before the deletion began, so no replay can read a half-deleted
    // job.
    assert!(!f.runner.has_job_record(expired));
    assert!(!directory.exists());
    drop(release);
    assert!(poll(Duration::from_secs(10), || fs::read_dir(
        f.runner.trash()
    )
    .unwrap()
    .next()
    .is_none()));
}

/// Expiring a job's returned files saves its record (two fsyncs) and then
/// deletes the files; neither may hold the state lock.
#[test]
fn cancel_proceeds_while_returned_files_expire() {
    for label in ["record_save", "returned_files_remove"] {
        let f = LockFixture::new(label);
        let finished = f.finished(true);
        let running = f.running();
        let record = f.runner.record_path(finished);
        *f.held_record.lock().unwrap() = Some(record.clone());
        f.hold.arm();
        f.fixture
            .clock
            .fetch_add(RETURN_RETENTION_MS, Ordering::SeqCst);
        let release = f.hold_in(|runner| runner.maintenance().unwrap());
        f.assert_cancel_completes(running);
        drop(release);
        assert!(poll(Duration::from_secs(10), || !record
            .parent()
            .unwrap()
            .join("returned")
            .exists()));
        let saved: Job = read_json(&record).unwrap();
        assert!(saved.returned.unwrap().expired);
        assert!(
            f.runner.core.inner.lock().unwrap().jobs[&finished]
                .returned
                .as_ref()
                .unwrap()
                .expired
        );
    }
}

/// The last save of a job's record, read from disk. A job's lifecycle saves
/// follow one another, so this names the save that comes next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Saved {
    /// No record yet: the next save is the admission.
    Nothing,
    /// Awaiting bundles: next is the fetch's Queued save.
    Admitted,
    /// Queued, not set up: next is the setup save, or a withdrawal.
    Queued,
    /// Set up, not started: next is the Started save.
    SetUp,
    /// Running: next is the terminal record (or a kill record).
    Started,
}

fn saved(path: &Path) -> Option<Saved> {
    match read_json::<Job>(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(Saved::Nothing),
        Ok(job) if job.terminal.is_none() => match job.state {
            JobState::AwaitingBundles => Some(Saved::Admitted),
            JobState::Queued if job.setup_ms.is_none() => Some(Saved::Queued),
            JobState::Queued if job.started.is_none() => Some(Saved::SetUp),
            JobState::Running => Some(Saved::Started),
            _ => None,
        },
        _ => None,
    }
}

/// Faults a `SaveFixture` injects. Each record fault names a record and the
/// stage it was last saved at, and applies once, to that record's next save.
#[derive(Default)]
struct SaveFaults {
    /// Wait on the `record_save` hold.
    hold: Option<(PathBuf, Saved)>,
    /// Fail, after any hold.
    fail: Option<(PathBuf, Saved)>,
    /// Panic.
    panic: Option<(PathBuf, Saved)>,
    /// Fail every catalog write at this step until cleared.
    catalog: Option<CatalogWrite>,
    /// Panic in the long step with this label, after any hold on it.
    panic_work: Option<&'static str>,
    /// Catalog writes failed so far.
    catalog_failures: usize,
}

/// A runner with the host backend whose scheduler runs only when a test
/// calls `tick`, which can hold one chosen job record save, or a long step
/// labelled with its hold's label (such as `catalog_save`), open, and which
/// injects `SaveFaults`.
struct SaveFixture {
    runner: Runner,
    fixture: Fixture,
    hold: Arc<Hold>,
    faults: Arc<Mutex<SaveFaults>>,
}

impl SaveFixture {
    fn new(label: &'static str) -> Self {
        Self::with(label, |_| {})
    }

    fn with(label: &'static str, configure: impl FnOnce(&mut Config)) -> Self {
        let fixture = Fixture::new();
        let hold = Hold::new(label);
        let faults = Arc::new(Mutex::new(SaveFaults::default()));
        let mut config = fixture.config();
        config.manual_tick = true;
        config.disable_warm_builds = true;
        config.long_work = Some({
            let hold = hold.hook();
            let faults = faults.clone();
            Arc::new(move |label| {
                hold(label);
                let panic = {
                    let mut faults = faults.lock().unwrap();
                    let due = faults.panic_work == Some(label);
                    if due {
                        faults.panic_work = None;
                    }
                    due
                };
                if panic {
                    panic!("injected panic in {label}");
                }
            })
        });
        config.before_job_write = Some({
            let hold = hold.clone();
            let faults = faults.clone();
            Arc::new(move |path| {
                let stage = saved(path);
                let due = |fault: &mut Option<(PathBuf, Saved)>| {
                    let due = fault
                        .as_ref()
                        .is_some_and(|(record, at)| record == path && stage == Some(*at));
                    if due {
                        *fault = None;
                    }
                    due
                };
                let (held, failed, panicked) = {
                    let mut faults = faults.lock().unwrap();
                    (
                        due(&mut faults.hold),
                        due(&mut faults.fail),
                        due(&mut faults.panic),
                    )
                };
                if held {
                    hold.hook()("record_save");
                }
                if panicked {
                    panic!("injected panic in a record save");
                }
                if failed {
                    return Err(io::Error::other("injected record save failure"));
                }
                Ok(())
            })
        });
        config.catalog_write = Some({
            let faults = faults.clone();
            Arc::new(move |step| {
                let mut faults = faults.lock().unwrap();
                if faults.catalog != Some(step) {
                    return Ok(());
                }
                faults.catalog_failures += 1;
                Err(io::Error::other("injected catalog write failure"))
            })
        });
        configure(&mut config);
        let runner = Runner::open(config).unwrap();
        Self {
            runner,
            fixture,
            hold,
            faults,
        }
    }

    /// Hold the next save of `id`'s record made after a save at `stage`.
    fn hold_save(&self, id: Uuid, stage: Saved) {
        self.faults.lock().unwrap().hold = Some((self.runner.record_path(id), stage));
        self.hold.arm();
    }

    /// Fail the next save of `id`'s record made after a save at `stage`.
    fn fail_save(&self, id: Uuid, stage: Saved) {
        self.faults.lock().unwrap().fail = Some((self.runner.record_path(id), stage));
    }

    /// Panic in the next save of `id`'s record made after a save at `stage`.
    fn panic_save(&self, id: Uuid, stage: Saved) {
        self.faults.lock().unwrap().panic = Some((self.runner.record_path(id), stage));
    }

    /// Fail catalog writes at `step` until called again with `None`.
    fn fail_catalog(&self, step: Option<CatalogWrite>) {
        self.faults.lock().unwrap().catalog = step;
    }

    /// Panic in the next long step labelled `label`, after any hold on it.
    fn panic_work(&self, label: &'static str) {
        self.faults.lock().unwrap().panic_work = Some(label);
    }

    /// Let a job whose start failed be dispatched again: its first retry
    /// delay passes on the runner's clock.
    fn pass_start_retry_delay(&self, ms: u64) {
        self.fixture.clock.fetch_add(ms, Ordering::SeqCst);
    }

    fn catalog_failures(&self) -> usize {
        self.faults.lock().unwrap().catalog_failures
    }

    /// As `LockFixture::hold_in`: run `work` until something reaches the
    /// armed hold.
    fn hold_in(&self, work: impl FnOnce(Runner) + Send + 'static) -> HeldWork {
        let runner = self.runner.clone();
        let held = HeldWork {
            hold: self.hold.clone(),
            worker: Some(std::thread::spawn(move || work(runner))),
        };
        assert!(
            poll(Duration::from_secs(30), || self.hold.held()),
            "the work never reached the {:?} hold",
            self.hold.label
        );
        held
    }

    /// A queued job in workspace `name`, running `command`.
    fn queued(&self, name: &str, command: &str) -> Run {
        let (request, bytes) = job_at(&self.fixture, name, command);
        self.fixture.submit(&self.runner, &request, &bytes);
        request
    }

    /// As `queued`, but the admission and the fetch must each complete while
    /// a save is held, instead of hanging the test if they wait for it.
    fn queued_while_held(&self, name: &str, command: &str) -> Run {
        let (request, bytes) = job_at(&self.fixture, name, command);
        let runner = self.runner.clone();
        let admitted = request.clone();
        let admission = completes("an admission", move || runner.accept(admitted).unwrap());
        assert!(matches!(admission, Admission::Accepted(_)));
        let upload = self.fixture.spool(&self.runner, &request, &bytes);
        let runner = self.runner.clone();
        let fetched = request.clone();
        let directory = upload.directory.clone();
        completes("a bundle fetch", move || {
            runner.fetch_run(&fetched, &directory, true).unwrap()
        });
        request
    }

    fn job(&self, id: Uuid) -> Option<Job> {
        self.runner
            .core
            .inner
            .lock()
            .unwrap()
            .jobs
            .get(&id)
            .cloned()
    }

    /// Tick until `done` holds, as the background scheduler would.
    fn tick_until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        assert!(
            poll(Duration::from_secs(60), || {
                self.runner.tick().unwrap();
                done(self)
            }),
            "{what}: {}",
            runner_diagnostics(&self.runner)
        );
    }

    fn terminal(&self, id: Uuid) -> Option<TerminalRecord> {
        self.job(id).and_then(|job| job.terminal)
    }

    /// The status reply and another job's admission, in a workspace of its
    /// own, both complete while the hold is held.
    fn assert_unrelated_requests_complete(&self) {
        let runner = self.runner.clone();
        completes("the status reply", move || runner.status());
        let (other, _) = job_at(&self.fixture, &format!("other-{}", Uuid::now_v7()), "true");
        let runner = self.runner.clone();
        let admission = completes("another job's admission", move || {
            runner.accept(other).unwrap()
        });
        assert!(matches!(admission, Admission::Accepted(_)));
        assert!(self.hold.held(), "the held save is still in progress");
    }
}

/// Run `work` on its own thread and require it to finish while a save is
/// held. A `work` that waits for the held save fails the test instead of
/// hanging it.
fn completes<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let worker = std::thread::spawn(work);
    assert!(
        poll(Duration::from_secs(10), || worker.is_finished()),
        "{what} waited for the held save"
    );
    worker.join().unwrap()
}

/// The admission record save runs without the state lock. The admission
/// still counts against the queue limit while it saves, so a concurrent
/// admission cannot take the same last slot; a duplicate Run waits for it
/// and is then told the job exists.
#[test]
fn admission_save_holds_no_lock_and_keeps_its_queue_slot() {
    let f = SaveFixture::with("record_save", |config| config.max_queue_depth = 2);
    let (first, _) = job_at(&f.fixture, "first", "true");
    assert_run_accepted(&f.runner, &first);
    let (last, _) = job_at(&f.fixture, "last", "true");
    let id = last.job_id;
    f.hold_save(id, Saved::Nothing);
    let held = f.hold_in({
        let last = last.clone();
        move |runner| {
            assert!(matches!(
                runner.accept(last).unwrap(),
                Admission::Accepted(1)
            ))
        }
    });
    let runner = f.runner.clone();
    completes("the status reply", move || runner.status());
    let (late, _) = job_at(&f.fixture, "late", "true");
    let runner = f.runner.clone();
    let late = completes("a racing admission", move || runner.accept(late).unwrap());
    assert!(
        matches!(late, Admission::Refused(RefusalReason::RunnerFull)),
        "a racing admission took the last queue slot"
    );
    // Not a job until its record is durable.
    assert!(!f.runner.has_job_record(id));
    let duplicate = {
        let runner = f.runner.clone();
        let last = last.clone();
        std::thread::spawn(move || runner.accept(last).unwrap())
    };
    assert!(f.hold.held());
    drop(held);
    assert!(matches!(duplicate.join().unwrap(), Admission::Duplicate));
    let saved: Job = read_json(&f.runner.record_path(id)).unwrap();
    assert_eq!(saved.state, JobState::AwaitingBundles);
    assert_eq!(f.job(id).unwrap().state, JobState::AwaitingBundles);
}

/// The fetch's Queued save runs without the state lock. Until it is durable
/// the job is not queued in memory, so nothing dispatches it, and a cancel
/// waits for it instead of saving a terminal the Queued record would then
/// overwrite.
#[test]
fn queued_save_holds_no_lock_and_publishes_only_once_durable() {
    let f = SaveFixture::new("record_save");
    let (request, bytes) = job_at(&f.fixture, "fetched", "true");
    let id = request.job_id;
    assert_run_accepted(&f.runner, &request);
    let upload = f.fixture.spool(&f.runner, &request, &bytes);
    let directory = upload.directory.clone();
    f.hold_save(id, Saved::Admitted);
    let held = f.hold_in(move |runner| runner.fetch_run(&request, &directory, true).unwrap());
    f.assert_unrelated_requests_complete();
    let runner = f.runner.clone();
    completes("a dispatch pass", move || runner.tick().unwrap());
    assert_eq!(f.job(id).unwrap().state, JobState::AwaitingBundles);
    assert!(!f.runner.core.inner.lock().unwrap().starting.contains(&id));
    let cancel = {
        let runner = f.runner.clone();
        std::thread::spawn(move || runner.cancel(id).unwrap())
    };
    assert!(f.hold.held());
    drop(held);
    cancel.join().unwrap();
    drop(upload);
    let saved: Job = read_json(&f.runner.record_path(id)).unwrap();
    assert_eq!(saved.state, JobState::Tombstone);
    assert_eq!(
        saved.terminal.unwrap().outcome,
        Outcome::Cancelled,
        "the cancel's terminal is the durable record"
    );
    assert_eq!(f.terminal(id).unwrap().outcome, Outcome::Cancelled);
}

/// Dispatch reserves a job's permits, CPUs and workspace under the state
/// lock and its setup thread saves the setup record without it, before any
/// setup work. Meanwhile the reservation keeps another pass from starting
/// the job again or starting another job in its workspace.
#[test]
fn setup_save_holds_no_lock_and_keeps_the_reservation() {
    let f = SaveFixture::new("record_save");
    let first = f.queued("shared", "true");
    f.hold_save(first.job_id, Saved::Queued);
    let held = f.hold_in(|runner| runner.tick().unwrap());
    f.assert_unrelated_requests_complete();
    let second = f.queued_while_held("shared", "true");
    let runner = f.runner.clone();
    completes("a dispatch pass", move || runner.tick().unwrap());
    {
        // Read under the lock, but assert after releasing it: a failed
        // assertion must not poison the lock for the teardown.
        let (starting, reserved, held_weight, weight) = {
            let inner = f.runner.core.inner.lock().unwrap();
            (
                inner.starting.clone(),
                inner.reserved.contains_key(&first.workspace_key),
                Runner::held_weight(&inner),
                inner.jobs[&first.job_id].weight,
            )
        };
        assert_eq!(starting, BTreeSet::from([first.job_id]));
        assert!(reserved);
        assert_eq!(
            held_weight, weight,
            "the held job's permits were granted once"
        );
    }
    assert_eq!(
        saved(&f.runner.record_path(first.job_id)),
        Some(Saved::Queued),
        "set up before its setup record is durable"
    );
    drop(held);
    f.tick_until("both jobs finish in turn", |f| {
        f.terminal(first.job_id).is_some() && f.terminal(second.job_id).is_some()
    });
    for id in [first.job_id, second.job_id] {
        assert_eq!(f.terminal(id).unwrap().outcome, Outcome::Exit { code: 0 });
    }
}

/// The launch writes the catalog, spawns the job and saves its Started
/// record without the state lock. The job stays reserved and invisible as
/// started until that record is durable, and its gate stays closed.
#[test]
fn started_save_holds_no_lock_and_keeps_the_gate_closed() {
    let f = SaveFixture::new("record_save");
    let marker = f.fixture.scratch.join("ran");
    let first = f.queued("shared", &format!("touch {}", marker.display()));
    let id = first.job_id;
    f.hold_save(id, Saved::SetUp);
    let held = f.hold_in(|runner| runner.tick().unwrap());
    f.assert_unrelated_requests_complete();
    let second = f.queued_while_held("shared", "true");
    let runner = f.runner.clone();
    completes("a dispatch pass", move || runner.tick().unwrap());
    {
        let (starting, reserved, state) = {
            let inner = f.runner.core.inner.lock().unwrap();
            (
                inner.starting.clone(),
                inner.reserved.contains_key(&first.workspace_key),
                inner.jobs[&id].state,
            )
        };
        assert_eq!(starting, BTreeSet::from([id]));
        assert!(reserved);
        assert_eq!(state, JobState::Queued);
    }
    assert!(
        !f.runner
            .replay(id, 0)
            .unwrap()
            .iter()
            .any(|frame| matches!(frame, Frame::Started { .. })),
        "replay showed a Started that is not durable"
    );
    // Only Linux launches hold the command behind a gate.
    #[cfg(target_os = "linux")]
    assert!(
        !marker.exists(),
        "the command ran before its start was durable"
    );
    drop(held);
    f.tick_until("both jobs finish in turn", |f| {
        f.terminal(id).is_some() && f.terminal(second.job_id).is_some()
    });
    assert_eq!(f.terminal(id).unwrap().outcome, Outcome::Exit { code: 0 });
    assert!(marker.exists());
}

/// A withdrawal's terminal record is saved without the state lock. Until it
/// is durable, dispatch does not start the job and a drain does not refuse
/// it with a record of its own.
#[test]
fn withdrawal_save_holds_no_lock_and_the_job_is_not_dispatched() {
    let f = SaveFixture::new("record_save");
    let (mut request, bytes) = job_at(&f.fixture, "withdrawn", "true");
    request.queue_wait_limit_s = 0;
    let id = request.job_id;
    f.fixture.submit(&f.runner, &request, &bytes);
    f.hold_save(id, Saved::Queued);
    let held = f.hold_in(move |runner| {
        assert_eq!(runner.withdraw(id).unwrap(), WithdrawOutcome::Withdrawn)
    });
    f.assert_unrelated_requests_complete();
    let runner = f.runner.clone();
    completes("a dispatch pass", move || runner.tick().unwrap());
    assert!(!f.runner.core.inner.lock().unwrap().starting.contains(&id));
    f.runner.begin_drain(Duration::from_secs(3600), false);
    let runner = f.runner.clone();
    completes("a draining pass", move || runner.tick().unwrap());
    assert!(f.hold.held());
    drop(held);
    let saved: Job = read_json(&f.runner.record_path(id)).unwrap();
    let refused = Outcome::RefusedBeforeStart {
        reason: RefusalReason::QueueWaitExceeded,
    };
    assert_eq!(saved.terminal.unwrap().outcome, refused);
    assert_eq!(f.terminal(id).unwrap().outcome, refused);
    f.runner.resume();
}

/// A finishing job writes its workspace's catalog record without the state
/// lock, and before its terminal is published.
#[test]
fn finish_catalog_save_holds_no_lock_and_precedes_the_terminal() {
    let f = SaveFixture::new("catalog_save");
    let release = f.fixture.scratch.join("release");
    let request = f.queued(
        "finishing",
        &format!("while [ ! -e {} ]; do sleep 0.01; done", release.display()),
    );
    let id = request.job_id;
    f.tick_until("the job runs", |f| {
        f.job(id).is_some_and(|job| job.state == JobState::Running)
    });
    let finished_ms = f.fixture.clock.fetch_add(1000, Ordering::SeqCst) + 1000;
    f.hold.arm();
    let held = f.hold_in(move |_| fs::write(release, b"").unwrap());
    f.assert_unrelated_requests_complete();
    assert!(
        f.terminal(id).is_none(),
        "the terminal was published before the catalog was written"
    );
    drop(held);
    assert!(poll(Duration::from_secs(30), || f.terminal(id).is_some()));
    let saved: Catalog = read_json(&f.runner.core.trees.state.join("catalog.json")).unwrap();
    assert_eq!(
        saved.workspaces[&request.workspace_key].used_ms,
        finished_ms
    );
}

/// Maintenance writes the catalog without the state lock.
#[test]
fn maintenance_catalog_save_holds_no_lock() {
    let f = SaveFixture::new("catalog_save");
    f.hold.arm();
    let held = f.hold_in(|runner| runner.maintenance().unwrap());
    f.assert_unrelated_requests_complete();
    drop(held);
}

/// A workspace prepare writes the catalog without the state lock, and keeps
/// its workspace reserved until the workspace's catalog entry is durable:
/// drop and retention find the workspaces they delete through those entries.
#[test]
fn prepare_catalog_save_holds_no_lock_and_keeps_the_reservation() {
    let f = SaveFixture::new("catalog_save");
    // A prepare checks out from the mirror, which a fetched job fills.
    f.queued("seed", "true");
    let (request, _) = job_at(&f.fixture, "prepared", "true");
    let prepare = prepare_for(&request);
    let key = prepare.workspace_key.clone();
    f.hold.arm();
    let held = f.hold_in(move |runner| runner.prepare(&prepare, true).unwrap());
    f.assert_unrelated_requests_complete();
    assert!(Runner::occupied(&f.runner.core.inner.lock().unwrap(), &key));
    drop(held);
    assert!(!Runner::occupied(
        &f.runner.core.inner.lock().unwrap(),
        &key
    ));
    let saved: Catalog = read_json(&f.runner.core.trees.state.join("catalog.json")).unwrap();
    assert!(saved.workspaces.contains_key(&key));
}

/// Catalog snapshots are numbered under the state lock and may be written
/// after it, in any order. An older one never replaces a newer one on disk.
#[test]
fn an_older_catalog_snapshot_never_overwrites_a_newer_one() {
    let f = SaveFixture::new("catalog_save");
    let (older, newer) = {
        let mut inner = f.runner.core.inner.lock().unwrap();
        inner
            .catalog
            .workspaces
            .insert("/older".into(), Workspace::default());
        let older = f.runner.catalog_snapshot(&inner).unwrap();
        inner
            .catalog
            .workspaces
            .insert("/newer".into(), Workspace::default());
        (older, f.runner.catalog_snapshot(&inner).unwrap())
    };
    f.runner.write_catalog(newer).unwrap();
    f.runner.write_catalog(older).unwrap();
    let saved: Catalog = read_json(&f.runner.core.trees.state.join("catalog.json")).unwrap();
    assert!(saved.workspaces.contains_key("/older"));
    assert!(saved.workspaces.contains_key("/newer"));
}

/// A catalog write can rename its file into place and still fail, in the
/// directory sync that follows. The test injects its failure at that point,
/// after the rename and before the sync: the newer bytes are then what the
/// path holds, and an older snapshot written later must not replace them.
#[test]
fn a_renamed_catalog_is_never_replaced_by_an_older_snapshot() {
    let f = SaveFixture::new("catalog_save");
    let (older, newer) = {
        let mut inner = f.runner.core.inner.lock().unwrap();
        inner
            .catalog
            .workspaces
            .insert("/older".into(), Workspace::default());
        let older = f.runner.catalog_snapshot(&inner).unwrap();
        inner
            .catalog
            .workspaces
            .insert("/newer".into(), Workspace::default());
        (older, f.runner.catalog_snapshot(&inner).unwrap())
    };
    f.fail_catalog(Some(CatalogWrite::Renamed));
    assert!(f.runner.write_catalog(newer).is_err());
    f.fail_catalog(None);
    f.runner.write_catalog(older).unwrap();
    let saved: Catalog = read_json(&f.runner.core.trees.state.join("catalog.json")).unwrap();
    assert!(saved.workspaces.contains_key("/newer"));
}

/// A command that runs until `release` exists, so a test can finish it at a
/// moment of its choosing.
fn waits_for(release: &Path) -> String {
    format!("while [ ! -e {} ]; do sleep 0.01; done", release.display())
}

/// A finishing job's catalog update is durable before its terminal record
/// is: a crash between the two saves leaves the workspace's last use
/// recorded, never a finished job whose workspace looks long unused.
#[test]
fn finish_writes_the_catalog_before_the_terminal_record() {
    let f = SaveFixture::new("record_save");
    let release = f.fixture.scratch.join("release");
    let request = f.queued("finishing", &waits_for(&release));
    let id = request.job_id;
    f.tick_until("the job runs", |f| {
        f.job(id).is_some_and(|job| job.state == JobState::Running)
    });
    let finished_ms = f.fixture.clock.fetch_add(1000, Ordering::SeqCst) + 1000;
    f.hold_save(id, Saved::Started);
    let held = f.hold_in(move |_| fs::write(release, b"").unwrap());
    let saved: Catalog = read_json(&f.runner.core.trees.state.join("catalog.json")).unwrap();
    assert_eq!(
        saved.workspaces[&request.workspace_key].used_ms, finished_ms,
        "the terminal record is being saved before the catalog records the finish"
    );
    assert!(f.terminal(id).is_none());
    drop(held);
    assert!(poll(Duration::from_secs(30), || f.terminal(id).is_some()));
}

/// A failed catalog write at finish keeps the job being finalized: its
/// terminal is neither saved nor published, and both saves are retried.
#[test]
fn failed_finish_catalog_write_is_retried_before_the_terminal() {
    let f = SaveFixture::new("record_save");
    let release = f.fixture.scratch.join("release");
    let request = f.queued("finishing", &waits_for(&release));
    let id = request.job_id;
    f.tick_until("the job runs", |f| {
        f.job(id).is_some_and(|job| job.state == JobState::Running)
    });
    let finished_ms = f.fixture.clock.fetch_add(1000, Ordering::SeqCst) + 1000;
    f.fail_catalog(Some(CatalogWrite::Starting));
    fs::write(&release, b"").unwrap();
    assert!(
        poll(Duration::from_secs(30), || f.catalog_failures() >= 2),
        "the finish did not retry its catalog write"
    );
    assert!(f.terminal(id).is_none(), "published without its catalog");
    assert!(f.runner.core.inner.lock().unwrap().finalizing.contains(&id));
    assert_eq!(
        saved(&f.runner.record_path(id)),
        Some(Saved::Started),
        "terminal record saved before its catalog"
    );
    f.fail_catalog(None);
    assert!(poll(Duration::from_secs(30), || f.terminal(id).is_some()));
    let saved: Catalog = read_json(&f.runner.core.trees.state.join("catalog.json")).unwrap();
    assert_eq!(
        saved.workspaces[&request.workspace_key].used_ms,
        finished_ms
    );
}

/// Startup finds a finished job's terminal record with a catalog that last
/// saw its workspace in use long before, as an older runner could leave
/// after a crash between the two saves. The workspace's last use is raised
/// to the job's finish before startup maintenance looks for expired
/// workspaces.
#[test]
fn restart_counts_a_finished_job_as_its_workspace_last_use() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    config.disable_warm_builds = true;
    let now = (config.clock)();
    let (request, _) = f.run("true");
    let id = request.job_id;
    let mut job = Job::new(id, Some(request.clone()), now - 20_000, 1, 2);
    job.started_ms = Some(now - 10_000);
    job.terminal(Outcome::Exit { code: 0 }, now - 1000);
    let state = config.state_dir();
    atomic_json(
        &state.join("jobs").join(id.to_string()).join("record.json"),
        &job,
    )
    .unwrap();
    let mut catalog = Catalog::default();
    catalog.workspaces.insert(
        request.workspace_key.clone(),
        Workspace {
            repository: request.repository_root.clone(),
            used_ms: now - RETENTION_MS - 5000,
            ..Workspace::default()
        },
    );
    atomic_json(&state.join("catalog.json"), &catalog).unwrap();
    fs::create_dir_all(&request.workspace_key).unwrap();
    let runner = Runner::open(config).unwrap();
    let used_ms = runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .get(&request.workspace_key)
        .map(|workspace| workspace.used_ms);
    assert_eq!(used_ms, Some(now - 1000), "startup expired the workspace");
    assert!(Path::new(&request.workspace_key).exists());
}

/// A job's CPUs, setup time, workspace rotation and queue clock (entry,
/// elapsed, initialized).
type DispatchState = (Vec<u32>, Option<u64>, Option<u64>, (u64, u64, bool));

/// The state a dispatch attempt changes, to compare before and after one
/// that is undone: the job's CPUs, setup time and queue clock, and its
/// workspace's rotation.
fn dispatch_state(f: &SaveFixture, id: Uuid) -> DispatchState {
    let inner = f.runner.core.inner.lock().unwrap();
    let job = &inner.jobs[&id];
    let clock = QueueClock::of(job);
    (
        job.cpus.clone(),
        job.setup_ms,
        inner.rotation.get(job.key()).copied(),
        (clock.entry, clock.elapsed, clock.initialized),
    )
}

/// Fail the catalog write of `id`'s next launch, then return its state, the
/// warm-build writer slot and whether its workspace is still reserved.
fn fail_launch_catalog_write(f: &SaveFixture, id: Uuid) -> (JobState, Option<Uuid>, bool) {
    let failures = f.catalog_failures();
    f.fail_catalog(Some(CatalogWrite::Starting));
    f.runner.tick().unwrap();
    assert!(
        poll(Duration::from_secs(60), || {
            f.catalog_failures() > failures
                && !f.runner.core.inner.lock().unwrap().starting.contains(&id)
        }),
        "the launch did not fail on its catalog write"
    );
    f.fail_catalog(None);
    let inner = f.runner.core.inner.lock().unwrap();
    (
        inner.jobs[&id].state,
        inner.writer,
        inner.reserved.contains_key(inner.jobs[&id].key()),
    )
}

/// A failed catalog write at launch puts the job back in the queue exactly
/// as dispatch found it: no CPUs, setup time or rotation of the attempt are
/// left behind and its queue clock is as it was, so it runs on a later pass.
#[test]
fn failed_launch_catalog_write_requeues_the_job_as_dispatch_found_it() {
    let f = SaveFixture::new("catalog_save");
    let id = f.queued("launched", "true").job_id;
    // The fetch started the job's queue clock; a dispatch pass leaves it
    // alone until it starts the job.
    let expected = dispatch_state(&f, id);
    let (state, _, reserved) = fail_launch_catalog_write(&f, id);
    assert_eq!(state, JobState::Queued);
    assert!(!reserved, "the job kept its workspace reservation");
    assert_eq!(dispatch_state(&f, id), expected, "the job was not restored");
    f.pass_start_retry_delay(START_RETRY_MS);
    f.tick_until("the job runs after the failed launch", |f| {
        f.terminal(id).is_some()
    });
    assert_eq!(f.terminal(id).unwrap().outcome, Outcome::Exit { code: 0 });
}

/// The same for a warm build, which must also give back the writer slot:
/// while it is held, no warm build is dispatched at all.
#[test]
fn failed_launch_catalog_write_gives_back_the_warm_build_writer() {
    let f = SaveFixture::with("catalog_save", |config| {
        config.disable_warm_builds = false;
    });
    // The default-branch fetch queues a warm build. Cancelling the job
    // itself leaves the warm build as the only queued job.
    let id = f.queued("launched", "true").job_id;
    f.runner.cancel(id).unwrap();
    let warm = {
        let inner = f.runner.core.inner.lock().unwrap();
        inner.jobs.values().find(|job| job.base).map(|job| job.id)
    }
    .expect("the default-branch fetch queued a warm build");
    let (cpus, setup_ms, rotation, _) = dispatch_state(&f, warm);
    let (state, writer, reserved) = fail_launch_catalog_write(&f, warm);
    assert_eq!(state, JobState::Queued);
    assert_eq!(writer, None, "the warm build kept the writer slot");
    assert!(!reserved, "the warm build kept its reservation");
    let (cpus_after, setup_after, rotation_after, _) = dispatch_state(&f, warm);
    assert_eq!(
        (cpus_after, setup_after, rotation_after),
        (cpus, setup_ms, rotation)
    );
    f.pass_start_retry_delay(START_RETRY_MS);
    f.tick_until("the warm build runs after the failed launch", |f| {
        f.terminal(warm).is_some()
    });
}

/// A prepare that fails after reserving its workspace (here, at a base
/// commit the mirror does not have) releases the reservation.
#[test]
fn failed_prepare_releases_its_workspace_reservation() {
    let f = SaveFixture::new("catalog_save");
    // The mirror a prepare checks out from is filled by a fetched job.
    f.queued("seed", "true");
    let (request, _) = job_at(&f.fixture, "prepared", "true");
    let mut prepare = prepare_for(&request);
    let key = prepare.workspace_key.clone();
    let valid = prepare.base_commit.clone();
    prepare.base_commit = "1".repeat(40);
    assert!(f.runner.prepare(&prepare, false).is_err());
    assert!(
        !Runner::occupied(&f.runner.core.inner.lock().unwrap(), &key),
        "the failed prepare kept its workspace reserved"
    );
    prepare.base_commit = valid;
    f.runner.prepare(&prepare, false).unwrap();
}

/// A withdrawal whose save fails leaves the job exactly as it was: its
/// accrued queue wait intact and its place in the queue kept. While the
/// save is pending, a younger job in the same workspace does not start.
#[test]
fn failed_withdrawal_keeps_the_queue_clock_and_the_job_s_place() {
    let f = SaveFixture::new("record_save");
    let (mut first, bytes) = job_at(&f.fixture, "shared", "true");
    first.queue_wait_limit_s = 0;
    let id = first.job_id;
    f.fixture.submit(&f.runner, &first, &bytes);
    let younger = f.queued("shared", "true");
    f.fixture.clock.fetch_add(5000, Ordering::SeqCst);
    let waited = |f: &SaveFixture| f.job(id).unwrap().queue_wait_ms(f.runner.now());
    assert_eq!(waited(&f), 5000);
    f.hold_save(id, Saved::Queued);
    f.fail_save(id, Saved::Queued);
    let (sender, outcome) = std::sync::mpsc::channel();
    let held = f.hold_in(move |runner| sender.send(runner.withdraw(id).is_err()).unwrap());
    let runner = f.runner.clone();
    completes("a dispatch pass", move || runner.tick().unwrap());
    assert!(
        f.runner.core.inner.lock().unwrap().starting.is_empty(),
        "a job started past the job being withdrawn"
    );
    drop(held);
    assert!(outcome.recv().unwrap(), "the withdrawal did not fail");
    assert_eq!(f.job(id).unwrap().state, JobState::Queued);
    assert_eq!(
        waited(&f),
        5000,
        "the failed withdrawal changed the queue wait"
    );
    f.runner.tick().unwrap();
    assert_eq!(
        f.runner.core.inner.lock().unwrap().starting,
        BTreeSet::from([id]),
        "the job lost its place to {}",
        younger.job_id
    );
}

/// A save that unwinds still clears the markers set around it: the
/// admission's queue slot, the withdrawal's dispatch fence and the setup's
/// reservation. It also poisons the job's log lock; every later step for
/// the job (admission, withdrawal, setup, the run itself) still works.
#[test]
fn unwinding_saves_release_their_markers() {
    let f = SaveFixture::with("record_save", |config| config.max_queue_depth = 2);
    // Admission.
    let (admitted, _) = job_at(&f.fixture, "admitted", "true");
    f.panic_save(admitted.job_id, Saved::Nothing);
    let runner = f.runner.clone();
    let admission = admitted.clone();
    assert!(std::thread::spawn(move || runner.accept(admission))
        .join()
        .is_err());
    // The panic poisoned the job log lock; the ID can still be admitted.
    assert!(matches!(
        f.runner.accept(admitted.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    f.runner.cancel(admitted.job_id).unwrap();
    assert!(f
        .runner
        .core
        .inner
        .lock()
        .unwrap()
        .admission_saves
        .is_empty());

    // Withdrawal.
    let (mut withdrawn, bytes) = job_at(&f.fixture, "withdrawn", "true");
    withdrawn.queue_wait_limit_s = 0;
    let id = withdrawn.job_id;
    f.fixture.submit(&f.runner, &withdrawn, &bytes);
    f.fixture.clock.fetch_add(5000, Ordering::SeqCst);
    f.panic_save(id, Saved::Queued);
    let runner = f.runner.clone();
    assert!(std::thread::spawn(move || runner.withdraw(id))
        .join()
        .is_err());
    assert!(f
        .runner
        .core
        .inner
        .lock()
        .unwrap()
        .terminal_saves
        .is_empty());
    let job = f.job(id).unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.queue_wait_ms(f.runner.now()), 5000);

    // Setup: the panic is on the setup thread.
    f.panic_save(id, Saved::Queued);
    f.runner.tick().unwrap();
    assert!(
        poll(Duration::from_secs(30), || {
            let inner = f.runner.core.inner.lock().unwrap();
            inner.starting.is_empty() && inner.reserved.is_empty()
        }),
        "the unwound setup kept its reservation"
    );
    assert_eq!(f.job(id).unwrap().setup_ms, None);
    f.pass_start_retry_delay(START_RETRY_MS);
    f.tick_until("the job runs once its setup can be saved", |f| {
        f.terminal(id).is_some()
    });
}

/// A workspace drop that finds an admission still saving its record
/// cancels it once saved; if that save fails, nothing is left for the ID.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_drop_leaves_nothing_for_an_admission_whose_save_failed() {
    let f = SaveFixture::new("record_save");
    let (request, _) = f.fixture.run("true");
    let id = request.job_id;
    let key = request.workspace_key.clone();
    f.hold_save(id, Saved::Nothing);
    f.fail_save(id, Saved::Nothing);
    let held = f.hold_in(move |runner| assert!(runner.accept(request).is_err()));
    let dropping = {
        let runner = f.runner.clone();
        let key = key.clone();
        tokio::spawn(async move { runner.drop_workspace(&key).await })
    };
    assert!(poll(Duration::from_secs(10), || f
        .runner
        .core
        .inner
        .lock()
        .unwrap()
        .dropping
        .contains(&key)));
    drop(held);
    let reply = dropping.await.unwrap().unwrap();
    assert!(!reply.cancelled_jobs.contains(&id));
    assert!(!f.runner.has_job_record(id));
    assert!(!f.runner.record_path(id).exists());
}

/// A catalog write skipped because a newer snapshot was renamed into place
/// is acknowledged only once that newer snapshot is durable: if the newer
/// write's directory sync failed, the skipped write retries the sync, and
/// fails if that fails.
#[test]
fn a_skipped_catalog_write_is_acknowledged_only_once_durable() {
    let f = SaveFixture::new("catalog_save");
    let (older, newer) = {
        let mut inner = f.runner.core.inner.lock().unwrap();
        inner
            .catalog
            .workspaces
            .insert("/older".into(), Workspace::default());
        let older = f.runner.catalog_snapshot(&inner).unwrap();
        inner
            .catalog
            .workspaces
            .insert("/newer".into(), Workspace::default());
        (older, f.runner.catalog_snapshot(&inner).unwrap())
    };
    let (again, latest) = {
        let inner = f.runner.core.inner.lock().unwrap();
        (
            f.runner.catalog_snapshot(&inner).unwrap(),
            f.runner.catalog_snapshot(&inner).unwrap(),
        )
    };
    f.fail_catalog(Some(CatalogWrite::Renamed));
    assert!(f.runner.write_catalog(newer).is_err());
    f.fail_catalog(Some(CatalogWrite::SyncRetry));
    assert!(
        f.runner.write_catalog(older).is_err(),
        "acknowledged a catalog whose directory sync had failed"
    );
    f.fail_catalog(None);
    f.runner.write_catalog(again).unwrap();
    f.runner.write_catalog(latest).unwrap();
    let saved: Catalog = read_json(&f.runner.core.trees.state.join("catalog.json")).unwrap();
    assert!(saved.workspaces.contains_key("/older"));
    assert!(saved.workspaces.contains_key("/newer"));
}

/// Dispatch does not start a job while an earlier attempt to start it is
/// still being cleaned up (a Windows VM whose stop maintenance retries):
/// that attempt's files are named by the job's ID.
#[test]
fn dispatch_waits_for_an_earlier_attempt_s_cleanup() {
    let f = SaveFixture::new("record_save");
    let id = f.queued("cleaning", "true").job_id;
    let runtime = f.runner.config().windows_runtime.clone();
    runtime.set_cleanup_pending(id, true);
    f.runner.tick().unwrap();
    assert!(
        !f.runner.core.inner.lock().unwrap().starting.contains(&id),
        "started while an earlier attempt's cleanup was pending"
    );
    runtime.set_cleanup_pending(id, false);
    f.runner.tick().unwrap();
    assert!(f.runner.core.inner.lock().unwrap().starting.contains(&id));
}

/// A cancel can end a job after its setup record is saved and before its
/// setup completes. If the setup then unwinds, its reservation is released
/// even though there is no queued job left to put back.
#[test]
fn a_start_unwinding_after_its_job_was_cancelled_releases_the_reservation() {
    let f = SaveFixture::new("materialize");
    let id = f.queued("cancelled", "true").job_id;
    f.hold.arm();
    f.panic_work("materialize");
    let held = f.hold_in(|runner| runner.tick().unwrap());
    f.runner.cancel(id).unwrap();
    assert_eq!(f.terminal(id).unwrap().outcome, Outcome::Cancelled);
    drop(held);
    assert!(
        poll(Duration::from_secs(30), || {
            let inner = f.runner.core.inner.lock().unwrap();
            inner.starting.is_empty() && inner.reserved.is_empty()
        }),
        "the unwound start kept the cancelled job's reservation"
    );
}

/// A job whose start keeps failing is not set up again on every scheduler
/// pass: each failure doubles the delay before it is dispatched again.
#[test]
fn a_failing_start_is_retried_after_a_growing_delay() {
    let f = SaveFixture::new("catalog_save");
    let id = f.queued("failing", "true").job_id;
    f.fail_catalog(Some(CatalogWrite::Starting));
    let failed_once = |f: &SaveFixture, failures: usize| {
        poll(Duration::from_secs(60), || {
            f.catalog_failures() == failures
                && !f.runner.core.inner.lock().unwrap().starting.contains(&id)
        })
    };
    let starts = |f: &SaveFixture| {
        f.runner.tick().unwrap();
        f.runner.core.inner.lock().unwrap().starting.contains(&id)
    };
    assert!(starts(&f));
    assert!(failed_once(&f, 1));
    for _ in 0..5 {
        assert!(!starts(&f), "set up again before the retry delay passed");
    }
    f.pass_start_retry_delay(START_RETRY_MS);
    assert!(starts(&f));
    assert!(failed_once(&f, 2));
    // The delay doubled: one more first delay is not enough.
    f.pass_start_retry_delay(START_RETRY_MS);
    assert!(!starts(&f), "the retry delay did not grow");
    f.fail_catalog(None);
    f.pass_start_retry_delay(START_RETRY_MS);
    f.tick_until("the job runs once its catalog can be saved", |f| {
        f.terminal(id).is_some()
    });
}

/// Queue clocks change at the time after a section run without the state
/// lock, not at a time taken before it: a job paused behind a workspace
/// owner that finishes during another job's Queued save resumes then, and
/// is not credited with the wait that passed before.
#[test]
fn queue_clocks_resume_at_the_time_after_a_fetch_save() {
    let f = SaveFixture::new("record_save");
    let (owner_request, _) = job_at(&f.fixture, "owned", "true");
    let owner = owner_request.job_id;
    {
        let now = f.runner.now();
        let mut job = Job::new(owner, Some(owner_request), now, 1, 2);
        job.state = JobState::Running;
        job.started_ms = Some(now);
        f.runner.core.inner.lock().unwrap().jobs.insert(owner, job);
    }
    let paused = f.queued("owned", "true").job_id;
    assert_eq!(
        f.job(paused).unwrap().queue_entry.load(Ordering::Acquire),
        PAUSED_QUEUE,
        "the job behind the running owner was not paused"
    );
    let (other, bytes) = job_at(&f.fixture, "other", "true");
    assert_run_accepted(&f.runner, &other);
    let upload = f.fixture.spool(&f.runner, &other, &bytes);
    let directory = upload.directory.clone();
    f.hold_save(other.job_id, Saved::Admitted);
    let held = f.hold_in(move |runner| runner.fetch_run(&other, &directory, false).unwrap());
    f.fixture.clock.fetch_add(10_000, Ordering::SeqCst);
    {
        let mut inner = f.runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&owner).unwrap();
        job.terminal(Outcome::Exit { code: 0 }, f.runner.now());
    }
    drop(held);
    drop(upload);
    assert_eq!(
        f.job(paused).unwrap().queue_wait_ms(f.runner.now()),
        0,
        "the job was credited with wait from before the save"
    );
}

/// The same for a scheduler pass that reads warm-build inputs from a mirror
/// without the state lock before dispatching.
#[test]
fn queue_clocks_change_at_the_time_after_a_pass_s_mirror_reads() {
    let f = SaveFixture::with("base_plan", |config| {
        config.disable_warm_builds = false;
    });
    let (owner_request, _) = job_at(&f.fixture, "owned", "true");
    let owner = owner_request.job_id;
    {
        let now = f.runner.now();
        let mut job = Job::new(owner, Some(owner_request), now, 1, 2);
        job.state = JobState::Running;
        job.started_ms = Some(now);
        f.runner.core.inner.lock().unwrap().jobs.insert(owner, job);
    }
    // Fetched as a non-default tip, so it queues no warm build itself.
    let (request, bytes) = job_at(&f.fixture, "owned", "true");
    let paused = request.job_id;
    assert_run_accepted(&f.runner, &request);
    let upload = f.fixture.spool(&f.runner, &request, &bytes);
    f.runner
        .fetch_run(&request, &upload.directory, false)
        .unwrap();
    assert_eq!(
        f.job(paused).unwrap().queue_entry.load(Ordering::Acquire),
        PAUSED_QUEUE
    );
    {
        // A repository that wants a warm build makes the pass read the mirror.
        let mut inner = f.runner.core.inner.lock().unwrap();
        let repository = inner
            .catalog
            .repositories
            .entry(request.repository_root.clone())
            .or_default();
        repository.desired = Some(BaseRequest {
            tip: request.integration_tip.clone(),
            workspace: request.workspace_key.clone(),
            siblings: vec![],
            git_dependencies: vec![],
            command: None,
        });
        repository.requested_descriptor = Some("wanted".into());
    }
    f.hold.arm();
    let held = f.hold_in(|runner| runner.tick().unwrap());
    f.fixture.clock.fetch_add(10_000, Ordering::SeqCst);
    {
        let mut inner = f.runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&owner).unwrap();
        job.terminal(Outcome::Exit { code: 0 }, f.runner.now());
    }
    drop(held);
    drop(upload);
    // The pass resumed the job's clock and then started it, stopping the
    // clock again at the current time.
    let job = f.job(paused).unwrap();
    assert!(job.setup_ms.is_some(), "the job was not started");
    assert_eq!(
        job.queue_wait_ms(f.runner.now()),
        0,
        "the job was credited with wait from before the mirror reads"
    );
}

/// Ranks the call sites that hold the runner's state lock while real jobs
/// start, print, get cancelled and finish on a disk kept busy by a separate
/// fsync loop, then while maintenance expires their records. Not a pass/fail
/// test: run it with `--ignored --nocapture` and read the table.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "load test: run with --ignored --nocapture to rank state lock holders"]
async fn state_lock_holders_under_load() {
    let f = Fixture::new();
    let mut config = f.config();
    // MOTOR_LOAD_SAVE_MS stretches every job record save by that many
    // milliseconds, to see holds as a host whose fsyncs take that long would.
    if let Some(delay) = std::env::var("MOTOR_LOAD_SAVE_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        config.before_job_write = Some(Arc::new(move |path| {
            if path.ends_with("record.json") {
                std::thread::sleep(Duration::from_millis(delay));
            }
            Ok(())
        }));
    }
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    // Other jobs on a shared build host keep its disk busy; fsyncs under the
    // state lock cost what that contention makes them cost.
    let stop = Arc::new(AtomicBool::new(false));
    let disk = {
        let stop = stop.clone();
        let path = f.scratch.join("disk-load");
        std::thread::spawn(move || {
            let block = vec![7u8; 4 << 20];
            while !stop.load(Ordering::SeqCst) {
                let mut file = File::create(&path).unwrap();
                file.write_all(&block).unwrap();
                file.sync_all().unwrap();
            }
        })
    };
    let jobs = std::env::var("MOTOR_LOAD_JOBS")
        .ok()
        .and_then(|jobs| jobs.parse().ok())
        .unwrap_or(24);
    let mut ids = Vec::new();
    for job in 0..jobs {
        let (request, bytes) = job_at(
            &f,
            &format!("load-{job}"),
            "for i in $(seq 1 300); do echo line $i; done; sleep 3",
        );
        f.submit(&runner, &request, &bytes);
        ids.push(request.job_id);
    }
    for id in ids.iter().step_by(2) {
        until(|| {
            runner.core.inner.lock().unwrap().jobs[id].state != JobState::Queued
                || runner.core.inner.lock().unwrap().jobs[id]
                    .terminal
                    .is_some()
        })
        .await;
        runner.cancel(*id).unwrap();
    }
    for id in &ids {
        terminal(&runner, *id).await;
    }
    f.clock.fetch_add(RETENTION_MS + 1, Ordering::SeqCst);
    runner.maintenance().unwrap();
    stop.store(true, Ordering::SeqCst);
    disk.join().unwrap();
    println!("state lock holds, {jobs} jobs (site: count, total ms, longest ms):");
    for ((file, line), holds) in runner.core.inner.site_holds().into_iter().take(20) {
        println!(
            "{file}:{line}: {} {:.1} {:.1}",
            holds.count,
            holds.total.as_secs_f64() * 1000.0,
            holds.longest.as_secs_f64() * 1000.0
        );
    }
}
