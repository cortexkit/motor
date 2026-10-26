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
