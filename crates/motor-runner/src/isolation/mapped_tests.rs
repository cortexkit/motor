//! Sandbox planning for a job whose checkout the runner stores under a caller
//! root's host directory H: every job-derived bind has its host source under H
//! and its destination at the caller path, and nothing the unit is told
//! (destinations, working directory, environment, `.gitconfig`) names H.
use super::*;

const CALLER_WORKSPACE: &str = "/home/u/proj";
const CALLER_SIBLING: &str = "/home/v/library";

/// The destination of one systemd bind entry: `word`, `source:destination`
/// or `-source:destination`. `word` escapes `:`, so the first one separates
/// the two paths.
fn destination(entry: &str) -> &str {
    let entry = entry.strip_prefix('-').unwrap_or(entry);
    entry
        .split_once(':')
        .map_or(entry, |(_, destination)| destination)
}

fn cargo_workspace(path: &Path, name: &str) -> String {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(path.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        path.join("Cargo.toml"),
        format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n"),
    )
    .unwrap();
    fs::write(
        path.join("Cargo.lock"),
        format!("version = 3\n\n[[package]]\nname = '{name}'\nversion = '0.1.0'\n"),
    )
    .unwrap();
    workspace::git(path, &["init", "-q"]).unwrap();
    workspace::git(path, &["add", "."]).unwrap();
    workspace::git(path, &["write-tree"]).unwrap()
}

fn assert_no_host_destination(sandbox: &Sandbox, host_root: &Path) {
    let host = word(host_root).unwrap();
    for entry in sandbox.read_only.iter().chain(&sandbox.read_write) {
        assert!(
            !destination(entry).starts_with(&host),
            "{entry} is destined under H"
        );
    }
}

/// The unit argv for `cwd`, the caller form a mapped job is launched at.
fn unit_argv(sandbox: &Sandbox, cwd: &Path, env: &[(String, String)]) -> Vec<String> {
    sandbox
        .command("true", cwd, env, true, &[], 2)
        .unwrap()
        .get_args()
        .map(|arg| arg.to_str().unwrap().to_owned())
        .collect()
}

/// H may appear in the argv only as a bind source: never as a destination,
/// the working directory, an environment value or anywhere else.
fn assert_argv_hides_h(argv: &[String], host_root: &Path) {
    let h = host_root.to_str().unwrap();
    for arg in argv {
        let mounts = arg
            .strip_prefix("--property=BindReadOnlyPaths=")
            .or_else(|| arg.strip_prefix("--property=BindPaths="));
        match mounts {
            Some(mounts) => {
                for mount in mounts.split_whitespace() {
                    assert!(!destination(mount).starts_with(h), "{mount}");
                    // A bind whose source is under H is paired with a
                    // destination of its own.
                    if mount.trim_start_matches('-').starts_with(h) {
                        assert!(mount.contains(':'), "{mount}");
                    }
                }
            }
            None => assert!(!arg.contains(h), "{arg}"),
        }
    }
}

#[test]
fn mapped_job_binds_host_trees_at_caller_paths_and_never_names_h() {
    let f = Fixture::new();
    let host_root = f.scratch.join("callers/home");
    let host_workspace = host_root.join("u/proj");
    fs::create_dir_all(&host_workspace).unwrap();
    fs::write(host_workspace.join("README.md"), "mapped\n").unwrap();
    workspace::git(&host_workspace, &["init", "-q"]).unwrap();
    workspace::git(&host_workspace, &["add", "."]).unwrap();
    let tree = workspace::git(&host_workspace, &["write-tree"]).unwrap();
    let target = host_workspace.join("target");
    fs::create_dir_all(&target).unwrap();
    let snapshot = f.state.join("sibling-snapshots/library/s1");
    fs::create_dir_all(&snapshot).unwrap();
    let siblings = [SiblingSnapshot {
        canonical_path: CALLER_SIBLING.into(),
        commit: "1111111111111111111111111111111111111111".into(),
        tree_hash: "2222222222222222222222222222222222222222".into(),
        base_commit: None,
    }];
    let config = crate::Config::new(&f.scratch, "main");
    let snapshots = SnapshotSiblings {
        config: &config,
        siblings: vec![(snapshot.clone(), PathBuf::from(CALLER_SIBLING))],
    };
    let caller = Path::new(CALLER_WORKSPACE);
    super::compatibility::with_capture_host(|| {
        let sandbox = Sandbox::prepare_role(
            Uuid::now_v7(),
            &f.state,
            crate::caller_path::JobTree::new(&host_workspace, caller),
            &target,
            &siblings,
            None,
            &f.state.join("slots"),
            1,
            &BTreeMap::new(),
            &tree,
            CALLER_WORKSPACE,
            false,
            false,
            false,
            Some(&snapshots),
            &JavascriptTools::for_role(ToolchainRole::BuildServer),
            None,
        )
        .unwrap();
        let w = |path: &Path| word(path).unwrap();
        for expected in [
            bind(&host_workspace, caller).unwrap(),
            bind(&target, &caller.join("target")).unwrap(),
        ] {
            assert!(
                sandbox.read_write.contains(&expected),
                "{expected} missing from {:?}",
                sandbox.read_write
            );
        }
        for expected in [
            bind(&host_workspace.join(".git"), &caller.join(".git")).unwrap(),
            bind(&snapshot, Path::new(CALLER_SIBLING)).unwrap(),
            // `../library` from the workspace resolves beside the caller
            // workspace, not beside its host checkout.
            bind(&snapshot, Path::new("/home/u/library")).unwrap(),
        ] {
            assert!(
                sandbox.read_only.contains(&expected),
                "{expected} missing from {:?}",
                sandbox.read_only
            );
        }
        assert_no_host_destination(&sandbox, &host_root);
        // The job's private root carries the caller ancestors, never H's.
        assert!(sandbox.root.join("home/u/proj").is_dir());
        assert!(!sandbox
            .root
            .join(host_root.strip_prefix("/").unwrap())
            .exists());
        let gitconfig = fs::read_to_string(sandbox.runtime.0.join("home/.gitconfig")).unwrap();
        for directory in [CALLER_WORKSPACE, CALLER_SIBLING, "/home/u/library"] {
            assert!(
                gitconfig.contains(&format!("directory = {directory}\n")),
                "{gitconfig}"
            );
        }
        assert!(
            !gitconfig.contains(host_root.to_str().unwrap()),
            "{gitconfig}"
        );
        let cwd = caller.join("src");
        let argv = unit_argv(
            &sandbox,
            &cwd,
            &[(
                "CARGO_TARGET_DIR".into(),
                caller.join("target").to_string_lossy().into_owned(),
            )],
        );
        assert!(
            argv.contains(&format!("--property=WorkingDirectory={}", w(&cwd))),
            "{argv:?}"
        );
        assert_argv_hides_h(&argv, &host_root);
    });
}

