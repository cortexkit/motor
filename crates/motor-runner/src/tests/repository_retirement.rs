use super::*;
use crate::engine::retirement::MIRROR_UNUSED_MS;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn stamp(runner: &Runner, repository: &str) -> Option<u64> {
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .mirror_used_ms
        .get(&workspace::name(repository))
        .copied()
}

fn set_stamp(runner: &Runner, repository: &str, at: u64) {
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .mirror_used_ms
        .insert(workspace::name(repository), at);
}

/// A JavaScript cache entry owned by `owner`'s mirror, as an install leaves it.
fn javascript_entry(runner: &Runner, kind: &str, key: char, owner: &str) -> PathBuf {
    let entry = runner
        .core
        .trees
        .state
        .join(kind)
        .join(key.to_string().repeat(64));
    fs::create_dir_all(entry.join("node_modules")).unwrap();
    fs::write(
        entry.join(workspace::JAVASCRIPT_CACHE_OWNER),
        workspace::name(owner),
    )
    .unwrap();
    entry
}

/// A runner with no background scheduler, so only the test runs retirement.
fn quiet_runner(f: &Fixture) -> Runner {
    let mut config = f.config();
    config.manual_tick = true;
    config.disable_warm_builds = true;
    Runner::open(config).unwrap()
}

#[tokio::test]
async fn unused_repository_retires_after_thirty_days_with_its_derived_state_across_a_restart() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let runner = Runner::open(config.clone()).unwrap();
    // The job runs in the repository's Head workspace, with one sibling.
    let (mut r, bytes) = f.run("true");
    r.workspace_kind = Some(WorkspaceKind::Head);
    r.workspace_key = f.root.clone();
    r.cwd = f.root.clone();
    f.submit(&runner, &r, &bytes);
    terminal(&runner, r.job_id).await;
    runner.maintenance().unwrap();
    let used = runner.core.inner.lock().unwrap().jobs[&r.job_id]
        .finished_ms
        .unwrap();
    assert_eq!(stamp(&runner, &f.root), Some(used));
    assert_eq!(stamp(&runner, &f.canonical), Some(used));
    let main_install = javascript_entry(&runner, "js-installs", 'a', &f.root);
    let other_install = javascript_entry(&runner, "js-installs", 'b', "/elsewhere/repository");
    let sibling_manager = javascript_entry(&runner, "js-manager-caches", 'c', &f.canonical);

    // A restart twenty days later is neither use nor a reset of the clock.
    drop(runner);
    f.clock.store(used + 20 * DAY_MS, Ordering::SeqCst);
    let runner = reopen(config);
    runner.maintenance().unwrap();
    assert_eq!(stamp(&runner, &f.root), Some(used));
    assert_eq!(stamp(&runner, &f.canonical), Some(used));

    f.clock.store(used + MIRROR_UNUSED_MS - 1, Ordering::SeqCst);
    runner.maintenance().unwrap();
    let main_mirror = runner.core.trees.mirror(&f.root);
    let sibling_mirror = runner.core.trees.mirror(&f.canonical);
    assert!(main_mirror.exists() && sibling_mirror.exists());
    assert!(Path::new(&f.root).exists());

    f.clock.store(used + MIRROR_UNUSED_MS, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(!main_mirror.exists());
    assert!(!sibling_mirror.exists());
    // The Head workspace and the sibling checkout borrowed the mirrors'
    // objects; they went first, through the trash.
    assert!(!Path::new(&f.root).exists());
    assert!(!Path::new(&f.canonical).exists());
    assert!(!main_install.exists());
    assert!(!sibling_manager.exists());
    assert!(other_install.exists());
    let inner = runner.core.inner.lock().unwrap();
    assert!(!inner.catalog.workspaces.contains_key(&f.root));
    assert!(inner.catalog.sibling_commits.is_empty());
    assert!(!inner.catalog.repositories.contains_key(&f.root));
    assert!(inner.catalog.mirror_used_ms.is_empty());
}

#[test]
fn a_retained_job_a_pending_fill_or_an_in_use_workspace_keeps_an_old_mirror() {
    let f = Fixture::new();
    let runner = quiet_runner(&f);
    let mirror = runner.core.trees.ensure_mirror(&f.root).unwrap();
    let now = runner.now();
    // Every dependent below carries a use older than thirty days, so only the
    // fact that it still depends on the mirror can keep it.
    let old = now - MIRROR_UNUSED_MS - DAY_MS;
    let (request, _) = f.run("true");
    for dependent in ["retained job", "pending fill", "workspace"] {
        set_stamp(&runner, &f.root, old);
        {
            let mut inner = runner.core.inner.lock().unwrap();
            match dependent {
                "retained job" => {
                    let mut job = Job::new(request.job_id, Some(request.clone()), old, 1, 1);
                    job.state = JobState::AwaitingBundles;
                    inner.jobs.insert(job.id, job);
                }
                "pending fill" => {
                    inner.catalog.repositories.insert(
                        f.root.clone(),
                        Repository {
                            result: Some(RebuildResult::Building),
                            finished_ms: Some(old),
                            ..Repository::default()
                        },
                    );
                }
                _ => workspace::record_workspace(
                    &mut inner.catalog,
                    &f.key,
                    &f.root,
                    "snapshot",
                    &[],
                    old,
                ),
            }
        }
        runner.retire_unused_repositories().unwrap();
        assert!(mirror.exists(), "{dependent} must keep the mirror");
        assert_eq!(stamp(&runner, &f.root), Some(old), "{dependent}");
        let mut inner = runner.core.inner.lock().unwrap();
        inner.jobs.clear();
        inner.catalog.repositories.clear();
        inner.catalog.workspaces.clear();
    }
    runner.retire_unused_repositories().unwrap();
    assert!(!mirror.exists());
    assert_eq!(stamp(&runner, &f.root), None);
}

