//! Disk rules for a Boat sandbox, which serves one job at a time for one
//! tenant on a 64 GiB disk shared with the endpoint's journal.
use super::*;

const GIB: u64 = 1 << 30;

// Exercise scheduling with native fixture processes, without requiring a root
// XFS quota mount. The production host backend has no such quota bypass.
#[derive(Default)]
pub(super) struct SchedulerFixtureBackend {
    #[cfg(target_os = "linux")]
    policy: Option<Arc<FixtureCachePolicy>>,
}
#[cfg(target_os = "linux")]
struct FixtureCachePolicy {
    cap: u64,
    initial: u64,
    attempts: AtomicU64,
}
impl backend::Backend for SchedulerFixtureBackend {
    fn materialize(&self, config: &Config, trees: &Trees, setup: &Setup) -> io::Result<Prepared> {
        let prepared = backend::HostBackend.materialize(config, trees, setup)?;
        #[cfg(target_os = "linux")]
        if let Some(policy) = &self.policy {
            policy.attempts.fetch_add(1, Ordering::SeqCst);
            let initial = if prepared.target.join("warm-marker").exists() {
                policy.initial
            } else {
                0
            };
            backend::setup_step(
                "fixture quota capacity",
                crate::boat_quota::capacity(
                    initial / 512,
                    policy.cap,
                    config.boat_cold_materialization,
                    crate::boat_quota::MIN_COLD_CAP_BYTES,
                ),
            )?;
        }
        Ok(prepared)
    }
    fn launch(
        &self,
        config: &Config,
        trees: &Trees,
        setup: &Setup,
        prepared: &Prepared,
        weight: u32,
    ) -> io::Result<backend::Launch> {
        let mut native = Config::new(config.state_root.clone(), config.instance.clone());
        native.fixture_cargo_home = config.fixture_cargo_home.clone();
        native.test_processes = config.test_processes.clone();
        backend::HostBackend.launch(&native, trees, setup, prepared, weight)
    }
    fn cancel(&self, config: &Config, job: &Job, hard: bool) -> io::Result<()> {
        backend::HostBackend.cancel(config, job, hard)
    }
    fn unit_alive(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        backend::HostBackend.unit_alive(config, process)
    }
    fn collect_changes(&self, trees: &Trees, job: &Job) -> io::Result<Option<Vec<String>>> {
        backend::HostBackend.collect_changes(trees, job)
    }
    fn reconcile(&self, config: &Config, interrupted: &[Job]) -> io::Result<Vec<Uuid>> {
        backend::HostBackend.reconcile(config, interrupted)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boat_queues_a_second_small_job_through_setup_and_execution() {
    let f = Fixture::new();
    let hold = Hold::new("materialize");
    let _release = Release(hold.clone());
    let mut config = boat_config(&f, |_| Ok(63 * GIB));
    config.manual_tick = false;
    config.long_work = Some(hold.hook());
    let runner =
        Runner::open_with_backend(config, Box::new(SchedulerFixtureBackend::default())).unwrap();
    let marker = f.scratch.join("release-first");
    hold.arm();
    let (mut first, mut first_bytes) = job_at(
        &f,
        "singleton-a",
        &format!("while test ! -e '{}'; do sleep .02; done", marker.display()),
    );
    first.weight_hint = Some(1);
    first.siblings.clear();
    first.bundles.truncate(1);
    first_bytes.truncate(1);
    f.submit(&runner, &first, &first_bytes);
    reached(&hold).await;
    let (mut second, mut second_bytes) = job_at(&f, "singleton-b", "true");
    second.weight_hint = Some(1);
    second.siblings.clear();
    second.bundles.truncate(1);
    second_bytes.truncate(1);
    f.submit(&runner, &second, &second_bytes);
    settle().await;
    assert!(
        !begun(&runner, second.job_id),
        "Boat started a second setup even though a job already owns the sandbox"
    );
    hold.release();
    wait_for(|| {
        runner.core.inner.lock().unwrap().jobs[&first.job_id]
            .started_ms
            .is_some()
    })
    .await;
    settle().await;
    assert!(
        !begun(&runner, second.job_id),
        "Boat ran two small jobs concurrently on different workspaces"
    );
    fs::write(marker, b"release").unwrap();
    for id in [first.job_id, second.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn over_cap_baseline_is_evicted_and_the_job_runs_cold() {
    let f = Fixture::new();
    let mut config = boat_config(&f, |_| Ok(63 * GIB));
    config.manual_tick = false;
    let policy = Arc::new(FixtureCachePolicy {
        cap: 16 * GIB,
        initial: 24 * GIB,
        attempts: AtomicU64::new(0),
    });
    let runner = Runner::open_with_backend(
        config,
        Box::new(SchedulerFixtureBackend {
            policy: Some(policy.clone()),
        }),
    )
    .unwrap();
    let (mut warm, mut warm_bytes) = f.run("true");
    warm.siblings.clear();
    warm.bundles.truncate(1);
    warm_bytes.truncate(1);
    f.submit(&runner, &warm, &warm_bytes);
    assert_eq!(
        terminal(&runner, warm.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    policy.attempts.store(0, Ordering::SeqCst);
    let path = PathBuf::from(&f.key);
    fs::write(path.join("target/warm-marker"), b"reused cache").unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .catalog
            .workspaces
            .get_mut(&f.key)
            .unwrap()
            .sibling_freshness
            .insert("old-cache".into(), Default::default());
    }
    let (mut request, mut bytes) =
        f.run("if test -e target/warm-marker; then exit 31; fi; printf cold-after-eviction");
    request.siblings.clear();
    request.bundles.truncate(1);
    bytes.truncate(1);
    f.submit(&runner, &request, &bytes);
    let result = terminal(&runner, request.job_id).await;
    assert_eq!(
        result.outcome,
        Outcome::Exit { code: 0 },
        "{result:?}; {}",
        runner_diagnostics(&runner)
    );
    assert_eq!(policy.attempts.load(Ordering::SeqCst), 2);
    assert!(!path.join("target/warm-marker").exists());
    let inner = runner.core.inner.lock().unwrap();
    assert!(inner.catalog.workspaces[&f.key]
        .sibling_freshness
        .is_empty());
    assert_eq!(inner.last_eviction.as_ref().unwrap().path, f.key);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cold_quota_shortage_refuses_before_start_and_withdraws_ready_marker() {
    let f = Fixture::new();
    let marker = f.scratch.join("endpoint-ready");
    fs::write(&marker, b"sandbox").unwrap();
    let withdrawals = Arc::new(AtomicU64::new(0));
    let mut config = boat_config(&f, |_| Ok(63 * GIB));
    config.manual_tick = false;
    config.boat_readiness_withdraw = {
        let (marker, withdrawals) = (marker.clone(), withdrawals.clone());
        Arc::new(move || {
            withdrawals.fetch_add(1, Ordering::SeqCst);
            fs::remove_file(&marker)
        })
    };
    let policy = Arc::new(FixtureCachePolicy {
        cap: 4 * GIB,
        initial: 0,
        attempts: AtomicU64::new(0),
    });
    let runner = Runner::open_with_backend(
        config,
        Box::new(SchedulerFixtureBackend {
            policy: Some(policy.clone()),
        }),
    )
    .unwrap();
    let (mut request, mut bytes) = f.run("touch should-not-run");
    request.siblings.clear();
    request.bundles.truncate(1);
    bytes.truncate(1);
    f.submit(&runner, &request, &bytes);
    let terminal = terminal(&runner, request.job_id).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDiskFull
        }
    );
    assert_eq!(policy.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(withdrawals.load(Ordering::SeqCst), 1);
    assert!(!marker.exists());
    assert!(!Path::new(&request.workspace_key)
        .join("should-not-run")
        .exists());
    assert!(runner.core.inner.lock().unwrap().jobs[&request.job_id]
        .started_ms
        .is_none());
}

#[test]
fn boat_reserve_restore_never_allocates_beside_live_or_starting_work() {
    for starting in [false, true] {
        let f = Fixture::new();
        let runner = Runner::open(boat_config(&f, |_| Ok(63 * GIB))).unwrap();
        runner.disk_upkeep().unwrap();
        let reserve = runner.core.trees.state.join("disk-reserve");
        fs::remove_file(&reserve).unwrap();
        let (request, _) = f.run("true");
        runner.accept(request.clone()).unwrap();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            if starting {
                inner
                    .jobs
                    .get_mut(&request.job_id)
                    .unwrap()
                    .terminal(Outcome::Exit { code: 0 }, runner.now());
                inner.starting.insert(request.job_id);
            } else {
                inner.jobs.get_mut(&request.job_id).unwrap().state = JobState::Running;
            }
        }
        runner.disk_upkeep().unwrap();
        assert!(
            !reserve.exists(),
            "reserve allocation overlapped a sampled job cap"
        );
    }
}

#[test]
fn boat_reserve_write_does_not_hold_the_state_lock_or_block_status() {
    let f = Fixture::new();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    let mut config = boat_config(&f, |_| Ok(63 * GIB));
    config.long_work = Some(Arc::new(move |label| {
        if label == "disk_reserve_write" {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
    }));
    let runner = Runner::open(config).unwrap();
    std::thread::scope(|scope| {
        let writing = scope.spawn(|| runner.disk_upkeep());
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let status_runner = &runner;
        scope.spawn(move || {
            status_tx.send(status_runner.status()).unwrap();
        });
        let status = status_rx.recv_timeout(Duration::from_secs(2));
        // Release the writer before asserting, so a regression that blocks
        // status can finish instead of stranding either thread during unwinding.
        release_tx.send(()).unwrap();
        writing.join().unwrap().unwrap();
        assert!(
            status.is_ok(),
            "status waited behind the reserve-file write"
        );
    });
    assert!(!runner.core.inner.lock().unwrap().restoring_reserve);
}

#[test]
fn boat_reserve_restore_claim_blocks_setup_and_clears_on_write_error() {
    let f = Fixture::new();
    let runner = Runner::open(boat_config(&f, |_| Ok(63 * GIB))).unwrap();
    let (request, _) = f.run("true");
    runner.accept(request.clone()).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.restoring_reserve = true;
        runner
            .start(&mut inner, request.job_id, &BTreeMap::new())
            .unwrap();
        runner.dispatch(&mut inner, runner.now()).unwrap();
        assert!(
            inner.starting.is_empty(),
            "reserve allocation admitted setup"
        );
        inner.restoring_reserve = false;
        inner
            .jobs
            .get_mut(&request.job_id)
            .unwrap()
            .terminal(Outcome::Exit { code: 0 }, runner.now());
    }
    fs::create_dir_all(runner.core.trees.state.join("disk-reserve.tmp")).unwrap();
    assert!(runner.disk_upkeep().is_err());
    assert!(
        !runner.core.inner.lock().unwrap().restoring_reserve,
        "failed write leaked its idle claim"
    );
}

#[test]
fn boat_reserve_restore_claim_clears_after_failed_publication() {
    let f = Fixture::new();
    let mut config = boat_config(&f, |_| Ok(63 * GIB));
    let reserve = config.state_dir().join("disk-reserve");
    config.long_work = Some(Arc::new(move |label| {
        if label == "disk_reserve_write" {
            fs::create_dir(&reserve).unwrap();
        }
    }));
    let runner = Runner::open(config).unwrap();
    assert!(runner.disk_upkeep().is_err());
    assert!(!runner.core.inner.lock().unwrap().restoring_reserve);
    assert!(!runner.core.trees.state.join("disk-reserve.tmp").exists());
}

#[tokio::test]
async fn boat_persisted_fill_is_refused_before_claiming_writer_or_setup() {
    let f = Fixture::new();
    let runner = Runner::open(boat_config(&f, |_| Ok(63 * GIB))).unwrap();
    let (mut request, _) = f.run("touch writer-should-not-run");
    request.siblings.clear();
    let id = request.job_id;
    let observed = {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .catalog
            .repositories
            .insert(request.repository_root.clone(), Repository::default());
        let mut job = Job::new(id, Some(request), runner.now(), 1, 1);
        job.base = true;
        job.state = JobState::Queued;
        inner.jobs.insert(id, job);
        runner.start(&mut inner, id, &BTreeMap::new()).unwrap();
        (
            inner.jobs[&id].terminal.clone(),
            inner.starting.contains(&id),
            inner.writer,
        )
    };
    if observed.0.is_none() {
        let _ = terminal(&runner, id).await;
    }
    assert!(
        matches!(
            observed.0.map(|record| record.outcome),
            Some(Outcome::RefusedBeforeStart {
                reason: RefusalReason::WorkspaceSetupFailed
            })
        ),
        "Boat fill entered setup instead of a synchronous no-start refusal"
    );
    assert!(!observed.1);
    assert!(observed.2.is_none());
    assert!(!Path::new(&f.key).join("writer-should-not-run").exists());
}

fn workspace(runner: &Runner, path: &Path, used_ms: u64) {
    fs::create_dir_all(path.join("target")).unwrap();
    fs::write(path.join("target/artifact"), vec![1; 8192]).unwrap();
    let mut inner = runner.core.inner.lock().unwrap();
    inner.catalog.workspaces.insert(
        text(path).into(),
        Workspace {
            used_ms,
            ..Workspace::default()
        },
    );
    runner.catalog(&inner).unwrap();
}

/// The Boat image's cold-cap thresholds with a free
/// space probe the test controls.
fn boat_config(
    f: &Fixture,
    probe: impl Fn(&Path) -> io::Result<u64> + Send + Sync + 'static,
) -> Config {
    let mut c = f.config();
    c.boat_profile = true;
    c.boat_memory_max_bytes = Some(2 << 30);
    c.free_space_floor_bytes = (32 * GIB) + (64 << 20);
    c.free_space_eviction_bytes = Some(40 * GIB);
    c.disk_reserve_bytes = 4096;
    c.free_space_probe = Arc::new(probe);
    c.manual_tick = true;
    c
}

#[test]
fn boat_admission_evicts_every_idle_workspace_except_its_own_even_with_space_free() {
    let f = Fixture::new();
    // Plenty of space: the ordinary pass would keep every cache.
    let runner = Runner::open(boat_config(&f, |_| Ok(63 * GIB))).unwrap();
    let now = runner.now();
    let root = f.scratch.join("remote/worktrees");
    let others = [root.join("b"), root.join("c")];
    // Used seconds ago, as with jobs run back to back.
    for other in &others {
        workspace(&runner, other, now - 1);
    }
    let own = PathBuf::from(&f.key);
    workspace(&runner, &own, now - 1);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    for other in &others {
        assert!(
            !other.exists(),
            "{} must be evicted before admission",
            other.display()
        );
    }
    assert!(
        own.join("target/artifact").exists(),
        "the workspace this job reuses must stay warm"
    );
    let inner = runner.core.inner.lock().unwrap();
    assert!(inner.catalog.workspaces.contains_key(&f.key));
    assert!(!inner.catalog.workspaces.contains_key(text(&others[0])));
}

#[test]
fn non_boat_admission_keeps_recent_workspaces_above_the_soft_goal() {
    let f = Fixture::new();
    let mut c = boat_config(&f, |_| Ok(63 * GIB));
    c.boat_profile = false;
    c.boat_memory_max_bytes = None;
    let runner = Runner::open(c).unwrap();
    let other = f.scratch.join("remote/worktrees/b");
    workspace(&runner, &other, runner.now() - 1);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(other.exists(), "the build-server rule is unchanged");
}

#[test]
fn boat_admission_refuses_typed_when_eviction_leaves_free_space_under_the_floor() {
    let f = Fixture::new();
    let runner = Runner::open(boat_config(&f, |_| Ok(10 * GIB))).unwrap();
    let other = f.scratch.join("remote/worktrees/b");
    workspace(&runner, &other, runner.now() - 1);
    let own = PathBuf::from(&f.key);
    workspace(&runner, &own, runner.now() - 1);
    let (request, _) = f.run("touch should-not-run");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::DiskFull(RunnerDiskFull {
            free_bytes,
            floor_bytes,
        }) if free_bytes == 10 * GIB && floor_bytes == (32 * GIB) + (64 << 20)
    ));
    assert!(!other.exists());
    assert!(
        own.exists(),
        "an admitting job's own workspace is never evicted"
    );
    let job = runner.job_status(request.job_id).unwrap();
    assert_eq!(job.state, "tombstone");
    assert_eq!(
        job.terminal.unwrap().outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDiskFull
        },
        "the refusal is the typed pre-start refusal, so the job never ran"
    );
}

/// A job that filled the disk on its own leaves its workspace behind with free
/// space under the floor, and no admission arrives to evict it (the endpoint
/// is not ready below the floor). The idle runner evicts it itself, restores
/// its reserve, and admits again.
#[test]
fn idle_boat_runner_frees_its_disk_after_a_job_filled_it_and_admits_again() {
    let f = Fixture::new();
    let filled = f.scratch.join("remote/worktrees/b");
    let hog = filled.clone();
    let runner = Runner::open(boat_config(&f, move |_| {
        Ok(if hog.exists() { GIB } else { 60 * GIB })
    }))
    .unwrap();
    let reserve = runner.core.trees.state.join("disk-reserve");
    // The reserve was spent while the disk was full.
    let _ = fs::remove_file(&reserve);
    workspace(&runner, &filled, runner.now() - 1);
    runner.disk_upkeep().unwrap();
    assert!(!filled.exists(), "the idle runner evicts below the floor");
    assert_eq!(fs::metadata(&reserve).unwrap().len(), 4096);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
}

#[test]
fn disk_upkeep_leaves_a_live_job_alone() {
    let f = Fixture::new();
    let filled = f.scratch.join("remote/worktrees/b");
    let hog = filled.clone();
    let runner = Runner::open(boat_config(&f, move |_| {
        Ok(if hog.exists() { GIB } else { 60 * GIB })
    }))
    .unwrap();
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    workspace(&runner, &filled, runner.now() - 1);
    runner.disk_upkeep().unwrap();
    assert!(
        filled.exists(),
        "upkeep waits for the running job; its own writes fail with ENOSPC"
    );
}

/// When the job's own writes used up the disk, its terminal record cannot be
/// written either; the runner spends its reserve so the job can finish
/// instead of retrying forever with its workspace protected.
#[tokio::test]
async fn terminal_record_spends_the_disk_reserve_when_the_disk_is_full() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disk_reserve_bytes = 4096;
    let reserve = config.state_dir().join("disk-reserve");
    let full = reserve.clone();
    let armed = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicU64::new(0));
    config.before_job_write = Some({
        let (armed, attempts) = (armed.clone(), attempts.clone());
        Arc::new(move |_| {
            // The reserve is the only space left on this disk.
            if armed.load(Ordering::SeqCst) && full.exists() {
                attempts.fetch_add(1, Ordering::SeqCst);
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Ok(())
        })
    });
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    runner.accept(request.clone()).unwrap();
    let id = {
        let mut inner = runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&request.job_id).unwrap();
        job.state = JobState::Running;
        job.started_ms = Some(runner.now());
        job.durable_output = true;
        runner.save(job).unwrap();
        request.job_id
    };
    let directory = runner
        .core
        .trees
        .state
        .join("sandboxes")
        .join(id.to_string())
        .join("output");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("stdout"), "").unwrap();
    fs::write(directory.join("stderr"), "No space left on device\n").unwrap();
    fs::write(directory.join("exit"), "101\n").unwrap();
    fs::write(&reserve, vec![0; 4096]).unwrap();
    armed.store(true, Ordering::SeqCst);
    let monitor = {
        let runner = runner.clone();
        std::thread::spawn(move || runner.monitor_unit(id))
    };
    wait_for(|| read_json::<Job>(&runner.record_path(id)).is_ok_and(|job| job.terminal.is_some()))
        .await;
    monitor.join().unwrap();
    assert!(attempts.load(Ordering::SeqCst) >= 1);
    assert!(
        !reserve.exists(),
        "the reserve was spent on the terminal record"
    );
}
