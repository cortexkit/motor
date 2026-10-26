use super::*;
use crate::{sibling_snapshots::JavascriptEnvironment, test_cleanup::CheckoutWorkCapture};
use cortexkit_test_support::ScratchDir;
use std::{
    os::unix::fs::{symlink, PermissionsExt},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Barrier,
    },
    time::Duration,
};

/// A sibling repository with a runner mirror, and a runner config whose
/// snapshot mode is on and whose job-account ACL step does nothing.
struct Fixture {
    scratch: ScratchDir,
    source: PathBuf,
    repository: String,
    config: Config,
    trees: Trees,
}

fn git(path: &Path, args: &[&str]) -> String {
    crate::workspace::git(path, args).unwrap()
}

impl Fixture {
    fn new(name: &str) -> Self {
        let scratch = ScratchDir::new(name);
        let source = scratch.join("commons");
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q"]);
        let mut config = Config::new(scratch.join("runner"), "main");
        config.sibling_snapshot_mode_override = Some(true);
        config.sibling_snapshot_access = Some(Arc::new(|_: &Path| Ok(())));
        let state = config.state_dir();
        fs::create_dir_all(&state).unwrap();
        let trees = Trees {
            mirror_locks: Mutex::new(BTreeMap::new()),
            mirrors: state.join("mirrors"),
            state,
            compacting: Arc::default(),
        };
        let repository = source.to_string_lossy().into_owned();
        trees.ensure_mirror(&repository).unwrap();
        Self {
            scratch,
            source,
            repository,
            config,
            trees,
        }
    }

    /// Writes `files` into the source, commits them on top of its HEAD and
    /// fetches the commit into the mirror. Returns the commit and its tree.
    fn commit(&self, files: &[(&str, &str)]) -> (String, String) {
        for (path, content) in files {
            let path = self.source.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        self.commit_staged()
    }

    fn commit_staged(&self) -> (String, String) {
        git(&self.source, &["add", "-A"]);
        git(
            &self.source,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "-m",
                "snapshot",
            ],
        );
        let commit = git(&self.source, &["rev-parse", "HEAD"]);
        let tree = git(&self.source, &["rev-parse", "HEAD^{tree}"]);
        git(
            &self.trees.mirror(&self.repository),
            &[
                "fetch",
                "-q",
                "--no-tags",
                self.source.to_str().unwrap(),
                &format!("{commit}:refs/snapshots/{commit}"),
            ],
        );
        (commit, tree)
    }

    fn key(&self, commit: &str, caller: &str) -> SnapshotKey {
        let environment = JavascriptEnvironment::digest(
            &BTreeMap::from([("bun".to_string(), b"1.2.0\n".to_vec())]),
            b"v22.0.0\n",
            &[],
        );
        SnapshotKey::new(&self.repository, commit, caller, &environment).unwrap()
    }

    fn resolve(&self, store: &SnapshotStore, key: &SnapshotKey, tree: &str) -> io::Result<PathBuf> {
        store.resolve(&self.config, &self.trees, key, tree)
    }

    /// The repository's directory under the snapshot root.
    fn snapshots(&self) -> PathBuf {
        self.config
            .sibling_snapshots_dir()
            .join(crate::workspace::name(&self.repository))
    }
}

/// Sorted entry names of `directory`.
fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A published snapshot's directory and metadata file, and nothing else.
fn published(directory: &Path) -> Vec<String> {
    let name = directory
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    vec![name.clone(), format!("{name}.json")]
}

fn set_mtime(path: &Path, time: SystemTime) {
    fs::File::open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
}

fn mtime(path: &Path) -> SystemTime {
    fs::symlink_metadata(path).unwrap().modified().unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn old_time(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// Copies a tree like the reflink clone does: modes and mtimes are kept and
/// symbolic links are copied as links.
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let found = fs::symlink_metadata(source)?;
    if found.file_type().is_symlink() {
        return symlink(fs::read_link(source)?, target);
    }
    if found.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        fs::copy(source, target)?;
    }
    fs::set_permissions(target, found.permissions())?;
    fs::File::open(target)?.set_times(fs::FileTimes::new().set_modified(found.modified()?))
}

type CloneCalls = Arc<Mutex<Vec<(PathBuf, PathBuf)>>>;

/// A store whose predecessor copies use `copy_tree` and are recorded.
fn copying_store() -> (SnapshotStore, CloneCalls) {
    let calls = CloneCalls::default();
    let recorded = calls.clone();
    let store = SnapshotStore {
        clone_override: Some(Arc::new(move |source: &Path, target: &Path| {
            recorded
                .lock()
                .unwrap()
                .push((source.to_path_buf(), target.to_path_buf()));
            copy_tree(source, target)
        })),
        ..SnapshotStore::default()
    };
    (store, calls)
}

