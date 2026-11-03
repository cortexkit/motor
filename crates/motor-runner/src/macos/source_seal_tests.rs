use super::*;
use std::cell::{Cell, RefCell};
use std::os::fd::AsRawFd;

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        let root = cortexkit_test_support::scratch_root()
            .join(format!("mac-source-seal-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
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
        let _ = super::super::fs::remove(&self.0);
    }
}

// Only the flag syscall is simulated for portable crash-boundary tests. All
// descriptor opens, identity comparisons, journal I/O and recovery walks are real.
#[derive(Default)]
struct TestFlags {
    values: RefCell<BTreeMap<Identity, u32>>,
    calls: Cell<usize>,
    fail_at: Cell<Option<usize>>,
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
}

#[test]
fn snapshot_plan_selects_regular_modes_and_exact_collector_returns() {
    let mut request: motor_protocol::Run = serde_json::from_value(serde_json::json!({
        "job_id": Uuid::now_v7(), "workspace_key": "/workspace",
        "repository_root": "/repository", "cwd": "/workspace/sub", "command": "true",
        "env": {}, "queue_wait_limit_s": 60,
        "snapshot_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "tree_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "integration_tip": "", "siblings": [], "bundles": []
    }))
    .unwrap();
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
    assert_eq!(
        plan(regular_files(tree).unwrap(), &returns(&request).unwrap()).unwrap(),
        vec![PathBuf::from("directory/child"), PathBuf::from("sub/run")]
    );
    request.return_paths = vec!["/workspace-escape/sub/run".into()];
    assert!(returns(&request).is_err());
    for invalid in ["../escape", "/absolute", ""] {
        assert!(plan([PathBuf::from(invalid)], &BTreeSet::new()).is_err());
    }
    assert!(regular_files(b"broken\0").is_err());
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
    let _cleanup = NativeCleanup(vec![source.try_clone().unwrap()]);
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
    Native.set(&cloned, clone_flags & !IMMUTABLE).unwrap();
    forget(&journal).unwrap();
}
