use super::recovery::{Cut, Fixture};
use crate::snapshot;
use motor_protocol::Uuid;
use std::{fs, path::Path};

fn validate(f: &Fixture, key: &Path, root: &Path, cwd: &Path) -> Result<(), String> {
    snapshot::validate(
        &f.config,
        key.to_str().unwrap(),
        root.to_str().unwrap(),
        cwd.to_str().unwrap(),
    )
    .map_err(|e| e.to_string())
}

#[tokio::test]
async fn main_and_linked_workspaces_are_admitted_by_repository_identity() {
    let f = Fixture::new(Cut::None, 28).await;
    assert_eq!(validate(&f, &f.main, &f.main, &f.main), Ok(()));
    assert_eq!(validate(&f, &f.key, &f.main, &f.key), Ok(()));
    let outside = f.main.parent().unwrap().join("release-train");
    snapshot::git(
        &f.main,
        &["worktree", "add", "--detach", outside.to_str().unwrap()],
    )
    .unwrap();
    assert_eq!(validate(&f, &outside, &f.main, &outside), Ok(()));
    assert_eq!(
        validate(&f, &f.main, &f.sibling, &f.main).unwrap_err(),
        "not a linked worktree of the stated repository"
    );
    assert_eq!(
        validate(&f, &outside, &f.sibling, &outside).unwrap_err(),
        "not a linked worktree of the stated repository"
    );
    assert_eq!(
        validate(&f, &f.main, &f.main, &f.sibling).unwrap_err(),
        "cwd is outside workspace key"
    );
}

#[tokio::test]
async fn head_run_sends_head_kind_without_remapping_paths() {
    use super::recovery::{collect, terminal};
    use motor_protocol::*;
    let f = Fixture::new(Cut::None, 28).await;
    assert_eq!(
        snapshot::workspace_kind(&f.config, f.key.to_str().unwrap()).unwrap(),
        WorkspaceKind::Task
    );
    let motor = f.motor();
    let mut request = f.request("cat source");
    request.workspace_key = f.main.to_str().unwrap().into();
    request.cwd = request.workspace_key.clone();
    let records = collect(motor.run(request.clone()).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    let frames = f.proxy.frames.lock().unwrap();
    assert!(frames.iter().any(|frame| matches!(frame, Frame::Run(r) if r.workspace_kind == Some(WorkspaceKind::Head) && r.workspace_key == request.workspace_key && r.cwd == request.cwd)));
}

#[tokio::test]
async fn cwd_in_nested_checkouts_is_refused_before_snapshot() {
    use super::recovery::{collect, terminal};
    use motor_protocol::*;
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    for key in [&f.main, &f.key] {
        let plain = key.join("plain");
        fs::create_dir(&plain).unwrap();
        assert_eq!(validate(&f, key, &f.main, &plain), Ok(()));
        let linked = key.join("nested-linked");
        snapshot::git(
            &f.main,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        )
        .unwrap();
        let unrelated = key.join("nested-repository");
        fs::create_dir(&unrelated).unwrap();
        snapshot::git(&unrelated, &["init", "-q"]).unwrap();
        for cwd in [&linked, &unrelated] {
            let check = "cwd belongs to a different checkout than the workspace key";
            assert_eq!(validate(&f, key, &f.main, cwd).unwrap_err(), check);
            let mut request = f.request("true");
            request.workspace_key = key.to_str().unwrap().into();
            request.cwd = cwd.to_str().unwrap().into();
            let records = collect(motor.run(request).unwrap()).await;
            let t = terminal(&records);
            assert_eq!(
                t.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::WorkspaceKeyRejected
                }
            );
            assert_eq!(t.refusal_detail(), Some(check));
            assert_eq!(t.ran, Some(Ran::None));
            assert_eq!(t.tree_hash, None);
            assert!(f.proxy.frames.lock().unwrap().is_empty());
        }
    }
}

