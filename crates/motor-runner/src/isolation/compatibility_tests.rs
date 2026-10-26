//! Pins sandbox argv and bind planning for unchanged caller paths.
use super::*;

fn assert_golden(actual: &str, expected: &str) {
    assert_eq!(actual, expected, "identity sandbox changed:\n{actual}");
}

#[test]
fn users_sandbox_argv_matches_pre_mapping_bytes() {
    let f = Fixture::new();
    let workspace = Path::new("/Users/u/repo");
    let mut sandbox = f.command_sandbox();
    sandbox.read_only = vec![
        "/usr".into(),
        "/etc/passwd".into(),
        "/Users/u/repo/.git".into(),
    ];
    sandbox.read_write = vec!["/Users/u/repo".into(), "/Users/u/repo/target".into()];
    let command = sandbox
        .command(
            "cargo test",
            &workspace.join("src"),
            &[("RUST_LOG".into(), "info".into())],
            true,
            &[1, 2],
            2,
        )
        .unwrap();
    let mut rendered = format!("{}\n", command.get_program().to_str().unwrap());
    for arg in command.get_args() {
        rendered.push_str(
            &arg.to_str()
                .unwrap()
                .replace(f.scratch.to_str().unwrap(), "<SCRATCH>")
                .replace(&sandbox.id.to_string(), "<UNIT_ID>"),
        );
        rendered.push('\n');
    }
    assert_golden(
        &rendered,
        include_str!("../../test-fixtures/compatibility/users-sandbox-argv.txt"),
    );
}

#[test]
fn users_sibling_bind_list_matches_pre_mapping_bytes() {
    let f = Fixture::new();
    let config = crate::engine::Config::new("/state-root", "main");
    let workspace = Path::new("/Users/u/repo");
    let siblings = [SiblingSnapshot {
        canonical_path: "/Users/u/libraries/library".into(),
        commit: "1111111111111111111111111111111111111111".into(),
        tree_hash: "2222222222222222222222222222222222222222".into(),
        base_commit: None,
    }];
    let snapshots = SnapshotSiblings {
        config: &config,
        siblings: vec![(
            PathBuf::from("/state-root/main/sibling-snapshots/library/s1"),
            PathBuf::from(&siblings[0].canonical_path),
        )],
    };
    let mut rendered = String::new();
    for (name, snapshots) in [("in-place", None), ("snapshot", Some(&snapshots))] {
        rendered.push_str(&format!("== {name}\n"));
        let mounts = sibling_mounts(workspace, &siblings, snapshots).unwrap();
        for (mount, destination) in &mounts {
            rendered.push_str(&format!("{mount} -> {}\n", destination.display()));
        }
        let gitconfig = f.scratch.join(format!("{name}.gitconfig"));
        write_safe_directories(&gitconfig, &safe_directories(workspace, &mounts)).unwrap();
        rendered.push_str(&fs::read_to_string(gitconfig).unwrap());
    }
    assert_golden(
        &rendered,
        include_str!("../../test-fixtures/compatibility/users-sibling-binds.txt"),
    );
}

#[test]
fn identity_prepared_bind_list_matches_pre_mapping_bytes() {
    let f = Fixture::new();
    fs::create_dir_all(f.workspace.join("src")).unwrap();
    fs::write(f.workspace.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        f.workspace.join("Cargo.toml"),
        "[package]\nname='identity-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        f.workspace.join("Cargo.lock"),
        "version = 3\n\n[[package]]\nname = 'identity-fixture'\nversion = '0.1.0'\n",
    )
    .unwrap();
    workspace::git(&f.workspace, &["init", "-q"]).unwrap();
    workspace::git(&f.workspace, &["add", "."]).unwrap();
    let tree = workspace::git(&f.workspace, &["write-tree"]).unwrap();
    let target = f.scratch.join("target");
    fs::create_dir_all(&target).unwrap();
    let sibling = f.scratch.join("libraries/library");
    fs::create_dir_all(&sibling).unwrap();
    fs::write(sibling.join("README.md"), "library\n").unwrap();
    workspace::git(&sibling, &["init", "-q"]).unwrap();
    workspace::git(&sibling, &["add", "."]).unwrap();
    let sibling_tree = workspace::git(&sibling, &["write-tree"]).unwrap();
    let siblings = [SiblingSnapshot {
        canonical_path: sibling.to_str().unwrap().into(),
        commit: "1111111111111111111111111111111111111111".into(),
        tree_hash: sibling_tree,
        base_commit: None,
    }];
    crate::systemd::with_process_hook(
        Box::new(|command| fake_privileged_output(command).unwrap_or_else(|| command.output())),
        || {
            let sandbox = Sandbox::prepare_role(
                Uuid::now_v7(),
                &f.state,
                &f.workspace,
                &target,
                &siblings,
                None,
                &f.state.join("slots"),
                1,
                &BTreeMap::new(),
                &tree,
                f.workspace.to_str().unwrap(),
                false,
                false,
                false,
                None,
                &JavascriptTools::for_role(ToolchainRole::BuildServer),
                None,
            )
            .unwrap();
            let cargo = std::env::var("CARGO_HOME").unwrap();
            let rustup = std::env::var("RUSTUP_HOME").unwrap();
            let normalize = |text: &str| {
                text.replace(sandbox.runtime.0.to_str().unwrap(), "<RUNTIME>")
                    .replace(f.scratch.to_str().unwrap(), "<SCRATCH>")
                    .replace(&cargo, "<CARGO_HOME>")
                    .replace(&rustup, "<RUSTUP_HOME>")
            };
            let mut rendered = String::new();
            for (name, mounts) in [
                ("read-only", &sandbox.read_only),
                ("read-write", &sandbox.read_write),
            ] {
                rendered.push_str(&format!("== {name}\n"));
                for mount in mounts {
                    rendered.push_str(&normalize(mount));
                    rendered.push('\n');
                }
            }
            rendered.push_str("== gitconfig\n");
            rendered.push_str(&normalize(
                &fs::read_to_string(sandbox.runtime.0.join("home/.gitconfig")).unwrap(),
            ));
            assert_golden(
                &rendered,
                include_str!("../../test-fixtures/compatibility/identity-bind-list.txt"),
            );
        },
    );
}