fn is_temporary_of(path: &Path, directory: &Path) -> bool {
    path.parent() == directory.parent()
        && path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&format!(
                "{}{TEMPORARY_MARKER}",
                directory.file_name().unwrap().to_string_lossy()
            ))
}

#[test]
fn store_refuses_and_creates_nothing_while_snapshot_mode_is_off() {
    let mut f = Fixture::new("motor-snapshot-store-off");
    f.config.sibling_snapshot_mode_override = Some(false);
    let (commit, tree) = f.commit(&[("source", "one\n")]);
    let error = f
        .resolve(&SnapshotStore::default(), &f.key(&commit, &commit), &tree)
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported, "{error}");
    assert!(!f.config.sibling_snapshots_dir().exists());
}

#[test]
fn concurrent_setups_of_one_key_create_the_snapshot_once() {
    let f = Fixture::new("motor-snapshot-store-once");
    let (commit, tree) = f.commit(&[("source", "one\n")]);
    let key = f.key(&commit, &commit);
    let store = SnapshotStore::default();
    let work = CheckoutWorkCapture::begin(&f.config.sibling_snapshots_dir());
    let barrier = Barrier::new(6);
    let results: Vec<_> = std::thread::scope(|scope| {
        let setups: Vec<_> = (0..6)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    f.resolve(&store, &key, &tree)
                })
            })
            .collect();
        setups
            .into_iter()
            .map(|setup| setup.join().unwrap())
            .collect()
    });
    let directory = key.directory(&f.config);
    for result in results {
        assert_eq!(result.unwrap(), directory);
    }
    assert_eq!(work.events(), ["sync_workspace"]);
    assert_eq!(
        fs::read_to_string(directory.join("source")).unwrap(),
        "one\n"
    );
    assert_eq!(read_metadata(&directory).unwrap().tree_hash, tree);
    assert_eq!(entries(&f.snapshots()), published(&directory));
}

#[test]
fn snapshots_of_two_keys_of_one_repository_are_created_in_parallel() {
    let mut f = Fixture::new("motor-snapshot-store-parallel");
    let (first, first_tree) = f.commit(&[("source", "one\n")]);
    let (second, second_tree) = f.commit(&[("source", "two\n")]);
    let held = f.key(&first, &first);
    let held_name = held.directory_name();
    let (reached, reached_wait) = mpsc::channel();
    let (release, release_wait) = mpsc::channel::<()>();
    let reached = Mutex::new(reached);
    let release_wait = Mutex::new(release_wait);
    // Pause the first creation inside its key mutex.
    f.config.sibling_snapshot_access = Some(Arc::new(move |path: &Path| {
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&held_name)
        {
            reached.lock().unwrap().send(()).unwrap();
            release_wait
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(60))
                .map_err(io::Error::other)?;
        }
        Ok(())
    }));
    let store = SnapshotStore::default();
    std::thread::scope(|scope| {
        let holder = scope.spawn(|| f.resolve(&store, &held, &first_tree));
        reached_wait.recv_timeout(Duration::from_secs(60)).unwrap();
        let other = f
            .resolve(&store, &f.key(&second, &second), &second_tree)
            .unwrap();
        assert!(
            !holder.is_finished(),
            "the second key waited for the first key's creation"
        );
        assert_eq!(fs::read_to_string(other.join("source")).unwrap(), "two\n");
        assert!(!held.directory(&f.config).exists());
        release.send(()).unwrap();
        let first_directory = holder.join().unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(first_directory.join("source")).unwrap(),
            "one\n"
        );
    });
}

#[test]
fn creation_copies_the_most_recent_snapshot_and_registers_it_until_publication() {
    let mut f = Fixture::new("motor-snapshot-store-predecessor");
    let (first, first_tree) = f.commit(&[("kept", "same\n"), ("changed", "one\n")]);
    let (second, second_tree) = f.commit(&[("changed", "two\n")]);
    let (store, calls) = copying_store();
    let store = Arc::new(store);
    let first_key = f.key(&first, &first);
    let second_key = f.key(&second, &second);
    let first_directory = first_key.directory(&f.config);
    // What the second creation sees of its predecessor while it builds:
    // whether it is registered, and whether a deletion could claim it.
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (seen, watched, second_name) = (
        observed.clone(),
        first_directory.clone(),
        second_key.directory_name(),
    );
    let watcher = store.clone();
    f.config.sibling_snapshot_access = Some(Arc::new(move |path: &Path| {
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&second_name)
        {
            seen.lock().unwrap().push((
                watcher.is_creation_source(&watched),
                watcher.claim_for_deletion(&watched),
            ));
        }
        Ok(())
    }));

    assert_eq!(
        f.resolve(&store, &first_key, &first_tree).unwrap(),
        first_directory
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "the first snapshot has no predecessor"
    );
    // An old mtime on an unchanged file shows whether the next snapshot kept
    // the predecessor's copy of it or checked it out again.
    set_mtime(&first_directory.join("kept"), old_time(1_000_000_000));

    let second_directory = f.resolve(&store, &second_key, &second_tree).unwrap();
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].0, first_directory);
    assert!(is_temporary_of(&calls[0].1, &second_directory), "{calls:?}");
    assert_eq!(*observed.lock().unwrap(), [(true, false)]);
    assert!(!store.is_creation_source(&first_directory));
    assert!(store.claim_for_deletion(&first_directory));
    store.release_deletion(&first_directory);

    assert_eq!(
        fs::read_to_string(second_directory.join("changed")).unwrap(),
        "two\n"
    );
    assert_eq!(
        fs::read_to_string(second_directory.join("kept")).unwrap(),
        "same\n"
    );
    assert_eq!(
        mtime(&second_directory.join("kept")),
        old_time(1_000_000_000)
    );
    assert_eq!(
        read_metadata(&second_directory).unwrap().tree_hash,
        second_tree
    );
    assert_eq!(
        fs::read_to_string(first_directory.join("changed")).unwrap(),
        "one\n"
    );
    assert_eq!(
        f.trees.tree_hash(&first_directory).unwrap(),
        first_tree,
        "building from a predecessor must not change it"
    );
}