async fn assert_network_refused_before_capture_or_upload(network: motor_protocol::Network) {
    use super::recovery::{collect, terminal};
    use motor_protocol::*;

    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let spools = f.scratch.join("module-state/spools");
    assert!(!spools.exists());
    let request = f.request("true").with_network(network);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let records = collect(stream).await;
    assert_eq!(
        records.len(),
        1,
        "network requests must never be accepted or started"
    );
    let t = terminal(&records);
    assert_eq!(
        t.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::NetworkUnsupported,
        }
    );
    assert_eq!(
        t.refusal_detail(),
        Some("outbound network for remote jobs is not available yet; run without net")
    );
    assert_eq!(t.retry_after_ms(), None);
    assert_eq!(t.ran, Some(Ran::None));
    assert_eq!(t.tree_hash, None);
    assert_eq!(t.workspace_changes, None);
    assert_eq!((t.wall_ms, t.queue_wait_ms, t.bundle_bytes), (0, 0, 0));
    // If snapshot construction had begun, its spool parent would remain after cleanup.
    assert!(
        !spools.exists(),
        "network request started snapshot construction"
    );
    assert!(
        f.proxy.frames.lock().unwrap().is_empty(),
        "network request reached the runner"
    );
    assert!(f.jobs().is_empty());
    let replay = collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(replay, records);
    assert!(f.proxy.frames.lock().unwrap().is_empty());
}

#[tokio::test]
async fn outbound_network_is_refused_before_capture_or_upload() {
    assert_network_refused_before_capture_or_upload(motor_protocol::Network::Outbound).await;
}

#[tokio::test]
async fn unknown_network_is_refused_before_capture_or_upload() {
    assert_network_refused_before_capture_or_upload(motor_protocol::Network::Unknown("x".into()))
        .await;
}

#[tokio::test]
async fn oversized_untracked_content_is_refused_before_capture_or_upload() {
    use super::recovery::{collect, terminal};
    use motor_protocol::*;
    let f = Fixture::new(Cut::None, 28).await;
    let spool = f.scratch.join("size-preflight");
    fs::create_dir(&spool).unwrap();
    let db = f.key.join("store.db");
    let file = fs::File::create(&db).unwrap();
    // Keep the specimen size fixed even if the production budget changes.
    file.set_len(2_147_483_649).unwrap();
    // Check the metadata-only guard first so a regression cannot hash a huge fixture.
    let failure = snapshot::check_untracked_content(std::iter::once(f.key.as_path()), &spool)
        .expect_err("oversized sparse content must be rejected before reading it");
    assert!(failure
        .check
        .contains("untracked store.db is 2147483649 bytes"));
    assert!(failure.check.contains("the limit is 2147483648 bytes"));
    assert_eq!(fs::read_dir(&spool).unwrap().count(), 0);
    let motor = f.motor();
    let mut request = f.request("true");
    request.siblings.clear();
    let records = collect(motor.run(request).unwrap()).await;
    let t = terminal(&records);
    assert_eq!(
        t.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::SnapshotFailed
        }
    );
    assert_eq!(t.refusal_detail(), Some(failure.check.as_str()));
    assert_eq!(t.retry_after_ms(), None);
    assert_eq!(t.ran, Some(Ran::None));
    assert_eq!(t.tree_hash, None);
    assert!(f.proxy.frames.lock().unwrap().is_empty());

    file.set_len(1_073_741_825).unwrap();
    fs::File::create(f.sibling.join("second.db"))
        .unwrap()
        .set_len(1_073_741_825)
        .unwrap();
    let records = collect(motor.run(f.request("true")).unwrap()).await;
    let t = terminal(&records);
    assert_eq!(
        t.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::SnapshotFailed
        }
    );
    let detail = t.refusal_detail().unwrap();
    assert!(detail.contains("store.db is 1073741825 bytes"));
    assert!(detail.contains("second.db is 1073741825 bytes"));
    assert!(f.proxy.frames.lock().unwrap().is_empty());
}

#[tokio::test]
async fn untracked_size_preflight_excludes_ignored_files_and_symlink_targets() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(Cut::None, 28).await;
    let spool = f.scratch.join("size-preflight");
    fs::create_dir(&spool).unwrap();
    let ignored = f.key.join("ignored");
    fs::File::create(&ignored)
        .unwrap()
        .set_len(18_000_000_000)
        .unwrap();
    symlink(&ignored, f.key.join("small-link")).unwrap();
    fs::write(f.key.join("small-source"), "normal untracked content").unwrap();
    snapshot::check_untracked_content(std::iter::once(f.key.as_path()), &spool).unwrap();
    assert_eq!(fs::read_dir(&spool).unwrap().count(), 0);
}

