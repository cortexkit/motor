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
        plan,
        handles: vec![Handle {
            path: path.clone(),
            kind: 1,
            bytes: vec![1, 2],
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
fn leftover_record_holds_workspace_and_ancestors_after_reload() {
    let scratch = ScratchDir::new("motor-seal-hold");
    let id = Uuid::now_v7();
    let record = Record {
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
