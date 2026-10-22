//! What deletes sibling snapshots and legacy sibling checkouts, and what
//! keeps them: references derived from jobs, reservations, admissions,
//! Prepares and creations, also after a restart; the cap after creation, at
//! startup and at maintenance; idle expiry; pressure eviction; and the
//! cleanup of in-place sibling checkouts that older runners left.
use super::sibling_snapshots::{
    host_checkout, main_workspace_job, manual_runner, sibling_repository_job, sibling_user_job,
    snapshot_config,
};
use super::*;
use crate::sibling_snapshots::{
    store::{self, metadata_path},
    JavascriptEnvironment, SnapshotKey,
};

const HOUR: Duration = Duration::from_secs(60 * 60);

/// The full object ID these tests use for commit number `n` of the fixture
/// sibling. Snapshots made with `snapshot` never check it out.
fn commit_id(n: u32) -> String {
    format!("{n:040x}")
}

/// Publishes a snapshot of the fixture sibling at commit `n` for caller
/// commit `caller`, last used `age` ago.
fn snapshot_for(
    f: &Fixture,
    config: &Config,
    environment: &JavascriptEnvironment,
    n: u32,
    caller: u32,
    age: Duration,
) -> PathBuf {
    let key =
        SnapshotKey::new(&f.canonical, &commit_id(n), &commit_id(caller), environment).unwrap();
    store::publish_for_test(config, &key, &commit_id(n), SystemTime::now() - age)
}

fn snapshot(
    f: &Fixture,
    config: &Config,
    environment: &JavascriptEnvironment,
    n: u32,
    age: Duration,
) -> PathBuf {
    snapshot_for(f, config, environment, n, n, age)
}

/// The fixture sibling declared at commit `n`.
fn sibling_at(f: &Fixture, n: u32) -> SiblingSnapshot {
    SiblingSnapshot {
        canonical_path: f.canonical.clone(),
        commit: commit_id(n),
        tree_hash: commit_id(n),
        base_commit: None,
    }
}

/// A request of the fixture's repository that declares the sibling at
/// commit `n`.
fn run_with_sibling(f: &Fixture, n: u32) -> Run {
    let (mut run, _) = f.run("true");
    run.job_id = Uuid::now_v7();
    run.siblings = vec![sibling_at(f, n)];
    run
}

/// A running job, started by this runner, that declares the sibling at
/// commit `n`.
fn running_job(f: &Fixture, n: u32, now: u64) -> Job {
    let run = run_with_sibling(f, n);
    let mut job = Job::new(run.job_id, Some(run), now, 1, 1);
    job.state = JobState::Running;
    job.started_ms = Some(now);
    job
}

fn reservation(f: &Fixture, n: u32, now: u64) -> Reservation {
    Reservation {
        repository: f.root.clone(),
        commit: String::new(),
        siblings: vec![sibling_at(f, n)],
        since_ms: now,
    }
}

/// Whether the snapshot is published: its directory and its metadata exist.
/// A deleted snapshot has neither.
fn published(directory: &Path) -> bool {
    let (directory_exists, metadata_exists) =
        (directory.exists(), metadata_path(directory).exists());
    assert_eq!(
        directory_exists,
        metadata_exists,
        "{} is half deleted",
        directory.display()
    );
    directory_exists
}

fn which_published(snapshots: &[PathBuf]) -> Vec<bool> {
    snapshots.iter().map(|s| published(s)).collect()
}

