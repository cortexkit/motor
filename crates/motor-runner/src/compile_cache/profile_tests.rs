use super::*;
use cortexkit_test_support::ScratchDir;

const DEBUG_KEYS: [&str; 2] = ["CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"];

fn assert_defaults(repository: &Path) {
    let mut env = BTreeMap::new();
    profile_environment(&mut env, repository);
    assert_eq!(env.len(), 2);
    for key in DEBUG_KEYS {
        assert_eq!(env[key], "line-tables-only");
    }
}

#[test]
fn linux_debug_defaults_keep_line_tables() {
    let scratch = ScratchDir::new("cargo-debug-defaults");
    assert_defaults(scratch.path());
}

#[test]
fn either_submitted_debug_variable_suppresses_both_defaults() {
    let scratch = ScratchDir::new("cargo-debug-submitted");
    for key in DEBUG_KEYS {
        for value in ["", "true", "0", "line-tables-only", "custom"] {
            let mut env = BTreeMap::from([(key.into(), value.into())]);
            let submitted = env.clone();
            profile_environment(&mut env, scratch.path());
            assert_eq!(env, submitted);
        }
    }
    let mut env = BTreeMap::from([
        (DEBUG_KEYS[0].into(), "true".into()),
        (DEBUG_KEYS[1].into(), "0".into()),
    ]);
    let submitted = env.clone();
    profile_environment(&mut env, scratch.path());
    assert_eq!(env, submitted);
}

fn assert_profile_opt_out(file: &str) {
    let scratch = ScratchDir::new("cargo-debug-profile");
    let path = scratch.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for profile in ["dev", "test"] {
        for value in ["true", "false", "0", "2", "'full'"] {
            fs::write(&path, format!("[profile.{profile}]\ndebug = {value}\n")).unwrap();
            let mut env = BTreeMap::new();
            profile_environment(&mut env, scratch.path());
            assert!(env.is_empty(), "{file}: {profile} debug={value}: {env:?}");
        }
        fs::write(&path, format!("profile.{profile}.debug = 'full'\n")).unwrap();
        let mut env = BTreeMap::new();
        profile_environment(&mut env, scratch.path());
        assert!(env.is_empty(), "{file}: dotted {profile}: {env:?}");
    }
}

#[test]
fn root_manifest_profile_debug_suppresses_both_defaults() {
    assert_profile_opt_out("Cargo.toml");
}

#[test]
fn cargo_config_toml_profile_debug_suppresses_both_defaults() {
    assert_profile_opt_out(".cargo/config.toml");
}

#[test]
fn legacy_cargo_config_profile_debug_suppresses_both_defaults() {
    assert_profile_opt_out(".cargo/config");
}

#[test]
fn unparsable_cargo_files_keep_debug_defaults() {
    for file in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
        let scratch = ScratchDir::new("cargo-debug-unparsable");
        let path = scratch.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "[profile.dev]\ndebug = ").unwrap();
        assert_defaults(scratch.path());
    }
}

#[test]
fn package_debug_overrides_do_not_suppress_profile_defaults() {
    for file in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
        let scratch = ScratchDir::new("cargo-debug-package");
        let path = scratch.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            "[profile.dev.package.'*']\ndebug = true\n[profile.test.package.example]\ndebug = 0\n",
        )
        .unwrap();
        assert_defaults(scratch.path());
    }
}
