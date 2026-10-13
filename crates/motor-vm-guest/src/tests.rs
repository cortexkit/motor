use super::*;
use cortexkit_test_support::ScratchDir;
use std::{fs, io::Cursor};

#[test]
fn protocol_roundtrips_binary_streams_and_rejects_malformed_frames() {
    let expected = Frame::Stdout {
        bytes: vec![0, 255, 10],
    };
    let mut wire = Vec::new();
    write_frame(&mut wire, &expected).unwrap();
    assert_eq!(
        read_frame::<Frame>(&mut Cursor::new(&wire)).unwrap(),
        expected
    );
    assert!(read_frame::<Frame>(&mut Cursor::new((MAX_FRAME as u32 + 1).to_be_bytes())).is_err());
    assert!(read_frame::<Frame>(&mut Cursor::new(&wire[..wire.len() - 1])).is_err());
    let data =
        br#"{"kind":"status","termination":{"kind":"exit","code":0},"clean":true,"extra":1}"#;
    let mut wire = Vec::from((data.len() as u32).to_be_bytes());
    wire.extend_from_slice(data);
    assert!(read_frame::<Frame>(&mut Cursor::new(wire)).is_err());
}

#[test]
fn start_rejects_protected_overlapping_and_traversing_mounts() {
    let mut s = Start {
        realtime_seconds: 1,
        realtime_nanoseconds: 0,
        entropy: [0; 32],
        trees: vec![TreeMount {
            subdirectory: "workspace".into(),
            destination: "/job/workspace".into(),
        }],
        env: BTreeMap::new(),
        vcpus: 1,
        proxy_port: None,
        mode: Mode::Job {
            command: vec!["true".into()],
            cwd: "/job/workspace".into(),
        },
    };
    s.validate().unwrap();
    for path in [
        "/",
        "/etc/shadow",
        "/run/motor-vm",
        "/job/workspace/../etc",
        "relative",
    ] {
        s.trees[0].destination = path.into();
        assert!(s.validate().is_err(), "{path}");
    }
    s.trees[0].destination = "/job/workspace".into();
    s.trees.push(TreeMount {
        subdirectory: "sibling".into(),
        destination: "/job/workspace/nested".into(),
    });
    assert!(s.validate().is_err());
}

#[test]
fn job_mounts_cannot_shadow_image_binaries_or_any_other_root() {
    let mut s = Start {
        realtime_seconds: 1,
        realtime_nanoseconds: 0,
        entropy: [0; 32],
        trees: vec![TreeMount {
            subdirectory: "workspace".into(),
            destination: "/job/workspace".into(),
        }],
        env: BTreeMap::new(),
        vcpus: 1,
        proxy_port: None,
        mode: Mode::Job {
            command: vec!["true".into()],
            cwd: "/job/workspace".into(),
        },
    };
    s.validate().unwrap();
    for path in [
        "/opt/motor/bin",
        "/etc",
        "/usr",
        "/home",
        "/unknown",
        "/job",
        "/jobs/workspace",
    ] {
        s.trees[0].destination = path.into();
        // Keep cwd inside the proposed tree so only mount validation can refuse it.
        s.mode = Mode::Job {
            command: vec!["true".into()],
            cwd: path.into(),
        };
        assert!(s.validate().is_err(), "accepted system mount {path}");
    }
}

