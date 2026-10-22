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
    // The record left the state and the directory its place before the
    // deletion started, so no replay can read a half-deleted job.
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
