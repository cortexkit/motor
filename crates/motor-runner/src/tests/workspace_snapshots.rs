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
            .sync_workspace(&f.root, path, &head, &head_tree, &head)
            .unwrap();
    }
    trees
        .sync_workspace(&f.root, path, &snapshot, &tree, &head)
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
        .sync_workspace(&f.root, path, &snapshot, &tree, &head)
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
