use super::*;
use cortexkit_test_support::ScratchDir;

fn request() -> Run {
    serde_json::from_value(serde_json::json!({
        "job_id": Uuid::now_v7(), "workspace_key": "/caller/task",
        "repository_root": "/caller/repo", "cwd": "/caller/task/pkg",
        "command": "true", "env": {}, "queue_wait_limit_s": 60,
        "snapshot_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "tree_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "integration_tip": "", "siblings": [], "bundles": []
    }))
    .unwrap()
}

#[test]
fn sealed_set_uses_only_regular_blob_modes() {
    let tree = b"100644 blob a\tfile\0\
                 100755 blob b\texecutable\0\
                 120000 blob c\tlink\0\
                 160000 commit d\tsubmodule\0\
                 040000 tree e\tdirectory\0";
    let plan = plan(&Config::new("/state", "test"), &request(), &[], tree).unwrap();
    assert_eq!(
        plan.files,
        [PathBuf::from("file"), PathBuf::from("executable")]
    );
}

#[test]
fn sealed_set_excludes_only_prepared_node_modules_and_exact_returns() {
    let mut run = request();
    run.return_paths = vec!["/caller/task/pkg/golden".into()];
    let tree = b"100644 blob a\tnode_modules/root\0\
                 100644 blob b\tpkg/node_modules/prepared\0\
                 100644 blob c\tother/node_modules/unprepared\0\
                 100644 blob d\tpkg/golden\0\
                 100644 blob e\tpkg/golden-more\0\
                 100644 blob f\tnode_modules-not/file\0";
    let plan = plan(
        &Config::new("/state", "test"),
        &run,
        &["".into(), "pkg".into()],
        tree,
    )
    .unwrap();
    assert_eq!(
        plan.files,
        [
            "other/node_modules/unprepared",
            "pkg/golden-more",
            "node_modules-not/file"
        ]
        .map(PathBuf::from)
    );
}

#[test]
fn subfolder_cwd_relative_return_is_mapped_to_snapshot_path() {
    let mut run = request();
    // The client joins its cwd to a relative return before sending the Run.
    run.return_paths = vec![Path::new(&run.cwd)
        .join("golden")
        .to_string_lossy()
        .into_owned()];
    let mut config = Config::new("/state", "test");
    config.caller_roots = vec![crate::CallerRoot {
        caller_prefix: "/caller".into(),
        host_root: "/host/checkouts".into(),
    }];
    let plan = plan(
        &config,
        &run,
        &[],
        b"100644 blob a\tpkg/golden\0\
        100644 blob b\tgolden\0\
        100644 blob c\tpkg/other\0",
    )
    .unwrap();
    assert_eq!(plan.workspace, PathBuf::from("/host/checkouts/task"));
    assert_eq!(
        plan.files,
        [PathBuf::from("golden"), PathBuf::from("pkg/other")]
    );
}

#[test]
fn sealed_set_rejects_escape_paths_and_unmapped_returns() {
    let config = Config::new("/state", "test");
    assert!(plan(&config, &request(), &[], b"100644 blob a\t../escape\0").is_err());
    assert!(plan(&config, &request(), &[], b"100644 blob a\t/absolute\0").is_err());
    let mut run = request();
    run.return_paths = vec!["/caller/other/file".into()];
    assert!(plan(&config, &run, &[], b"").is_err());
}

#[test]
fn seal_record_roundtrips_non_utf8_snapshot_paths() {
    let plan = plan(
        &Config::new("/state", "test"),
        &request(),
        &[],
        b"100644 blob a\tfile\xff\0",
    )
    .unwrap();
    let path = plan.files[0].clone();
    let record = Record {
        filesystem: None,
        plan,
        handles: vec![Handle {
            path: path.clone(),
            kind: 1,
            bytes: vec![1, 2],
            prior_flags: 0,
        }],
    };
    let restored: Record = serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(restored.plan.files.as_slice(), std::slice::from_ref(&path));
    assert_eq!(restored.handles[0].path, path);
}