#[test]
fn creation_never_copies_a_snapshot_claimed_for_deletion() {
    let f = Fixture::new("motor-snapshot-store-deleting");
    let (first, first_tree) = f.commit(&[("source", "one\n")]);
    let (second, second_tree) = f.commit(&[("source", "two\n")]);
    let (third, third_tree) = f.commit(&[("source", "three\n")]);
    let (fourth, fourth_tree) = f.commit(&[("source", "four\n")]);
    let (store, calls) = copying_store();
    let older = f
        .resolve(&store, &f.key(&first, &first), &first_tree)
        .unwrap();
    let newer = f
        .resolve(&store, &f.key(&second, &second), &second_tree)
        .unwrap();
    set_mtime(&older, old_time(1_000_000_000));
    set_mtime(&newer, old_time(2_000_000_000));
    calls.lock().unwrap().clear();

    // The most recently used snapshot is being deleted: take the next one.
    assert!(store.claim_for_deletion(&newer));
    let third_directory = f
        .resolve(&store, &f.key(&third, &third), &third_tree)
        .unwrap();
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|(source, _)| source.clone())
            .collect::<Vec<_>>(),
        std::slice::from_ref(&older)
    );
    assert_eq!(
        fs::read_to_string(third_directory.join("source")).unwrap(),
        "three\n"
    );
    store.release_deletion(&newer);

    // Every ready snapshot is being deleted: clone the mirror instead.
    calls.lock().unwrap().clear();
    for directory in [&older, &newer, &third_directory] {
        assert!(store.claim_for_deletion(directory));
    }
    let fourth_directory = f
        .resolve(&store, &f.key(&fourth, &fourth), &fourth_tree)
        .unwrap();
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(fourth_directory.join("source")).unwrap(),
        "four\n"
    );
}

#[test]
fn failed_predecessor_copy_falls_back_to_a_fresh_clone() {
    let f = Fixture::new("motor-snapshot-store-copy-fails");
    let (first, first_tree) = f.commit(&[("source", "one\n")]);
    let (second, second_tree) = f.commit(&[("source", "two\n")]);
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let store = SnapshotStore {
        clone_override: Some(Arc::new(move |_: &Path, target: &Path| {
            counted.fetch_add(1, Ordering::SeqCst);
            fs::create_dir_all(target)?;
            fs::write(target.join("partial"), "left by a failed copy\n")?;
            Err(io::Error::other("this filesystem has no reflinks"))
        })),
        ..SnapshotStore::default()
    };
    f.resolve(&store, &f.key(&first, &first), &first_tree)
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    let directory = f
        .resolve(&store, &f.key(&second, &second), &second_tree)
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(!directory.join("partial").exists());
    assert_eq!(
        fs::read_to_string(directory.join("source")).unwrap(),
        "two\n"
    );
    assert_eq!(f.trees.tree_hash(&directory).unwrap(), second_tree);
}

