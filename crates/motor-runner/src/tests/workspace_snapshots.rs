use super::*;

const INPUTS: [&str; 3] = ["inputs/a space.tgz", "inputs/b*.tgz", "inputs/c\nline.tgz"];

fn ignored_inputs(path: &Path) {
    fs::create_dir_all(path.join("inputs")).unwrap();
    for name in INPUTS {
        fs::write(path.join(name), name).unwrap();
        git(path, &["--literal-pathspecs", "add", "-f", "--", name]);
    }
}

#[test]
fn tree_hash_includes_force_added_ignored_inputs_from_the_real_index() {
    let f = Fixture::new();
    fs::write(f.main.join(".gitignore"), "inputs/\n").unwrap();
    let (_, head_tree) = commit(&f.main, "first\n");
    ignored_inputs(&f.main);
    let expected = git(&f.main, &["write-tree"]);
    assert_ne!(expected, head_tree);
    // This untracked file must not be selected by the staged wildcard name.
    fs::write(f.main.join("inputs/b-other.tgz"), "not staged").unwrap();
    fs::write(f.main.join("inputs/deleted.tgz"), "deleted").unwrap();
    git(&f.main, &["add", "-f", "inputs/deleted.tgz"]);
    fs::remove_file(f.main.join("inputs/deleted.tgz")).unwrap();
    let runner = Runner::open(f.config()).unwrap();
    assert_eq!(runner.core.trees.tree_hash(&f.main).unwrap(), expected);
}

#[test]
fn staged_target_paths_never_enter_the_tree_hash() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    for ignore in ["", "target/\n"] {
        fs::write(f.main.join(".gitignore"), ignore).unwrap();
        let (_, head_tree) = commit(&f.main, "first\n");
        fs::create_dir_all(f.main.join("target")).unwrap();
        fs::write(f.main.join("target/staged-artifact"), "runner owned").unwrap();
        git(&f.main, &["add", "-f", "target/staged-artifact"]);
        assert_ne!(git(&f.main, &["write-tree"]), head_tree);
        assert_eq!(runner.core.trees.tree_hash(&f.main).unwrap(), head_tree);
        git(&f.main, &["reset", "HEAD", "--", "target/staged-artifact"]);
        fs::remove_dir_all(f.main.join("target")).unwrap();
    }
}

#[test]
fn committed_top_level_target_is_excluded_but_nested_target_is_captured() {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("target")).unwrap();
    fs::create_dir_all(f.main.join("src/target")).unwrap();
    fs::write(f.main.join("target/committed"), "build output").unwrap();
    let (_, head_tree) = commit(&f.main, "first\n");
    git(&f.main, &["rm", "--cached", "target/committed"]);
    let expected = git(&f.main, &["write-tree"]);
    fs::write(f.main.join("target/committed"), "changed output").unwrap();
    fs::write(f.main.join("target/untracked"), "untracked output").unwrap();
    let runner = Runner::open(f.config()).unwrap();
    assert_ne!(expected, head_tree);
    assert_eq!(runner.core.trees.tree_hash(&f.main).unwrap(), expected);
    fs::write(f.main.join("src/target/input"), "source input").unwrap();
    git(&f.main, &["add", "src/target/input"]);
    let nested = git(&f.main, &["write-tree"]);
    assert_ne!(nested, expected);
    assert_eq!(runner.core.trees.tree_hash(&f.main).unwrap(), nested);
}

#[tokio::test]
async fn mismatch_refusal_names_computed_tree_and_bounded_differing_paths() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    for n in 0..7 {
        let name = format!("different-{n}-secret-mismatch-value");
        fs::write(f.main.join(&name), "different").unwrap();
    }
    let (_, expected) = commit(&f.main, "first\n");
    for n in 0..7 {
        git(
            &f.main,
            &["rm", &format!("different-{n}-secret-mismatch-value")],
        );
    }
    commit(&f.main, "second\n");
    let (mut request, bytes) = f.run("touch marker");
    let computed = request.tree_hash.clone();
    request.tree_hash = expected;
    request
        .env
        .insert("LANG".into(), "secret-mismatch-value".into());
    f.submit(&runner, &request, &bytes);
    let record = terminal(&runner, request.job_id).await;
    assert_eq!(
        record.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TreeHashMismatch
        }
    );
    let detail = record.refusal_detail().expect("missing mismatch detail");
    assert!(detail.contains(&computed), "{detail}");
    assert!(detail.contains("different-0"), "{detail}");
    assert!(detail.contains("different-4"), "{detail}");
    assert!(!detail.contains("different-5"), "{detail}");
    assert!(detail.contains("3 more"), "{detail}");
    assert!(detail.contains("runner environment"), "{detail}");
    assert!(!detail.contains("secret-mismatch-value"), "{detail}");
    assert!(detail.len() <= REFUSAL_DETAIL_MAX_BYTES);
    assert!(!Path::new(&f.key).join("marker").exists());
}

