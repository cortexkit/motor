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
}

fn saved(path: &Path) -> Option<Saved> {
    match read_json::<Job>(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(Saved::Nothing),
        Ok(job) if job.terminal.is_none() => match job.state {
            JobState::AwaitingBundles => Some(Saved::Admitted),
            JobState::Queued if job.setup_ms.is_none() => Some(Saved::Queued),
            JobState::Queued if job.started.is_none() => Some(Saved::SetUp),
            _ => None,
        },
        _ => None,
    }
}

/// A runner with the host backend whose scheduler runs only when a test
/// calls `tick`, and which can hold one chosen job record save, or a long
/// step labelled with its hold's label (such as `catalog_save`), open.
struct SaveFixture {
    runner: Runner,
    fixture: Fixture,
    hold: Arc<Hold>,
    /// The record, and the stage it was saved at, whose next save waits on
    /// the `record_save` hold.
    held: Arc<Mutex<Option<(PathBuf, Saved)>>>,
}

impl SaveFixture {
    fn new(label: &'static str) -> Self {
        Self::with(label, |_| {})
    }

    fn with(label: &'static str, configure: impl FnOnce(&mut Config)) -> Self {
        let fixture = Fixture::new();
        let hold = Hold::new(label);
        let held = Arc::new(Mutex::new(None::<(PathBuf, Saved)>));
        let mut config = fixture.config();
        config.manual_tick = true;
        config.disable_warm_builds = true;
        config.long_work = Some(hold.hook());
        config.before_job_write = Some({
            let hold = hold.clone();
            let held = held.clone();
            Arc::new(move |path| {
                let next = held
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(record, stage)| record == path && saved(path) == Some(*stage));
                if next {
                    hold.hook()("record_save");
                }
                Ok(())
            })
        });
        configure(&mut config);
        let runner = Runner::open(config).unwrap();
        Self {
            runner,
            fixture,
            hold,
            held,
        }
    }

    /// Hold the next save of `id`'s record made after a save at `stage`.
    fn hold_save(&self, id: Uuid, stage: Saved) {
        *self.held.lock().unwrap() = Some((self.runner.record_path(id), stage));
        self.hold.arm();
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
        self.runner.core.inner.lock().unwrap().jobs.get(&id).cloned()
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
    assert!(matches!(
        duplicate.join().unwrap(),
        Admission::Duplicate
    ));
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
    let held =
        f.hold_in(move |runner| runner.fetch_run(&request, &directory, true).unwrap());
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
        let inner = f.runner.core.inner.lock().unwrap();
        assert_eq!(inner.starting, BTreeSet::from([first.job_id]));
        assert!(inner.reserved.contains_key(&first.workspace_key));
        assert_eq!(
            Runner::held_weight(&inner),
            inner.jobs[&first.job_id].weight,
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
        let inner = f.runner.core.inner.lock().unwrap();
        assert_eq!(inner.starting, BTreeSet::from([id]));
        assert!(inner.reserved.contains_key(&first.workspace_key));
        assert_eq!(inner.jobs[&id].state, JobState::Queued);
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
    assert!(!marker.exists(), "the command ran before its start was durable");
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
    assert_eq!(saved.workspaces[&request.workspace_key].used_ms, finished_ms);
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
/// its workspace reserved until the catalog record, which drop and retention
/// delete the workspace by, is durable.
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
    assert!(Runner::occupied(
        &f.runner.core.inner.lock().unwrap(),
        &key
    ));
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