#[test]
fn use_as_a_sibling_resets_the_clock() {
    let f = Fixture::new();
    let runner = quiet_runner(&f);
    let main = runner.core.trees.ensure_mirror(&f.root).unwrap();
    let sibling = runner.core.trees.ensure_mirror(&f.canonical).unwrap();
    let now = runner.now();
    let old = now - MIRROR_UNUSED_MS;
    set_stamp(&runner, &f.root, old);
    set_stamp(&runner, &f.canonical, old);
    // Another repository's job, a day ago, declared the sibling.
    let (mut request, _) = f.run("true");
    request.repository_root = text(&f.scratch.join("remote/source/other")).into();
    let recent = now - DAY_MS;
    let mut job = Job::new(request.job_id, Some(request.clone()), recent, 1, 1);
    job.terminal(Outcome::Exit { code: 0 }, recent);
    runner.core.inner.lock().unwrap().jobs.insert(job.id, job);
    runner.retire_unused_repositories().unwrap();
    assert_eq!(stamp(&runner, &f.canonical), Some(recent));
    assert!(
        !main.exists(),
        "the main repository had no use for thirty days"
    );
    // The job record expiring later does not take that use back.
    runner.core.inner.lock().unwrap().jobs.clear();
    runner.retire_unused_repositories().unwrap();
    assert!(sibling.exists());
    f.clock
        .store(recent + MIRROR_UNUSED_MS - 1, Ordering::SeqCst);
    runner.retire_unused_repositories().unwrap();
    assert!(sibling.exists());
    f.clock.store(recent + MIRROR_UNUSED_MS, Ordering::SeqCst);
    runner.retire_unused_repositories().unwrap();
    assert!(!sibling.exists());
}

/// A have-reply or job admission that arrives while a mirror is being retired
/// waits for the retirement to finish, then sees no mirror and starts fresh:
/// never a mirror whose dependents are gone but whose refs remain.
#[test]
fn work_arriving_mid_retirement_waits_and_then_starts_fresh() {
    let f = Fixture::new();
    let hold = Hold::new("retire_mirror");
    let _release = Release(hold.clone());
    let mut config = f.config();
    config.manual_tick = true;
    config.disable_warm_builds = true;
    config.long_work = Some(hold.hook());
    let runner = Runner::open(config).unwrap();
    let mirror = runner.core.trees.ensure_mirror(&f.root).unwrap();
    let head = git(&f.main, &["rev-parse", "HEAD"]);
    git(
        &mirror,
        &[
            "fetch",
            "-q",
            text(&f.main),
            &format!("HEAD:refs/tips/{head}"),
        ],
    );
    set_stamp(&runner, &f.root, runner.now() - MIRROR_UNUSED_MS);
    hold.arm();
    let retiring = std::thread::spawn({
        let runner = runner.clone();
        move || runner.retire_unused_repositories()
    });
    assert!(poll(Duration::from_secs(10), || hold
        .reached
        .load(Ordering::SeqCst)));
    let asking = std::thread::spawn({
        let runner = runner.clone();
        let candidates = vec![RepositoryCandidates {
            repository: f.root.clone(),
            commits: vec![head.clone()],
        }];
        move || runner.have(candidates)
    });
    let (request, bytes) = f.run("true");
    let admitting = std::thread::spawn({
        let runner = runner.clone();
        let request = request.clone();
        move || runner.accept(request)
    });
    // Both wait on the retirement's locks rather than reading a mirror
    // that is part way through being removed.
    assert!(!poll(Duration::from_millis(300), || asking.is_finished()
        || admitting.is_finished()));
    assert!(mirror.exists());
    hold.release();
    retiring.join().unwrap().unwrap();
    let held = asking.join().unwrap();
    assert!(held[0].commits.is_empty(), "{held:?}");
    assert!(matches!(
        admitting.join().unwrap().unwrap(),
        Admission::Accepted(_)
    ));
    // The admitted job brings its full history and gets a fresh mirror.
    let spool = f.spool(&runner, &request, &bytes);
    runner.fetch_run(&request, &spool.directory, true).unwrap();
    assert_eq!(
        git(
            &mirror,
            &["rev-parse", "--verify", &format!("{head}^{{commit}}")]
        ),
        head
    );
}