/// Every kind of runner state that references a snapshot keeps it through
/// the cap, a snapshot whose name starts with a referenced commit counts as
/// referenced whatever its caller commit, and of the unreferenced ones the
/// cap keeps the most recently used. When all those references end at once,
/// with no creation after, the next maintenance tick enforces the cap.
#[test]
fn cap_keeps_every_referenced_snapshot_and_the_most_recently_used_others() {
    let f = Fixture::new();
    let (config, environment) = snapshot_config(&f);
    let runner = manual_runner(config);
    let config = &runner.core.config;
    assert_eq!(config.sibling_snapshot_cap, 4);
    // 0 to 3 are the least recently used; 4 to 8 are referenced by nothing.
    let mut snapshots: Vec<_> = (0..9)
        .map(|n| snapshot(&f, config, &environment, n, HOUR * (20 - n)))
        .collect();
    // A snapshot of referenced commit 0 made for another caller commit.
    snapshots.push(snapshot_for(&f, config, &environment, 0, 99, HOUR * 30));
    let now = runner.now();
    let job = running_job(&f, 0, now);
    let job_id = job.id;
    let reserved = f.scratch.join("remote/worktrees/reserved");
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.jobs.insert(job_id, job);
        inner
            .reserved
            .insert(text(&reserved).into(), reservation(&f, 1, now));
        inner
            .admitting
            .insert(Uuid::now_v7(), run_with_sibling(&f, 2));
        inner
            .preparing
            .insert(Uuid::now_v7(), prepare_for(&run_with_sibling(&f, 3)));
    }
    runner.maintenance().unwrap();
    assert_eq!(
        which_published(&snapshots),
        [true, true, true, true, false, true, true, true, true, true],
        "five unreferenced snapshots and a cap of four: the least recently used goes"
    );

    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .jobs
            .get_mut(&job_id)
            .unwrap()
            .terminal(Outcome::Exit { code: 0 }, now);
        inner.reserved.clear();
        inner.admitting.clear();
        inner.preparing.clear();
    }
    f.clock.fetch_add(60_000, Ordering::SeqCst);
    runner.tick().unwrap();
    assert_eq!(
        which_published(&snapshots),
        [false, false, false, false, false, true, true, true, true, false],
        "the maintenance tick after the references ended keeps the four most recently used"
    );
}

/// At maintenance an unreferenced snapshot unused for 24 hours is deleted;
/// a referenced one as old, and an unreferenced one used more recently, stay.
#[test]
fn idle_expiry_deletes_only_unreferenced_snapshots_unused_for_a_day() {
    let f = Fixture::new();
    let (config, environment) = snapshot_config(&f);
    let runner = manual_runner(config);
    let config = &runner.core.config;
    assert_eq!(config.sibling_snapshot_idle, HOUR * 24);
    let idle = snapshot(&f, config, &environment, 0, HOUR * 25);
    let referenced = snapshot(&f, config, &environment, 1, HOUR * 25);
    let recent = snapshot(&f, config, &environment, 2, HOUR * 23);
    let job = running_job(&f, 1, runner.now());
    runner.core.inner.lock().unwrap().jobs.insert(job.id, job);
    runner.maintenance().unwrap();
    assert!(!published(&idle));
    assert!(published(&referenced));
    assert!(published(&recent));
}

/// Recovers every interrupted job as still running, then finds its unit
/// gone, so the job ends right after startup.
struct Reattaching;

impl backend::Backend for Reattaching {
    fn materialize(&self, _: &Config, _: &Trees, _: &Setup) -> io::Result<Prepared> {
        unreachable!("no job is dispatched")
    }
    fn launch(
        &self,
        _: &Config,
        _: &Trees,
        _: &Setup,
        _: &Prepared,
        _: u32,
    ) -> io::Result<backend::Launch> {
        unreachable!("no job is dispatched")
    }
    fn cancel(&self, _: &Config, _: &Job, _: bool) -> io::Result<()> {
        Ok(())
    }
    fn unit_alive(
        &self,
        _: &Config,
        _: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        Ok(Some(false))
    }
    fn collect_changes(&self, _: &Trees, _: &Job) -> io::Result<Option<Vec<String>>> {
        Ok(None)
    }
    fn reconcile(&self, _: &Config, jobs: &[Job]) -> io::Result<Vec<Uuid>> {
        Ok(jobs.iter().map(|job| job.id).collect())
    }
}

