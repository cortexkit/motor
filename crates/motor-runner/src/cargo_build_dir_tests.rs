use super::*;
use cortexkit_test_support::ScratchDir;
use std::{fs::FileTimes, os::unix::fs::symlink};

fn run(target: &Path, roots: &[&Path], now: SystemTime) {
    maintain(target, roots, now, |_| Ok(())).unwrap();
}

fn folder(target: &Path, hash: &str, profile: &str, source: Option<&str>) -> PathBuf {
    let path = target.join("bd/ab").join(hash);
    let deps = path.join(profile).join("deps");
    fs::create_dir_all(&deps).unwrap();
    if let Some(source) = source {
        fs::write(deps.join("crate.d"), format!("out: {source} second.rs\n")).unwrap();
    }
    path
}

fn old(path: &Path, now: SystemTime) {
    File::open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(now - UNIDENTIFIED_AGE - Duration::from_secs(1)))
        .unwrap();
}

#[test]
fn migration_removes_only_legacy_intermediates_once() {
    let scratch = ScratchDir::new("motor-cargo-layout-migration");
    let target = scratch.join("target");
    for profile in ["debug", "custom", "x86_64-unknown-linux-gnu/release"] {
        for name in ["deps", ".fingerprint", "build", "incremental", "examples"] {
            let path = target.join(profile).join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("keep-or-delete"), "output").unwrap();
        }
        fs::write(target.join(profile).join("final-binary"), "binary").unwrap();
    }
    let own = folder(
        &target,
        "0123456789abcd",
        "debug",
        Some("/checkout/src/lib.rs"),
    );
    fs::create_dir_all(target.join("unknown/build")).unwrap();
    run(&target, &[Path::new("/checkout")], SystemTime::now());
    for profile in ["debug", "custom", "x86_64-unknown-linux-gnu/release"] {
        for name in ["deps", ".fingerprint", "build", "incremental"] {
            assert!(
                !target.join(profile).join(name).exists(),
                "{profile}/{name}"
            );
        }
        assert_eq!(
            fs::read(target.join(profile).join("final-binary")).unwrap(),
            b"binary"
        );
        assert!(target
            .join(profile)
            .join("examples/keep-or-delete")
            .exists());
    }
    assert!(own.join("debug/deps/crate.d").exists());
    assert!(target.join("unknown/build").exists());
    assert!(target.join(MARKER).is_file());
    fs::create_dir_all(target.join("debug/deps")).unwrap();
    fs::write(target.join("debug/deps/after-marker"), "new").unwrap();
    run(&target, &[Path::new("/checkout")], SystemTime::now());
    assert!(target.join("debug/deps/after-marker").exists());
}

