use std::{path::Path, process::Command};

#[test]
fn cli_commands_do_not_write_the_module_backup_catalog() {
    let scratch = cortexkit_test_support::ScratchDir::new("ck-motor-engram-cli");
    let data_home = scratch.path().to_path_buf();
    let catalog = data_home.join("cortexkit/ck-motor/engram-catalog.json");
    let ck_motor = cortexkit_test_support::ckdev_binary(Path::new(env!("CARGO_BIN_EXE_ck-motor")));
    for args in [
        vec!["--manifest"],
        vec!["run", "--help"],
        vec!["attach", "--help"],
    ] {
        let output = Command::new(&ck_motor)
            .args(&args)
            .env("XDG_DATA_HOME", &data_home)
            .env("HOME", &data_home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!catalog.exists(), "{args:?} wrote {}", catalog.display());
    }
}