/// The cap runs at startup, and a job recovered as running after a restart
/// references the snapshot of its sibling from its saved record. Once that
/// job has ended, the next maintenance deletes the snapshot.
#[tokio::test]
async fn startup_enforces_the_cap_and_a_recovered_job_keeps_its_snapshot() {
    let f = Fixture::new();
    let (mut config, environment) = snapshot_config(&f);
    config.sibling_snapshot_cap = 1;
    config.manual_tick = true;
    config.disable_warm_builds = true;
    let recovered = snapshot(&f, &config, &environment, 0, HOUR * 3);
    let older = snapshot(&f, &config, &environment, 1, HOUR * 2);
    let newer = snapshot(&f, &config, &environment, 2, HOUR);
    let job = running_job(&f, 0, f.clock.load(Ordering::SeqCst));
    let record = config
        .state_dir()
        .join("jobs")
        .join(job.id.to_string())
        .join("record.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    atomic_json(&record, &job).unwrap();

    let runner = Runner::open_with_backend(config, Box::new(Reattaching)).unwrap();
    let after_startup = which_published(&[recovered.clone(), older.clone(), newer.clone()]);
    terminal(&runner, job.id).await;
    runner.maintenance().unwrap();
    assert_eq!(
        after_startup,
        [true, false, true],
        "startup keeps the recovered job's snapshot and the most recently used other one"
    );
    assert_eq!(
        which_published(&[recovered, older, newer]),
        [false, false, true],
        "the snapshot goes once its job has ended"
    );
}

/// An idle catalogued workspace whose build target takes disk space.
fn idle_workspace(runner: &Runner, path: &Path, used_ms: u64) {
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
}

/// Whether a snapshot moved to the trash is still waiting to be deleted.
fn snapshot_in_trash(state: &Path) -> bool {
    fs::read_dir(state.join("trash")).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("sibling-snapshot-")
        })
    })
}

/// Pressure eviction deletes unreferenced snapshots, least recently used
/// first, before an idle workspace that was used even longer ago, and frees
/// each one's space before it probes again. A referenced snapshot stays.
#[test]
fn pressure_deletes_unreferenced_snapshots_before_workspaces_and_before_probing() {
    let f = Fixture::new();
    let (mut config, environment) = snapshot_config(&f);
    let state = config.state_dir();
    let referenced = snapshot(&f, &config, &environment, 0, HOUR * 3);
    let older = snapshot(&f, &config, &environment, 1, HOUR * 2);
    let newer = snapshot(&f, &config, &environment, 2, HOUR);
    let probes = Arc::new(Mutex::new(Vec::new()));
    {
        let (newer, state, probes) = (newer.clone(), state.clone(), probes.clone());
        config.free_space_floor_bytes = 100;
        config.free_space_probe = Arc::new(move |_| {
            // Space is back once the newer snapshot is gone from the trash
            // too, not merely renamed into it.
            let trashed = snapshot_in_trash(&state);
            probes.lock().unwrap().push(trashed);
            Ok(if newer.exists() || trashed { 10 } else { 200 })
        });
    }
    // A background purge waits here, so a probe that ran before the snapshot
    // was really deleted would see it in the trash.
    config.long_work = Some(Arc::new(|label| {
        if label == "remove" {
            std::thread::sleep(Duration::from_millis(500));
        }
    }));
    let runner = manual_runner(config);
    let workspace = f.scratch.join("remote/worktrees/idle");
    idle_workspace(&runner, &workspace, runner.now() - 5 * 60 * 60 * 1000);
    let job = running_job(&f, 0, runner.now());
    runner.core.inner.lock().unwrap().jobs.insert(job.id, job);

    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(published(&referenced));
    assert!(!published(&older));
    assert!(!published(&newer));
    assert!(
        workspace.join("target/artifact").exists(),
        "snapshots are evicted before workspaces"
    );
    assert!(
        !probes.lock().unwrap().iter().any(|trashed| *trashed),
        "a probe ran while an evicted snapshot was still in the trash"
    );
    let status = runner.runner_status();
    assert_eq!(
        status.disk.unwrap().last_eviction.unwrap().path,
        text(&newer),
        "least recently used first: the newer snapshot goes last"
    );
    assert_eq!(status.health.unwrap().counts.evictions, 2);
}