#[test]
fn pinned_open_refuses_symlinks_directories_and_escapes() {
    let scratch = ScratchDir::new("motor-seal-open");
    fs::create_dir(scratch.join("root")).unwrap();
    fs::write(scratch.join("outside"), b"outside").unwrap();
    std::os::unix::fs::symlink(scratch.join("outside"), scratch.join("root/link")).unwrap();
    std::os::unix::fs::symlink(scratch.path(), scratch.join("root/parent")).unwrap();
    fs::create_dir(scratch.join("root/directory")).unwrap();
    fs::write(scratch.join("root/file"), b"source").unwrap();
    let root = File::open(scratch.join("root")).unwrap();
    for path in [
        "link",
        "parent/outside",
        "directory",
        "../outside",
        "missing",
    ] {
        assert!(open_source(&root, Path::new(path)).is_err(), "{path}");
    }
    assert!(open_source(&root, Path::new("file")).is_ok());
}

#[test]
fn pinned_mount_and_source_descriptors_are_real_ioctl_capable_files() {
    let scratch = ScratchDir::new("motor-seal-fd-flags");
    fs::write(scratch.join("source"), b"source").unwrap();
    let mount = open_mount(scratch.path()).unwrap();
    let file = open_source(&mount, Path::new("source")).unwrap();
    for descriptor in [&mount, &file] {
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_PATH, 0);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
        assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
        // Reading inode flags needs no capability; this test does not set the
        // immutable bit.
        file_flags(descriptor).unwrap();
    }
}

#[test]
fn leftover_record_holds_workspace_and_ancestors_after_reload() {
    let scratch = ScratchDir::new("motor-seal-hold");
    let id = Uuid::now_v7();
    let record = Record {
        filesystem: None,
        plan: plan(&Config::new("/state", "test"), &request(), &[], b"").unwrap(),
        handles: vec![],
    };
    let path = record_path(scratch.path(), id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let runtime = Runtime::default();
    runtime.load(scratch.path()).unwrap();
    assert_eq!(runtime.ids(), vec![id]);
    assert_ne!(id, record.plan.unit);
    assert_eq!(runtime.unit(id), Some(record.plan.unit));
    for path in ["/caller/task", "/caller/task/target", "/caller"] {
        assert!(runtime.blocks_caller(path));
        assert!(runtime.check_host(Path::new(path)).is_err());
    }
    assert!(!runtime.blocks_caller("/caller/task-other"));
    assert!(runtime.check_host(Path::new("/caller/free")).is_ok());
}

fn helper_fixture(scratch: &ScratchDir) -> (PathBuf, Uuid, Plan) {
    let state = scratch.join("state");
    let id = Uuid::now_v7();
    fs::create_dir_all(state.join("jobs").join(id.to_string())).unwrap();
    let workspace = scratch.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let plan = Plan {
        unit: Uuid::now_v7(),
        caller_workspace: workspace.clone(),
        workspace,
        files: vec![],
    };
    fs::write(intent_path(&state, id), serde_json::to_vec(&plan).unwrap()).unwrap();
    (state, id, plan)
}

#[test]
fn helper_refuses_preexisting_staging_links_without_changing_the_target() {
    for hardlink in [true, false] {
        let scratch = ScratchDir::new("motor-seal-staging-link");
        let (state, id, plan) = helper_fixture(&scratch);
        let victim = scratch.join("unrelated-file");
        let original = b"must not truncate or change owner";
        fs::write(&victim, original).unwrap();
        let before = fs::metadata(&victim).unwrap();
        let staging = record_path(&state, id).with_extension("new");
        if hardlink {
            fs::hard_link(&victim, &staging).unwrap();
        } else {
            std::os::unix::fs::symlink(&victim, &staging).unwrap();
        }
        let error = seal(&state, id, plan).unwrap_err();
        assert_eq!(fs::read(&victim).unwrap(), original);
        let after = fs::metadata(&victim).unwrap();
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        assert!(error.to_string().contains("create record"), "{error}");
        assert!(!record_path(&state, id).exists());
    }
}

#[test]
fn recovery_unlinks_stale_staging_without_following_its_target() {
    let scratch = ScratchDir::new("motor-seal-staging-recovery");
    let (state, id, _) = helper_fixture(&scratch);
    let outside = scratch.join("outside");
    fs::write(&outside, b"outside").unwrap();
    let staging = record_path(&state, id).with_extension("new");
    std::os::unix::fs::symlink(&outside, &staging).unwrap();
    unseal(&state, id).unwrap();
    assert_eq!(fs::read(&outside).unwrap(), b"outside");
    assert!(
        fs::symlink_metadata(staging).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    );
    assert!(!intent_path(&state, id).exists());
}

#[test]
fn helper_refuses_symlinked_intent_and_seal_reads() {
    for intent in [true, false] {
        let scratch = ScratchDir::new("motor-seal-read-link");
        let (state, id, plan) = helper_fixture(&scratch);
        let outside = scratch.join("outside-record");
        let path = if intent {
            intent_path(&state, id)
        } else {
            record_path(&state, id)
        };
        let bytes = if intent {
            serde_json::to_vec(&plan).unwrap()
        } else {
            serde_json::to_vec(&Record {
                filesystem: None,
                plan: plan.clone(),
                handles: vec![],
            })
            .unwrap()
        };
        fs::write(&outside, &bytes).unwrap();
        if intent {
            fs::remove_file(&path).unwrap();
        }
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let result = if intent {
            seal(&state, id, plan)
        } else {
            unseal(&state, id)
        };
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ELOOP));
        assert_eq!(fs::read(&outside).unwrap(), bytes);
        assert!(fs::symlink_metadata(path).unwrap().is_symlink());
    }
}

