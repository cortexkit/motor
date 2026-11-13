use super::*;
use std::cell::{Cell, RefCell};
use std::os::fd::AsRawFd;

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        let root = cortexkit_test_support::scratch_root()
            .join(format!("mac-source-seal-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        // macOS TMPDIR can start with /var, a symlink to /private/var. Keep the
        // production no-follow walk strict and resolve only this fixture's root.
        #[cfg(target_os = "macos")]
        let root = std::fs::canonicalize(root).unwrap();
        Self(root)
    }
    fn file(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"original").unwrap();
        path
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = super::super::fs::remove_job(&self.0);
    }
}

// Only the flag syscall is simulated for portable crash-boundary tests. All
// descriptor opens, identity comparisons, journal I/O and recovery walks are real.
#[derive(Default)]
struct TestFlags {
    values: RefCell<BTreeMap<Identity, u32>>,
    calls: Cell<usize>,
    fail_at: Cell<Option<usize>>,
    fail_barrier: Cell<bool>,
}
impl Flags for TestFlags {
    fn inspect(&self, file: &File) -> io::Result<(Identity, u32)> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        assert_eq!(
            unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) },
            0
        );
        let key = identity(&unsafe { stat.assume_init() });
        let flags = *self.values.borrow().get(&key).unwrap_or(&0);
        Ok((key, flags))
    }
    fn set(&self, file: &File, flags: u32) -> io::Result<()> {
        let call = self.calls.get();
        self.calls.set(call + 1);
        if self.fail_at.get() == Some(call) {
            return Err(io::Error::other("injected flag interruption"));
        }
        let key = self.inspect(file)?.0;
        self.values.borrow_mut().insert(key, flags);
        Ok(())
    }
    fn clear_barrier(&self, roots: &[&Path]) -> io::Result<()> {
        if self.fail_barrier.get() {
            return Err(io::Error::other("injected bulk flush failure"));
        }
        Native.clear_barrier(roots)
    }
}

fn request() -> motor_protocol::Run {
    serde_json::from_value(serde_json::json!({
        "job_id": Uuid::now_v7(), "workspace_key": "/workspace",
        "repository_root": "/repository", "cwd": "/workspace/sub", "command": "true",
        "env": {}, "queue_wait_limit_s": 60,
        "snapshot_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "tree_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "integration_tip": "", "siblings": [], "bundles": []
    }))
    .unwrap()
}

#[test]
fn snapshot_plan_selects_regular_modes_and_exact_collector_returns() {
    let mut request = request();
    request.workspace_key = "/workspace".into();
    request.cwd = "/workspace/sub".into();
    request.return_paths = vec![
        "/workspace/sub/returned".into(),
        "/workspace/directory".into(),
    ];
    let tree = b"100644 blob aa\tsub/returned\0\
                 100755 blob bb\tsub/run\0\
                 100644 blob cc\tdirectory/child\0\
                 120000 blob dd\tlink\0\
                 160000 commit ee\tsubmodule\0\
                 100644 blob ff\tnode_modules/a\0\
                 100644 blob ff\tsub/node_modules/a\0\
                 100644 blob ff\ttarget/a\0\
                 100644 blob ff\tsub/target/a\0";
    let selected = plan(regular_files(tree).unwrap(), &returns(&request).unwrap()).unwrap();
    assert_eq!(
        selected.files,
        vec![PathBuf::from("directory/child"), PathBuf::from("sub/run")]
    );
    assert_eq!(
        selected.unprotected(),
        [
            "node_modules/a",
            "sub/node_modules/a",
            "sub/target/a",
            "target/a"
        ]
        .map(PathBuf::from)
    );
    assert!(!selected.source_read_only());
    request.return_paths = vec!["/workspace-escape/sub/run".into()];
    assert!(returns(&request).is_err());
    for invalid in ["../escape", "/absolute", ""] {
        assert!(plan([PathBuf::from(invalid)], &BTreeSet::new()).is_err());
    }
    assert!(regular_files(b"broken\0").is_err());
}