#[test]
fn snapshot_of_a_dirty_commit_shows_its_caller_commit_as_head() {
    let f = Fixture::new("motor-snapshot-store-caller");
    let (base, _) = f.commit(&[("source", "one\n")]);
    let (dirty, dirty_tree) = f.commit(&[("source", "two\n")]);
    let key = f.key(&dirty, &base);
    let directory = f
        .resolve(&SnapshotStore::default(), &key, &dirty_tree)
        .unwrap();
    assert!(directory
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with(&format!("{dirty}-{base}-")));
    assert_eq!(git(&directory, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        fs::read_to_string(directory.join("source")).unwrap(),
        "two\n"
    );
    let metadata = read_metadata(&directory).unwrap();
    assert_eq!(
        (metadata.caller.as_str(), metadata.tree_hash.as_str()),
        (base.as_str(), dirty_tree.as_str())
    );
}

#[test]
fn tree_mismatch_at_creation_refuses_and_publishes_nothing() {
    let f = Fixture::new("motor-snapshot-store-mismatch");
    let (first, _) = f.commit(&[("source", "one\n")]);
    let (_, second_tree) = f.commit(&[("source", "two\n")]);
    let key = f.key(&first, &first);
    let error = f
        .resolve(&SnapshotStore::default(), &key, &second_tree)
        .unwrap_err();
    assert!(error.to_string().contains("tree_hash_mismatch"), "{error}");
    let directory = key.directory(&f.config);
    assert!(!directory.exists());
    assert!(!metadata_path(&directory).exists());
    assert!(
        entries(&f.snapshots()).is_empty(),
        "{:?}",
        entries(&f.snapshots())
    );
}

#[test]
fn cache_hit_for_another_tree_is_refused_and_leaves_the_snapshot_untouched() {
    let f = Fixture::new("motor-snapshot-store-hit-mismatch");
    let (first, first_tree) = f.commit(&[("source", "one\n")]);
    let (_, second_tree) = f.commit(&[("source", "two\n")]);
    let key = f.key(&first, &first);
    let store = SnapshotStore::default();
    let directory = f.resolve(&store, &key, &first_tree).unwrap();
    // A fixed old use time shows whether the refused request stamped it.
    set_mtime(&directory, old_time(1_000_000_000));
    let metadata = fs::read(metadata_path(&directory)).unwrap();
    let work = CheckoutWorkCapture::begin(&f.config.sibling_snapshots_dir());

    let error = f.resolve(&store, &key, &second_tree).unwrap_err();
    assert!(
        error.to_string().contains("was verified to hold tree"),
        "{error}"
    );
    assert_eq!(mtime(&directory), old_time(1_000_000_000));
    assert_eq!(fs::read(metadata_path(&directory)).unwrap(), metadata);
    assert_eq!(
        fs::read_to_string(directory.join("source")).unwrap(),
        "one\n"
    );
    assert_eq!(f.trees.tree_hash(&directory).unwrap(), first_tree);
    assert!(work.events().is_empty(), "{:?}", work.events());
    assert_eq!(entries(&f.snapshots()), published(&directory));

    // The matching tree is a hit: nothing is rebuilt and the use is stamped.
    assert_eq!(f.resolve(&store, &key, &first_tree).unwrap(), directory);
    assert!(work.events().is_empty(), "{:?}", work.events());
    assert!(mtime(&directory) > old_time(1_000_000_000));
}

#[test]
fn snapshot_without_readable_metadata_is_recreated() {
    let f = Fixture::new("motor-snapshot-store-no-metadata");
    let (commit, tree) = f.commit(&[("source", "one\n")]);
    let key = f.key(&commit, &commit);
    let store = SnapshotStore::default();
    let directory = f.resolve(&store, &key, &tree).unwrap();
    let metadata = metadata_path(&directory);
    assert_eq!(metadata.parent(), directory.parent());
    let work = CheckoutWorkCapture::begin(&f.config.sibling_snapshots_dir());

    let mut other_version = read_metadata(&directory).unwrap();
    other_version.version = METADATA_VERSION + 1;
    let mut other_key = read_metadata(&directory).unwrap();
    other_key.caller = "f".repeat(40);
    // Each entry names a way the metadata can be damaged and applies it.
    type Damage<'a> = (&'a str, Box<dyn Fn() + 'a>);
    let damages: [Damage; 4] = [
        ("missing", Box::new(|| fs::remove_file(&metadata).unwrap())),
        (
            "unparsable",
            Box::new(|| fs::write(&metadata, "{\"version\":").unwrap()),
        ),
        (
            "another version",
            Box::new(|| atomic_json(&metadata, &other_version).unwrap()),
        ),
        (
            "another key",
            Box::new(|| atomic_json(&metadata, &other_key).unwrap()),
        ),
    ];
    for (rebuilt, (damage, apply)) in damages.iter().enumerate() {
        apply();
        fs::write(directory.join("stale"), "from the old directory\n").unwrap();
        assert_eq!(
            f.resolve(&store, &key, &tree).unwrap(),
            directory,
            "{damage}"
        );
        assert_eq!(work.events().len(), rebuilt + 1, "{damage}");
        assert!(!directory.join("stale").exists(), "{damage}");
        assert_eq!(
            read_metadata(&directory).unwrap(),
            Metadata {
                boot_id: current_boot(&f.config),
                ..Metadata::new(&key, &tree)
            }
        );
        assert_eq!(entries(&f.snapshots()), published(&directory), "{damage}");
    }
}

#[test]
fn creation_removes_group_and_other_write_before_the_access_step() {
    let mut f = Fixture::new("motor-snapshot-store-write-bits");
    let outside = f.scratch.join("outside");
    fs::write(&outside, "not part of the snapshot\n").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o666)).unwrap();
    symlink(&outside, f.source.join("link")).unwrap();
    let (first, first_tree) = f.commit(&[("kept", "same\n"), ("dir/file", "one\n")]);
    let (second, second_tree) = f.commit(&[("dir/file", "two\n")]);
    // The copy makes an unchanged file and a directory writable by everyone,
    // as a copy of a loose tree could.
    let store = SnapshotStore {
        clone_override: Some(Arc::new(|source: &Path, target: &Path| {
            copy_tree(source, target)?;
            fs::set_permissions(target.join("kept"), fs::Permissions::from_mode(0o666))?;
            fs::set_permissions(target.join("dir"), fs::Permissions::from_mode(0o777))
        })),
        ..SnapshotStore::default()
    };
    let observed = Arc::new(Mutex::new(Vec::new()));
    let seen = observed.clone();
    f.config.sibling_snapshot_access = Some(Arc::new(move |path: &Path| {
        seen.lock().unwrap().push((
            path.to_path_buf(),
            mode(&path.join("kept")),
            mode(&path.join("dir")),
        ));
        Ok(())
    }));
    f.resolve(&store, &f.key(&first, &first), &first_tree)
        .unwrap();
    let directory = f
        .resolve(&store, &f.key(&second, &second), &second_tree)
        .unwrap();

    assert_eq!(mode(&directory.join("kept")), 0o644);
    assert_eq!(mode(&directory.join("dir")), 0o755);
    let mut pending = vec![directory.clone()];
    while let Some(path) = pending.pop() {
        let found = fs::symlink_metadata(&path).unwrap();
        if found.file_type().is_symlink() {
            continue;
        }
        assert_eq!(mode(&path) & 0o022, 0, "{}", path.display());
        if found.is_dir() {
            pending.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        }
    }
    assert!(directory.join("link").is_symlink());
    assert_eq!(mode(&outside), 0o666, "a link's target was changed");
    // The access step runs on the unpublished tree, after the bits are gone.
    let observed = observed.lock().unwrap();
    let (path, kept, dir) = observed.last().unwrap();
    assert!(is_temporary_of(path, &directory), "{}", path.display());
    assert_eq!((*kept, *dir), (0o644, 0o755));
}

