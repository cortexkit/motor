use super::*;
use crate::{sibling_snapshots::JavascriptEnvironment, storage::Workspace};
use cortexkit_test_support::ScratchDir;
use std::{
    os::unix::fs::{symlink, PermissionsExt},
    process::Command,
    sync::{mpsc, Arc, Mutex},
};

/// A commit of the sibling and its tree.
#[derive(Clone, Debug)]
struct Tree {
    commit: String,
    tree: String,
}

/// A workspace `<scratch>/<name>/a` whose Cargo package depends on the
/// sibling crates `../b/x` and `../b/y`. `<scratch>/<name>/b` is a symbolic
/// link to the snapshot of the workspace's last setup: Cargo resolves and
/// stats the sibling's files through it exactly as through the bind mount a
/// job gets at the sibling's path.
struct Space {
    key: String,
    view: PathBuf,
}

/// What one build of a workspace printed, and whether Cargo reused each
/// sibling crate's artifact.
#[derive(Debug)]
struct Built {
    output: String,
    x_fresh: bool,
    y_fresh: bool,
}

/// A sibling repository with a runner mirror, a snapshot store whose
/// predecessor copies keep mtimes (as a reflink copy does), a runner config
/// in snapshot mode, and a catalog standing in for the runner's.
struct Fixture {
    scratch: ScratchDir,
    source: PathBuf,
    repository: String,
    config: Config,
    trees: Trees,
    store: SnapshotStore,
    catalog: Catalog,
    environment: JavascriptEnvironment,
    /// The predecessor of every snapshot created by copying one.
    copies: Arc<Mutex<Vec<PathBuf>>>,
}

fn git(path: &Path, args: &[&str]) -> String {
    workspace::git(path, args).unwrap()
}

fn mtime(path: &Path) -> SystemTime {
    fs::symlink_metadata(path).unwrap().modified().unwrap()
}

fn set_mtime_of(path: &Path, time: SystemTime) {
    super::set_mtime(path, time).unwrap();
}

/// Copies a tree keeping modes and mtimes, with links copied as links.
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let found = fs::symlink_metadata(source)?;
    if found.file_type().is_symlink() {
        symlink(fs::read_link(source)?, target)?;
        return super::set_mtime(target, found.modified()?);
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
    fs::set_permissions(
        target,
        fs::Permissions::from_mode(found.permissions().mode()),
    )?;
    super::set_mtime(target, found.modified()?)
}

impl Fixture {
    fn new(name: &str) -> Self {
        let scratch = ScratchDir::new(name);
        let source = scratch.join("source/b");
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q"]);
        let mut config = Config::new(scratch.join("runner"), "main");
        config.sibling_snapshot_mode_override = Some(true);
        config.sibling_snapshot_access = Some(Arc::new(|_: &Path| Ok(())));
        let state = config.state_dir();
        fs::create_dir_all(&state).unwrap();
        let trees = Trees {
            dependency_locks: Mutex::new(BTreeMap::new()),
            mirrors: state.join("mirrors"),
            state,
            compacting: Arc::default(),
        };
        // The sibling's host path; nothing here creates or reads it.
        let repository = scratch.join("host/b").to_string_lossy().into_owned();
        trees.ensure_mirror(&repository).unwrap();
        let copies = Arc::<Mutex<Vec<PathBuf>>>::default();
        let recorded = copies.clone();
        let mut store = SnapshotStore::default();
        store.clone_override = Some(Arc::new(move |source: &Path, target: &Path| {
            recorded.lock().unwrap().push(source.to_path_buf());
            copy_tree(source, target)
        }));
        let environment = JavascriptEnvironment::digest(&BTreeMap::new(), b"", &[]);
        Self {
            scratch,
            source,
            repository,
            config,
            trees,
            store,
            catalog: Catalog::default(),
            environment,
            copies,
        }
    }

    /// Starts an unrelated history, so the next commit has no parent and
    /// shares no tree with earlier ones.
    fn orphan(&self) {
        git(
            &self.source,
            &[
                "checkout",
                "-q",
                "--orphan",
                &uuid::Uuid::now_v7().to_string(),
            ],
        );
    }