#[test]
fn unchanged_file_keeps_time_across_staged_jobs_and_changed_file_advances() {
    use std::time::{Duration, UNIX_EPOCH};
    let f = Tree::new();
    let destination = "/Users/motor/repo";
    let store = f.0.join("targets/checkout.img");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    std::fs::write(&store, "build disk").unwrap();
    let history = store.with_extension("source-mtimes.json");
    let start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let end = start + Duration::from_secs(60);
    let trees = |changed: &str| {
        vec![crate::source_mtimes::Tree {
            destination: destination.into(),
            prefix: "repo".into(),
            entries: format!(
                "100644 blob aa\tunchanged.rs\0\
                          100644 blob {changed}\tchanged.rs\0\
                          100644 blob cc\treturned.rs\0"
            )
            .into_bytes(),
        }]
    };
    let selected = plan(
        ["unchanged.rs", "changed.rs", "returned.rs"].map(PathBuf::from),
        &[PathBuf::from("returned.rs")].into(),
    )
    .unwrap();
    let mut previous = None;
    for (job, blob, now) in [
        ("first", "bb", start),
        ("second", "dd", start + Duration::from_secs(20)),
    ] {
        let staging = f.0.join(job);
        for name in ["unchanged.rs", "changed.rs", "returned.rs"] {
            let path = f.file(&format!("{job}/repo/{name}"));
            // Make staging times distinct even on a fast or coarse-clock host.
            File::open(path)
                .unwrap()
                .set_modified(now + Duration::from_secs(120))
                .unwrap();
        }
        let (candidate, times) = prepare_times(
            &trees(blob),
            &staging,
            &store,
            &history,
            destination,
            &selected,
            now,
        )
        .unwrap();
        apply_times(&staging, &times).unwrap();
        let modified = |name| {
            std::fs::metadata(staging.join("repo").join(name))
                .unwrap()
                .modified()
                .unwrap()
        };
        let unchanged = modified("unchanged.rs");
        if let Some(previous) = previous {
            assert_eq!(
                unchanged, previous,
                "unchanged source time must survive staging"
            );
            assert_eq!(modified("changed.rs"), end + Duration::from_secs(1));
            assert_eq!(modified("returned.rs"), end + Duration::from_secs(1));
        }
        previous = Some(unchanged);
        // Publication renames the job's build disk over the kept one before
        // saving history, so the store's identity changes every job.
        let published = f.0.join(format!("published-{job}"));
        std::fs::write(&published, "build disk").unwrap();
        std::fs::rename(&published, &store).unwrap();
        crate::source_mtimes::replaced(
            candidate,
            crate::source_mtimes::Store::File(&store),
            &history,
            end,
        )
        .unwrap();
    }
}

#[test]
fn source_times_never_follow_leaf_or_parent_symlinks() {
    let f = Tree::new();
    let outside = f.file("outside/file");
    let before = std::fs::metadata(&outside).unwrap().modified().unwrap();
    std::fs::create_dir(f.0.join("staged")).unwrap();
    for (name, target, path) in [
        ("link", outside.clone(), "link"),
        ("directory", f.0.join("outside"), "directory/file"),
    ] {
        let link = f.0.join("staged").join(name);
        std::os::unix::fs::symlink(target, &link).unwrap();
        let link_before = std::fs::symlink_metadata(&link)
            .unwrap()
            .modified()
            .unwrap();
        let times = [(path.to_owned(), 1_800_000_000_123)].into();
        assert!(apply_times(&f.0.join("staged"), &times).is_err());
        assert_eq!(
            std::fs::metadata(&outside).unwrap().modified().unwrap(),
            before
        );
        assert_eq!(
            std::fs::symlink_metadata(link).unwrap().modified().unwrap(),
            link_before
        );
    }
}

