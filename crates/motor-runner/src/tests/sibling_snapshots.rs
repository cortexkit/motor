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