#[test]
fn helper_state_operations_stay_in_the_pinned_job_after_ancestor_swap() {
    let scratch = ScratchDir::new("motor-seal-state-swap");
    let (state, id, _) = helper_fixture(&scratch);
    let directory = JobDirectory::open(&state, id).unwrap();
    let original_intent = directory.read(c"seal-intent").unwrap();
    let outside = scratch.join("outside");
    fs::create_dir_all(outside.join(id.to_string())).unwrap();
    let outside_file = outside.join(id.to_string()).join("seal-intent");
    fs::write(&outside_file, b"outside").unwrap();
    let retained = state.join("retained-jobs");
    fs::rename(state.join("jobs"), &retained).unwrap();
    std::os::unix::fs::symlink(&outside, state.join("jobs")).unwrap();
    assert!(JobDirectory::open(&state, id).is_err());
    assert_eq!(directory.read(c"seal-intent").unwrap(), original_intent);
    let mut created = directory
        .file(c"seal.new", libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
        .unwrap();
    created.write_all(b"pinned").unwrap();
    assert_eq!(
        fs::read(retained.join(id.to_string()).join("seal.new")).unwrap(),
        b"pinned"
    );
    directory.unlink(c"seal-intent").unwrap();
    assert!(!retained.join(id.to_string()).join("seal-intent").exists());
    assert_eq!(fs::read(&outside_file).unwrap(), b"outside");
    assert!(!outside.join(id.to_string()).join("seal.new").exists());
}

#[test]
fn concurrent_runtime_claims_have_one_winner_per_job_without_blocking_other_ids() {
    let runtime = std::sync::Arc::new(Runtime::default());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let id = Uuid::now_v7();
    let plan = plan(&Config::new("/state", "test"), &request(), &[], b"").unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            let plan = plan.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let shared = runtime.claim(id, plan.clone());
                runtime.claim(Uuid::now_v7(), plan).unwrap();
                shared
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(Result::is_ok)
            .count(),
        1
    );
    assert_eq!(runtime.ids().len(), 9);
    assert!(runtime.claim(id, plan).is_err());
}

#[test]
fn stale_handles_and_failed_durability_are_errors() {
    let handle = Handle {
        path: "tracked".into(),
        kind: 1,
        bytes: vec![],
        prior_flags: 0,
    };
    for errno in [libc::ESTALE, libc::EIO] {
        let error = clear_handles(std::slice::from_ref(&handle), |_| {
            Err(io::Error::from_raw_os_error(errno))
        })
        .unwrap_err();
        assert!(error.to_string().contains("tracked"));
    }
}

