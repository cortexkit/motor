//! Pins sandbox argv and bind planning for unchanged caller paths.
use super::*;
use crate::compatibility_config::{assert_identity, configure, modes};

// Keep the original capture's shared Cargo home equal to the sandbox's private
// Cargo home. Both sides of each registry/git bind must retain that spelling.
const CAPTURE_CARGO_HOME: &str = "/motor-home/cargo";
const CAPTURE_RUSTUP_HOME: &str = "/fixture-rustup";

pub(super) fn with_capture_host<T>(f: impl FnOnce() -> T) -> T {
    use std::os::unix::process::ExitStatusExt;
    let inputs = host_inputs::fixture::Inputs {
        environment: BTreeMap::from([
            ("CARGO_HOME", CAPTURE_CARGO_HOME.into()),
            ("RUSTUP_HOME", CAPTURE_RUSTUP_HOME.into()),
        ]),
        existing: [
            "/usr",
            "/bin",
            "/lib",
            "/lib64",
            "/etc/ld.so.cache",
            "/etc/passwd",
            "/etc/group",
            "/etc/nsswitch.conf",
            "/etc/alternatives",
            "/etc/ssl/certs",
            "/etc/ssl/openssl.cnf",
        ]
        .into_iter()
        .map(|path| (PathBuf::from(path), true))
        .collect(),
        directories: [
            "/home/ubuntu/.bun/bin",
            "/motor-home/cargo/registry",
            "/motor-home/cargo/git",
        ]
        .into_iter()
        .map(|path| (PathBuf::from(path), true))
        .collect(),
    };
    host_inputs::fixture::with(inputs, || {
        crate::systemd::with_process_hook(
            Box::new(|command| {
                // Dependency fetching is not mount planning. Do not run the
                // machine's Cargo against the fixture's synthetic toolchains.
                if command.get_program() == "cargo" {
                    assert!(command.get_args().any(|arg| arg == "fetch"));
                    return Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
                fake_privileged_output(command).unwrap_or_else(|| command.output())
            }),
            f,
        )
    })
}

fn assert_golden(actual: &str, expected: &str) {
    assert_eq!(actual, expected, "identity sandbox changed:\n{actual}");
}

#[test]
fn users_sandbox_argv_matches_pre_mapping_bytes() {
    for &enabled in modes() {
        users_sandbox_argv_case(enabled);
    }
}

fn users_sandbox_argv_case(enabled: bool) {
    let f = Fixture::new();
    let mut config = crate::Config::new("/state-root", "main");
    configure(&mut config, &f.scratch, enabled);
    let spelling = "/Users/u/repo".to_string();
    let plan = crate::caller_path::identity(&spelling);
    assert_identity(&config, plan);
    let workspace = plan.host.as_path();
    let mut sandbox = f.command_sandbox();
    sandbox.tenant_vm = config.tenant_vm;
    sandbox.boat_profile = config.boat_profile;
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
    for &enabled in modes() {
        users_sibling_bind_list_case(enabled);
    }
}

fn users_sibling_bind_list_case(enabled: bool) {
    let f = Fixture::new();
    let mut config = crate::engine::Config::new("/state-root", "main");
    configure(&mut config, &f.scratch, enabled);
    let spelling = "/Users/u/repo".to_string();
    let plan = crate::caller_path::identity(&spelling);
    assert_identity(&config, plan);
    let workspace = plan.host.as_path();
    let siblings = [SiblingSnapshot {
        canonical_path: "/Users/u/libraries/library".into(),
        commit: "1111111111111111111111111111111111111111".into(),
        tree_hash: "2222222222222222222222222222222222222222".into(),
        base_commit: None,
    }];
    assert_identity(&config, crate::caller_path::sibling(&siblings[0]));
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
    for &enabled in modes() {
        identity_prepared_bind_list_case(enabled);
    }
}

fn identity_prepared_bind_list_case(enabled: bool) {
    let f = Fixture::new();
    let mut config = crate::Config::new(
        f.state.parent().unwrap(),
        f.state.file_name().unwrap().to_str().unwrap(),
    );
    config.build_slot_root = f.state.join("slots");
    configure(&mut config, &f.scratch, enabled);
    let spelling = f.workspace.to_str().unwrap().to_string();
    let plan = crate::caller_path::identity(&spelling);
    assert_identity(&config, plan);
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
    with_capture_host(|| {
        let sandbox = Sandbox::prepare_role(
            Uuid::now_v7(),
            &config.state_dir(),
            plan.host.as_path(),
            &target,
            &siblings,
            None,
            &config.build_slot_root,
            1,
            &BTreeMap::new(),
            &tree,
            plan.caller.text(),
            false,
            config.tenant_vm,
            false,
            None,
            &JavascriptTools::for_config(&config),
            None,
        )
        .unwrap();
        let normalize = |text: &str| {
            text.replace(sandbox.runtime.0.to_str().unwrap(), "<RUNTIME>")
                .replace(f.scratch.to_str().unwrap(), "<SCRATCH>")
                .replace(CAPTURE_CARGO_HOME, "<CARGO_HOME>")
                .replace(CAPTURE_RUSTUP_HOME, "<RUSTUP_HOME>")
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
    });
}
