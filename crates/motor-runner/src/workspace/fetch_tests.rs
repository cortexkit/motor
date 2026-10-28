use super::*;
use cortexkit_test_support::ScratchDir;
use std::sync::{mpsc, Barrier, Condvar};
use std::time::Duration;

fn trees(root: &Path) -> Trees {
    Trees {
        mirrors: root.join("mirrors"),
        state: root.join("state"),
        compacting: Arc::default(),
        mirror_locks: Mutex::new(BTreeMap::new()),
    }
}

#[test]
fn workspace_changes_list_edits_and_report_a_failed_scan_as_an_error() {
    let scratch = ScratchDir::new("motor-workspace-changes");
    let trees = trees(&scratch);
    let repo = committed_repo(&scratch, "changes");
    assert_eq!(
        trees.changes(&repo, &[], None).unwrap(),
        Vec::<String>::new()
    );
    fs::write(repo.join("source"), "edited").unwrap();
    fs::write(repo.join("new"), "untracked").unwrap();
    fs::write(repo.join(".gitignore"), "ignored/\n").unwrap();
    fs::create_dir_all(repo.join("ignored")).unwrap();
    fs::write(repo.join("ignored/out"), "build output").unwrap();
    let mut changes = trees.changes(&repo, &[], None).unwrap();
    changes.sort();
    // Writes under ignored paths are not listed.
    assert_eq!(changes, [".gitignore", "new", "source"]);
    // A path git cannot scan is a failed scan, not "no changes".
    assert!(trees.changes(&scratch.join("missing"), &[], None).is_err());
}

/// After an isolated job the workspace belongs to the job account. Git's own
/// test switch makes it see a different owner without needing root.
#[test]
fn workspace_changes_scan_a_tree_owned_by_another_account() {
    let scratch = ScratchDir::new("motor-workspace-changes-owner");
    let trees = trees(&scratch);
    let repo = committed_repo(&scratch, "owned");
    fs::create_dir_all(&trees.state).unwrap();
    fs::write(repo.join("source"), "caller edit").unwrap();
    let commit = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    let tree = trees.tree_hash(&repo).unwrap();
    fs::write(repo.join("source"), "edited by the job").unwrap();
    std::env::set_var("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1");
    let changes = trees.changes(&repo, &[], Some((&commit, &tree)));
    std::env::remove_var("GIT_TEST_ASSUME_DIFFERENT_OWNER");
    assert_eq!(changes.unwrap(), ["source"]);
}

fn dirty_snapshot(root: &Path) -> (Trees, PathBuf, String, String) {
    let trees = trees(root);
    fs::create_dir_all(&trees.state).unwrap();
    let repo = committed_repo(root, "dirty-snapshot");
    fs::write(repo.join("deleted"), "caller will delete this").unwrap();
    fs::write(repo.join("clean"), "unchanged by caller").unwrap();
    fs::write(repo.join(".gitignore"), "ignored/\n").unwrap();
    git(&repo, &["add", "."]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    )
    .unwrap();
    let commit = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    fs::write(repo.join("source"), "caller edit").unwrap();
    fs::write(repo.join("untracked"), "caller new file").unwrap();
    fs::remove_file(repo.join("deleted")).unwrap();
    fs::create_dir_all(repo.join("ignored")).unwrap();
    fs::write(repo.join("ignored/input"), "force-added caller input").unwrap();
    git(&repo, &["add", "-f", "ignored/input"]).unwrap();
    let tree = trees.tree_hash(&repo).unwrap();
    git(&repo, &["read-tree", "HEAD"]).unwrap();
    stage_snapshot_ignored(&repo, &tree).unwrap();
    (trees, repo, commit, tree)
}

