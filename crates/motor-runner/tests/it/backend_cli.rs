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