/// A request admitted after maintenance picked a snapshot to delete, and
/// before the deletion's recheck, keeps that snapshot.
#[test]
fn admission_after_victim_selection_keeps_the_snapshot() {
    let f = Fixture::new();
    let hold = Hold::new("sibling_snapshot_selected");
    let _release = Release(hold.clone());
    let (mut config, environment) = snapshot_config(&f);
    config.sibling_snapshot_cap = 0;
    config.long_work = Some(hold.hook());
    let runner = manual_runner(config);
    let victim = snapshot(&f, &runner.core.config, &environment, 0, HOUR);
    hold.arm();
    let maintenance = std::thread::spawn({
        let runner = runner.clone();
        move || runner.maintenance()
    });
    assert!(poll(Duration::from_secs(30), || hold
        .reached
        .load(Ordering::SeqCst)));
    let admission = {
        let mut inner = runner.core.inner.lock().unwrap();
        runner.protect_admission(&mut inner, &run_with_sibling(&f, 0))
    };
    hold.release();
    maintenance.join().unwrap().unwrap();
    let kept = published(&victim);
    drop(admission);
    runner.maintenance().unwrap();
    assert!(
        kept,
        "the admission referenced the snapshot before its recheck"
    );
    assert!(
        !published(&victim),
        "the snapshot goes once the admission ends"
    );
}

/// Two setups create snapshots of two commits of one repository at the same
/// time with a cap of one. Each enforces the cap after its own creation,
/// holding no key mutex, and both finish; each keeps its own snapshot while
/// it is reserved, and once both reservations end only the most recently
/// used remains.
#[test]
fn concurrent_creations_of_one_repository_with_a_cap_of_one_both_finish() {
    let f = Fixture::new();
    let first = git(&f.sibling, &["rev-parse", "HEAD"]);
    let first_tree = git(&f.sibling, &["rev-parse", "HEAD^{tree}"]);
    let (second, second_tree) = commit(&f.sibling, "second\n");
    let (mut config, environment) = snapshot_config(&f);
    config.sibling_snapshot_cap = 1;
    // Each creation waits, inside its key mutex, until both are there.
    let inside = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    config.sibling_snapshot_access = Some({
        let inside = inside.clone();
        Arc::new(move |_: &Path| {
            inside.fetch_add(1, Ordering::SeqCst);
            if poll(Duration::from_secs(60), || {
                inside.load(Ordering::SeqCst) >= 2
            }) {
                Ok(())
            } else {
                Err(io::Error::other("the other creation never started"))
            }
        })
    });
    let runner = manual_runner(config);
    let mirror = runner.core.trees.ensure_mirror(&f.canonical).unwrap();
    git(
        &mirror,
        &[
            "fetch",
            "-q",
            text(&f.sibling),
            &format!("HEAD:refs/tips/{second}"),
        ],
    );
    let sibling = |commit: &str, tree: &str| SiblingSnapshot {
        canonical_path: f.canonical.clone(),
        commit: commit.into(),
        tree_hash: tree.into(),
        base_commit: None,
    };
    let setups = [
        (sibling(&first, &first_tree), "remote/worktrees/one"),
        (sibling(&second, &second_tree), "remote/worktrees/two"),
    ];
    let now = runner.now();
    for (sibling, workspace) in &setups {
        runner.core.inner.lock().unwrap().reserved.insert(
            text(&f.scratch.join(workspace)).into(),
            Reservation {
                repository: f.root.clone(),
                commit: String::new(),
                siblings: vec![sibling.clone()],
                since_ms: now,
            },
        );
    }
    let (done, finished) = std::sync::mpsc::channel();
    let (runner, environment) = (&runner, &environment);
    let directories: Vec<_> = std::thread::scope(|scope| {
        let threads: Vec<_> = setups
            .iter()
            .map(|(sibling, _)| {
                let done = done.clone();
                scope.spawn(move || {
                    let key = SnapshotKey::new(
                        &sibling.canonical_path,
                        &sibling.commit,
                        &sibling.commit,
                        environment,
                    )
                    .unwrap();
                    let directory = runner.core.snapshots.resolve(
                        &runner.core.config,
                        &runner.core.trees,
                        &key,
                        &sibling.tree_hash,
                    );
                    runner.cap_sibling_snapshots(std::slice::from_ref(sibling));
                    done.send(()).unwrap();
                    directory
                })
            })
            .collect();
        for _ in &threads {
            finished
                .recv_timeout(Duration::from_secs(120))
                .expect("both creations finish");
        }
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .collect()
    });
    assert_eq!(
        which_published(&directories),
        [true, true],
        "a reserved setup keeps its own snapshot"
    );
    assert_eq!(
        fs::read_to_string(directories[0].join("source")).unwrap(),
        "first\n"
    );
    assert_eq!(
        fs::read_to_string(directories[1].join("source")).unwrap(),
        "second\n"
    );

    fs::File::open(&directories[0])
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - HOUR))
        .unwrap();
    runner.core.inner.lock().unwrap().reserved.clear();
    runner.maintenance().unwrap();
    assert_eq!(which_published(&directories), [false, true]);
}