#[test]
fn path_pin_is_not_a_read_open_and_reopen_preserves_identity() {
    let scratch = ScratchDir::new("seal-path-pin");
    fs::write(scratch.join("file"), b"source").unwrap();
    let root = open_mount(scratch.path()).unwrap();
    let pin = pin_source(&root, Path::new("file")).unwrap();
    assert_ne!(
        unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFL) } & libc::O_PATH,
        0
    );
    let file = reopen_regular(&pin).unwrap();
    assert_eq!(
        (pin.metadata().unwrap().dev(), pin.metadata().unwrap().ino()),
        (
            file.metadata().unwrap().dev(),
            file.metadata().unwrap().ino()
        )
    );
    let fifo = CString::new(scratch.join("fifo").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(pin_source(&root, Path::new("fifo")).is_err());
}

#[test]
fn helper_refuses_multiply_linked_source_before_flags_change() {
    let scratch = ScratchDir::new("seal-hardlink-refusal");
    let (state, id, mut plan) = helper_fixture(&scratch);
    fs::write(plan.workspace.join("source"), b"source").unwrap();
    fs::hard_link(plan.workspace.join("source"), scratch.join("alias")).unwrap();
    plan.files = vec!["source".into()];
    fs::write(intent_path(&state, id), serde_json::to_vec(&plan).unwrap()).unwrap();
    assert!(seal(&state, id, plan)
        .unwrap_err()
        .to_string()
        .contains("exactly one hard link"));
    assert_eq!(fs::read(scratch.join("alias")).unwrap(), b"source");
    assert!(!record_path(&state, id).exists());
}

fn root_source(plan: &mut Plan, count: usize) {
    assert_eq!(unsafe { libc::geteuid() }, 0, "requires root");
    let identity = crate::isolation::job_identity().unwrap();
    for index in 0..count {
        let name = PathBuf::from(format!("source-{index}"));
        let file = File::create(plan.workspace.join(&name)).unwrap();
        assert_eq!(
            unsafe { libc::fchown(file.as_raw_fd(), identity.uid, identity.gid) },
            0
        );
        plan.files.push(name);
    }
}

#[test]
#[ignore = "requires root, file handles and CAP_LINUX_IMMUTABLE"]
fn isolated_recovery_retains_stale_handles_and_filesystem_mismatches() {
    let scratch = ScratchDir::new("seal-recovery-identity");
    let (state, id, mut plan) = helper_fixture(&scratch);
    root_source(&mut plan, 1);
    let runtime = Runtime::default();
    runtime.seal(&state, id, plan.clone()).unwrap();
    let path = record_path(&state, id);
    let original = fs::read(&path).unwrap();
    let mut record: Record = serde_json::from_slice(&original).unwrap();
    record.filesystem.as_mut().unwrap().device ^= 1;
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(runtime
        .unseal(&state, id)
        .unwrap_err()
        .to_string()
        .contains("filesystem identity mismatch"));
    assert!(path.exists());
    assert!(runtime.check_host(&plan.workspace).is_err());
    record = serde_json::from_slice(&original).unwrap();
    let dead = plan.workspace.join("dead");
    let stale = {
        let file = File::create(&dead).unwrap();
        handle(&file, "dead".into()).unwrap()
    };
    fs::remove_file(dead).unwrap();
    assert_eq!(
        open_handle(&open_mount(&plan.workspace).unwrap(), &stale)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ESTALE)
    );
    record.handles.push(stale);
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(runtime.unseal(&state, id).is_err());
    assert!(path.exists());
    assert!(runtime.check_host(&plan.workspace).is_err());
    fs::write(&path, original).unwrap();
    runtime.unseal(&state, id).unwrap();
    assert!(!path.exists());
}

