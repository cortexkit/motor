//! Sibling snapshot mode end to end: what a job's setup does with its
//! siblings in each mode, and which sibling fences scheduling applies.
//!
//! With in-place siblings (non-isolated runners) a job that uses a sibling
//! gets it checked out at the sibling's host path, and the runner never
//! creates the snapshot store. In snapshot mode (isolated Linux runners) each
//! sibling comes from a snapshot, the host path is only ever a main
//! workspace, and only a job started with its siblings bound from their host
//! paths still fences them. The Windows instance applies no sibling fence.
use super::*;

/// A configuration in sibling snapshot mode that portable fixtures can run:
/// no job isolation, no job-account ACLs, a fixed JavaScript environment, and
/// no systemd. Returns the environment too, which is part of each snapshot's
/// directory name.
#[cfg(unix)]
pub(super) fn snapshot_config(
    f: &Fixture,
) -> (Config, crate::sibling_snapshots::JavascriptEnvironment) {
    let environment =
        crate::sibling_snapshots::JavascriptEnvironment::digest(&BTreeMap::new(), b"", &[]);
    let mut config = f.config();
    config.sibling_snapshot_mode_override = Some(true);
    config.sibling_snapshot_access = Some(Arc::new(|_: &Path| Ok(())));
    config.javascript_environment_override = Some(environment.clone());
    // Startup in snapshot mode looks for helper units an earlier run left;
    // there are none.
    config.systemd_command = Some(Arc::new(|_: &[&str]| {
        use std::os::unix::process::ExitStatusExt;
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }));
    (config, environment)
}

/// Clones the fixture sibling at its current commit to its canonical host
/// path, as that repository's own main checkout, and returns that commit.
pub(super) fn host_checkout(f: &Fixture) -> String {
    let host = Path::new(&f.canonical);
    fs::create_dir_all(host.parent().unwrap()).unwrap();
    git(
        host.parent().unwrap(),
        &["clone", "-q", text(&f.sibling), text(host)],
    );
    git(host, &["rev-parse", "HEAD"])
}

/// The host checkout is exactly as `host_checkout` cloned it.
fn assert_host_checkout_untouched(f: &Fixture, commit: &str) {
    let host = Path::new(&f.canonical);
    assert_eq!(git(host, &["rev-parse", "HEAD"]), commit);
    assert_eq!(fs::read_to_string(host.join("source")).unwrap(), "first\n");
    assert_eq!(
        git(host, &["status", "--porcelain", "--ignored"]),
        "",
        "the host checkout must be exactly as cloned"
    );
}

/// Runs one job that reads its sibling and checks that the sibling was checked
/// out at its host path and that `<state>/sibling-snapshots` was not created.
async fn sibling_user_creates_no_snapshot_store(f: &Fixture, config: Config) {
    assert!(!config.sibling_snapshot_mode());
    assert!(config.in_place_siblings());
    let state = config.state_dir();
    let snapshots = config.sibling_snapshots_dir();
    assert_eq!(snapshots, state.join("sibling-snapshots"));
    let runner = Runner::open(config).unwrap();
    let (run, bytes) = f.run("cat ../commons/source");
    let sibling_commit = run.siblings[0].commit.clone();
    f.submit(&runner, &run, &bytes);
    let t = terminal(&runner, run.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::Exit { code: 0 },
        "{:?}",
        t.refusal_detail
    );
    assert_eq!(job_output(&runner, run.job_id), "first\n");
    base_done(&runner).await;
    assert_eq!(
        git(Path::new(&f.canonical), &["rev-parse", "HEAD"]),
        sibling_commit,
        "the sibling must still be checked out in place at its host path"
    );
    // The runner did write its state, so an absent store is not a wrong path.
    assert!(state.join("jobs").is_dir());
    assert!(
        !snapshots.exists(),
        "{} must not exist while sibling snapshot mode is off",
        snapshots.display()
    );
}

#[tokio::test]
async fn sibling_user_leaves_no_sibling_snapshot_store() {
    let f = Fixture::new();
    sibling_user_creates_no_snapshot_store(&f, f.config()).await;
}

/// The in-place mode, which isolated runners no longer use by default, still
/// creates no snapshot store when it is selected explicitly.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_sibling_user_leaves_no_sibling_snapshot_store() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.sibling_snapshot_mode_override = Some(false);
    sibling_user_creates_no_snapshot_store(&f, config).await;
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}