/// An in-place sibling checkout an older runner left at `remote/source/<name>`.
fn legacy_checkout(f: &Fixture, runner: &Runner, name: &str) -> String {
    let path = f.scratch.join("remote/source").join(name);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("source"), "legacy\n").unwrap();
    let path = text(&path).to_string();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .sibling_commits
        .insert(path.clone(), commit_id(7));
    path
}

fn recorded(runner: &Runner, path: &str) -> bool {
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .sibling_commits
        .contains_key(path)
}

/// In snapshot mode maintenance discards an in-place sibling checkout that
/// nothing can use any more and drops its record; one that is also a
/// workspace keeps its directory and loses only the record. A checkout stays
/// while its path, or a path overlapping it, is reserved, being prepared or
/// admitted, being dropped, a workspace inside it, or the workspace of a job
/// that has not ended, and while a running job started with its siblings
/// bound from their host paths declares it.
#[test]
fn legacy_sibling_checkouts_go_once_nothing_can_use_them() {
    let f = Fixture::new();
    let runner = manual_runner(snapshot_config(&f).0);
    let now = runner.now();
    let unused = legacy_checkout(&f, &runner, "unused");
    let snapshotted = legacy_checkout(&f, &runner, "snapshotted");
    let workspace = legacy_checkout(&f, &runner, "workspace");
    let parent = legacy_checkout(&f, &runner, "parent");
    let reserved = legacy_checkout(&f, &runner, "reserved");
    let preparing = legacy_checkout(&f, &runner, "preparing");
    let admitting = legacy_checkout(&f, &runner, "admitting");
    let dropping = legacy_checkout(&f, &runner, "dropping");
    let queued = legacy_checkout(&f, &runner, "queued");
    host_checkout(&f);
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .sibling_commits
        .insert(f.canonical.clone(), commit_id(7));
    let legacy_user = sibling_user_job(&f, "legacy", now, 1, JobState::Running);
    let legacy_id = legacy_user.id;
    {
        let mut inner = runner.core.inner.lock().unwrap();
        for key in [workspace.clone(), format!("{parent}/child")] {
            inner.catalog.workspaces.insert(
                key,
                Workspace {
                    used_ms: now,
                    ..Workspace::default()
                },
            );
        }
        inner
            .reserved
            .insert(format!("{reserved}/nested"), reservation(&f, 0, now));
        let mut prepare = prepare_for(&f.run("true").0);
        prepare.workspace_key = preparing.clone();
        inner.preparing.insert(prepare.transfer_id, prepare);
        let mut admission = f.run("true").0;
        admission.workspace_key = admitting.clone();
        inner.admitting.insert(admission.job_id, admission);
        inner.dropping.insert(dropping.clone());
        let mut waiting = main_workspace_job(&f, now, 2, JobState::Queued);
        let request = waiting.request.as_mut().unwrap();
        request.workspace_key = queued.clone();
        request.cwd = queued.clone();
        inner.jobs.insert(waiting.id, waiting);
        // A sibling user started with snapshots never binds the host path.
        let mut mounted = running_job(&f, 0, now);
        mounted.request.as_mut().unwrap().siblings[0].canonical_path = snapshotted.clone();
        mounted.sibling_sources = Some(vec!["/state/sibling-snapshots/snapshotted/x".into()]);
        inner.jobs.insert(mounted.id, mounted);
        inner.jobs.insert(legacy_id, legacy_user);
    }
    runner.maintenance().unwrap();

    for gone in [&unused, &snapshotted] {
        assert!(!Path::new(gone).exists(), "{gone}");
        assert!(!recorded(&runner, gone), "{gone}");
    }
    assert!(Path::new(&workspace).join("source").exists());
    assert!(!recorded(&runner, &workspace));
    for kept in [
        &parent,
        &reserved,
        &preparing,
        &admitting,
        &dropping,
        &queued,
        &f.canonical,
    ] {
        assert!(Path::new(kept).exists(), "{kept}");
        assert!(recorded(&runner, kept), "{kept}");
    }

    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&legacy_id)
        .unwrap()
        .terminal(Outcome::Exit { code: 0 }, now);
    runner.maintenance().unwrap();
    assert!(
        !Path::new(&f.canonical).exists(),
        "the checkout goes once its legacy user has ended"
    );
    assert!(!recorded(&runner, &f.canonical));
}