#[test]
fn failed_migration_does_not_mark_partial_deletion_complete() {
    let scratch = ScratchDir::new("motor-cargo-layout-retry");
    let target = scratch.join("target");
    fs::create_dir_all(target.join("debug/deps")).unwrap();
    fs::create_dir_all(target.join("debug/build")).unwrap();
    let error = maintain(&target, &[], SystemTime::now(), |path| {
        if path.ends_with("deps") {
            Err(io::Error::other("injected deletion failure"))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(error.to_string().contains("injected"));
    assert!(!target.join(MARKER).exists());
    assert!(!target.join("debug/build").exists());
    run(&target, &[], SystemTime::now());
    assert!(!target.join("debug/deps").exists());
    assert!(target.join(MARKER).exists());
}

#[test]
fn cleanup_keeps_workspace_and_sibling_but_removes_foreign_and_old_unidentified() {
    let scratch = ScratchDir::new("motor-cargo-build-cleanup");
    let target = scratch.join("target");
    let now = SystemTime::now();
    let own = folder(
        &target,
        "00000000000001",
        "debug",
        Some("/caller/work/src/lib.rs"),
    );
    let sibling = folder(
        &target,
        "00000000000002",
        "triple/release",
        Some("/caller/sibling\\ with\\ spaces/src/lib.rs"),
    );
    let foreign = folder(
        &target,
        "00000000000003",
        "debug",
        Some("/scratch/copy/src/lib.rs"),
    );
    let prefix = folder(
        &target,
        "00000000000004",
        "debug",
        Some("/caller/work-other/src/lib.rs"),
    );
    let unidentified = folder(&target, "00000000000005", "debug", None);
    old(&unidentified, now);
    let recent = folder(&target, "00000000000006", "debug", None);
    run(
        &target,
        &[
            Path::new("/caller/work"),
            Path::new("/caller/sibling with spaces"),
        ],
        now,
    );
    assert!(own.exists());
    assert!(sibling.exists());
    assert!(
        !foreign.exists(),
        "foreign workspace intermediates were retained"
    );
    assert!(
        !prefix.exists(),
        "path prefixes are not checkout descendants"
    );
    assert!(!unidentified.exists());
    assert!(recent.exists());
}

#[test]
fn cleanup_does_not_follow_symlinked_folders_or_dep_info() {
    let scratch = ScratchDir::new("motor-cargo-build-symlinks");
    let target = scratch.join("target");
    let outside = scratch.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("sentinel"), "untouched").unwrap();
    fs::create_dir_all(target.join("bd/ab")).unwrap();
    symlink(&outside, target.join("bd/ab/00000000000001")).unwrap();
    symlink(&outside, target.join("bd/cd")).unwrap();
    let now = SystemTime::now();
    let linked = folder(&target, "00000000000002", "debug", None);
    let dep_info = outside.join("own.d");
    fs::write(&dep_info, "out: /caller/work/src/lib.rs\n").unwrap();
    symlink(&dep_info, linked.join("debug/deps/linked.d")).unwrap();
    old(&linked, now);
    let linked_profile = target.join("bd/ab/00000000000003");
    fs::create_dir_all(&linked_profile).unwrap();
    symlink(&outside, linked_profile.join("debug")).unwrap();
    old(&linked_profile, now);
    run(&target, &[Path::new("/caller/work")], now);
    assert!(target.join("bd/ab/00000000000001").is_symlink());
    assert!(target.join("bd/cd").is_symlink());
    assert!(
        !linked.exists(),
        "a symlinked dep-info must not identify a folder"
    );
    assert!(!linked_profile.exists());
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
    assert!(dep_info.is_file());
}

#[test]
fn migration_does_not_follow_symlinked_profiles_or_target() {
    let scratch = ScratchDir::new("motor-cargo-migration-symlinks");
    let outside = scratch.join("outside");
    fs::create_dir_all(outside.join("deps")).unwrap();
    fs::write(outside.join("deps/sentinel"), "untouched").unwrap();
    let target = scratch.join("target");
    fs::create_dir_all(&target).unwrap();
    symlink(&outside, target.join("debug")).unwrap();
    run(&target, &[], SystemTime::now());
    let linked_target = scratch.join("linked-target");
    symlink(&outside, &linked_target).unwrap();
    run(&linked_target, &[], SystemTime::now());
    assert!(outside.join("deps/sentinel").is_file());
    assert!(!outside.join(MARKER).exists());
}

#[test]
fn dep_info_reads_are_bounded_and_decode_escaped_spaces() {
    assert_eq!(
        dependency_path(b"out: /caller/my\\ workspace/src/lib.rs other\n"),
        Some(PathBuf::from("/caller/my workspace/src/lib.rs"))
    );
    let scratch = ScratchDir::new("motor-cargo-dep-info-limit");
    let target = scratch.join("target");
    let path = folder(&target, "00000000000001", "debug", None);
    let mut bytes = vec![b'x'; 64 * 1024];
    bytes.extend_from_slice(b": /foreign/src/lib.rs\n");
    fs::write(path.join("debug/deps/large.d"), bytes).unwrap();
    run(&target, &[Path::new("/caller/work")], SystemTime::now());
    assert!(
        path.exists(),
        "dependency beyond the read limit must not be used"
    );
}