/// In snapshot mode a Prepare takes each sibling from its snapshot and never
/// reads or writes the sibling's host path, which here is the sibling
/// repository's own checkout at an older commit.
#[cfg(unix)]
#[tokio::test]
async fn prepare_in_snapshot_mode_leaves_the_sibling_host_path_alone() {
    let f = Fixture::new();
    let host_commit = host_checkout(&f);
    let (sibling_commit, sibling_tree) = commit(&f.sibling, "second\n");
    assert_ne!(sibling_commit, host_commit);

    let (config, environment) = snapshot_config(&f);
    let snapshot = crate::sibling_snapshots::SnapshotKey::new(
        &f.canonical,
        &sibling_commit,
        &sibling_commit,
        &environment,
    )
    .unwrap()
    .directory(&config);
    let runner = Runner::open(config).unwrap();
    let (run, bytes) = f.run("true");
    assert_eq!(run.siblings[0].commit, sibling_commit);
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

    assert_host_checkout_untouched(&f, &host_commit);
    assert_eq!(
        fs::read_to_string(snapshot.join("source")).unwrap(),
        "second\n"
    );
    let workspace = Path::new(&f.key);
    assert_eq!(
        fs::read_to_string(workspace.join("source")).unwrap(),
        "first\n"
    );
    let alias = workspace.parent().unwrap().join("commons");
    assert!(
        fs::symlink_metadata(&alias).is_err(),
        "no host alias link to the sibling's host path is made"
    );
    let inner = runner.core.inner.lock().unwrap();
    assert!(!inner.catalog.sibling_commits.contains_key(&f.canonical));
    let entry = &inner.catalog.workspaces[&f.key];
    assert_eq!(entry.siblings, run.siblings);
    assert_eq!(
        serde_json::to_value(&entry.sibling_freshness[&f.canonical]).unwrap(),
        serde_json::json!([{ "tree": sibling_tree }]),
        "a first Prepare of the workspace records the snapshot's tree as current"
    );
}

/// In snapshot mode a job's setup takes its sibling from a snapshot and
/// records the snapshot in the job's record; the sibling's host path, here
/// the sibling repository's own checkout at an older commit, is left alone.
/// Without isolation the job itself cannot see the snapshot, so this checks
/// the setup and its records; the isolated tests below check what jobs see.
#[cfg(unix)]
#[tokio::test]
async fn job_in_snapshot_mode_records_its_snapshots_and_leaves_the_host_path_alone() {
    let f = Fixture::new();
    let host_commit = host_checkout(&f);
    let (sibling_commit, sibling_tree) = commit(&f.sibling, "second\n");
    let (mut config, environment) = snapshot_config(&f);
    config.disable_warm_builds = true;
    let snapshot = crate::sibling_snapshots::SnapshotKey::new(
        &f.canonical,
        &sibling_commit,
        &sibling_commit,
        &environment,
    )
    .unwrap()
    .directory(&config);
    let runner = Runner::open(config).unwrap();
    let (run, bytes) = f.run("true");
    f.submit(&runner, &run, &bytes);
    let t = terminal(&runner, run.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::Exit { code: 0 },
        "{:?}",
        t.refusal_detail
    );

    let record: Job = read_json(&runner.record_path(run.job_id)).unwrap();
    assert_eq!(
        record.sibling_sources,
        Some(vec![snapshot.to_string_lossy().into_owned()]),
        "the saved record names the snapshot the job's unit binds"
    );
    assert_eq!(
        fs::read_to_string(snapshot.join("source")).unwrap(),
        "second\n"
    );
    assert!(
        !snapshot.join(".git/objects/info/alternates").exists(),
        "a snapshot's Git directory must not borrow objects from the mirror, \
         which jobs cannot read"
    );
    assert_eq!(git(&snapshot, &["rev-parse", "HEAD"]), sibling_commit);
    assert_host_checkout_untouched(&f, &host_commit);
    let alias = Path::new(&f.key).parent().unwrap().join("commons");
    assert!(
        fs::symlink_metadata(&alias).is_err(),
        "no host alias link to the sibling's host path is made"
    );
    let inner = runner.core.inner.lock().unwrap();
    assert!(
        !inner.catalog.sibling_commits.contains_key(&f.canonical),
        "snapshot mode records no in-place sibling commit"
    );
    assert_eq!(
        serde_json::to_value(&inner.catalog.workspaces[&f.key].sibling_freshness[&f.canonical])
            .unwrap(),
        serde_json::json!([{ "tree": sibling_tree }]),
        "a completed setup records the snapshot's tree as the workspace's current one"
    );
}