/// Maintenance that runs while the first setup of a main-workspace job on a
/// legacy sibling checkout's path is paused leaves that checkout alone. Once
/// the setup has made the path a workspace, only the record goes.
#[tokio::test]
async fn paused_first_main_workspace_setup_keeps_the_legacy_checkout() {
    let f = Fixture::new();
    let host_commit = host_checkout(&f);
    let hold = Hold::new("materialize");
    let _release = Release(hold.clone());
    let (mut config, _) = snapshot_config(&f);
    config.disable_warm_builds = true;
    config.long_work = Some(hold.hook());
    let runner = Runner::open(config).unwrap();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .sibling_commits
        .insert(f.canonical.clone(), host_commit);
    let (run, bytes) = sibling_repository_job(&f, "true");
    hold.arm();
    f.submit(&runner, &run, &bytes);
    reached(&hold).await;
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(&f.canonical));
    runner.maintenance().unwrap();
    let kept_while_paused = (
        Path::new(&f.canonical).join(".git").exists(),
        recorded(&runner, &f.canonical),
    );
    hold.release();
    let t = terminal(&runner, run.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::Exit { code: 0 },
        "{:?}",
        t.refusal_detail
    );
    runner.maintenance().unwrap();
    assert_eq!(kept_while_paused, (true, true));
    assert!(Path::new(&f.canonical).join(".git").exists());
    assert!(!recorded(&runner, &f.canonical));
}

/// With in-place siblings a sibling checkout is live, so maintenance keeps
/// it and its record even when nothing uses it right now.
#[test]
fn in_place_runners_keep_their_sibling_checkouts() {
    let f = Fixture::new();
    let runner = manual_runner(f.config());
    assert!(runner.core.config.in_place_siblings());
    let unused = legacy_checkout(&f, &runner, "unused");
    runner.maintenance().unwrap();
    assert!(Path::new(&unused).join("source").exists());
    assert!(recorded(&runner, &unused));
}

