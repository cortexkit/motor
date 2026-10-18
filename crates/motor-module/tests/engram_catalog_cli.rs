use std::{fs, process::Command};

#[test]
fn cli_commands_do_not_write_the_module_backup_catalog() {
    let data_home = std::env::temp_dir().join(format!(
        "ck-motor-engram-cli-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    fs::create_dir_all(&data_home).unwrap();
    let catalog = data_home.join("cortexkit/ck-motor/engram-catalog.json");
    for args in [
        vec!["--manifest"],
        vec!["run", "--help"],
        vec!["attach", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ck-motor"))
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
    fs::remove_dir_all(data_home).unwrap();
}