#[test]
fn tracked_cache_paths_make_the_persisted_source_fact_false() {
    for (tracked, returned, expected) in [
        ("target/tracked", None, false),
        ("src/target/tracked", None, false),
        ("node_modules/tracked", None, false),
        ("src/node_modules/tracked", None, false),
        ("src/source", None, true),
        ("target/tracked", Some("target/tracked"), true),
        (
            "src/node_modules/tracked",
            Some("src/node_modules/tracked"),
            true,
        ),
        ("target/tracked", Some("target"), false),
    ] {
        let tree = Tree::new();
        let root = tree.0.join("state");
        let config = crate::engine::Config::new(&root, "macos");
        let runtime = &config.macos_runtime;
        let entry = runtime.begin(&root, Uuid::now_v7(), "/workspace").unwrap();
        let record = entry.owned.lock().unwrap().record.clone();
        let workspace = record.paths.tree.join("workspace");
        std::fs::create_dir_all(workspace.join(tracked).parent().unwrap()).unwrap();
        std::fs::write(workspace.join(tracked), b"tracked").unwrap();
        std::fs::write(workspace.join("protected"), b"still protected").unwrap();
        let staged = super::super::Prepared {
            tree: record.paths.tree.clone(),
            workspace: workspace.clone(),
            cargo_home: record.paths.cargo.clone(),
            siblings: Vec::new(),
            prepared_javascript_directories: Vec::new(),
            source_files: vec![tracked.into(), "protected".into()],
        };
        // The trusted manifest must still account for a tracked root target
        // after the build disk's empty mount point replaces that directory in
        // the staged tree.
        super::super::execution::target_in(&staged).unwrap();
        if tracked.starts_with("target/") {
            assert!(!workspace.join(tracked).exists());
        }
        let returns = returned.map(PathBuf::from).into_iter().collect();
        let planned = plan(staged.source_files, &returns).unwrap();
        let flags = TestFlags::default();
        seal_with(&path(&root, record.id), &workspace, &planned.files, &flags).unwrap();
        assert_ne!(
            flags
                .inspect(&File::open(workspace.join("protected")).unwrap())
                .unwrap()
                .1
                & IMMUTABLE,
            0
        );
        runtime.record_sealed(&root, &entry, &planned).unwrap();
        assert_eq!(
            runtime.source_read_only(record.id),
            expected,
            "tracked={tracked}, returned={returned:?}"
        );
        let saved: super::super::lifecycle::Record = crate::storage::read_json(
            &root
                .join("macvm-records")
                .join(format!("{}.json", record.id)),
        )
        .unwrap();
        assert_eq!(saved.source_read_only, expected);
        assert_eq!(planned.unprotected().len(), usize::from(!expected));
        assert!(
            entry.active.load(std::sync::atomic::Ordering::Acquire),
            "incomplete coverage must not refuse the job"
        );
    }
}

#[test]
fn recovery_at_every_flag_crash_boundary_keeps_the_full_intent() {
    for boundary in ["before-first", "mid-seal", "sealed", "mid-unseal"] {
        let tree = Tree::new();
        tree.file("job/src/a");
        tree.file("job/src/b");
        let journal = tree.0.join("seals/job.json");
        let flags = TestFlags::default();
        let selected = vec!["src/a".into(), "src/b".into()];
        flags.fail_at.set(match boundary {
            "before-first" => Some(0),
            "mid-seal" => Some(1),
            _ => None,
        });
        let sealed = seal_with(&journal, &tree.0.join("job"), &selected, &flags);
        assert_eq!(sealed.is_ok(), matches!(boundary, "sealed" | "mid-unseal"));
        let intent = Journal::load(&journal).unwrap().unwrap();
        assert_eq!(intent.entries.len(), 2, "{boundary}");
        assert_eq!(intent.complete, sealed.is_ok());
        assert!(!intent.cleared);
        // Reopen durable state and relocate the source directory before retry.
        std::fs::rename(tree.0.join("job/src"), tree.0.join("job/renamed")).unwrap();
        flags.fail_at.set(if boundary == "mid-unseal" {
            Some(flags.calls.get() + 1)
        } else {
            None
        });
        if boundary == "mid-unseal" {
            assert!(unseal_with(&journal, &[&tree.0.join("job")], &flags).is_err());
            assert!(!Journal::load(&journal).unwrap().unwrap().cleared);
            assert!(forget(&journal).is_err());
            flags.fail_at.set(None);
        }
        unseal_with(&journal, &[&tree.0.join("job")], &flags).unwrap();
        assert!(
            flags.values.borrow().values().all(|v| v & IMMUTABLE == 0),
            "{boundary}"
        );
        assert!(Journal::load(&journal).unwrap().unwrap().cleared);
        unseal_with(&journal, &[], &flags).unwrap();
        forget(&journal).unwrap();
        assert!(!journal.exists());
    }
}