/// Creates a real snapshot of the fixture sibling at its current commit, so
/// that a later creation can copy it as its predecessor, then commits a new
/// sibling commit for the next request to declare.
fn snapshot_of_current_sibling(
    f: &Fixture,
    runner: &Runner,
    environment: &JavascriptEnvironment,
) -> PathBuf {
    let head = git(&f.sibling, &["rev-parse", "HEAD"]);
    let tree = git(&f.sibling, &["rev-parse", "HEAD^{tree}"]);
    let mirror = runner.core.trees.ensure_mirror(&f.canonical).unwrap();
    git(
        &mirror,
        &[
            "fetch",
            "-q",
            text(&f.sibling),
            &format!("HEAD:refs/tips/{head}"),
        ],
    );
    let key = SnapshotKey::new(&f.canonical, &head, &head, environment).unwrap();
    let directory = runner
        .core
        .snapshots
        .resolve(&runner.core.config, &runner.core.trees, &key, &tree)
        .unwrap();
    commit(&f.sibling, "second\n");
    directory
}

/// A job's setup enforces the cap as soon as it has resolved its sibling
/// snapshots, without waiting for maintenance, and keeps the snapshot it
/// just created for itself.
#[tokio::test]
async fn job_setup_enforces_the_cap_after_resolving_its_snapshots() {
    let f = Fixture::new();
    let (mut config, environment) = snapshot_config(&f);
    config.sibling_snapshot_cap = 0;
    config.disable_warm_builds = true;
    let runner = Runner::open(config).unwrap();
    let unreferenced = snapshot_of_current_sibling(&f, &runner, &environment);
    let (run, bytes) = f.run("true");
    let created = SnapshotKey::new(
        &f.canonical,
        &run.siblings[0].commit,
        &run.siblings[0].commit,
        &environment,
    )
    .unwrap()
    .directory(&runner.core.config);
    f.submit(&runner, &run, &bytes);
    let t = terminal(&runner, run.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::Exit { code: 0 },
        "{:?}",
        t.refusal_detail
    );
    assert!(!published(&unreferenced));
    assert!(published(&created));
}

/// A Prepare enforces the cap once it has resolved its sibling snapshots.
#[tokio::test]
async fn prepare_enforces_the_cap_after_resolving_its_snapshots() {
    let f = Fixture::new();
    let (mut config, environment) = snapshot_config(&f);
    config.sibling_snapshot_cap = 0;
    let runner = Runner::open(config).unwrap();
    let unreferenced = snapshot_of_current_sibling(&f, &runner, &environment);
    let (run, bytes) = f.run("true");
    let created = SnapshotKey::new(
        &f.canonical,
        &run.siblings[0].commit,
        &run.siblings[0].commit,
        &environment,
    )
    .unwrap()
    .directory(&runner.core.config);
    let spool = f.spool(&runner, &run, &bytes);
    runner
        .core
        .trees
        .fetch(
            &spool.directory,
            &run.bundles,
            &run.repository_root,
            &run.snapshot_commit,
            "",
            &run.siblings,
        )
        .unwrap();
    spawn_prepare(&runner, &prepare_for(&run))
        .await
        .unwrap()
        .unwrap();
    assert!(!published(&unreferenced));
    assert!(published(&created));
}