const CHECKSUM: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
fn policy(git: bool) -> fetch::Policy {
    let mut lock = format!("version = 3\n[[package]]\nname = 'sample'\nversion = '1.0.0'\nsource = 'registry+https://github.com/rust-lang/crates.io-index'\nchecksum = '{CHECKSUM}'\n");
    let mut dirs = BTreeMap::new();
    if git {
        let source = "git+https://example.org/lib#1234567890123456789012345678901234567890";
        lock.push_str(&format!(
            "[[package]]\nname = 'lib'\nversion = '1.0.0'\nsource = '{source}'\n"
        ));
        dirs.insert(source.into(), "lib-abc".into());
    }
    fetch::Policy::from_lock(&lock, &dirs, b"[net]\nretry=2\n".to_vec()).unwrap()
}
fn home() -> ScratchDir {
    let scratch = ScratchDir::new("motor-vm-guest-fetch");
    let root = scratch.path();
    fs::create_dir_all(root.join("registry/cache/index")).unwrap();
    fs::create_dir_all(root.join("registry/src/index/sample-1.0.0")).unwrap();
    fs::write(root.join("config.toml"), b"[net]\nretry=2\n").unwrap();
    fs::write(root.join("registry/cache/index/sample-1.0.0.crate"), b"abc").unwrap();
    fs::write(
        root.join("registry/src/index/sample-1.0.0/lib.rs"),
        b"trusted extracted source",
    )
    .unwrap();
    scratch
}
#[test]
fn cargo_allowlist_keeps_extracted_registry_sources() {
    let home = home();
    policy(false)
        .validate(home.path(), Path::new("/usr/bin/git"))
        .unwrap();
    assert!(home
        .path()
        .join("registry/src/index/sample-1.0.0/lib.rs")
        .is_file());
}
#[test]
fn cargo_allowlist_rejects_bad_checksum_unlocked_crate_and_extra_path() {
    for case in 0..4 {
        let home = home();
        let root = home.path();
        match case {
            0 => fs::write(
                root.join("registry/cache/index/sample-1.0.0.crate"),
                b"corrupt",
            )
            .unwrap(),
            1 => fs::write(root.join("registry/cache/index/extra-1.0.0.crate"), b"abc").unwrap(),
            2 => fs::write(root.join("marker"), b"workspace code ran").unwrap(),
            _ => fs::write(root.join("config.toml"), b"modified config").unwrap(),
        }
        assert!(
            policy(false)
                .validate(root, Path::new("/usr/bin/git"))
                .is_err(),
            "case {case}"
        );
    }
}
#[cfg(unix)]
#[test]
fn cargo_allowlist_rejects_symlinks_and_unlocked_git_checkout() {
    let home = home();
    std::os::unix::fs::symlink(
        "/etc/passwd",
        home.path().join("registry/src/index/sample-1.0.0/escape"),
    )
    .unwrap();
    assert!(policy(false)
        .validate(home.path(), Path::new("/usr/bin/git"))
        .is_err());
    fs::remove_file(home.path().join("registry/src/index/sample-1.0.0/escape")).unwrap();
    fs::create_dir_all(home.path().join("git/checkouts/lib-abc/deadbee")).unwrap();
    assert!(policy(true)
        .validate(home.path(), Path::new("/usr/bin/git"))
        .is_err());
}