#[test]
fn workspace_changes_noop_dirty_snapshot_reports_nothing() {
    let scratch = ScratchDir::new("motor-workspace-changes-noop");
    let (trees, repo, commit, tree) = dirty_snapshot(&scratch);
    assert!(!git(&repo, &["status", "--porcelain"]).unwrap().is_empty());
    assert_eq!(
        trees.changes(&repo, &[], Some((&commit, &tree))).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn workspace_changes_dirty_snapshot_reports_only_job_edits() {
    let scratch = ScratchDir::new("motor-workspace-changes-dirty");
    let (trees, repo, commit, tree) = dirty_snapshot(&scratch);
    fs::write(repo.join("source"), "job edit after caller edit").unwrap();
    fs::remove_file(repo.join("untracked")).unwrap();
    fs::write(repo.join("clean"), "job edit to a clean file").unwrap();
    fs::write(repo.join("new"), "job new file").unwrap();
    let mut changes = trees.changes(&repo, &[], Some((&commit, &tree))).unwrap();
    changes.sort();
    assert_eq!(changes, ["clean", "new", "source", "untracked"]);
}

#[test]
fn workspace_changes_clean_snapshot_keeps_status_behavior() {
    let scratch = ScratchDir::new("motor-workspace-changes-clean");
    let trees = trees(&scratch);
    let repo = committed_repo(&scratch, "clean");
    let commit = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]).unwrap();
    fs::write(repo.join("source"), "job edit").unwrap();
    fs::write(repo.join("staged"), "job staged file").unwrap();
    git(&repo, &["add", "staged"]).unwrap();
    fs::write(repo.join("new"), "job new file").unwrap();
    fs::write(repo.join(".gitignore"), "ignored/\n").unwrap();
    fs::create_dir_all(repo.join("ignored")).unwrap();
    fs::write(repo.join("ignored/output"), "job ignored output").unwrap();
    let mut changes = trees.changes(&repo, &[], Some((&commit, &tree))).unwrap();
    changes.sort();
    assert_eq!(changes, [".gitignore", "new", "source", "staged"]);
    assert_eq!(
        trees.changes(&repo, &[], Some((&commit, &tree))).unwrap(),
        trees.changes(&repo, &[], None).unwrap()
    );
}

#[test]
fn workspace_changes_detect_reverts_and_force_added_ignored_edits() {
    let scratch = ScratchDir::new("motor-workspace-changes-reverted");
    let (trees, repo, commit, tree) = dirty_snapshot(&scratch);
    git(&repo, &["checkout", "HEAD", "--", "source", "deleted"]).unwrap();
    fs::write(repo.join("ignored/input"), "job changed staged input").unwrap();
    let mut changes = trees.changes(&repo, &[], Some((&commit, &tree))).unwrap();
    changes.sort();
    assert_eq!(changes, ["deleted", "ignored/input", "source"]);
}