/// A runner whose scheduler runs only when a test calls `dispatch`.
#[cfg(unix)]
pub(super) fn manual_runner(mut config: Config) -> Runner {
    config.max_weight = 8;
    config.manual_tick = true;
    config.disable_warm_builds = true;
    Runner::open(config).unwrap()
}

/// A job of the fixture's repository in its own workspace `name`, which
/// declares the fixture sibling, in `state`.
pub(super) fn sibling_user_job(
    f: &Fixture,
    name: &str,
    now: u64,
    order: u64,
    state: JobState,
) -> Job {
    let (request, _) = job_at(f, name, "true");
    assert_eq!(request.siblings[0].canonical_path, f.canonical);
    let mut job = Job::new(request.job_id, Some(request), now, order, 2);
    job.state = state;
    job.queued_ms = Some(now);
    if state == JobState::Running {
        job.started_ms = Some(now);
    }
    job
}

/// A job whose main workspace is the fixture sibling's canonical path, the
/// directory the sibling user's sibling is checked out at in in-place mode.
pub(super) fn main_workspace_job(f: &Fixture, now: u64, order: u64, state: JobState) -> Job {
    let (mut request, _) = f.run("true");
    request.workspace_key = f.canonical.clone();
    request.cwd = f.canonical.clone();
    request.siblings.clear();
    let mut job = Job::new(request.job_id, Some(request), now, order, 2);
    job.state = state;
    job.queued_ms = Some(now);
    if state == JobState::Running {
        job.started_ms = Some(now);
    }
    job
}

/// Waits for a start that dispatch began to settle. The jobs these tests
/// start were never uploaded, so their setup refuses them; waiting keeps the
/// setup thread from outliving the fixture.
fn settle_start(runner: &Runner, id: Uuid) {
    assert!(
        poll(Duration::from_secs(30), || !runner
            .core
            .inner
            .lock()
            .unwrap()
            .starting
            .contains(&id)),
        "the start of {id} did not settle"
    );
}

/// In snapshot mode a running job whose record has no `sibling_sources` was
/// started with its sibling bound from the host path, by an older runner, so
/// a main-workspace job on that path waits until it ends, named as waiting
/// on a sibling user of its checkout.
#[cfg(unix)]
#[test]
fn legacy_sibling_user_keeps_a_main_workspace_job_on_its_sibling_queued_until_it_ends() {
    let f = Fixture::new();
    let runner = manual_runner(snapshot_config(&f).0);
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let legacy = sibling_user_job(&f, "legacy", now, 1, JobState::Running);
    assert_eq!(legacy.sibling_sources, None);
    let (legacy_id, legacy_key) = (legacy.id, legacy.key().to_owned());
    inner.jobs.insert(legacy_id, legacy);
    let main = main_workspace_job(&f, now, 2, JobState::Queued);
    let main_id = main.id;
    inner.jobs.insert(main_id, main);

    runner.dispatch(&mut inner, now).unwrap();
    let started_while_legacy_ran = inner.starting.contains(&main_id);
    let occupied = Runner::occupied(&inner, &f.canonical);
    let phase = runner
        .queue_wait_phase(&inner, &DispatchIndex::build(&inner), main_id)
        .map(str::to_owned);
    let detail = runner.queue_wait_detail(&inner, main_id);

    inner
        .jobs
        .get_mut(&legacy_id)
        .unwrap()
        .terminal(Outcome::Exit { code: 0 }, now);
    runner.dispatch(&mut inner, now).unwrap();
    let started_after = inner.starting.contains(&main_id);
    drop(inner);
    if started_after {
        settle_start(&runner, main_id);
    }

    assert!(!started_while_legacy_ran);
    assert!(occupied);
    assert_eq!(phase.as_deref(), Some(SIBLING_USER_WAIT));
    assert!(
        detail.starts_with(&format!(
            "waiting_on=sibling user of this checkout; sibling_users=[job={legacy_id} \
             workspace={legacy_key} permits=2];"
        )),
        "{detail}"
    );
    assert!(
        started_after,
        "the main-workspace job starts once the legacy user ends"
    );
}