#[test]
fn cleared_marker_waits_for_bulk_flush_and_retries_after_failure() {
    let tree = Tree::new();
    tree.file("job/source");
    let journal = tree.0.join("seals/job.json");
    let flags = TestFlags::default();
    seal_with(&journal, &tree.0.join("job"), &["source".into()], &flags).unwrap();
    flags.fail_barrier.set(true);
    assert!(unseal_with(&journal, &[&tree.0.join("job")], &flags).is_err());
    assert!(!Journal::load(&journal).unwrap().unwrap().cleared);
    assert!(forget(&journal).is_err());
    // Simulate a flag that survived a crash while its earlier clear did not.
    let file = File::open(tree.0.join("job/source")).unwrap();
    flags.set(&file, IMMUTABLE).unwrap();
    flags.fail_barrier.set(false);
    unseal_with(&journal, &[&tree.0.join("job")], &flags).unwrap();
    assert_eq!(flags.inspect(&file).unwrap().1, 0);
    assert!(Journal::load(&journal).unwrap().unwrap().cleared);
}

#[test]
fn unseal_finds_target_moves_and_detached_quarantine_without_following_links() {
    let tree = Tree::new();
    tree.file("job/src/source");
    tree.file("outside/source");
    let journal = tree.0.join("seals/job.json");
    let flags = TestFlags::default();
    seal_with(
        &journal,
        &tree.0.join("job"),
        &["src/source".into()],
        &flags,
    )
    .unwrap();
    std::fs::create_dir(tree.0.join("job/target")).unwrap();
    std::fs::rename(tree.0.join("job/src"), tree.0.join("job/target/moved")).unwrap();
    std::os::unix::fs::symlink(tree.0.join("outside"), tree.0.join("job/absolute-link")).unwrap();
    // Detaching target must not lose the only path to a protected inode.
    std::fs::rename(tree.0.join("job/target"), tree.0.join("detached-target")).unwrap();
    std::fs::rename(tree.0.join("job"), tree.0.join("quarantine")).unwrap();
    unseal_with(
        &journal,
        &[
            &tree.0.join("job"),
            &tree.0.join("detached-target"),
            &tree.0.join("quarantine"),
        ],
        &flags,
    )
    .unwrap();
    assert_eq!(
        flags
            .inspect(&File::open(tree.0.join("detached-target/moved/source")).unwrap())
            .unwrap()
            .1,
        0
    );
    assert_eq!(
        flags.values.borrow().len(),
        1,
        "outside symlink must not be visited"
    );
}

#[test]
fn missing_or_replaced_inode_retains_obligation_and_does_not_clear_an_alias() {
    let tree = Tree::new();
    tree.file("job/src/source");
    let journal = tree.0.join("seals/job.json");
    let flags = TestFlags::default();
    seal_with(
        &journal,
        &tree.0.join("job"),
        &["src/source".into()],
        &flags,
    )
    .unwrap();
    std::fs::rename(tree.0.join("job/src"), tree.0.join("outside")).unwrap();
    tree.file("job/src/source");
    std::os::unix::fs::symlink(tree.0.join("outside"), tree.0.join("job/alias")).unwrap();
    assert!(unseal_with(&journal, &[&tree.0.join("job")], &flags).is_err());
    assert!(!Journal::load(&journal).unwrap().unwrap().cleared);
    assert!(require_cleared(&journal).is_err());
    assert!(flags.values.borrow().values().all(|v| *v == IMMUTABLE));
    std::fs::rename(tree.0.join("outside"), tree.0.join("job/recovered")).unwrap();
    unseal_with(&journal, &[&tree.0.join("job")], &flags).unwrap();
}