#[tokio::test]
async fn subdirectories_cannot_pose_as_main_or_linked_roots() {
    let f = Fixture::new(Cut::None, 28).await;
    for key in [&f.main, &f.key] {
        let subdir = key.join("subdir");
        fs::create_dir(&subdir).unwrap();
        assert_eq!(
            validate(&f, &subdir, &f.main, &subdir).unwrap_err(),
            "key is not the worktree root"
        );
    }
}

#[tokio::test]
async fn changing_checkout_is_refused_after_three_capture_attempts() {
    let f = Fixture::new(Cut::None, 28).await;
    let spool = f.scratch.join("capture");
    fs::create_dir(&spool).unwrap();
    for key in [&f.main, &f.key] {
        let mut calls = 0;
        let result = snapshot::snapshot_with_hook(
            key,
            f.main.to_str().unwrap().into(),
            &spool,
            0,
            false,
            |attempt| {
                calls += 1;
                fs::write(key.join("source"), format!("state-{attempt}\n")).unwrap();
            },
        );
        let failure = result
            .err()
            .expect("a moving checkout must not ship a tree");
        assert_eq!(failure.check, "checkout changed during capture");
        assert_eq!(failure.tree_hash, None);
        assert_eq!(calls, 3);
        assert_eq!(fs::read_dir(&spool).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn one_checkout_change_retries_then_captures_the_quiet_state() {
    let f = Fixture::new(Cut::None, 28).await;
    let spool = f.scratch.join("capture");
    fs::create_dir(&spool).unwrap();
    for key in [&f.main, &f.key] {
        let mut calls = 0;
        let captured = snapshot::snapshot_with_hook(
            key,
            f.main.to_str().unwrap().into(),
            &spool,
            0,
            false,
            |attempt| {
                calls += 1;
                if attempt == 0 {
                    fs::write(key.join("source"), "quiet state\n").unwrap();
                    fs::write(key.join("new-file"), "new untracked state\n").unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(
            snapshot::git(key, &["show", &format!("{}:source", captured.commit)]).unwrap(),
            "quiet state"
        );
        assert_eq!(
            snapshot::git(key, &["show", &format!("{}:new-file", captured.commit)]).unwrap(),
            "new untracked state"
        );
    }
}

#[tokio::test]
async fn capture_fingerprint_detects_transient_untracked_files_and_head_changes() {
    let f = Fixture::new(Cut::None, 28).await;
    let spool = f.scratch.join("capture");
    fs::create_dir(&spool).unwrap();
    fs::create_dir_all(f.main.join("empty/nested")).unwrap();
    let mut calls = 0;
    let captured = snapshot::snapshot_with_hook(
        &f.main,
        f.main.to_str().unwrap().into(),
        &spool,
        0,
        false,
        |attempt| {
            calls += 1;
            if attempt == 0 {
                let transient = f.main.join("empty/nested/transient");
                fs::write(&transient, "transient").unwrap();
                fs::remove_file(transient).unwrap();
            } else if attempt == 1 {
                snapshot::git(
                    &f.main,
                    &[
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.invalid",
                        "commit",
                        "--allow-empty",
                        "-m",
                        "advance",
                    ],
                )
                .unwrap();
            }
        },
    )
    .unwrap();
    assert_eq!(calls, 3);
    assert_eq!(
        captured.head,
        snapshot::git(&f.main, &["rev-parse", "HEAD"]).unwrap()
    );
}

#[tokio::test]
async fn fingerprint_includes_ignored_ignore_and_attribute_files() {
    let f = Fixture::new(Cut::None, 28).await;
    snapshot::git(&f.main, &["rm", "--cached", ".gitignore"]).unwrap();
    snapshot::git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "remove ignore file from tree",
        ],
    )
    .unwrap();
    fs::write(f.main.join(".gitignore"), ".gitignore\n.gitattributes\n").unwrap();
    fs::write(f.main.join(".gitattributes"), "# no attributes\n").unwrap();
    let spool = f.scratch.join("capture");
    fs::create_dir(&spool).unwrap();
    let mut calls = 0;
    let _ = snapshot::snapshot_with_hook(
        &f.main,
        f.main.to_str().unwrap().into(),
        &spool,
        0,
        false,
        |attempt| {
            calls += 1;
            if attempt == 0 {
                fs::write(
                    f.main.join(".gitignore"),
                    ".gitignore\n.gitattributes\n# edited\n",
                )
                .unwrap();
            } else if attempt == 1 {
                fs::write(f.main.join(".gitattributes"), "# still no attributes\n").unwrap();
            }
        },
    )
    .unwrap();
    assert_eq!(calls, 3);
}

#[tokio::test]
async fn head_snapshot_leaves_head_branch_real_index_and_working_files_untouched() {
    let f = Fixture::new(Cut::None, 28).await;
    fs::write(f.main.join("source"), "staged\n").unwrap();
    snapshot::git(&f.main, &["add", "source"]).unwrap();
    fs::write(f.main.join("source"), "unstaged\n").unwrap();
    fs::write(f.main.join("new-file"), [0, 255, 8]).unwrap();
    let state = || {
        let index = snapshot::git(
            &f.main,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )
        .unwrap();
        (
            snapshot::git(&f.main, &["rev-parse", "HEAD"]).unwrap(),
            snapshot::git(&f.main, &["symbolic-ref", "HEAD"]).unwrap(),
            fs::read(index).unwrap(),
            fs::read(f.main.join("source")).unwrap(),
            fs::read(f.main.join("new-file")).unwrap(),
        )
    };
    let before = state();
    let captured = snapshot::Snapshots::build(
        &f.config,
        f.main.to_str().unwrap(),
        f.main.to_str().unwrap(),
        &[],
        &f.scratch.join("capture"),
        Uuid::now_v7(),
        None,
        &crate::storage::Storage::open(&f.scratch.join("capture-store")).unwrap(),
    )
    .unwrap();
    assert_eq!(before, state());
    assert_eq!(
        snapshot::git(
            &f.main,
            &["show", &format!("{}:source", captured.main.commit)]
        )
        .unwrap(),
        "unstaged"
    );
}

#[tokio::test]
async fn workspace_refusals_log_each_failed_check_without_changing_reason() {
    use super::recovery::{collect, terminal};
    use crate::logging::{scoped, Captured};
    use motor_protocol::*;
    let f = Fixture::new(Cut::None, 28).await;
    let captured = Captured::default();
    let _guard = scoped(captured.clone());
    let motor = f.motor();
    let subdir = f.main.join("subdir");
    let absent = f.main.join("absent");
    let nongit = f.scratch.join("nongit");
    fs::create_dir(&subdir).unwrap();
    fs::create_dir(&nongit).unwrap();
    let text = |p: &Path| p.to_str().unwrap().to_string();
    let cases = [
        (
            "relative".into(),
            text(&f.main),
            text(&f.main),
            "paths must be absolute",
        ),
        (
            text(&absent),
            text(&f.main),
            text(&f.main),
            "key cannot be canonicalized",
        ),
        (
            text(&f.main),
            text(&absent),
            text(&f.main),
            "repository_root cannot be canonicalized",
        ),
        (
            text(&f.main),
            text(&f.main),
            text(&absent),
            "cwd cannot be canonicalized",
        ),
        (
            text(&f.main),
            text(&f.main),
            text(&f.sibling),
            "cwd is outside workspace key",
        ),
        (
            text(&subdir),
            text(&f.main),
            text(&subdir),
            "key is not the worktree root",
        ),
        (
            text(&f.main),
            text(&subdir),
            text(&f.main),
            "repository_root is not a repository root",
        ),
        (
            text(&nongit),
            text(&f.main),
            text(&nongit),
            "key has no repository toplevel",
        ),
        (
            text(&f.main),
            text(&nongit),
            text(&f.main),
            "repository_root has no repository toplevel",
        ),
        (
            text(&f.key),
            text(&f.sibling),
            text(&f.key),
            "not a linked worktree of the stated repository",
        ),
    ];
    for (key, root, cwd, check) in cases {
        let mut request = f.request("private-command-never-logged");
        request.workspace_key = key.clone();
        request.repository_root = root.clone();
        request.cwd = cwd.clone();
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let records = collect(stream).await;
        assert_eq!(terminal(&records).refusal_detail(), Some(check));
        assert_eq!(terminal(&records).retry_after_ms(), None);
        assert_eq!(
            terminal(&records).outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::WorkspaceKeyRejected
            }
        );
        let logs = captured.text();
        let line = logs.lines().last().unwrap();
        assert!(line.contains("WARN"));
        assert!(line.contains(&format!("job_id={id}")));
        assert!(line.contains(&format!("check={check}")), "{line}");
    }
    assert!(!captured.text().contains("private-command-never-logged"));
    assert!(f.proxy.frames.lock().unwrap().is_empty());
}

#[tokio::test]
async fn untracked_linked_worktree_and_nested_repo_are_excluded_from_snapshot_tree() {
    let f = Fixture::new(Cut::None, 28).await;
    let nested_wt = f.main.join("nested-worktree");
    snapshot::git(
        &f.main,
        &["worktree", "add", "--detach", nested_wt.to_str().unwrap()],
    )
    .unwrap();

    let nested_empty = f.main.join("nested-empty");
    fs::create_dir_all(&nested_empty).unwrap();
    snapshot::git(&nested_empty, &["init", "-q"]).unwrap();

    let nested_committed = f.main.join("nested-committed");
    fs::create_dir_all(&nested_committed).unwrap();
    snapshot::git(&nested_committed, &["init", "-q"]).unwrap();
    snapshot::git(&nested_committed, &["config", "user.name", "Test"]).unwrap();
    snapshot::git(
        &nested_committed,
        &["config", "user.email", "test@example.invalid"],
    )
    .unwrap();
    fs::write(nested_committed.join("file"), "content\n").unwrap();
    snapshot::git(&nested_committed, &["add", "file"]).unwrap();
    snapshot::git(&nested_committed, &["commit", "-m", "initial"]).unwrap();

    let spool = f.scratch.join("capture");
    fs::create_dir_all(&spool).unwrap();
    let captured = snapshot::snapshot_with_hook(
        &f.main,
        f.main.to_str().unwrap().into(),
        &spool,
        0,
        false,
        |_| {},
    )
    .unwrap();

    let tree_entries =
        snapshot::git(&f.main, &["ls-tree", "-r", "--name-only", &captured.commit]).unwrap();
    let names: Vec<&str> = tree_entries.lines().collect();
    assert!(
        !names.contains(&"nested-worktree"),
        "snapshot tree must not contain untracked worktree gitlink"
    );
    assert!(
        !names.contains(&"nested-empty"),
        "snapshot tree must not contain untracked empty nested repository"
    );
    assert!(
        !names.contains(&"nested-committed"),
        "snapshot tree must not contain untracked committed nested repository"
    );

    let runner_tree = f.runner.tree_hash(&f.main).unwrap();
    assert_eq!(captured.tree_hash, runner_tree);
}

#[tokio::test]
async fn head_workspace_job_with_untracked_worktree_and_nested_repo_runs() {
    use super::recovery::{collect, terminal};
    use motor_protocol::*;
    let f = Fixture::new(Cut::None, 28).await;

    let nested_wt = f.main.join("nested-worktree");
    snapshot::git(
        &f.main,
        &["worktree", "add", "--detach", nested_wt.to_str().unwrap()],
    )
    .unwrap();

    let nested_empty = f.main.join("nested-empty");
    fs::create_dir_all(&nested_empty).unwrap();
    snapshot::git(&nested_empty, &["init", "-q"]).unwrap();

    let nested_committed = f.main.join("nested-committed");
    fs::create_dir_all(&nested_committed).unwrap();
    snapshot::git(&nested_committed, &["init", "-q"]).unwrap();
    snapshot::git(&nested_committed, &["config", "user.name", "Test"]).unwrap();
    snapshot::git(
        &nested_committed,
        &["config", "user.email", "test@example.invalid"],
    )
    .unwrap();
    fs::write(nested_committed.join("file"), "content\n").unwrap();
    snapshot::git(&nested_committed, &["add", "file"]).unwrap();
    snapshot::git(&nested_committed, &["commit", "-m", "initial"]).unwrap();

    let motor = f.motor();
    let mut request = f.request("cat source");
    request.workspace_key = f.main.to_str().unwrap().into();
    request.cwd = request.workspace_key.clone();
    let records = collect(motor.run(request).unwrap()).await;
    let t = terminal(&records);
    assert_eq!(
        t.outcome,
        Outcome::Exit { code: 0 },
        "head workspace job from checkout containing nested worktree/repo must run, got: {t:?}"
    );
}

#[tokio::test]
async fn tracked_submodule_gitlink_is_preserved_in_snapshot() {
    let f = Fixture::new(Cut::None, 28).await;

    let sub_origin = f.scratch.join("sub-origin");
    fs::create_dir_all(&sub_origin).unwrap();
    snapshot::git(&sub_origin, &["init", "-q"]).unwrap();
    snapshot::git(&sub_origin, &["config", "user.name", "Test"]).unwrap();
    snapshot::git(
        &sub_origin,
        &["config", "user.email", "test@example.invalid"],
    )
    .unwrap();
    fs::write(sub_origin.join("sub-file"), "sub-file content\n").unwrap();
    snapshot::git(&sub_origin, &["add", "sub-file"]).unwrap();
    snapshot::git(&sub_origin, &["commit", "-m", "sub initial"]).unwrap();

    snapshot::git(
        &f.main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_origin.to_str().unwrap(),
            "tracked-sub",
        ],
    )
    .unwrap();
    snapshot::git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "add tracked submodule",
        ],
    )
    .unwrap();

    let spool = f.scratch.join("capture");
    fs::create_dir_all(&spool).unwrap();
    let captured = snapshot::snapshot_with_hook(
        &f.main,
        f.main.to_str().unwrap().into(),
        &spool,
        0,
        false,
        |_| {},
    )
    .unwrap();

    let tree_entries = snapshot::git(&f.main, &["ls-tree", "-r", "-z", &captured.commit]).unwrap();
    assert!(
        tree_entries
            .split('\0')
            .any(|e| e.starts_with("160000 commit") && e.ends_with("\ttracked-sub")),
        "tracked submodule gitlink must be preserved in snapshot tree: {tree_entries}"
    );

    let runner_tree = f.runner.tree_hash(&f.main).unwrap();
    assert_eq!(captured.tree_hash, runner_tree);
}

