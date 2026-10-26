use cortexkit_test_support::ScratchDir;
use std::process::Command;

#[test]
fn vm_without_configuration_exits_with_an_actionable_message() {
    let scratch = ScratchDir::new("motor-vm-cli");
    let state = scratch.join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_motor-runner"))
        .args(["serve", "--backend", "vm", "--state-root"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--backend vm requires --vm-config <file>"),
        "{stderr}"
    );
    assert!(!state.exists());
}

#[test]
fn windows_prepare_process_emits_a_structured_step_failure() {
    let scratch = ScratchDir::new("windows-prep-cli");
    let staging = scratch.join("staging");
    std::fs::create_dir_all(staging.join("m/repository/Cargo.lock")).unwrap();
    let plan = staging.join("prepare.json");
    std::fs::write(
        &plan,
        serde_json::to_vec(&serde_json::json!({
            "staging": staging,
            "snapshots": [], "routes": {}, "workspace": "/repository",
            "tree_hash": "", "cargo": "", "rustup": "", "toolchain_bin": "", "toolchain_source": ""
        }))
        .unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_motor-runner"))
        .arg("windows-prepare")
        .arg(&plan)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let line = stderr
        .lines()
        .find_map(|line| line.strip_prefix("MOTOR_PREP_ERROR "))
        .expect("prep process must write the structured diagnostic to stderr");
    let failure: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(failure["step"], "validate manifests");
    assert_eq!(
        failure["reason"],
        "root Cargo.lock is not a tracked regular file"
    );
    assert!(!line.contains(staging.to_str().unwrap()), "{line}");
}

#[cfg(unix)]
#[test]
fn windows_prepare_logs_failed_nested_lockfiles_and_requires_the_root_fetch() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = ScratchDir::new("windows-nested-prep-cli");
    let repo = scratch.join("repo");
    for name in ["", "playground"] {
        let root = repo.join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "version = 3\n").unwrap();
    }
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "nested workspace",
    ]);
    let cargo = scratch.join("cargo");
    std::fs::write(&cargo, "#!/bin/sh\ncase \"$*\" in *playground/Cargo.toml*) echo 'private Cargo stderr' >&2; exit 1;; esac\nexit 0\n").unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    for root_fails in [false, true] {
        if root_fails {
            std::fs::write(&cargo, "#!/bin/sh\nexit 2\n").unwrap();
        }
        let staging = scratch.join(if root_fails {
            "root-fails"
        } else {
            "nested-fails"
        });
        std::fs::create_dir_all(&staging).unwrap();
        let plan = staging.join("prepare.json");
        std::fs::write(&plan, serde_json::to_vec(&serde_json::json!({
            "staging": staging,
            "snapshots": [{"mirror": repo.join(".git"), "commit": git(&["rev-parse", "HEAD"]), "destination": "/repo"}],
            "routes": {}, "workspace": "/repo", "tree_hash": git(&["rev-parse", "HEAD^{tree}"]),
            "cargo": cargo, "rustup": "", "toolchain_bin": "", "toolchain_source": ""
        })).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_motor-runner"))
            .arg("windows-prepare")
            .arg(&plan)
            .output()
            .unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if root_fails {
            assert!(!output.status.success());
            assert!(
                stderr.contains("MOTOR_PREP_ERROR {\"step\":\"fetch dependencies\""),
                "{stderr}"
            );
            assert!(!stderr.contains("MOTOR_PREP_WARNING"), "{stderr}");
        } else {
            assert!(output.status.success(), "{stderr}");
            assert!(stderr.contains("MOTOR_PREP_WARNING {\"step\":\"fetch nested lockfile m/repo/playground/Cargo.lock\""), "{stderr}");
            assert!(!stderr.contains("private Cargo stderr"), "{stderr}");
        }
        assert!(!stderr.contains(staging.to_str().unwrap()), "{stderr}");
    }
}