/// In snapshot mode a sibling user that mounts its sibling from a snapshot
/// and a job whose main workspace is the sibling's host path never hold each
/// other, in either start order.
#[cfg(unix)]
#[test]
fn snapshot_sibling_user_and_main_workspace_job_start_in_either_order() {
    for main_runs_first in [false, true] {
        let f = Fixture::new();
        let runner = manual_runner(snapshot_config(&f).0);
        let now = runner.now();
        let mut inner = runner.core.inner.lock().unwrap();
        let (running, queued) = if main_runs_first {
            (
                main_workspace_job(&f, now, 1, JobState::Running),
                sibling_user_job(&f, "user", now, 2, JobState::Queued),
            )
        } else {
            let mut user = sibling_user_job(&f, "user", now, 1, JobState::Running);
            user.sibling_sources = Some(vec!["/state/sibling-snapshots/commons/b2".into()]);
            (user, main_workspace_job(&f, now, 2, JobState::Queued))
        };
        let queued_id = queued.id;
        inner.jobs.insert(running.id, running);
        inner.jobs.insert(queued_id, queued);
        let phase = runner
            .queue_wait_phase(&inner, &DispatchIndex::build(&inner), queued_id)
            .map(str::to_owned);
        runner.dispatch(&mut inner, now).unwrap();
        let started = inner.starting.contains(&queued_id);
        drop(inner);
        if started {
            settle_start(&runner, queued_id);
        }
        assert_eq!(phase.as_deref(), Some("queue order"), "{main_runs_first}");
        assert!(started, "main runs first: {main_runs_first}");
    }
}

fn sibling(path: &str, commit: &str) -> SiblingSnapshot {
    SiblingSnapshot {
        canonical_path: path.into(),
        commit: commit.into(),
        tree_hash: String::new(),
        base_commit: None,
    }
}

/// The fencing table of snapshot mode. A running or reserved job that mounts
/// its siblings from snapshots holds only its own workspace, sibling users of
/// one path never block each other at any commit, a queued main-workspace job
/// holds no later sibling user back, and a drop of a sibling's host path
/// fences only that workspace. Only a running legacy sibling user still holds
/// the paths of its siblings.
#[cfg(unix)]
#[test]
fn snapshot_mode_fences_workspaces_only_apart_from_legacy_sibling_users() {
    let f = Fixture::new();
    let runner = manual_runner(snapshot_config(&f).0);
    let now = runner.now();
    let (request, _) = f.run("true");
    let mut inner = runner.core.inner.lock().unwrap();
    let mut ids = Vec::new();
    for (key, path, sources) in [
        (
            "/active/workspace",
            "/shared/running",
            Some(vec!["/state/sibling-snapshots/running/old".to_string()]),
        ),
        ("/legacy/workspace", "/shared/legacy", None),
    ] {
        let mut request = request.clone();
        request.job_id = Uuid::now_v7();
        request.workspace_key = key.into();
        request.cwd = key.into();
        request.siblings = vec![sibling(path, "old")];
        let mut job = Job::new(request.job_id, Some(request), now, 1, 2);
        job.state = JobState::Running;
        job.started_ms = Some(now);
        job.sibling_sources = sources;
        ids.push(job.id);
        inner.jobs.insert(job.id, job);
    }
    inner.reserved.insert(
        "/setup/workspace".into(),
        Reservation {
            repository: String::new(),
            commit: String::new(),
            siblings: vec![sibling("/shared/reserved", "new")],
            since_ms: now,
        },
    );
    let index = DispatchIndex::build(&inner);
    let mut occupied = Vec::new();
    for key in [
        "/active/workspace",
        "/setup/workspace",
        "/shared/running/child",
        "/shared/reserved",
        "/shared/legacy/child",
        "/free",
    ] {
        occupied.push((key, index.occupied(key), Runner::occupied(&inner, key)));
    }
    let mut blocked = Vec::new();
    for (path, commit) in [
        ("/shared/running", "different"),
        ("/shared/reserved", "different"),
        ("/shared/legacy", "different"),
        ("/active/workspace", "any"),
        ("/setup/workspace/child", "any"),
    ] {
        let siblings = [sibling(path, commit)];
        blocked.push((
            path,
            index.siblings_blocked(&siblings, &inner.catalog.sibling_commits),
            Runner::siblings_blocked(&inner, &siblings),
        ));
    }
    // A queued main-workspace job on a path, and a sibling user of that path
    // accepted after it.
    let mut main = Job::new(Uuid::now_v7(), Some(request.clone()), now, 2, 2);
    main.request.as_mut().unwrap().workspace_key = "/queued/main".into();
    main.request.as_mut().unwrap().siblings.clear();
    main.state = JobState::Queued;
    let mut later = Job::new(Uuid::now_v7(), Some(request.clone()), now, 3, 2);
    later.request.as_mut().unwrap().workspace_key = "/later/user".into();
    later.request.as_mut().unwrap().siblings = vec![sibling("/queued/main", "any")];
    later.state = JobState::Queued;
    let later_job = later.clone();
    ids.extend([main.id, later.id]);
    inner.jobs.insert(main.id, main);
    inner.jobs.insert(later.id, later);
    let held = DispatchIndex::build(&inner).held_by_queued_workspace(
        &inner,
        &later_job,
        runner.core.config.max_weight,
    );
    inner.dropping.insert("/shared/running".into());
    let deleting_user = Runner::deleting(
        &inner,
        "/other/workspace",
        &[sibling("/shared/running", "old")],
    );
    let deleting_workspace = Runner::deleting(&inner, "/shared/running", &[]);
    inner.dropping.clear();
    for id in ids {
        inner.jobs.remove(&id);
    }
    inner.reserved.clear();
    drop(inner);

    assert_eq!(
        occupied,
        [
            ("/active/workspace", true, true),
            ("/setup/workspace", true, true),
            ("/shared/running/child", false, false),
            ("/shared/reserved", false, false),
            ("/shared/legacy/child", true, true),
            ("/free", false, false),
        ]
    );
    assert!(
        blocked
            .iter()
            .all(|(_, indexed, established)| !indexed && !established),
        "{blocked:?}"
    );
    assert!(!held);
    assert!(!deleting_user);
    assert!(deleting_workspace);
}

