use super::*;
use cortexkit_test_support::ScratchDir;
use std::process::Command;
use std::time::{Duration, SystemTime};
use std::{fs::FileTimes, os::unix::fs::symlink};

#[test]
fn real_cargo_relative_dep_info_keeps_own_warm_build_directory() {
    let scratch = ScratchDir::new("motor-cargo-relative-dep-info");
    let checkout = scratch.join("checkout");
    fs::create_dir_all(checkout.join("member/src")).unwrap();
    fs::write(
        checkout.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(
        checkout.join("member/Cargo.toml"),
        "[package]\nname = \"real-relative\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        checkout.join("member/src/lib.rs"),
        "pub fn answer() -> u8 { 42 }\n",
    )
    .unwrap();
    let target = scratch.join("target");
    let build = target.join("bd/ab/00000000000001");
    let output = Command::new("cargo")
        .current_dir(&checkout)
        .args(["build", "--workspace", "--offline"])
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_BUILD_BUILD_DIR", &build)
        .env_remove("RUSTC_WRAPPER")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let deps = build.join("debug/deps");
    let dep_info = fs::read_dir(&deps)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension() == Some(OsStr::new("d")))
        .unwrap();
    let bytes = fs::read(&dep_info).unwrap();
    assert_eq!(
        dependency_path(&bytes).unwrap(),
        Path::new("member/src/lib.rs"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let directory = paths::owner::directory(&build).unwrap();
    assert!(
        matches!(
            dependency(&directory, &[&checkout]).unwrap(),
            Dependency::Checkout
        ),
        "real relative dep-info must identify the checkout, not a foreign folder"
    );
    run(&target, &[&checkout]);
    assert!(
        dep_info.is_file(),
        "cleanup deleted real Cargo's warm outputs"
    );
}

#[test]
fn real_rustc_foreign_absolute_dep_info_is_still_removed() {
    let scratch = ScratchDir::new("motor-cargo-absolute-dep-info");
    let foreign = scratch.join("foreign.rs");
    fs::write(&foreign, "pub fn answer() -> u8 { 42 }\n").unwrap();
    let target = scratch.join("target");
    let build = folder(&target, "00000000000001", "debug", None);
    let dep_info = build.join("debug/deps/foreign.d");
    let output = Command::new("rustc")
        .args(["--crate-type", "lib", "--emit"])
        .arg(format!("dep-info={}", dep_info.display()))
        .arg(&foreign)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        dependency_path(&fs::read(&dep_info).unwrap()).unwrap(),
        foreign
    );
    let checkout = scratch.join("checkout");
    let directory = paths::owner::directory(&build).unwrap();
    assert!(matches!(
        dependency(&directory, &[&checkout]).unwrap(),
        Dependency::Foreign
    ));
    run(&target, &[&checkout]);
    assert!(
        !build.exists(),
        "positively foreign absolute-source outputs must be removed"
    );
}

fn run(target: &Path, roots: &[&Path]) {
    maintain(target, roots, |_| Ok(())).unwrap();
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
        .set_times(FileTimes::new().set_modified(now - Duration::from_secs(48 * 60 * 60)))
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
    run(&target, &[Path::new("/checkout")]);
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
    run(&target, &[Path::new("/checkout")]);
    assert!(target.join("debug/deps/after-marker").exists());
}

#[test]
fn failed_migration_does_not_mark_partial_deletion_complete() {
    let scratch = ScratchDir::new("motor-cargo-layout-retry");
    let target = scratch.join("target");
    fs::create_dir_all(target.join("debug/deps")).unwrap();
    fs::create_dir_all(target.join("debug/build")).unwrap();
    let error = maintain(&target, &[], |path| {
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
    run(&target, &[]);
    assert!(!target.join("debug/deps").exists());
    assert!(target.join(MARKER).exists());
}

#[test]
fn cleanup_removes_only_foreign_and_keeps_even_old_unidentified() {
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
    assert!(unidentified.exists(), "age does not prove a folder foreign");
    assert!(recent.exists());
}

#[test]
fn cleanup_keeps_checkout_when_registry_dep_info_sorts_first() {
    let scratch = ScratchDir::new("motor-cargo-mixed-dep-info");
    let target = scratch.join("target");
    let own = folder(&target, "00000000000001", "debug", None);
    let deps = own.join("debug/deps");
    fs::write(
        deps.join("addr2line-000.d"),
        "out: /motor-home/cargo/registry/src/addr2line/src/lib.rs\n",
    )
    .unwrap();
    fs::write(
        deps.join("workspace-000.d"),
        "out: /caller/work/src/lib.rs\n",
    )
    .unwrap();
    run(&target, &[Path::new("/caller/work")]);
    assert!(
        deps.join("workspace-000.d").is_file(),
        "registry dep-info must not hide a later checkout dependency"
    );
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
    run(&target, &[Path::new("/caller/work")]);
    assert!(target.join("bd/ab/00000000000001").is_symlink());
    assert!(target.join("bd/cd").is_symlink());
    assert!(
        linked.exists(),
        "unreadable symlinked dep-info must not prove a folder foreign"
    );
    assert!(linked_profile.exists());
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
    run(&target, &[]);
    let linked_target = scratch.join("linked-target");
    symlink(&outside, &linked_target).unwrap();
    run(&linked_target, &[]);
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
    run(&target, &[Path::new("/caller/work")]);
    assert!(
        path.exists(),
        "dependency beyond the read limit must not be used"
    );
}

#[test]
fn unreadable_or_unknown_dep_info_prevents_foreign_classification() {
    let scratch = ScratchDir::new("motor-cargo-unknown-dep-info");
    let target = scratch.join("target");
    let outside = scratch.join("outside.d");
    fs::write(&outside, "out: /caller/work/src/lib.rs\n").unwrap();
    let linked = folder(&target, "00000000000001", "debug", Some("/foreign/lib.rs"));
    symlink(&outside, linked.join("debug/deps/unknown.d")).unwrap();
    let malformed = folder(&target, "00000000000002", "debug", Some("/foreign/lib.rs"));
    fs::write(malformed.join("debug/deps/unknown.d"), "not dep-info").unwrap();
    let oversized = folder(&target, "00000000000003", "debug", Some("/foreign/lib.rs"));
    let mut bytes = b"out: /foreign/lib.rs ".to_vec();
    bytes.resize(64 * 1024 + 1, b'x');
    fs::write(oversized.join("debug/deps/unknown.d"), bytes).unwrap();
    for path in [&linked, &malformed, &oversized] {
        old(path, SystemTime::now());
    }
    run(&target, &[Path::new("/caller/work")]);
    assert!(linked.is_dir());
    assert!(malformed.is_dir());
    assert!(oversized.is_dir());
    assert!(outside.is_file());
}