#[test]
fn mismatch_detail_redacts_whole_paths_before_truncation() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let computed = git(&f.main, &["rev-parse", "HEAD^{tree}"]);
    let secret = format!("{}\nprivate", "secret-".repeat(20));
    let prefix = "p".repeat(80);
    for n in 0..7 {
        let name = format!("{n}-{prefix}-{secret}");
        fs::write(f.main.join(&name), "different").unwrap();
        git(&f.main, &["add", &name]);
    }
    let expected = git(&f.main, &["write-tree"]);
    let error = Trees::tree_hash_mismatch(&f.main, &expected, &computed);
    let bounded = runner.safe_refusal_detail(&error, []);
    assert_eq!(bounded.len(), REFUSAL_DETAIL_MAX_BYTES);
    assert!(bounded.contains("2 more"), "{bounded}");
    let detail = runner.safe_refusal_detail(&error, [secret]);
    assert!(detail.contains(&computed), "{detail}");
    assert!(detail.contains("2 more"), "{detail}");
    assert!(detail.contains("runner environment"), "{detail}");
    assert!(!detail.contains("secret-"), "{detail}");
    assert!(!detail.contains("private"), "{detail}");
    assert!(!detail.contains(&format!("5-{prefix}")), "{detail}");
    assert!(detail.len() <= REFUSAL_DETAIL_MAX_BYTES);
}

fn sync_stages_ignored_snapshot_inputs(reuse: bool) {
    let f = Fixture::new();
    fs::write(f.main.join(".gitignore"), "inputs/\n").unwrap();
    let (head, head_tree) = commit(&f.main, "first\n");
    ignored_inputs(&f.main);
    let tree = git(&f.main, &["write-tree"]);
    let snapshot = git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            "snapshot",
        ],
    );
    let runner = Runner::open(f.config()).unwrap();
    let trees = &runner.core.trees;
    let mirror = trees.ensure_mirror(&f.root).unwrap();
    git(&mirror, &["fetch", text(&f.main), &snapshot]);
    let path = Path::new(&f.key);
    if reuse {
        trees
            .sync_workspace(
                &f.root,
                crate::caller_path::host(path),
                &head,
                &head_tree,
                &head,
            )
            .unwrap();
    }
    trees
        .sync_workspace(
            &f.root,
            crate::caller_path::host(path),
            &snapshot,
            &tree,
            &head,
        )
        .unwrap();
    assert_eq!(git(path, &["rev-parse", "HEAD"]), head);
    let staged = git(path, &["diff", "--cached", "--name-only", "-z"]);
    let names: Vec<_> = staged.split('\0').filter(|name| !name.is_empty()).collect();
    assert_eq!(names, INPUTS);
    for name in INPUTS {
        assert_eq!(fs::read_to_string(path.join(name)).unwrap(), name);
    }
    assert_eq!(trees.tree_hash(path).unwrap(), tree);
    // A later run must retain staged inputs even when no checkout is needed.
    trees
        .sync_workspace(
            &f.root,
            crate::caller_path::host(path),
            &snapshot,
            &tree,
            &head,
        )
        .unwrap();
    assert_eq!(
        git(path, &["diff", "--cached", "--name-only", "-z"]),
        staged
    );
}

#[test]
fn sync_workspace_stages_ignored_snapshot_inputs_on_fresh_clone() {
    sync_stages_ignored_snapshot_inputs(false);
}

#[test]
fn sync_workspace_stages_ignored_snapshot_inputs_on_reuse() {
    sync_stages_ignored_snapshot_inputs(true);
}

#[tokio::test]
async fn staged_renames_preserve_destination_paths_and_finish_ownership() {
    let f = Fixture::new();
    for source in ["ascii-file.rs", "abé-file.rs", "é", "a"] {
        fs::write(f.main.join(source), "rename content\n").unwrap();
    }
    git(&f.main, &["add", "."]);
    git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "rename sources",
        ],
    );
    let mut config = f.config();
    config.disable_warm_builds = true;
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("git mv ascii-file.rs ascii-dest.rs; git mv abé-file.rs unicode-dest.rs; git mv é short-unicode.rs; git mv a short-ascii.rs");
    f.submit(&runner, &request, &bytes);
    let result = terminal(&runner, request.job_id).await;
    assert_eq!(result.outcome, Outcome::Exit { code: 0 });
    let mut changes = result.workspace_changes.unwrap();
    changes.sort();
    assert_eq!(
        changes,
        [
            "ascii-dest.rs",
            "short-ascii.rs",
            "short-unicode.rs",
            "unicode-dest.rs"
        ]
    );
    let inner = runner.core.inner.lock().unwrap();
    assert!(!inner.finalizing.contains(&request.job_id));
    assert!(!Runner::occupied(&inner, &request.workspace_key));
}