/// The Windows instance stages a per-job export of every sibling and never
/// uses a host path, so it applies no sibling fence: not between sibling
/// users of one path at two commits, and not between a sibling user and a
/// main-workspace job on that path, even though its job records never carry
/// `sibling_sources`. The instance runs one job at a time, so a queued job
/// waits only for permits.
#[cfg(unix)]
#[test]
fn windows_instance_applies_no_sibling_fence() {
    let f = Fixture::new();
    let mut config = super::windows_instance::config(&f);
    config.manual_tick = true;
    let runner = Runner::open_with_backend(config, Box::new(NoWindowsSetup)).unwrap();
    assert!(!runner.core.config.in_place_siblings());
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let mut user = sibling_user_job(&f, "user", now, 1, JobState::Running);
    user.weight = 4;
    assert_eq!(user.sibling_sources, None);
    let commit = user.request.as_ref().unwrap().siblings[0].commit.clone();
    let mut other = sibling_user_job(&f, "other", now, 2, JobState::Queued);
    other.weight = 4;
    other.request.as_mut().unwrap().siblings[0].commit = "0".repeat(40);
    assert_ne!(commit, "0".repeat(40));
    let other_siblings = other.request.as_ref().unwrap().siblings.clone();
    let mut main = main_workspace_job(&f, now, 3, JobState::Queued);
    main.weight = 4;
    let (other_id, main_id) = (other.id, main.id);
    for job in [user, other, main] {
        inner.jobs.insert(job.id, job);
    }
    let index = DispatchIndex::build(&inner);
    let occupied = (
        index.occupied(&f.canonical),
        Runner::occupied(&inner, &f.canonical),
    );
    let blocked = (
        index.siblings_blocked(&other_siblings, &inner.catalog.sibling_commits),
        Runner::siblings_blocked(&inner, &other_siblings),
    );
    let phases = [other_id, main_id].map(|id| {
        runner
            .queue_wait_phase(&inner, &index, id)
            .map(str::to_owned)
    });
    drop(inner);
    assert_eq!(occupied, (false, false));
    assert_eq!(blocked, (false, false));
    assert_eq!(phases, [Some("permits".into()), Some("permits".into())]);
}