/// The sequence a runner goes through when it is deployed with sibling
/// snapshots on top of a state left by in-place siblings: a job checked the
/// sibling out at its path and recorded it, then the runner restarts in
/// snapshot mode. Startup maintenance retires that checkout when nothing
/// claims it, and keeps the directory, dropping only the record, when the
/// path is also a catalogued workspace.
#[tokio::test]
async fn in_place_sibling_checkout_is_retired_when_the_runner_reopens_in_snapshot_mode() {
    for catalogued in [false, true] {
        let f = Fixture::new();
        let mut config = f.config();
        config.disable_warm_builds = true;
        let runner = Runner::open(config).unwrap();
        assert!(runner.core.config.in_place_siblings());
        let (run, bytes) = f.run("cat ../commons/source");
        f.submit(&runner, &run, &bytes);
        let t = terminal(&runner, run.job_id).await;
        assert_eq!(
            t.outcome,
            Outcome::Exit { code: 0 },
            "{:?}",
            t.refusal_detail
        );
        let checkout = Path::new(&f.canonical);
        assert!(checkout.join("source").exists());
        assert!(recorded(&runner, &f.canonical));
        if catalogued {
            let mut inner = runner.core.inner.lock().unwrap();
            let now = runner.now();
            inner.catalog.workspaces.insert(
                f.canonical.clone(),
                Workspace {
                    used_ms: now,
                    ..Workspace::default()
                },
            );
            runner.catalog(&inner).unwrap();
        }
        drop(runner);

        let (mut config, _) = snapshot_config(&f);
        config.disable_warm_builds = true;
        let runner = reopen(config);
        assert!(!recorded(&runner, &f.canonical), "{catalogued}");
        assert_eq!(
            checkout.join("source").exists(),
            catalogued,
            "catalogued: {catalogued}"
        );
    }
}

/// Workspaces are accepted through a symlinked prefix, so a legacy sibling
/// record can name a directory by another spelling than the workspace or
/// work that owns it. Legacy cleanup compares resolved paths too: a checkout
/// that is a catalogued workspace under the other spelling loses only its
/// record, and one that a running job, a reservation, a Prepare or a nested
/// catalogued workspace holds under the other spelling stays.
#[test]
fn legacy_cleanup_sees_owners_through_a_symlinked_prefix() {
    let f = Fixture::new();
    let runner = manual_runner(snapshot_config(&f).0);
    let now = runner.now();
    let real = f.scratch.join("remote/real");
    let alias = f.scratch.join("remote/alias");
    fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let names = [
        "unused",
        "catalogued",
        "nested",
        "running",
        "reserved",
        "preparing",
    ];
    for name in names {
        fs::create_dir_all(real.join(name)).unwrap();
        fs::write(real.join(name).join("source"), "legacy\n").unwrap();
    }
    let spelled = |base: &Path, name: &str| text(&base.join(name)).to_string();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        for name in names {
            inner
                .catalog
                .sibling_commits
                .insert(spelled(&alias, name), commit_id(7));
        }
        for key in [spelled(&real, "catalogued"), spelled(&real, "nested/inner")] {
            inner.catalog.workspaces.insert(
                key,
                Workspace {
                    used_ms: now,
                    ..Workspace::default()
                },
            );
        }
        let mut job = running_job(&f, 0, now);
        let request = job.request.as_mut().unwrap();
        request.workspace_key = spelled(&real, "running");
        request.cwd = request.workspace_key.clone();
        inner.jobs.insert(job.id, job);
        inner
            .reserved
            .insert(spelled(&real, "reserved"), reservation(&f, 0, now));
        let mut prepare = prepare_for(&f.run("true").0);
        prepare.workspace_key = spelled(&real, "preparing");
        inner.preparing.insert(prepare.transfer_id, prepare);
    }
    runner.maintenance().unwrap();
    let outcome = |name: &str| {
        (
            real.join(name).join("source").exists(),
            recorded(&runner, &spelled(&alias, name)),
        )
    };
    assert_eq!(outcome("unused"), (false, false), "nothing owns it");
    assert_eq!(
        outcome("catalogued"),
        (true, false),
        "a workspace: only the record goes"
    );
    for held in ["nested", "running", "reserved", "preparing"] {
        assert_eq!(outcome(held), (true, true), "{held}");
    }
}