#[test]
fn failed_access_step_publishes_nothing() {
    let mut f = Fixture::new("motor-snapshot-store-access-fails");
    f.config.sibling_snapshot_access =
        Some(Arc::new(|_: &Path| Err(io::Error::other("setfacl failed"))));
    let (commit, tree) = f.commit(&[("source", "one\n")]);
    let key = f.key(&commit, &commit);
    let error = f
        .resolve(&SnapshotStore::default(), &key, &tree)
        .unwrap_err();
    assert!(error.to_string().contains("setfacl failed"), "{error}");
    assert!(
        entries(&f.snapshots()).is_empty(),
        "{:?}",
        entries(&f.snapshots())
    );
}

#[test]
fn failed_metadata_write_publishes_no_directory() {
    let f = Fixture::new("motor-snapshot-store-metadata-fails");
    let (commit, tree) = f.commit(&[("source", "one\n")]);
    let key = f.key(&commit, &commit);
    let directory = key.directory(&f.config);
    // A non-empty directory where the metadata file goes makes its atomic
    // rename fail. The snapshot directory is renamed into place only after
    // its metadata is written, so nothing may be published.
    let blocker = metadata_path(&directory);
    fs::create_dir_all(blocker.join("occupied")).unwrap();
    f.resolve(&SnapshotStore::default(), &key, &tree)
        .unwrap_err();
    assert!(!directory.exists());
    assert!(!entries(&f.snapshots())
        .iter()
        .any(|name| name.contains(TEMPORARY_MARKER)));
}

