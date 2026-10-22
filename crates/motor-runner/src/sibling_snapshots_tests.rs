use super::*;
use crate::backend::BackendKind;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const CALLER: &str = "89abcdef0123456789abcdef0123456789abcdef";
const SHA256_COMMIT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn config() -> Config {
    Config::new("/state-root", "main")
}

fn environment(node: &str) -> JavascriptEnvironment {
    JavascriptEnvironment::digest(
        &BTreeMap::from([("bun".to_string(), b"1.2.0\n".to_vec())]),
        node.as_bytes(),
        &[],
    )
}

#[test]
fn snapshot_mode_is_off_and_siblings_stay_in_place_on_every_runner_kind() {
    let portable = config();
    let mut isolated = config();
    isolated.isolate_jobs = true;
    let mut windows = config();
    windows.backend = BackendKind::WindowsVm;
    windows.isolate_jobs = true;
    for config in [&portable, &isolated, &windows] {
        assert!(!config.sibling_snapshot_mode());
    }
    assert!(portable.in_place_siblings());
    assert!(isolated.in_place_siblings());
    // The Windows instance stages per-job exports and never checks out at P.
    assert!(!windows.in_place_siblings());
}

#[test]
fn fixture_overrides_replace_each_sibling_mode_predicate() {
    let mut snapshots = config();
    snapshots.sibling_snapshot_mode_override = Some(true);
    assert!(snapshots.sibling_snapshot_mode());
    // Without its own override, in-place mode follows the snapshot mode.
    assert!(!snapshots.in_place_siblings());

    let mut isolated_off = config();
    isolated_off.isolate_jobs = true;
    isolated_off.sibling_snapshot_mode_override = Some(false);
    assert!(!isolated_off.sibling_snapshot_mode());
    assert!(isolated_off.in_place_siblings());

    let mut independent = config();
    independent.sibling_snapshot_mode_override = Some(true);
    independent.in_place_siblings_override = Some(true);
    assert!(independent.sibling_snapshot_mode());
    assert!(independent.in_place_siblings());

    let mut windows = config();
    windows.backend = BackendKind::WindowsVm;
    windows.in_place_siblings_override = Some(true);
    assert!(windows.in_place_siblings());
}

#[test]
fn snapshots_live_under_the_instance_state() {
    assert_eq!(
        config().sibling_snapshots_dir(),
        PathBuf::from("/state-root/main/sibling-snapshots")
    );
}

#[test]
fn key_accepts_full_sha1_and_sha256_object_ids() {
    let env = environment("v22.0.0\n");
    for (commit, caller) in [
        (COMMIT, COMMIT),
        (COMMIT, CALLER),
        (SHA256_COMMIT, SHA256_COMMIT),
    ] {
        let key = SnapshotKey::new("/src/commons", commit, caller, &env).unwrap();
        assert_eq!(key.commit(), commit);
    }
}

#[test]
fn key_refuses_any_commit_or_caller_that_is_not_a_full_lowercase_object_id() {
    let env = environment("v22.0.0\n");
    let refused = [
        "",
        "HEAD",
        "main",
        "refs/heads/main",
        &COMMIT[..7],
        &COMMIT[..39],
        &format!("{COMMIT}0"),
        &SHA256_COMMIT[..63],
        &format!("{SHA256_COMMIT}0"),
        &COMMIT.to_uppercase(),
        &format!("{}g", &COMMIT[..39]),
        &format!("{}^", &COMMIT[..39]),
        &format!("{COMMIT}^{{tree}}"),
        &format!(" {}", &COMMIT[..39]),
        &format!("{}/", &COMMIT[..39]),
    ];
    for value in refused {
        let as_commit = SnapshotKey::new("/src/commons", value, CALLER, &env).unwrap_err();
        assert_eq!(as_commit.kind(), io::ErrorKind::InvalidInput, "{value:?}");
        assert!(
            as_commit.to_string().contains("commons commit"),
            "{as_commit}"
        );
        let as_caller = SnapshotKey::new("/src/commons", COMMIT, value, &env).unwrap_err();
        assert_eq!(as_caller.kind(), io::ErrorKind::InvalidInput, "{value:?}");
        assert!(
            as_caller.to_string().contains("caller commit"),
            "{as_caller}"
        );
    }
}