#[tokio::test]
async fn nested_repo_internals_are_not_counted_as_capture_churn() {
    let f = Fixture::new(Cut::None, 28).await;

    let nested_wt = f.main.join("nested-worktree");
    snapshot::git(
        &f.main,
        &["worktree", "add", "--detach", nested_wt.to_str().unwrap()],
    )
    .unwrap();

    let nested_repo = f.main.join("nested-repo");
    fs::create_dir_all(&nested_repo).unwrap();
    snapshot::git(&nested_repo, &["init", "-q"]).unwrap();
    snapshot::git(&nested_repo, &["config", "user.name", "Test"]).unwrap();
    snapshot::git(
        &nested_repo,
        &["config", "user.email", "test@example.invalid"],
    )
    .unwrap();
    fs::write(nested_repo.join("nested-file"), "nested content\n").unwrap();
    snapshot::git(&nested_repo, &["add", "nested-file"]).unwrap();
    snapshot::git(&nested_repo, &["commit", "-m", "nested initial"]).unwrap();

    let spool = f.scratch.join("capture");
    fs::create_dir_all(&spool).unwrap();
    let mut calls = 0;
    let captured = snapshot::snapshot_with_hook(
        &f.main,
        f.main.to_str().unwrap().into(),
        &spool,
        0,
        false,
        |attempt| {
            calls += 1;
            fs::write(nested_wt.join("wt-churn"), format!("churn-{attempt}")).unwrap();
            fs::write(nested_repo.join("repo-churn"), format!("churn-{attempt}")).unwrap();
            let _ = snapshot::git(&nested_repo, &["add", "repo-churn"]);
            let _ = snapshot::git(
                &nested_repo,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-m",
                    "nested churn",
                ],
            );
        },
    )
    .unwrap();

    assert_eq!(calls, 1);
    assert_eq!(captured.tree_hash, f.runner.tree_hash(&f.main).unwrap());
}