/// The real ACL step: an entry that a copy left writable by everyone, with a
/// named write grant for the job account, ends up readable but not writable
/// by the job account, and no default ACL remains.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_sibling_snapshot_creation_drops_job_write_grants() {
    fn setfacl(path: &Path, entries: &str) -> io::Result<()> {
        let output = std::process::Command::new("/usr/bin/setfacl")
            .args(["-m", entries, "--"])
            .arg(path)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(())
    }
    fn getfacl(path: &Path) -> String {
        let output = std::process::Command::new("/usr/bin/getfacl")
            .args(["-cp", "--"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    let mut f = Fixture::new("motor-snapshot-store-acl");
    f.config.sibling_snapshot_access = None;
    let (first, first_tree) = f.commit(&[("kept", "same\n"), ("changed", "one\n")]);
    let (second, second_tree) = f.commit(&[("changed", "two\n")]);
    let store = SnapshotStore {
        clone_override: Some(Arc::new(|source: &Path, target: &Path| {
            copy_tree(source, target)?;
            let kept = target.join("kept");
            fs::set_permissions(&kept, fs::Permissions::from_mode(0o666))?;
            setfacl(&kept, "u:motor-job:rwx,g::rw,m::rwx")?;
            setfacl(target, "d:u:motor-job:rwx,u:motor-job:rwx")
        })),
        ..SnapshotStore::default()
    };
    f.resolve(&store, &f.key(&first, &first), &first_tree)
        .unwrap();
    let directory = f
        .resolve(&store, &f.key(&second, &second), &second_tree)
        .unwrap();
    let file = getfacl(&directory.join("kept"));
    for entry in [
        "user:motor-job:r--",
        "group::r--",
        "mask::r--",
        "other::r--",
    ] {
        assert!(
            file.lines().any(|line| line == entry),
            "missing {entry}:\n{file}"
        );
    }
    let root = getfacl(&directory);
    for entry in ["user:motor-job:r-x", "mask::r-x"] {
        assert!(
            root.lines().any(|line| line == entry),
            "missing {entry}:\n{root}"
        );
    }
    assert!(!root.contains("default:"), "{root}");
    assert_eq!(mode(&directory.join("kept")) & 0o022, 0);
}

/// What a deletion's reference check answers in these tests: it claims the
/// snapshot, as `Runner::delete_sibling_snapshot` does when nothing
/// references it.
fn unreferenced(claim: &dyn Fn() -> bool) -> Option<bool> {
    Some(claim())
}

#[test]
fn deletion_moves_the_directory_away_then_removes_its_metadata_and_releases_its_claim() {
    let f = Fixture::new("motor-snapshot-store-delete");
    let (commit, tree) = f.commit(&[("source", "one\n")]);
    let key = f.key(&commit, &commit);
    let store = SnapshotStore::default();
    let directory = f.resolve(&store, &key, &tree).unwrap();
    let aside = f.scratch.join("aside");

    let referenced = store
        .delete(&directory, &aside, |_: &dyn Fn() -> bool| None)
        .unwrap();
    assert_eq!(referenced, Deletion::Referenced);
    assert_eq!(entries(&f.snapshots()), published(&directory));
    assert!(!aside.exists());
    assert!(
        store.claim_for_deletion(&directory),
        "a referenced snapshot is not left claimed"
    );
    store.release_deletion(&directory);

    assert_eq!(
        store.delete(&directory, &aside, unreferenced).unwrap(),
        Deletion::Removed
    );
    assert_eq!(entries(&f.snapshots()), Vec::<String>::new());
    assert_eq!(fs::read_to_string(aside.join("source")).unwrap(), "one\n");
    assert!(
        store.claim_for_deletion(&directory),
        "the deletion's claim is released"
    );
    store.release_deletion(&directory);
    assert_eq!(
        store.delete(&directory, &aside, unreferenced).unwrap(),
        Deletion::Absent
    );

    // The next setup that needs the snapshot builds it again.
    assert_eq!(f.resolve(&store, &key, &tree).unwrap(), directory);
    assert_eq!(entries(&f.snapshots()), published(&directory));
}

/// A deletion that finds the directory already gone still removes the
/// metadata an interrupted deletion left, and maintenance removes such
/// metadata on its own; metadata beside a directory stays.
#[test]
fn metadata_left_without_its_directory_is_removed() {
    let f = Fixture::new("motor-snapshot-store-orphan");
    let (commit, tree) = f.commit(&[("source", "one\n")]);
    let (other, other_tree) = f.commit(&[("source", "two\n")]);
    let store = SnapshotStore::default();
    let kept = f.resolve(&store, &f.key(&commit, &commit), &tree).unwrap();
    let orphan = f
        .resolve(&store, &f.key(&other, &other), &other_tree)
        .unwrap();
    // As a crash between the two steps of a deletion leaves it.
    fs::rename(&orphan, f.scratch.join("moved")).unwrap();
    fs::create_dir(f.snapshots().join(format!(
        "{}{TEMPORARY_MARKER}{}",
        kept.file_name().unwrap().to_string_lossy(),
        Uuid::now_v7()
    )))
    .unwrap();

    assert_eq!(
        orphaned_metadata(&f.snapshots()).unwrap(),
        std::slice::from_ref(&orphan)
    );
    assert!(store.remove_orphaned_metadata(&orphan).unwrap());
    assert!(!store.remove_orphaned_metadata(&kept).unwrap());
    assert!(!metadata_path(&orphan).exists());
    assert!(metadata_path(&kept).exists());
    assert_eq!(
        orphaned_metadata(&f.snapshots()).unwrap(),
        Vec::<PathBuf>::new()
    );
    // Only the final name is listed, not the temporary entry beside it.
    let listed: Vec<_> = list(&f.snapshots())
        .unwrap()
        .into_iter()
        .map(|snapshot| snapshot.directory)
        .collect();
    assert_eq!(listed, std::slice::from_ref(&kept));
    assert_eq!(temporaries(&f.snapshots()).unwrap().len(), 1);
}

#[test]
fn listing_puts_the_most_recently_used_snapshot_first() {
    let f = Fixture::new("motor-snapshot-store-list");
    let store = SnapshotStore::default();
    let mut directories = Vec::new();
    for (index, content) in ["one\n", "two\n", "three\n"].into_iter().enumerate() {
        let (commit, tree) = f.commit(&[("source", content)]);
        let directory = f.resolve(&store, &f.key(&commit, &commit), &tree).unwrap();
        set_mtime(&directory, old_time(1_000_000_000 + [20, 30, 10][index]));
        directories.push(directory);
    }
    let listed: Vec<_> = list(&f.snapshots())
        .unwrap()
        .into_iter()
        .map(|snapshot| (snapshot.directory, snapshot.used))
        .collect();
    assert_eq!(
        listed,
        [
            (directories[1].clone(), old_time(1_000_000_030)),
            (directories[0].clone(), old_time(1_000_000_020)),
            (directories[2].clone(), old_time(1_000_000_010)),
        ]
    );
}

/// A deletion that reaches a snapshot while a creation copies it as its
/// predecessor is refused and changes nothing; the creation finishes from
/// the intact copy.
#[test]
fn deletion_during_predecessor_copy_is_refused() {
    let f = Fixture::new("motor-snapshot-store-delete-copying");
    let (first, first_tree) = f.commit(&[("source", "one\n")]);
    let (second, second_tree) = f.commit(&[("source", "two\n")]);
    let (copying, copying_wait) = mpsc::channel();
    let (resume, resume_wait) = mpsc::channel::<()>();
    let (copying, resume_wait) = (Mutex::new(copying), Mutex::new(resume_wait));
    let store = SnapshotStore {
        clone_override: Some(Arc::new(move |source: &Path, target: &Path| {
            copying.lock().unwrap().send(()).unwrap();
            resume_wait
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(60))
                .map_err(io::Error::other)?;
            copy_tree(source, target)
        })),
        ..SnapshotStore::default()
    };
    let predecessor = f
        .resolve(&store, &f.key(&first, &first), &first_tree)
        .unwrap();
    let aside = f.scratch.join("aside");
    std::thread::scope(|scope| {
        let creation = scope.spawn(|| f.resolve(&store, &f.key(&second, &second), &second_tree));
        copying_wait.recv_timeout(Duration::from_secs(60)).unwrap();
        // The creation holds its own key mutex, not the predecessor's.
        let deletion = store.delete(&predecessor, &aside, unreferenced).unwrap();
        resume.send(()).unwrap();
        let created = creation.join().unwrap().unwrap();
        assert_eq!(deletion, Deletion::Copying);
        assert_eq!(fs::read_to_string(created.join("source")).unwrap(), "two\n");
    });
    assert!(!aside.exists());
    assert_eq!(read_metadata(&predecessor).unwrap().tree_hash, first_tree);
    assert_eq!(f.trees.tree_hash(&predecessor).unwrap(), first_tree);
    // Once the creation has published, the same deletion goes through.
    assert_eq!(
        store.delete(&predecessor, &aside, unreferenced).unwrap(),
        Deletion::Removed
    );
}

/// A snapshot a deletion has claimed is never picked as a predecessor: a
/// creation that starts while the deletion holds the victim's key mutex
/// finishes from a fresh clone, without waiting for that mutex or reading
/// the victim.
#[test]
fn creation_during_a_deletion_neither_waits_for_nor_reads_the_victim() {
    let f = Fixture::new("motor-snapshot-store-delete-claimed");
    let (first, first_tree) = f.commit(&[("source", "one\n")]);
    let (second, second_tree) = f.commit(&[("source", "two\n")]);
    let (claimed, claimed_wait) = mpsc::channel();
    let (resume, resume_wait) = mpsc::channel::<()>();
    let (claimed, resume_wait) = (Mutex::new(claimed), Mutex::new(resume_wait));
    let (copying_store, calls) = copying_store();
    let store = SnapshotStore {
        deletion_claimed: Some(Arc::new(move |_: &Path| {
            claimed.lock().unwrap().send(()).unwrap();
            resume_wait
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(60))
                .unwrap();
        })),
        ..copying_store
    };
    let victim = f
        .resolve(&store, &f.key(&first, &first), &first_tree)
        .unwrap();
    let aside = f.scratch.join("aside");
    let (done, done_wait) = mpsc::channel();
    std::thread::scope(|scope| {
        let deletion = scope.spawn(|| store.delete(&victim, &aside, unreferenced));
        claimed_wait.recv_timeout(Duration::from_secs(60)).unwrap();
        let creation = scope.spawn(|| {
            let created = f.resolve(&store, &f.key(&second, &second), &second_tree);
            done.send(()).unwrap();
            created
        });
        let finished_while_claimed = done_wait.recv_timeout(Duration::from_secs(60)).is_ok();
        resume.send(()).unwrap();
        let created = creation.join().unwrap().unwrap();
        assert!(
            finished_while_claimed,
            "the creation must not wait for the victim's key mutex"
        );
        assert_eq!(deletion.join().unwrap().unwrap(), Deletion::Removed);
        assert_eq!(fs::read_to_string(created.join("source")).unwrap(), "two\n");
    });
    assert!(
        calls.lock().unwrap().is_empty(),
        "the claimed victim was the only ready snapshot, so nothing is copied"
    );
    assert_eq!(fs::read_to_string(aside.join("source")).unwrap(), "one\n");
}