#[test]
fn original_immutable_and_unrelated_flags_are_preserved() {
    let tree = Tree::new();
    tree.file("job/a");
    tree.file("job/b");
    let journal = tree.0.join("seals/job.json");
    let flags = TestFlags::default();
    let a = File::open(tree.0.join("job/a")).unwrap();
    let b = File::open(tree.0.join("job/b")).unwrap();
    flags.set(&a, IMMUTABLE | 0x8000).unwrap();
    flags.set(&b, 0x8000).unwrap();
    seal_with(
        &journal,
        &tree.0.join("job"),
        &["a".into(), "b".into()],
        &flags,
    )
    .unwrap();
    unseal_with(&journal, &[&tree.0.join("job")], &flags).unwrap();
    assert_eq!(flags.inspect(&a).unwrap().1, IMMUTABLE | 0x8000);
    assert_eq!(flags.inspect(&b).unwrap().1, 0x8000);
}

#[test]
fn malformed_unknown_and_oversized_journals_fail_closed() {
    let tree = Tree::new();
    let journal = tree.file("seals/job.json");
    for bad in [
        b"broken".as_slice(),
        br#"{"version":2,"entries":[],"complete":true,"cleared":true}"#,
    ] {
        std::fs::write(&journal, bad).unwrap();
        assert!(unseal_with(&journal, &[], &TestFlags::default()).is_err());
        assert!(forget(&journal).is_err());
        assert!(journal.exists());
    }
    File::options()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_len(MAX_JOURNAL + 1)
        .unwrap();
    assert!(Journal::load(&journal).is_err());
    assert!(require_complete(&tree.0.join("missing")).is_err());
}

#[test]
fn selection_refuses_symlinks_hardlinks_and_missing_files_before_first_flag() {
    let tree = Tree::new();
    tree.file("job/source");
    std::os::unix::fs::symlink("source", tree.0.join("job/link")).unwrap();
    let flags = TestFlags::default();
    let journal = tree.0.join("seals/job.json");
    for path in ["missing", "link"] {
        assert!(seal_with(&journal, &tree.0.join("job"), &[path.into()], &flags).is_err());
    }
    std::fs::hard_link(tree.0.join("job/source"), tree.0.join("outside-alias")).unwrap();
    assert!(seal_with(&journal, &tree.0.join("job"), &["source".into()], &flags).is_err());
    assert_eq!(flags.calls.get(), 0);
    assert!(!journal.exists());
}

