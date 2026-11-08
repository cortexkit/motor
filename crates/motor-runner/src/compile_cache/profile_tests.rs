use super::*;
use cortexkit_test_support::ScratchDir;

const DEBUG_KEYS: [&str; 2] = ["CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"];

fn assert_defaults(repository: &Path) {
    assert_defaults_at(repository, &snapshot(repository));
}

fn snapshot(repository: &Path) -> String {
    crate::workspace::git(repository, &["init", "-q"]).unwrap();
    crate::workspace::git(repository, &["add", "."]).unwrap();
    crate::workspace::git(repository, &["write-tree"]).unwrap()
}

fn assert_defaults_at(repository: &Path, tree: &str) {
    let mut env = BTreeMap::new();
    profile_environment(&mut env, repository, tree);
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
    let tree = snapshot(scratch.path());
    for key in DEBUG_KEYS {
        for value in ["", "true", "0", "line-tables-only", "custom"] {
            let mut env = BTreeMap::from([(key.into(), value.into())]);
            let submitted = env.clone();
            profile_environment(&mut env, scratch.path(), &tree);
            assert_eq!(env, submitted);
        }
    }
    let mut env = BTreeMap::from([
        (DEBUG_KEYS[0].into(), "true".into()),
        (DEBUG_KEYS[1].into(), "0".into()),
    ]);
    let submitted = env.clone();
    profile_environment(&mut env, scratch.path(), &tree);
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
            profile_environment(&mut env, scratch.path(), &snapshot(scratch.path()));
            assert!(env.is_empty(), "{file}: {profile} debug={value}: {env:?}");
        }
        fs::write(&path, format!("profile.{profile}.debug = 'full'\n")).unwrap();
        let mut env = BTreeMap::new();
        profile_environment(&mut env, scratch.path(), &snapshot(scratch.path()));
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

#[test]
fn symlinked_cargo_profiles_keep_defaults_without_following_targets() {
    for file in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
        let scratch = ScratchDir::new("cargo-debug-symlink");
        let repository = scratch.join("repository");
        fs::create_dir(&repository).unwrap();
        let target = scratch.join("outside-profile");
        fs::write(&target, "[profile.dev]\ndebug = true\n").unwrap();
        let path = repository.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, path).unwrap();
        assert_defaults(&repository);
    }
}

#[test]
fn symlinked_cargo_directory_keeps_defaults_without_following_targets() {
    let scratch = ScratchDir::new("cargo-debug-directory-symlink");
    let repository = scratch.join("repository");
    fs::create_dir(&repository).unwrap();
    let outside = scratch.join("outside");
    fs::create_dir(&outside).unwrap();
    for file in ["config.toml", "config"] {
        fs::write(outside.join(file), "[profile.test]\ndebug = true\n").unwrap();
    }
    std::os::unix::fs::symlink(&outside, repository.join(".cargo")).unwrap();
    assert_defaults(&repository);
}

#[test]
fn oversized_cargo_profiles_keep_defaults() {
    for file in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
        let scratch = ScratchDir::new("cargo-debug-oversized");
        let path = scratch.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = "[profile.dev]\ndebug = true\n#".to_owned();
        text.push_str(&"x".repeat(1024 * 1024 + 1 - text.len()));
        fs::write(path, text).unwrap();
        assert_defaults(scratch.path());
    }
}

#[test]
fn cargo_profile_at_size_limit_still_opts_out() {
    let scratch = ScratchDir::new("cargo-debug-size-limit");
    let mut text = "[profile.test]\ndebug = true\n#".to_owned();
    text.push_str(&"x".repeat(1024 * 1024 - text.len()));
    fs::write(scratch.join("Cargo.toml"), text).unwrap();
    let mut env = BTreeMap::new();
    profile_environment(&mut env, scratch.path(), &snapshot(scratch.path()));
    assert!(env.is_empty());
}

#[test]
fn profile_defaults_use_snapshot_not_host_checkout_files() {
    let scratch = ScratchDir::new("cargo-debug-immutable-tree");
    let empty_tree = snapshot(scratch.path());
    fs::write(scratch.join("Cargo.toml"), "[profile.dev]\ndebug = true\n").unwrap();
    assert_defaults_at(scratch.path(), &empty_tree);
    let profile_tree = snapshot(scratch.path());
    fs::remove_file(scratch.join("Cargo.toml")).unwrap();
    let mut env = BTreeMap::new();
    profile_environment(&mut env, scratch.path(), &profile_tree);
    assert!(env.is_empty());
}

#[test]
fn executable_cargo_profile_blob_still_opts_out() {
    let scratch = ScratchDir::new("cargo-debug-executable");
    fs::write(scratch.join("Cargo.toml"), "[profile.test]\ndebug = true\n").unwrap();
    snapshot(scratch.path());
    crate::workspace::git(
        scratch.path(),
        &["update-index", "--chmod=+x", "Cargo.toml"],
    )
    .unwrap();
    let tree = crate::workspace::git(scratch.path(), &["write-tree"]).unwrap();
    let mut env = BTreeMap::new();
    profile_environment(&mut env, scratch.path(), &tree);
    assert!(env.is_empty());
}