/// Makes every file under the snapshot's `.git/objects` empty, as a host
/// crash can leave files whose data never reached the disk.
fn empty_git_objects(directory: &Path) {
    let mut pending = vec![directory.join(".git/objects")];
    while let Some(path) = pending.pop() {
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                pending.push(entry.unwrap().path());
            }
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            fs::write(&path, b"").unwrap();
        }
    }
}

fn git_is_whole(directory: &Path) -> bool {
    crate::workspace::git(directory, &["fsck", "--no-progress", "--no-dangling"]).is_ok()
}

/// A snapshot whose metadata is valid but whose files were damaged by a host
/// crash: a daemon restart in the same boot uses it as it is, without
/// checking it, while the first use after a host restart checks it, rebuilds
/// it when its Git directory or its checkout is damaged, and otherwise
/// records that it is whole in the new boot. Snapshots from the earlier boot
/// are not copied as predecessors before they are checked.
#[test]
fn snapshot_from_an_earlier_boot_is_checked_once_and_rebuilt_when_damaged() {
    let mut f = Fixture::new("motor-snapshot-store-reboot");
    f.config.boot_id_override = Some("boot-one".into());
    let (first, first_tree) = f.commit(&[("source", "one\n")]);
    let (second, second_tree) = f.commit(&[("source", "two\n")]);
    let (third, third_tree) = f.commit(&[("source", "three\n")]);
    let keys = [
        (f.key(&first, &first), first_tree, "one\n"),
        (f.key(&second, &second), second_tree, "two\n"),
        (f.key(&third, &third), third_tree, "three\n"),
    ];
    let (store, _) = copying_store();
    let directories: Vec<_> = keys
        .iter()
        .map(|(key, tree, _)| f.resolve(&store, key, tree).unwrap())
        .collect();
    for directory in &directories {
        assert_eq!(
            read_metadata(directory).unwrap().boot_id.as_deref(),
            Some("boot-one")
        );
    }
    let broken_git = &directories[0];
    let changed_file = &directories[1];
    empty_git_objects(broken_git);
    fs::write(changed_file.join("source"), "lost\n").unwrap();
    assert!(!git_is_whole(broken_git));
    let work = CheckoutWorkCapture::begin(&f.config.sibling_snapshots_dir());

    // A daemon restart: same boot, so nothing is checked or rebuilt.
    let (restarted, _) = copying_store();
    for (key, tree, _) in &keys {
        f.resolve(&restarted, key, tree).unwrap();
    }
    assert!(work.events().is_empty(), "{:?}", work.events());
    assert!(!git_is_whole(broken_git));

    // A host restart: the damaged snapshots are rebuilt, the whole one kept.
    f.config.boot_id_override = Some("boot-two".into());
    let (rebooted, rebooted_calls) = copying_store();
    for ((key, tree, content), directory) in keys.iter().zip(&directories) {
        assert_eq!(&f.resolve(&rebooted, key, tree).unwrap(), directory);
        assert_eq!(
            fs::read_to_string(directory.join("source")).unwrap(),
            *content
        );
        assert!(git_is_whole(directory));
        assert_eq!(
            read_metadata(directory).unwrap().boot_id.as_deref(),
            Some("boot-two")
        );
    }
    assert_eq!(
        work.events(),
        ["sync_workspace", "sync_workspace"],
        "only the two damaged snapshots are rebuilt"
    );
    // The first rebuild found no snapshot checked in this boot and cloned
    // the mirror; the second copied the first, rebuilt in this boot.
    let sources: Vec<_> = rebooted_calls
        .lock()
        .unwrap()
        .iter()
        .map(|(source, _)| source.clone())
        .collect();
    assert_eq!(sources, std::slice::from_ref(broken_git));
}