#[test]
fn cargo_allowlist_keeps_locked_git_checkout_and_verifies_head() {
    use std::process::Command;
    let home = home();
    let checkout = home.path().join("build-checkout");
    fs::create_dir(&checkout).unwrap();
    let git = |args: &[&str]| {
        let result = Command::new("/usr/bin/git")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .arg("-C")
            .arg(&checkout)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    git(&["init"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.org",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "--allow-empty",
        "-m",
        "fixture",
    ]);
    let revision = git(&["rev-parse", "HEAD"]);
    let short = &revision[..7];
    let destination = home.path().join(format!("git/checkouts/lib-abc/{short}"));
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::rename(&checkout, &destination).unwrap();
    let source = format!("git+https://example.org/lib#{revision}");
    let lock = format!("[[package]]\nname='lib'\nversion='1.0.0'\nsource='{source}'\n");
    let p = fetch::Policy::from_lock(
        &lock,
        &BTreeMap::from([(source, "lib-abc".into())]),
        b"[net]\nretry=2\n".to_vec(),
    )
    .unwrap();
    // The registry fixture must not introduce packages absent from this lock.
    fs::remove_dir_all(home.path().join("registry")).unwrap();
    p.validate(home.path(), Path::new("/usr/bin/git")).unwrap();
    assert!(destination.join(".git").is_dir());
    fs::write(
        destination.join(".git/HEAD"),
        "0123456789012345678901234567890123456789\n",
    )
    .unwrap();
    assert!(p.validate(home.path(), Path::new("/usr/bin/git")).is_err());
}

#[cfg(feature = "trusted-cargo-tests")]
#[test]
fn trusted_cargo_fetch_generation_satisfies_allowlist() {
    use std::process::Command;
    let scratch = ScratchDir::new("motor-vm-guest-trusted-cargo");
    let home = scratch.path().join("cargo");
    let workspace = scratch.path().join("workspace");
    let cwd = scratch.path().join("trusted-cwd");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    fs::write(workspace.join("Cargo.toml"), "[workspace]\n[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nitoa='=1.0.18'\n[lib]\npath='lib.rs'\n").unwrap();
    fs::write(workspace.join("lib.rs"), "").unwrap();
    let config = b"[net]\nretry=2\n";
    fs::write(home.join("config.toml"), config).unwrap();
    let result = Command::new(env!("CARGO"))
        .env("CARGO_HOME", &home)
        .env_remove("RUSTC_WRAPPER")
        .current_dir(&cwd)
        .args(["fetch", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(home.join("registry/CACHEDIR.TAG")).unwrap(),
        fetch::CARGO_CACHE_TAG
    );
    let lock = fs::read_to_string(workspace.join("Cargo.lock")).unwrap();
    let p = fetch::Policy::from_lock(&lock, &BTreeMap::new(), config.to_vec()).unwrap();
    p.validate(&home, Path::new("/usr/bin/git"))
        .unwrap_or_else(|error| {
            let entries: Vec<_> = fs::read_dir(home.join("registry"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            panic!("{error}; registry entries: {entries:?}")
        });
}

#[cfg(unix)]
#[test]
fn cargo_cache_tag_requires_exact_bytes_at_only_registry_and_git_tags() {
    for kind in ["registry", "git"] {
        let home = home();
        let path = home.path().join(format!("{kind}/CACHEDIR.TAG"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, fetch::CARGO_CACHE_TAG).unwrap();
        policy(false)
            .validate(home.path(), Path::new("/usr/bin/git"))
            .unwrap();
        fs::write(&path, b"unexpected").unwrap();
        assert!(policy(false)
            .validate(home.path(), Path::new("/usr/bin/git"))
            .is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
        assert!(policy(false)
            .validate(home.path(), Path::new("/usr/bin/git"))
            .is_err());
    }
    for path in [
        "CACHEDIR.TAG",
        "registry/src/index/sample-1.0.0/CACHEDIR.TAG",
    ] {
        let home = home();
        fs::write(home.path().join(path), fetch::CARGO_CACHE_TAG).unwrap();
        assert!(policy(false)
            .validate(home.path(), Path::new("/usr/bin/git"))
            .is_err());
    }
}

#[cfg(feature = "trusted-cargo-tests")]
#[test]
fn trusted_git_fetch_keeps_actual_cargo_checkout() {
    use std::process::Command;
    let scratch = ScratchDir::new("motor-vm-guest-trusted-git");
    let home = scratch.path().join("cargo");
    let repo = scratch.path().join("mirror");
    let workspace = scratch.path().join("workspace");
    let cwd = scratch.path().join("trusted-cwd");
    for dir in [&home, &repo, &workspace, &cwd] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='dependency'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n",
    )
    .unwrap();
    fs::write(repo.join("lib.rs"), "").unwrap();
    for args in [
        vec!["init"],
        vec!["-c", "core.hooksPath=/dev/null", "add", "."],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.org",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-m",
            "fixture",
        ],
    ] {
        assert!(Command::new("/usr/bin/git")
            .env_remove("GIT_CONFIG_COUNT")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    fs::write(workspace.join("Cargo.toml"), format!("[workspace]\n[package]\nname='fixture'\nversion='0.1.0'\n[dependencies]\ndependency={{git='file://{}'}}\n[lib]\npath='lib.rs'\n", repo.display())).unwrap();
    fs::write(workspace.join("lib.rs"), "").unwrap();
    let config = b"[net]\ngit-fetch-with-cli=true\n";
    fs::write(home.join("config.toml"), config).unwrap();
    let result = Command::new(env!("CARGO"))
        .env("CARGO_HOME", &home)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("GIT_CONFIG_COUNT")
        .current_dir(&cwd)
        .args(["fetch", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let lock = fs::read_to_string(workspace.join("Cargo.lock")).unwrap();
    let data: toml::Value = toml::from_str(&lock).unwrap();
    let source = data["package"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|p| p.get("source").and_then(toml::Value::as_str))
        .unwrap();
    let directory = fs::read_dir(home.join("git/db"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    let p = fetch::Policy::from_lock(
        &lock,
        &BTreeMap::from([(source.into(), directory)]),
        config.to_vec(),
    )
    .unwrap();
    assert_eq!(
        fs::read(home.join("git/CACHEDIR.TAG")).unwrap(),
        fetch::CARGO_CACHE_TAG
    );
    p.validate(&home, Path::new("/usr/bin/git"))
        .unwrap_or_else(|e| {
            let entries: Vec<_> = fs::read_dir(home.join("git"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            panic!("{e}; git entries: {entries:?}")
        });
}