#[test]
#[ignore = "requires root, file handles and CAP_LINUX_IMMUTABLE"]
fn isolated_seal_preserves_preexisting_immutable_flags() {
    let scratch = ScratchDir::new("seal-prior-flags");
    let (state, id, mut plan) = helper_fixture(&scratch);
    root_source(&mut plan, 1);
    let file = File::open(plan.workspace.join(&plan.files[0])).unwrap();
    struct Clear(File);
    impl Drop for Clear {
        fn drop(&mut self) {
            let _ = immutable(&self.0, false);
            let _ = self.0.sync_all();
        }
    }
    immutable(&file, true).unwrap();
    let _clear = Clear(file.try_clone().unwrap());
    let runtime = Runtime::default();
    runtime.seal(&state, id, plan).unwrap();
    let record: Record =
        serde_json::from_slice(&fs::read(record_path(&state, id)).unwrap()).unwrap();
    assert!(record.handles.is_empty());
    runtime.unseal(&state, id).unwrap();
    assert_ne!(file_flags(&file).unwrap() & IMMUTABLE, 0);
}

#[test]
#[ignore = "requires root, file handles and CAP_LINUX_IMMUTABLE; measures 7111-file seal and fsync unseal"]
fn isolated_source_seal_7111_file_timing() {
    let scratch = ScratchDir::new("seal-timing");
    let (state, id, mut plan) = helper_fixture(&scratch);
    root_source(&mut plan, 7111);
    let runtime = Runtime::default();
    let start = std::time::Instant::now();
    runtime.seal(&state, id, plan).unwrap();
    let seal = start.elapsed();
    let start = std::time::Instant::now();
    runtime.unseal(&state, id).unwrap();
    eprintln!(
        "SOURCE_SEAL_7111_US={} SOURCE_UNSEAL_FSYNC_7111_US={}",
        seal.as_micros(),
        start.elapsed().as_micros()
    );
}

#[test]
fn runner_owned_or_writable_records_are_not_trusted() {
    assert!(trusted_record(0, 0o644));
    assert!(!trusted_record(1000, 0o644));
    assert!(!trusted_record(0, 0o664));
    assert!(!trusted_record(0, 0o646));
}

#[test]
fn unseal_requires_successful_sync_and_rejects_prior_immutable_provenance() {
    let scratch = ScratchDir::new("seal-sync-error");
    fs::write(scratch.join("source"), b"source").unwrap();
    let file = File::open(scratch.join("source")).unwrap();
    let mut entry = Handle {
        path: "source".into(),
        kind: 1,
        bytes: vec![],
        prior_flags: 0,
    };
    let error = clear_inode(&file, &entry, |_| {
        Err(io::Error::from_raw_os_error(libc::EIO))
    })
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    entry.prior_flags = IMMUTABLE;
    assert!(clear_inode(&file, &entry, |_| panic!(
        "must not sync an inode the seal did not change"
    ))
    .unwrap_err()
    .to_string()
    .contains("already immutable"));
}

#[test]
#[ignore = "requires root and the motor-job account; verifies inode and record ownership refusals"]
fn isolated_helper_refuses_root_owned_source_and_runner_forged_record() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let scratch = ScratchDir::new("seal-helper-owner");
    let (state, id, mut plan) = helper_fixture(&scratch);
    fs::write(plan.workspace.join("source"), b"root source").unwrap();
    plan.files = vec!["source".into()];
    fs::write(intent_path(&state, id), serde_json::to_vec(&plan).unwrap()).unwrap();
    assert!(seal(&state, id, plan.clone())
        .unwrap_err()
        .to_string()
        .contains("non-root owner"));
    let path = record_path(&state, id);
    let record = Record {
        filesystem: Some(Filesystem::read(&open_mount(&plan.workspace).unwrap()).unwrap()),
        plan,
        handles: vec![],
    };
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let file = File::open(&path).unwrap();
    let user = crate::isolation::job_identity().unwrap();
    assert_eq!(
        unsafe { libc::fchown(file.as_raw_fd(), user.uid, user.gid) },
        0
    );
    assert!(unseal(&state, id)
        .unwrap_err()
        .to_string()
        .contains("root-owned"));
    assert!(path.exists());
}