#[test]
fn key_directory_names_commit_caller_and_javascript_environment() {
    let env = environment("v22.0.0\n");
    let own = SnapshotKey::new("/src/commons", COMMIT, COMMIT, &env).unwrap();
    let other = SnapshotKey::new("/src/commons", COMMIT, CALLER, &env).unwrap();
    assert_eq!(own.directory_name(), format!("{COMMIT}-{}", env.as_str()));
    assert_eq!(
        other.directory_name(),
        format!("{COMMIT}-{CALLER}-{}", env.as_str())
    );
    let newer_node = environment("v24.0.0\n");
    let reinstalled = SnapshotKey::new("/src/commons", COMMIT, COMMIT, &newer_node).unwrap();
    assert_ne!(reinstalled.directory_name(), own.directory_name());
    assert_ne!(reinstalled, own);
    for key in [&own, &other, &reinstalled] {
        assert!(key.directory_name().starts_with(COMMIT));
    }
    assert_eq!(
        own.directory(&config()),
        PathBuf::from("/state-root/main/sibling-snapshots")
            .join(crate::workspace::name("/src/commons"))
            .join(own.directory_name())
    );
    let elsewhere = SnapshotKey::new("/src/insula", COMMIT, COMMIT, &env).unwrap();
    assert_eq!(elsewhere.directory_name(), own.directory_name());
    assert_ne!(elsewhere.directory(&config()), own.directory(&config()));
}

#[test]
fn javascript_environment_digest_covers_every_runner_input() {
    let managers = BTreeMap::from([
        ("bun".to_string(), b"1.2.0\n".to_vec()),
        ("npm".to_string(), b"10.0.0\n".to_vec()),
    ]);
    let selection = vec![(OsString::from("NODE_ENV"), OsString::from("production"))];
    let base = JavascriptEnvironment::digest(&managers, b"v22.0.0\n", &selection);
    assert_eq!(base.as_str().len(), 64);
    assert!(base.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        base,
        JavascriptEnvironment::digest(&managers, b"v22.0.0\n", &selection)
    );

    let mut manager_version = managers.clone();
    manager_version.insert("npm".into(), b"11.0.0\n".to_vec());
    let mut manager_added = managers.clone();
    manager_added.insert("pnpm".into(), b"9.0.0\n".to_vec());
    let changed = [
        JavascriptEnvironment::digest(&manager_version, b"v22.0.0\n", &selection),
        JavascriptEnvironment::digest(&manager_added, b"v22.0.0\n", &selection),
        JavascriptEnvironment::digest(&managers, b"v24.0.0\n", &selection),
        JavascriptEnvironment::digest(&managers, b"v22.0.0\n", &[]),
        JavascriptEnvironment::digest(
            &managers,
            b"v22.0.0\n",
            &[(OsString::from("NODE_ENV"), OsString::from("development"))],
        ),
        JavascriptEnvironment::digest(
            &managers,
            b"v22.0.0\n",
            &[(
                OsString::from("NPM_CONFIG_OMIT"),
                OsString::from("production"),
            )],
        ),
    ];
    for (index, digest) in changed.iter().enumerate() {
        assert_ne!(digest, &base, "input change {index} kept the digest");
    }

    // Fields are framed, so moving bytes between adjacent fields changes it.
    let split = |name: &str, value: &str| {
        JavascriptEnvironment::digest(
            &managers,
            b"v22.0.0\n",
            &[(OsString::from(name), OsString::from(value))],
        )
    };
    assert_ne!(split("NODE_ENVa", "b"), split("NODE_ENV", "ab"));
}

#[cfg(target_os = "linux")]
#[test]
fn runner_javascript_environment_uses_the_install_cache_selection_variables() {
    let managers = BTreeMap::from([("npm".to_string(), b"10.0.0\n".to_vec())]);
    assert_eq!(
        JavascriptEnvironment::of_runner(&managers, b"v22.0.0\n"),
        JavascriptEnvironment::digest(
            &managers,
            b"v22.0.0\n",
            &crate::isolation::javascript_selection_environment()
        )
    );
}