#[test]
fn mapped_writer_fill_binds_its_target_inside_the_caller_checkout() {
    let f = Fixture::new();
    let host_root = f.scratch.join("callers/home");
    let host_checkout = host_root.join("u/.motor-base-main-repo");
    let caller = Path::new("/home/u/.motor-base-main-repo");
    let tree = cargo_workspace(&host_checkout, "mapped-writer-fixture");
    // A fill builds in its writer scratch, which lives in the runner state.
    let target = f.state.join("writer-scratch/fill");
    fs::create_dir_all(&target).unwrap();
    super::compatibility::with_capture_host(|| {
        let sandbox = Sandbox::prepare_writer(
            Uuid::now_v7(),
            &f.state,
            crate::caller_path::JobTree::new(&host_checkout, caller),
            &target,
            None,
            &f.state.join("slots"),
            1,
            &BTreeMap::new(),
            &tree,
            &JavascriptTools::for_role(ToolchainRole::BuildServer),
            false,
            false,
            true,
            None,
        )
        .unwrap();
        for expected in [
            bind(&host_checkout, caller).unwrap(),
            word(&target).unwrap(),
            bind(&target, &caller.join("target")).unwrap(),
        ] {
            assert!(
                sandbox.read_write.contains(&expected),
                "{expected} missing from {:?}",
                sandbox.read_write
            );
        }
        assert!(sandbox
            .read_write
            .contains(&bind(&target, Path::new(crate::compile_cache::TARGET)).unwrap()));
        assert_no_host_destination(&sandbox, &host_root);
        let gitconfig = fs::read_to_string(sandbox.runtime.0.join("home/.gitconfig")).unwrap();
        assert!(
            !gitconfig.contains(host_root.to_str().unwrap()),
            "{gitconfig}"
        );
        assert_argv_hides_h(&unit_argv(&sandbox, caller, &[]), &host_root);
    });
}

#[test]
fn identity_job_tree_keeps_single_word_mounts() {
    let tree = crate::caller_path::JobTree::from(Path::new("/Users/u/repo"));
    assert_eq!(
        tree.caller_form(Path::new("/Users/u/repo/target")),
        Path::new("/Users/u/repo/target")
    );
    assert_eq!(
        mount(Path::new("/Users/u/repo"), Path::new("/Users/u/repo")).unwrap(),
        "/Users/u/repo"
    );
    let mapped = crate::caller_path::JobTree::new(Path::new("/srv/h/u/p"), Path::new("/home/u/p"));
    assert_eq!(
        mapped.caller_form(Path::new("/srv/h/u/p/target")),
        Path::new("/home/u/p/target")
    );
    assert_eq!(
        mapped.caller_form(Path::new("/srv/h/u/p")),
        Path::new("/home/u/p")
    );
    // A path outside the tree, such as a writer's scratch, keeps its place.
    assert_eq!(
        mapped.caller_form(Path::new("/state/scratch")),
        Path::new("/state/scratch")
    );
}