    /// Makes `path` in the sibling a symbolic link to `target`.
    fn link(&self, path: &str, target: &str) {
        let path = self.source.join(path);
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path).unwrap();
        }
        symlink(target, path).unwrap();
    }

    /// Commits the sibling with crate `x` defining `x` and crate `y` defining
    /// `y`, plus `extra` files, on top of its HEAD, and fetches the commit
    /// into the mirror.
    fn commit_crates(&self, x: &str, y: &str, extra: &[(&str, &str)]) -> Tree {
        for name in ["x", "y"] {
            let directory = self.source.join(name);
            fs::create_dir_all(directory.join("src")).unwrap();
            fs::write(
                directory.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .unwrap();
        }
        fs::write(
            self.source.join("x/src/lib.rs"),
            format!("pub const VALUE: &str = \"{x}\";\n"),
        )
        .unwrap();
        fs::write(
            self.source.join("y/src/lib.rs"),
            format!("pub const VALUE: &str = \"{y}\";\n"),
        )
        .unwrap();
        self.commit(extra)
    }

    /// Writes `files` into the sibling, commits everything and fetches the
    /// commit into the mirror.
    fn commit(&self, files: &[(&str, &str)]) -> Tree {
        for (path, content) in files {
            let path = self.source.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
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
                "--allow-empty",
                "-m",
                "sibling",
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
        Tree { commit, tree }
    }

    fn sibling(&self, at: &Tree) -> SiblingSnapshot {
        SiblingSnapshot {
            canonical_path: self.repository.clone(),
            commit: at.commit.clone(),
            tree_hash: at.tree.clone(),
            base_commit: Some(at.commit.clone()),
        }
    }

    fn key(&self, at: &Tree) -> SnapshotKey {
        SnapshotKey::new(&self.repository, &at.commit, &at.commit, &self.environment).unwrap()
    }

    /// Creates the snapshot of `at`, as an earlier setup elsewhere would.
    fn create(&self, at: &Tree) -> PathBuf {
        self.store
            .resolve(&self.config, &self.trees, &self.key(at), &at.tree)
            .unwrap()
    }

    fn snapshot(&self, at: &Tree) -> PathBuf {
        self.key(at).directory(&self.config)
    }

    /// A new workspace `<scratch>/<name>/a` with no catalog entry.
    fn workspace(&self, name: &str) -> Space {
        let parent = self.scratch.join(name);
        let app = parent.join("a");
        fs::create_dir_all(app.join("src")).unwrap();
        fs::write(
            app.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nx = { path = \"../b/x\" }\ny = { path = \"../b/y\" }\n\n\
             [workspace]\n",
        )
        .unwrap();
        fs::write(
            app.join("src/main.rs"),
            "fn main() {\n    println!(\"{} {}\", x::VALUE, y::VALUE);\n}\n",
        )
        .unwrap();
        Space {
            key: app.to_string_lossy().into_owned(),
            view: parent.join("b"),
        }
    }

    /// Records the workspace in the catalog, as `note_workspace` does after a
    /// populated setup, keeping its freshness record.
    fn note(&mut self, space: &Space) {
        workspace::record_workspace_entry(&mut self.catalog, &space.key, "root", "commit", &[], 0);
    }

    /// Shows `snapshot` at the workspace's `../b`, as its job's mount would.
    fn point(&self, space: &Space, snapshot: &Path) {
        if fs::symlink_metadata(&space.view).is_ok() {
            fs::remove_file(&space.view).unwrap();
        }
        symlink(snapshot, &space.view).unwrap();
    }

    /// One setup or Prepare of `space` at `at` that starts now and completes:
    /// the snapshot is resolved and touched, the workspace is recorded and
    /// its freshness record updated, and its `../b` shows the snapshot.
    /// Returns how many paths the touch changed.
    fn setup(&mut self, space: &Space, at: &Tree) -> usize {
        self.setup_started(space, at, SystemTime::now()).unwrap()
    }

    fn setup_started(
        &mut self,
        space: &Space,
        at: &Tree,
        started: SystemTime,
    ) -> io::Result<usize> {
        let pending = self.touch(space, at, started)?;
        let touched = pending.touched();
        self.note(space);
        assert!(pending.apply(&mut self.catalog, &space.key));
        self.point(space, &self.snapshot(at));
        Ok(touched)
    }

    /// The resolve and touch steps of a setup, without completing it.
    fn touch(&self, space: &Space, at: &Tree, started: SystemTime) -> io::Result<Pending> {
        let prior = Prior::of(&self.catalog, &space.key);
        let mut pending = prepare_siblings(
            &self.config,
            &self.store,
            &self.trees,
            &prior,
            started,
            &[self.sibling(at)],
            &self.environment,
        )?;
        assert_eq!(pending.len(), 1);
        Ok(pending.remove(0))
    }

    fn record(&self, space: &Space) -> Option<Record> {
        self.catalog
            .workspaces
            .get(&space.key)
            .and_then(|entry| entry.sibling_freshness.get(&self.repository))
            .cloned()
    }

    /// Builds the workspace with Cargo and runs its binary.
    fn build(&self, space: &Space) -> Built {
        let app = Path::new(&space.key);
        let target = app.join("target");
        let output = Command::new(env!("CARGO"))
            .args([
                "build",
                "--offline",
                "--message-format=json",
                "--manifest-path",
            ])
            .arg(app.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_HOME", self.scratch.join("cargo-home"))
            .env("CARGO_INCREMENTAL", "0")
            // The worker shell disables the shared compile cache wrapper;
            // a nested build must not pick it up from the Cargo config.
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_BUILD_RUSTC_WRAPPER", "")
            .current_dir(app)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cargo build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut fresh = BTreeMap::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if message["reason"] == "compiler-artifact" {
                if let (Some(name), Some(is_fresh)) = (
                    message["target"]["name"].as_str(),
                    message["fresh"].as_bool(),
                ) {
                    fresh.insert(name.to_string(), is_fresh);
                }
            }
        }
        let run = Command::new(target.join("debug/app")).output().unwrap();
        assert!(run.status.success());
        Built {
            output: String::from_utf8_lossy(&run.stdout).trim().to_string(),
            x_fresh: fresh["x"],
            y_fresh: fresh["y"],
        }
    }
}

#[test]
fn switching_back_to_an_existing_older_snapshot_rebuilds_the_sibling_crate() {
    let mut f = Fixture::new("motor-freshness-switch-back");
    let b1 = f.commit_crates("x1", "y1", &[]);
    let b2 = f.commit_crates("x2", "y1", &[]);
    f.create(&b1);
    f.create(&b2);
    let w = f.workspace("w");
    f.setup(&w, &b1);
    assert_eq!(f.build(&w).output, "x1 y1");
    f.setup(&w, &b2);
    assert_eq!(f.build(&w).output, "x2 y1");
    // The older snapshot's files are older than both builds.
    f.setup(&w, &b1);
    assert_eq!(f.build(&w).output, "x1 y1");
}

#[test]
fn new_snapshot_copied_from_a_third_tree_rebuilds_what_differs_from_the_last_build() {
    let mut f = Fixture::new("motor-freshness-third-tree");
    let b1 = f.commit_crates("x1", "y1", &[]);
    let b3 = f.commit_crates("x2", "y3", &[]);
    let b2 = f.commit_crates("x2", "y2", &[]);
    f.create(&b1);
    let s3 = f.create(&b3);
    let w = f.workspace("w");
    f.setup(&w, &b1);
    assert_eq!(f.build(&w).output, "x1 y1");
    // Another workspace uses b3, so it is the most recently used snapshot
    // and the creation of b2 copies it: x keeps b3's file and its old mtime.
    f.create(&b3);
    f.copies.lock().unwrap().clear();
    f.setup(&w, &b2);
    assert_eq!(*f.copies.lock().unwrap(), vec![s3]);
    assert_eq!(f.build(&w).output, "x2 y2");
}

#[test]
fn retry_after_refused_setups_at_the_new_tree_rebuilds() {
    let mut f = Fixture::new("motor-freshness-refused");
    let b1 = f.commit_crates("x1", "y1", &[]);
    let b2 = f.commit_crates("x2", "y1", &[]);
    f.create(&b1);
    f.create(&b2);
    let w = f.workspace("w");
    f.setup(&w, &b1);
    assert_eq!(f.build(&w).output, "x1 y1");
    let before = f.record(&w);

    // The store refuses b2's snapshot for a tree it was not verified to hold.
    let mismatched = Tree {
        commit: b2.commit.clone(),
        tree: b1.tree.clone(),
    };
    let error = f.touch(&w, &mismatched, SystemTime::now()).unwrap_err();
    assert!(error.to_string().contains("refusing it"), "{error}");
    assert_eq!(f.record(&w), before);

    // This attempt touched b2 and was refused afterwards, so its update is
    // dropped.
    let refused = f.touch(&w, &b2, SystemTime::now()).unwrap();
    assert!(refused.touched() > 0);
    drop(refused);
    assert_eq!(f.record(&w), before);

    f.setup(&w, &b2);
    assert_eq!(f.build(&w).output, "x2 y1");
}

#[test]
fn run_after_a_prepare_at_another_tree_rebuilds() {
    let mut f = Fixture::new("motor-freshness-prepare");
    let b1 = f.commit_crates("x1", "y1", &[]);
    let b2 = f.commit_crates("x2", "y1", &[]);
    f.create(&b1);
    f.create(&b2);
    let w = f.workspace("w");
    f.setup(&w, &b1);
    assert_eq!(f.build(&w).output, "x1 y1");
    // A Prepare of the workspace at b2 builds nothing; it touches b2 and
    // records the switch, so the job's own setup has nothing left to touch.
    assert!(f.setup(&w, &b2) > 0);
    assert_eq!(f.setup(&w, &b2), 0);
    assert_eq!(f.build(&w).output, "x2 y1");
}

#[test]
fn build_after_a_setup_that_built_nothing_compares_every_retained_tree() {
    let mut f = Fixture::new("motor-freshness-no-build");
    let b1 = f.commit_crates("x1", "y1", &[]);
    let b2 = f.commit_crates("x2", "y2", &[]);
    let b3 = f.commit_crates("x2", "y3", &[]);
    for tree in [&b1, &b2, &b3] {
        f.create(tree);
    }
    let w = f.workspace("w");
    f.setup(&w, &b1);
    assert_eq!(f.build(&w).output, "x1 y1");
    // A successful job at b2 that builds nothing.
    f.setup(&w, &b2);
    // b3 equals b2 in x, but x's artifact was built from b1.
    f.setup(&w, &b3);
    assert_eq!(f.build(&w).output, "x2 y3");
}

#[test]
fn repeated_run_at_the_same_snapshot_keeps_the_sibling_crate_fresh() {
    let mut f = Fixture::new("motor-freshness-repeat");
    let b1 = f.commit_crates("x1", "y1", &[]);
    let b2 = f.commit_crates("x2", "y1", &[]);
    f.create(&b1);
    f.create(&b2);
    let w = f.workspace("w");
    f.setup(&w, &b2);
    assert_eq!(f.build(&w).output, "x2 y1");
    f.setup(&w, &b1);
    let switched = f.build(&w);
    assert_eq!(switched.output, "x1 y1");
    assert!(!switched.x_fresh);
    assert_eq!(f.setup(&w, &b1), 0);
    let repeated = f.build(&w);
    assert_eq!(repeated.output, "x1 y1");
    assert!(repeated.x_fresh, "{repeated:?}");
    assert!(repeated.y_fresh, "{repeated:?}");
}

#[test]
fn tree_missing_from_the_mirror_differs_everywhere_and_is_touched_once() {
    let mut f = Fixture::new("motor-freshness-missing");
    let b1 = f.commit_crates("x1", "y1", &[]);
    f.orphan();
    let b2 = f.commit_crates("x2", "y1", &[]);
    f.create(&b1);
    f.create(&b2);
    let w = f.workspace("w");
    f.setup(&w, &b1);
    assert_eq!(f.build(&w).output, "x1 y1");
    let mirror = f.trees.mirror(&f.repository);
    git(
        &mirror,
        &["update-ref", "-d", &format!("refs/snapshots/{}", b1.commit)],
    );
    git(&mirror, &["reflog", "expire", "--expire=now", "--all"]);
    git(&mirror, &["gc", "-q", "--prune=now"]);
    assert!(
        workspace::git(
            &mirror,
            &["cat-file", "-e", &format!("{}^{{tree}}", b1.tree)]
        )
        .is_err(),
        "b1's tree must be gone from the mirror"
    );
    assert!(f.setup(&w, &b2) > 0);
    assert_eq!(f.setup(&w, &b2), 0);
    assert_eq!(f.build(&w).output, "x2 y1");
}

#[test]
fn workspace_built_before_any_record_starts_from_an_unknown_tree() {
    let mut f = Fixture::new("motor-freshness-upgrade");
    let a = f.commit_crates("x1", "y1", &[]);
    let b = f.commit_crates("x2", "y2", &[]);
    let c = f.commit_crates("x2", "y3", &[]);
    let sa = f.create(&a);
    f.create(&b);
    f.create(&c);
    let w = f.workspace("w");
    // Built against a with in-place siblings: catalogued, but no record.
    f.note(&w);
    f.point(&w, &sa);
    assert_eq!(f.build(&w).output, "x1 y1");
    assert_eq!(f.record(&w), None);
    // A setup at b that builds nothing, then a build at c, which equals b in
    // x but differs from a there.
    f.setup(&w, &b);
    f.setup(&w, &c);
    let built = f.build(&w);
    assert_eq!(built.output, "x2 y3");
    assert!(!built.x_fresh);
}

#[test]
fn crate_changed_in_an_older_tree_may_rebuild_but_the_build_has_the_requested_values() {
    let mut f = Fixture::new("motor-freshness-other-crate");
    let a = f.commit_crates("x1", "y1", &[]);
    let b = f.commit_crates("x2", "y1", &[]);
    let c = f.commit_crates("x2", "y3", &[]);
    for tree in [&a, &b, &c] {
        f.create(tree);
    }
    let w = f.workspace("w");
    f.setup(&w, &a);
    assert_eq!(f.build(&w).output, "x1 y1");
    f.setup(&w, &b);
    assert_eq!(f.build(&w).output, "x2 y1");
    f.setup(&w, &c);
    assert_eq!(f.build(&w).output, "x2 y3");
}

#[test]
fn touch_decision_at_the_close_instant_writes_above_it_once() {
    let mut f = Fixture::new("motor-freshness-instant");
    let b1 = f.commit_crates("x1", "y1", &[]);
    let b2 = f.commit_crates("x2", "y1", &[]);
    f.create(&b1);
    let s2 = f.create(&b2);
    let w = f.workspace("w");
    f.setup(&w, &b1);
    let source = s2.join("x/src/lib.rs");
    let instant = SystemTime::now();
    set_mtime_of(&source, instant);
    // The switch closes b1 at exactly the source's mtime: that close time is
    // not earlier than the mtime, so the source is touched, above it.
    assert_eq!(f.setup_started(&w, &b2, instant).unwrap(), 1);
    assert!(mtime(&source) > instant);
    // The same decision again, with the same close time, finds nothing.
    assert_eq!(f.setup_started(&w, &b2, instant).unwrap(), 0);
}

#[test]
fn concurrent_touches_of_one_snapshot_keep_the_newer_invalidation() {
    let mut f = Fixture::new("motor-freshness-interleaved");
    let b1 = f.commit_crates("x1", "y1", &[]);
    let b2 = f.commit_crates("x2", "y1", &[]);
    f.create(&b1);
    let s2 = f.create(&b2);
    let older = f.workspace("older");
    let newer = f.workspace("newer");
    f.setup(&older, &b1);
    assert_eq!(f.build(&older).output, "x1 y1");
    let older_closed = SystemTime::now();
    f.setup(&newer, &b1);
    assert_eq!(f.build(&newer).output, "x1 y1");
    let newer_closed = SystemTime::now();

    // The first touch to reach inspection pauses there until released.
    let (inspected, paused) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let gate = Mutex::new(Some((inspected, released)));
    f.store.touch_inspected = Some(Arc::new(move |_: &Path| {
        if let Some((inspected, released)) = gate.lock().unwrap().take() {
            inspected.send(()).unwrap();
            released.recv().unwrap();
        }
    }));
    let (older_pending, newer_pending) = std::thread::scope(|scope| {
        let first = scope.spawn(|| f.touch(&older, &b2, older_closed));
        paused.recv().unwrap();
        let second = scope.spawn(|| f.touch(&newer, &b2, newer_closed));
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !second.is_finished(),
            "a touch of the snapshot must wait while another one is between inspection and writing"
        );
        release.send(()).unwrap();
        (
            first.join().unwrap().unwrap(),
            second.join().unwrap().unwrap(),
        )
    });
    f.store.touch_inspected = None;
    assert!(older_pending.touched() > 0);
    assert!(newer_pending.touched() > 0);
    // The newer workspace's x artifact was built after the older close time,
    // so only a mtime above the newer close time makes it stale.
    assert!(mtime(&s2.join("x/src/lib.rs")) > newer_closed);
    for (space, pending) in [(&older, older_pending), (&newer, newer_pending)] {
        f.note(space);
        assert!(pending.apply(&mut f.catalog, &space.key));
        f.point(space, &s2);
        assert_eq!(f.build(space).output, "x2 y1");
    }
}

#[test]
fn deletion_only_switch_touches_and_recreates_nothing() {
    let mut f = Fixture::new("motor-freshness-deletion");
    let b1 = f.commit(&[("keep", "kept\n"), ("gone/file", "deleted\n")]);
    fs::remove_dir_all(f.source.join("gone")).unwrap();
    let b2 = f.commit(&[]);
    f.create(&b1);
    let s2 = f.create(&b2);
    let kept = mtime(&s2.join("keep"));
    let w = f.workspace("w");
    f.setup(&w, &b1);
    assert_eq!(f.setup(&w, &b2), 0);
    assert!(fs::symlink_metadata(s2.join("gone")).is_err());
    assert_eq!(mtime(&s2.join("keep")), kept);
}

#[test]
fn internal_symlink_change_touches_the_link_and_its_new_referent() {
    let mut f = Fixture::new("motor-freshness-internal-link");
    f.link("link", "a.txt");
    let b1 = f.commit(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    f.link("link", "b.txt");
    let b2 = f.commit(&[]);
    f.create(&b1);
    let s2 = f.create(&b2);
    let untouched = mtime(&s2.join("a.txt"));
    let w = f.workspace("w");
    f.setup(&w, &b1);
    let started = SystemTime::now();
    assert_eq!(f.setup_started(&w, &b2, started).unwrap(), 2);
    assert!(mtime(&s2.join("link")) > started);
    assert!(mtime(&s2.join("b.txt")) > started);
    assert_eq!(mtime(&s2.join("a.txt")), untouched);
}

#[test]
fn symlink_to_a_file_outside_the_snapshot_never_touches_that_file() {
    let mut f = Fixture::new("motor-freshness-external-link");
    let outside = f.scratch.join("outside.txt");
    fs::write(&outside, "outside\n").unwrap();
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    set_mtime_of(&outside, old);
    f.link("link", "a.txt");
    let b1 = f.commit(&[("a.txt", "a\n")]);
    f.link("link", outside.to_str().unwrap());
    let b2 = f.commit(&[]);
    // A relative link that climbs from the snapshot directory to the same
    // file outside it.
    let depth = f
        .config
        .sibling_snapshots_dir()
        .strip_prefix(&*f.scratch)
        .unwrap()
        .components()
        .count()
        + 2;
    f.link("relative", &format!("{}outside.txt", "../".repeat(depth)));
    let b3 = f.commit(&[]);
    for tree in [&b1, &b2, &b3] {
        f.create(tree);
    }
    assert_eq!(
        fs::canonicalize(f.snapshot(&b3).join("relative")).unwrap(),
        fs::canonicalize(&outside).unwrap()
    );
    let w = f.workspace("w");
    f.setup(&w, &b1);
    let started = SystemTime::now();
    // Only the changed link itself is touched.
    assert_eq!(f.setup_started(&w, &b2, started).unwrap(), 1);
    assert!(mtime(&f.snapshot(&b2).join("link")) > started);
    // Both links of b3 differ from b1, which the workspace also built.
    assert_eq!(f.setup(&w, &b3), 2);
    assert_eq!(mtime(&outside), old);
}

fn entry(tree: Option<&str>, closed_ns: Option<u64>) -> Entry {
    Entry {
        tree: tree.map(str::to_string),
        closed_ns,
    }
}

#[test]
fn record_over_its_limit_folds_its_oldest_entries_into_one_unknown_entry() {
    let closed: Vec<Closed> = (1..=20)
        .map(|n| Closed {
            tree: Some(format!("tree-{n}")),
            closed_ns: n,
        })
        .rev()
        .collect();
    let record = Record::after(closed, "current", DEFAULT_RECORD_ENTRIES);
    assert_eq!(record.entries.len(), DEFAULT_RECORD_ENTRIES);
    assert_eq!(record.entries[0], entry(None, Some(6)));
    assert_eq!(record.entries[1], entry(Some("tree-7"), Some(7)));
    assert_eq!(record.entries[14], entry(Some("tree-20"), Some(20)));
    assert_eq!(record.entries[15], entry(Some("current"), None));

    let small: Vec<Closed> = (1..=15)
        .map(|n| Closed {
            tree: Some(format!("tree-{n}")),
            closed_ns: n,
        })
        .collect();
    assert_eq!(
        Record::after(small, "current", DEFAULT_RECORD_ENTRIES)
            .entries
            .len(),
        16,
        "a record at its limit folds nothing"
    );
}

#[test]
fn setup_closes_other_trees_at_its_start_and_reopens_its_own() {
    let mut f = Fixture::new("motor-freshness-record");
    f.config.sibling_freshness_entries = 3;
    let trees: Vec<Tree> = (1..=4)
        .map(|n| f.commit(&[("value", &format!("{n}\n"))]))
        .collect();
    for tree in &trees {
        f.create(tree);
    }
    let w = f.workspace("w");
    let started: Vec<SystemTime> = (1..=5)
        .map(|n| SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000 + n))
        .collect();
    let ns = |time: SystemTime| Some(nanos(time));
    f.setup_started(&w, &trees[0], started[0]).unwrap();
    assert_eq!(
        f.record(&w).unwrap().entries,
        vec![entry(Some(&trees[0].tree), None)],
        "a workspace without a catalog entry starts with no older tree"
    );
    f.setup_started(&w, &trees[1], started[1]).unwrap();
    f.setup_started(&w, &trees[0], started[2]).unwrap();
    assert_eq!(
        f.record(&w).unwrap().entries,
        vec![
            entry(Some(&trees[1].tree), ns(started[2])),
            entry(Some(&trees[0].tree), None),
        ]
    );
    f.setup_started(&w, &trees[2], started[3]).unwrap();
    f.setup_started(&w, &trees[3], started[4]).unwrap();
    assert_eq!(
        f.record(&w).unwrap().entries,
        vec![
            entry(None, ns(started[3])),
            entry(Some(&trees[2].tree), ns(started[4])),
            entry(Some(&trees[3].tree), None),
        ]
    );
}

#[test]
fn catalogued_workspace_without_a_record_starts_from_one_open_unknown_entry() {
    let mut catalog = Catalog::default();
    assert_eq!(Prior::of(&catalog, "/w").record("/b"), Record::default());
    catalog
        .workspaces
        .insert("/w".to_string(), Workspace::default());
    assert_eq!(
        Prior::of(&catalog, "/w").record("/b").entries,
        vec![entry(None, None)]
    );
}

#[test]
fn switch_refuses_while_snapshot_mode_is_off() {
    let mut f = Fixture::new("motor-freshness-off");
    let b1 = f.commit(&[("value", "1\n")]);
    let snapshot = f.create(&b1);
    f.config.sibling_snapshot_mode_override = Some(false);
    let error = switch(
        &f.config,
        &f.store,
        &f.trees,
        &Prior::default(),
        SystemTime::now(),
        &f.repository,
        &snapshot,
        &b1.tree,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
}