/// A backend for `windows_instance_applies_no_sibling_fence`, which never
/// starts a job.
#[cfg(unix)]
struct NoWindowsSetup;

#[cfg(unix)]
impl backend::Backend for NoWindowsSetup {
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
        Ok(None)
    }
    fn collect_changes(&self, _: &Trees, _: &Job) -> io::Result<Option<Vec<String>>> {
        Ok(None)
    }
    fn reconcile(&self, _: &Config, _: &[Job]) -> io::Result<Vec<Uuid>> {
        Ok(Vec::new())
    }
}

/// A job of the fixture sibling's own repository in its main workspace, the
/// sibling's canonical path, at the sibling repository's current commit.
#[cfg(unix)]
pub(super) fn sibling_repository_job(f: &Fixture, command: &str) -> (Run, Vec<Vec<u8>>) {
    let (mut run, _) = f.run(command);
    let (bundle, bytes) = f.bundle(&f.sibling, &f.canonical, 0);
    let head = git(&f.sibling, &["rev-parse", "HEAD"]);
    run.workspace_key = f.canonical.clone();
    run.repository_root = f.canonical.clone();
    run.cwd = f.canonical.clone();
    run.workspace_kind = Some(WorkspaceKind::Head);
    run.snapshot_commit = head.clone();
    run.tree_hash = git(&f.sibling, &["rev-parse", "HEAD^{tree}"]);
    run.integration_tip = head.clone();
    run.base_commit = Some(head);
    run.siblings.clear();
    run.bundles = vec![bundle];
    (run, vec![bytes])
}

/// Shell that runs Git read-only in the job's sibling at both places it is
/// mounted, the `../commons` alias and the canonical path: it prints the
/// sibling's HEAD at each and its last commit, and `git status`, which prints
/// nothing for the clean snapshot. The sibling belongs to the runner account,
/// so each command fails with "dubious ownership" unless the job's Git
/// config lists that path as safe.
#[cfg(target_os = "linux")]
fn sibling_git(f: &Fixture) -> String {
    format!(
        "git -C ../commons rev-parse HEAD; git -C '{0}' rev-parse HEAD; \
         git -C ../commons log -1 --format=%H; git -C ../commons status --porcelain; \
         git -C '{0}' status --porcelain",
        f.canonical
    )
}

/// Shell that prints what the job sees, marks itself started, waits for the
/// test's release file in its workspace, then prints `after` again.
#[cfg(target_os = "linux")]
fn held(before: &str, after: &str) -> String {
    format!("{before}; touch marker; while [ ! -f release ]; do sleep 0.05; done; {after}")
}

/// Waits until the job in `workspace` has written its start marker.
#[cfg(target_os = "linux")]
async fn reached_marker(runner: &Runner, id: Uuid, workspace: &str) {
    let marker = Path::new(workspace).join("marker");
    until_with_diagnostics(
        || marker.exists(),
        || {
            format!(
                "job {id} never started: {:?}",
                runner.core.inner.lock().unwrap().jobs[&id].terminal
            )
        },
    )
    .await;
}

/// Lets the held job in `workspace` finish. The workspace belongs to the job
/// account while its job runs.
#[cfg(target_os = "linux")]
fn release(workspace: &str) {
    assert!(Command::new("/usr/bin/sudo")
        .args(["-n", "-u", "motor-job", "/usr/bin/touch", "--"])
        .arg(Path::new(workspace).join("release"))
        .status()
        .unwrap()
        .success());
}