#[cfg(target_os = "macos")]
struct NativeCleanup(Vec<File>);
#[cfg(target_os = "macos")]
impl Drop for NativeCleanup {
    fn drop(&mut self) {
        for file in &self.0 {
            if let Ok((_, flags)) = Native.inspect(file) {
                let _ = Native.set(file, flags & !IMMUTABLE);
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn native_uchg_survives_parent_rename_and_unseals_in_detached_target() {
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new();
    tree.file("job/src/source");
    tree.file("job/returned");
    let source = File::open(tree.0.join("job/src/source")).unwrap();
    let mut cleanup = NativeCleanup(vec![source.try_clone().unwrap()]);
    let before = Native.inspect(&source).unwrap();
    assert!(
        before.0.birth_seconds > 0,
        "APFS birth time is part of recovery identity"
    );
    // Preparation/canonical copies must be independent file objects, not aliases.
    Dir::open(&tree.0.join("job/src"), false)
        .unwrap()
        .clone_into(
            &Dir::open(&tree.0.join("canonical"), true).unwrap(),
            &super::super::lifecycle::control(super::super::Cancellation::default()),
        )
        .unwrap();
    let canonical = File::open(tree.0.join("canonical/source")).unwrap();
    cleanup.0.push(canonical.try_clone().unwrap());
    assert_ne!(Native.inspect(&canonical).unwrap().0, before.0);
    let journal = tree.0.join("seals/job.json");
    seal(&journal, &tree.0.join("job"), &["src/source".into()]).unwrap();
    assert_eq!(Native.inspect(&source).unwrap().1, before.1 | IMMUTABLE);
    assert_eq!(
        std::fs::write(tree.0.join("job/src/source"), b"changed")
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
    std::fs::write(tree.0.join("job/returned"), b"return").unwrap();
    std::fs::write(tree.0.join("job/new"), b"output").unwrap();
    std::fs::write(tree.0.join("canonical/source"), b"independent").unwrap();
    std::fs::rename(tree.0.join("job/src"), tree.0.join("job/renamed")).unwrap();
    assert_eq!(Native.inspect(&source).unwrap().0, before.0);
    std::fs::create_dir(tree.0.join("job/target")).unwrap();
    std::fs::rename(tree.0.join("job/renamed"), tree.0.join("job/target/moved")).unwrap();
    std::fs::rename(tree.0.join("job/target"), tree.0.join("detached-target")).unwrap();
    std::fs::set_permissions(
        tree.0.join("detached-target/moved"),
        std::fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    unseal(
        &journal,
        &[&tree.0.join("job"), &tree.0.join("detached-target")],
    )
    .unwrap();
    assert_eq!(Native.inspect(&source).unwrap(), before);
    std::fs::write(
        tree.0.join("detached-target/moved/source"),
        b"writable again",
    )
    .unwrap();
    // Model a crash where cleared persisted but the inode's clear did not.
    assert!(Journal::load(&journal).unwrap().unwrap().cleared);
    Native.set(&source, before.1 | IMMUTABLE).unwrap();
    let canonical_flags = Native.inspect(&canonical).unwrap().1;
    Native.set(&canonical, canonical_flags | IMMUTABLE).unwrap();
    std::os::unix::fs::symlink(
        tree.0.join("canonical"),
        tree.0.join("detached-target/alias"),
    )
    .unwrap();
    assert_eq!(
        super::super::fs::remove(&tree.0.join("detached-target"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
    super::super::fs::remove_job(&tree.0.join("detached-target")).unwrap();
    assert_eq!(Native.inspect(&source).unwrap().1, before.1);
    assert_eq!(
        Native.inspect(&canonical).unwrap().1,
        canonical_flags | IMMUTABLE,
        "job cleanup must not follow links into another tree"
    );
    assert!(!tree.0.join("detached-target").exists());
    forget(&journal).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn native_apfs_clone_of_sealed_file_records_flag_propagation() {
    let tree = Tree::new();
    tree.file("job/source");
    let source = File::open(tree.0.join("job/source")).unwrap();
    let mut cleanup = NativeCleanup(vec![source.try_clone().unwrap()]);
    let journal = tree.0.join("seals/job.json");
    seal(&journal, &tree.0.join("job"), &["source".into()]).unwrap();
    Dir::open(&tree.0.join("job"), false)
        .unwrap()
        .clone_into(
            &Dir::open(&tree.0.join("cloned"), true).unwrap(),
            &super::super::lifecycle::control(super::super::Cancellation::default()),
        )
        .unwrap();
    let cloned = File::open(tree.0.join("cloned/source")).unwrap();
    cleanup.0.push(cloned.try_clone().unwrap());
    let (clone_id, clone_flags) = Native.inspect(&cloned).unwrap();
    assert_ne!(clone_id, Native.inspect(&source).unwrap().0);
    assert_eq!(
        std::fs::read(tree.0.join("cloned/source")).unwrap(),
        b"original"
    );
    eprintln!(
        "APFS clonefile of sealed source: clone_keeps_uchg={} flags={clone_flags:#x}",
        clone_flags & IMMUTABLE != 0
    );
    unseal(&journal, &[&tree.0.join("job")]).unwrap();
    assert_eq!(
        Native.inspect(&cloned).unwrap().1,
        clone_flags,
        "unseal must not clear a different cloned inode"
    );
    forget(&journal).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "operator: measure 7,000 real host APFS flag operations, never inside virtiofs"]
fn native_timing_seal_and_unseal_7000_small_files() {
    let tree = Tree::new();
    let job = tree.0.join("job");
    std::fs::create_dir(&job).unwrap();
    let mut selected = Vec::with_capacity(7000);
    for index in 0..7000 {
        let relative = PathBuf::from(format!("source-{index}"));
        std::fs::write(job.join(&relative), [b'x'; 200]).unwrap();
        selected.push(relative);
    }
    let journal = tree.0.join("seals/job.json");
    let started = std::time::Instant::now();
    seal(&journal, &job, &selected).unwrap();
    let seal_elapsed = started.elapsed();
    let started = std::time::Instant::now();
    unseal(&journal, &[&job]).unwrap();
    let unseal_elapsed = started.elapsed();
    eprintln!(
        "APFS source seal timing: files=7000 bytes_per_file=200 seal_ms={:.3} unseal_ms={:.3}",
        seal_elapsed.as_secs_f64() * 1000.0,
        unseal_elapsed.as_secs_f64() * 1000.0
    );
    assert!(Journal::load(&journal).unwrap().unwrap().cleared);
    for relative in [selected.first().unwrap(), selected.last().unwrap()] {
        assert_eq!(
            Native
                .inspect(&File::open(job.join(relative)).unwrap())
                .unwrap()
                .1
                & IMMUTABLE,
            0
        );
    }
    forget(&journal).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn native_return_alias_spooling_does_not_clone_sealed_flags() {
    use crate::return_paths::Collection;
    let tree = Tree::new();
    tree.file("job/source");
    let workspace = tree.0.join("job");
    std::os::unix::fs::symlink("source", workspace.join("out")).unwrap();
    let mut request = request();
    request.cwd = request.workspace_key.clone();
    request.return_paths = vec![format!("{}/out", request.workspace_key)];
    let planned = plan(
        regular_files(
            b"100644 blob aa\tsource\0\
        120000 blob bb\tout\0",
        )
        .unwrap(),
        &returns(&request).unwrap(),
    )
    .unwrap();
    assert_eq!(planned.files, vec![PathBuf::from("source")]);
    let source = File::open(workspace.join("source")).unwrap();
    let _cleanup = NativeCleanup(vec![source.try_clone().unwrap()]);
    let journal = tree.0.join("seals/job.json");
    seal(&journal, &workspace, &planned.files).unwrap();
    for chunked in [false, true] {
        request.return_chunked = chunked;
        let spool = tree
            .0
            .join("returns")
            .join(if chunked { "chunked" } else { "legacy" });
        // The alias is not the selected source pathname. Collection may read a
        // sealed inode, so output must be byte-written rather than APFS-cloned.
        match crate::return_paths::collect_staged(&request, &workspace, &spool) {
            Collection::Legacy(result) => {
                assert!(!chunked);
                assert!(result.return_paths_failed.is_none());
                let files = result.returned_files.unwrap();
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].data, "b3JpZ2luYWw=");
            }
            Collection::Chunked(result) => {
                assert!(chunked);
                assert_eq!(result.chunks.len(), 1);
                let chunk = spool.join(result.directory).join("chunk-0");
                assert_eq!(std::fs::read(&chunk).unwrap(), b"original");
                let (chunk_id, chunk_flags) = Native.inspect(&File::open(&chunk).unwrap()).unwrap();
                assert_ne!(chunk_id, Native.inspect(&source).unwrap().0);
                assert_eq!(chunk_flags & IMMUTABLE, 0);
                std::fs::write(&chunk, b"writable before source unseal").unwrap();
                std::fs::remove_file(&chunk).unwrap();
            }
        }
        assert_ne!(Native.inspect(&source).unwrap().1 & IMMUTABLE, 0);
        assert!(!Journal::load(&journal).unwrap().unwrap().cleared);
    }
    unseal(&journal, &[&workspace]).unwrap();
    forget(&journal).unwrap();
}