#[cfg(unix)]
#[test]
fn workspace_changes_compare_snapshot_modes_symlinks_and_clean_filters() {
    use std::os::unix::{fs::symlink, fs::PermissionsExt};

    let scratch = ScratchDir::new("motor-workspace-changes-modes");
    let trees = trees(&scratch);
    fs::create_dir_all(&trees.state).unwrap();
    let repo = committed_repo(&scratch, "modes");
    let commit = git(&repo, &["rev-parse", "HEAD"]).unwrap();
    fs::write(repo.join(".gitattributes"), "filtered text eol=crlf\n").unwrap();
    fs::write(repo.join("filtered"), "caller text\r\n").unwrap();
    fs::write(repo.join("executable"), "caller executable").unwrap();
    fs::set_permissions(repo.join("executable"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("missing-target", repo.join("link")).unwrap();
    let tree = trees.tree_hash(&repo).unwrap();
    assert!(trees
        .changes(&repo, &[], Some((&commit, &tree)))
        .unwrap()
        .is_empty());
    fs::set_permissions(repo.join("executable"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(repo.join("link")).unwrap();
    fs::write(repo.join("link"), "missing-target").unwrap();
    let mut changes = trees.changes(&repo, &[], Some((&commit, &tree))).unwrap();
    changes.sort();
    assert_eq!(changes, ["executable", "link"]);
}

/// A repository with one committed file, `source`.
fn committed_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet"]).unwrap();
    fs::write(repo.join("source"), name).unwrap();
    git(&repo, &["add", "source"]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ],
    )
    .unwrap();
    repo
}

fn bundle(root: &Path, name: &str) -> (PathBuf, BundleManifest, String) {
    let repo = root.join(name);
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet"]).unwrap();
    fs::write(repo.join("source"), name).unwrap();
    git(&repo, &["add", "source"]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ],
    )
    .unwrap();
    let commit = git(&repo, &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    git(
        &repo,
        &["update-ref", "refs/ck-motor/test-snapshot", &commit],
    )
    .unwrap();
    let directory = root.join(format!("{name}-upload"));
    fs::create_dir_all(&directory).unwrap();
    let spool = directory.join("0");
    git(
        &repo,
        &[
            "bundle",
            "create",
            spool.to_str().unwrap(),
            "refs/ck-motor/test-snapshot",
        ],
    )
    .unwrap();
    let bytes = fs::read(spool).unwrap();
    let manifest = BundleManifest {
        index: 0,
        repository: repo.to_str().unwrap().to_owned(),
        length: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    (directory, manifest, commit)
}

#[test]
fn concurrent_fetches_into_one_mirror_all_succeed_under_its_lock() {
    const N: usize = 8;
    let scratch = ScratchDir::new("mirror-fetches");
    let trees = Arc::new(trees(&scratch));
    let (directory, manifest, commit) = bundle(&scratch, "repository");
    let mirror = trees.mirror(&manifest.repository);
    let mirror_lock = trees.mirror_lock(&mirror);
    let start = Arc::new(Barrier::new(N));
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..N)
            .map(|_| {
                let trees = trees.clone();
                let start = start.clone();
                let lock = mirror_lock.clone();
                let directory = directory.clone();
                let manifest = manifest.clone();
                let commit = commit.clone();
                scope.spawn(move || {
                    start.wait();
                    trees.fetch_with(
                        &directory,
                        std::slice::from_ref(&manifest),
                        (&manifest.repository, &commit, "", &[]),
                        |_| {
                            // Check exclusion at the real Git-fetch boundary. This
                            // makes dropping the guard fail without depending on
                            // Git losing a scheduling race during the test.
                            if lock.try_lock().is_ok() {
                                Err(io::Error::other("fetch entered without its mirror lock"))
                            } else {
                                Ok(())
                            }
                        },
                    )
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.len(), N);
    for result in results {
        result.unwrap();
    }
    git(
        &mirror,
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
    )
    .unwrap();
}

#[test]
fn different_repository_fetches_make_progress_in_parallel() {
    let scratch = ScratchDir::new("parallel-mirrors");
    let trees = Arc::new(trees(&scratch));
    let first = bundle(&scratch, "first");
    let second = bundle(&scratch, "second");
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, ready) = mpsc::channel();
    let (both_entered, results) = std::thread::scope(|scope| {
        let workers: Vec<_> = [first, second]
            .into_iter()
            .map(|(directory, manifest, commit)| {
                let trees = trees.clone();
                let release = release.clone();
                let entered = entered.clone();
                scope.spawn(move || {
                    trees.fetch_with(
                        &directory,
                        std::slice::from_ref(&manifest),
                        (&manifest.repository, &commit, "", &[]),
                        |mirror| {
                            entered.send(mirror.to_path_buf()).unwrap();
                            let (lock, wake) = &*release;
                            let (released, _) = wake
                                .wait_timeout_while(
                                    lock.lock().unwrap(),
                                    Duration::from_secs(10),
                                    |released| !*released,
                                )
                                .unwrap();
                            if *released {
                                Ok(())
                            } else {
                                Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "parallel mirror fetch gate was not released",
                                ))
                            }
                        },
                    )
                })
            })
            .collect();
        // Both critical sections must be entered before either is released.
        // A global fetch lock would leave the second repository waiting here.
        let first_ready = ready.recv_timeout(Duration::from_secs(5));
        let second_ready = ready.recv_timeout(Duration::from_secs(5));
        let both = match (first_ready, second_ready) {
            (Ok(first), Ok(second)) => first != second,
            _ => false,
        };
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        (both, results)
    });
    assert!(both_entered, "unrelated repositories blocked one another");
    for result in results {
        result.unwrap();
    }
}