/// A job whose main workspace is the sibling repository's checkout at B1 and
/// a job in another workspace that declares that repository as a sibling at
/// B2 run at the same time, in both start orders. Each sees its own tree: the
/// sibling user at the sibling's canonical path and at `../commons`, which
/// here is a worktree alias in another directory, and the main-workspace job
/// in its own checkout. The host checkout stays at B1 throughout.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_main_workspace_and_sibling_user_run_concurrently() {
    for main_first in [true, false] {
        let f = Fixture::new();
        let mut config = f.config();
        config.isolate_jobs = true;
        config.disable_warm_builds = true;
        assert!(config.sibling_snapshot_mode());
        let snapshots = config
            .sibling_snapshots_dir()
            .join(workspace::name(&f.canonical));
        let runner = Runner::open(config).unwrap();
        let b1 = git(&f.sibling, &["rev-parse", "HEAD"]);
        let (main, main_bytes) =
            sibling_repository_job(&f, &held("git rev-parse HEAD; cat source", "cat source"));
        let (b2, _) = commit(&f.sibling, "second\n");
        let (user, user_bytes) = f.run(&held(
            &format!(
                "{}; cat ../commons/source; cat '{}/source'",
                sibling_git(&f),
                f.canonical
            ),
            "cat ../commons/source",
        ));
        assert_ne!(
            Path::new(&f.key).parent().unwrap().join("commons"),
            Path::new(&f.canonical)
        );
        let ((first, first_bytes), (second, second_bytes)) = if main_first {
            ((&main, &main_bytes), (&user, &user_bytes))
        } else {
            ((&user, &user_bytes), (&main, &main_bytes))
        };
        f.submit(&runner, first, first_bytes);
        reached_marker(&runner, first.job_id, &first.workspace_key).await;
        f.submit(&runner, second, second_bytes);
        reached_marker(&runner, second.job_id, &second.workspace_key).await;
        // Both are running: the second started before the first finished.
        {
            let inner = runner.core.inner.lock().unwrap();
            assert_eq!(inner.jobs[&first.job_id].state, JobState::Running);
            assert_eq!(inner.jobs[&second.job_id].state, JobState::Running);
        }
        let host_head = || {
            fs::read_to_string(Path::new(&f.canonical).join(".git/HEAD"))
                .unwrap()
                .trim()
                .to_string()
        };
        assert_eq!(host_head(), b1, "main first: {main_first}");
        let snapshot_names: Vec<_> = fs::read_dir(&snapshots)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            snapshot_names
                .iter()
                .any(|name| name.starts_with(&b2) && !name.contains('.')),
            "{snapshot_names:?}"
        );
        release(&first.workspace_key);
        release(&second.workspace_key);
        for id in [main.job_id, user.job_id] {
            assert_eq!(
                terminal(&runner, id).await.outcome,
                Outcome::Exit { code: 0 },
                "main first: {main_first}"
            );
        }
        assert_eq!(
            job_output(&runner, main.job_id),
            format!("{b1}\nfirst\nfirst\n")
        );
        assert_eq!(
            job_output(&runner, user.job_id),
            format!("{b2}\n{b2}\n{b2}\nsecond\nsecond\nsecond\n")
        );
        assert_eq!(host_head(), b1);
        drop(runner);
        crate::isolation::restore_runner_access(&f.scratch).unwrap();
    }
}

/// Two jobs that declare one sibling at two commits run at the same time and
/// each sees its own tree, and neither checks the sibling out at its host
/// path.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_sibling_users_at_two_commits_run_concurrently() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.disable_warm_builds = true;
    let runner = Runner::open(config).unwrap();
    let read = format!("{}; cat ../commons/source", sibling_git(&f));
    let b1 = git(&f.sibling, &["rev-parse", "HEAD"]);
    let (first, first_bytes) = job_at(&f, "first", &held(&read, "cat ../commons/source"));
    let (b2, _) = commit(&f.sibling, "second\n");
    let (second, second_bytes) = job_at(&f, "second", &held(&read, "cat ../commons/source"));
    f.submit(&runner, &first, &first_bytes);
    reached_marker(&runner, first.job_id, &first.workspace_key).await;
    f.submit(&runner, &second, &second_bytes);
    reached_marker(&runner, second.job_id, &second.workspace_key).await;
    {
        let inner = runner.core.inner.lock().unwrap();
        assert_eq!(inner.jobs[&first.job_id].state, JobState::Running);
        assert_eq!(inner.jobs[&second.job_id].state, JobState::Running);
    }
    release(&first.workspace_key);
    release(&second.workspace_key);
    for id in [first.job_id, second.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
    assert_eq!(
        job_output(&runner, first.job_id),
        format!("{b1}\n{b1}\n{b1}\nfirst\nfirst\n")
    );
    assert_eq!(
        job_output(&runner, second.job_id),
        format!("{b2}\n{b2}\n{b2}\nsecond\nsecond\n")
    );
    assert!(
        !Path::new(&f.canonical).exists(),
        "no sibling is checked out at its host path"
    );
    drop(runner);
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}
