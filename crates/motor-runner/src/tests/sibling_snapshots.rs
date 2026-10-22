//! Until sibling snapshots are switched on, a job that uses a sibling still
//! gets it checked out in place at the sibling's host path, and the runner
//! never creates the snapshot store.
use super::*;

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

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_sibling_user_leaves_no_sibling_snapshot_store() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
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
    let host = Path::new(&f.canonical);
    fs::create_dir_all(host.parent().unwrap()).unwrap();
    git(
        host.parent().unwrap(),
        &["clone", "-q", text(&f.sibling), text(host)],
    );
    let host_commit = git(host, &["rev-parse", "HEAD"]);
    let (sibling_commit, sibling_tree) = commit(&f.sibling, "second\n");
    assert_ne!(sibling_commit, host_commit);

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

    assert_eq!(git(host, &["rev-parse", "HEAD"]), host_commit);
    assert_eq!(fs::read_to_string(host.join("source")).unwrap(), "first\n");
    assert_eq!(
        git(host, &["status", "--porcelain", "--ignored"]),
        "",
        "the host checkout must be exactly as cloned"
    );
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
