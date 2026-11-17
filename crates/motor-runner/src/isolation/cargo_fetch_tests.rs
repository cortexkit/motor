use super::*;
use cortexkit_test_support::ScratchDir;
use std::{collections::BTreeMap, ffi::OsStr, fs, io::Write, os::unix::fs::symlink};

const LOCK_ERROR: &str =
    "error: the lock file Cargo.lock needs to be updated but --locked was passed to prevent this";

fn workspace(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname='probe'\nversion='0.2.0'\nedition='2021'\n[workspace]\n",
    )
    .unwrap();
    fs::write(workspace.join("src/lib.rs"), "pub fn probe() {}\n").unwrap();
    fs::write(
        workspace.join("Cargo.lock"),
        "version=4\n[[package]]\nname='probe'\nversion='0.1.0'\n",
    )
    .unwrap();
    workspace
}

fn command(directory: &Path, locked: bool) -> Command {
    let mut command = Command::new("cargo");
    command.arg("fetch");
    if locked {
        command.arg("--locked");
    }
    command
        .arg("--manifest-path")
        .arg(directory.join("Cargo.toml"));
    command
}

#[test]
fn stale_locked_fetch_runs_exactly_one_online_unlocked_fetch_on_a_copy() {
    for tenant in [false, true] {
        let root = ScratchDir::new("cargo-private-decision");
        let workspace = workspace(&root);
        symlink("src", workspace.join("source-link")).unwrap();
        let before = fs::read(workspace.join("Cargo.lock")).unwrap();
        let mut calls = 0;
        let mut private = PathBuf::new();
        assert!(prepare_via(
            &workspace,
            tenant,
            |path, locked| Ok(command(path, locked)),
            |command, limit, copy| {
                calls += 1;
                if let Some(copy) = copy {
                    private = copy.to_owned();
                    assert_ne!(copy, workspace);
                    assert!(copy.is_dir());
                    assert_eq!(
                        fs::read_link(copy.join("source-link")).unwrap(),
                        Path::new("src")
                    );
                    assert!(command.get_args().any(|arg| arg == copy.join("Cargo.toml")));
                    assert!(!command
                        .get_args()
                        .any(|arg| arg == "--locked" || arg == "--offline"));
                    assert_eq!(
                        command
                            .get_envs()
                            .find(|(key, _)| *key == "CARGO_NET_OFFLINE")
                            .unwrap()
                            .1,
                        Some(OsStr::new("false"))
                    );
                    assert_eq!(limit, tenant.then_some(ONLINE_TIMEOUT));
                    fs::write(copy.join("Cargo.lock"), "private new lock")?;
                    Ok(())
                } else {
                    assert!(command.get_args().any(|arg| arg == "--locked"));
                    Err(io::Error::other(LOCK_ERROR))
                }
            }
        )
        .unwrap());
        assert_eq!(calls, if tenant { 3 } else { 2 });
        assert!(!private.exists());
        assert_eq!(fs::read(workspace.join("Cargo.lock")).unwrap(), before);
    }
}

#[test]
fn other_fetch_failures_and_successful_offline_fallback_never_unlock() {
    for (failure, offline_success) in [
        ("registry unavailable", false),
        (
            "failed to read the lock file: --locked was passed to prevent this",
            false,
        ),
        (LOCK_ERROR, true),
    ] {
        let root = ScratchDir::new("cargo-private-no-unlock");
        let workspace = workspace(&root);
        let mut calls = 0;
        let result = prepare_via(
            &workspace,
            true,
            |path, locked| {
                assert!(locked);
                assert_eq!(path, workspace);
                Ok(command(path, locked))
            },
            |_, _, copy| {
                calls += 1;
                assert!(copy.is_none());
                if calls == 2 && offline_success {
                    Ok(())
                } else {
                    Err(io::Error::other(failure))
                }
            },
        );
        assert_eq!(calls, 2);
        if offline_success {
            assert!(!result.unwrap());
        } else {
            assert!(result.unwrap_err().to_string().contains(failure));
        }
        assert_eq!(fs::read_dir(&*root).unwrap().count(), 1);
    }
}

#[test]
fn failed_unlocked_fetch_reports_both_errors_and_removes_copy() {
    let root = ScratchDir::new("cargo-private-failure");
    let workspace = workspace(&root);
    let mut private = PathBuf::new();
    let mut calls = 0;
    let error = prepare_via(
        &workspace,
        true,
        |path, locked| Ok(command(path, locked)),
        |_, _, copy| {
            calls += 1;
            if let Some(copy) = copy {
                private = copy.to_owned();
                Err(io::Error::other(format!(
                    "unlocked registry failure {}",
                    "x".repeat(8192)
                )))
            } else {
                Err(io::Error::other(LOCK_ERROR))
            }
        },
    )
    .unwrap_err()
    .to_string();
    assert_eq!(calls, 3);
    assert!(
        error.starts_with("unlocked private Cargo fetch failed: unlocked registry failure"),
        "{error}"
    );
    assert!(error.contains(LOCK_ERROR), "{error}");
    assert!(error.len() < 4096);
    assert!(!private.exists());
}

#[test]
fn private_copy_preserves_symlinks_and_excludes_build_outputs() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let root = ScratchDir::new("cargo-private-links");
    let workspace = workspace(&root);
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), "not copied").unwrap();
    symlink(&outside, workspace.join("directory-link")).unwrap();
    symlink("missing", workspace.join("dangling-link")).unwrap();
    for name in ["target", "build", "src/target"] {
        fs::create_dir_all(workspace.join(name)).unwrap();
        fs::write(workspace.join(name).join("marker"), name).unwrap();
    }
    fs::create_dir(workspace.join("read-only")).unwrap();
    fs::write(workspace.join("read-only/source"), "source").unwrap();
    fs::set_permissions(
        workspace.join("read-only"),
        fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    let copy = PrivateCopy::new(&workspace).unwrap();
    assert_eq!(
        fs::read_link(copy.workspace.join("directory-link")).unwrap(),
        outside
    );
    assert_eq!(
        fs::read_link(copy.workspace.join("dangling-link")).unwrap(),
        Path::new("missing")
    );
    assert!(!copy.workspace.join("target").exists());
    assert!(copy.workspace.join("build/marker").is_file());
    assert!(copy.workspace.join("src/target/marker").is_file());
    assert_eq!(fs::metadata(&copy.root).unwrap().mode() & 0o777, 0o700);
    assert_ne!(
        fs::metadata(copy.workspace.join("Cargo.lock"))
            .unwrap()
            .ino(),
        fs::metadata(workspace.join("Cargo.lock")).unwrap().ino()
    );
    let private = copy.workspace.clone();
    copy.remove().unwrap();
    assert!(!private.exists());
    assert_eq!(
        fs::metadata(workspace.join("read-only")).unwrap().mode() & 0o777,
        0o500
    );
    fs::set_permissions(
        workspace.join("read-only"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::remove_file(workspace.join("Cargo.lock")).unwrap();
    symlink(outside.join("secret"), workspace.join("Cargo.lock")).unwrap();
    assert!(PrivateCopy::new(&workspace).is_err());
    assert_eq!(
        fs::read_to_string(outside.join("secret")).unwrap(),
        "not copied"
    );
    assert_eq!(
        fs::read_dir(&*root).unwrap().count(),
        2,
        "failed copy leaked scratch"
    );
}

#[test]
fn unsafe_lockfile_destinations_skip_unlocked_fetch_and_preserve_locked_error() {
    for case in [
        "manifest-symlink",
        "lock-symlink",
        "explicit-workspace",
        "discovered-workspace",
    ] {
        let root = ScratchDir::new("cargo-private-unsafe-root");
        let workspace = workspace(&root);
        match case {
            "manifest-symlink" | "lock-symlink" => {
                let name = if case == "manifest-symlink" {
                    "Cargo.toml"
                } else {
                    "Cargo.lock"
                };
                let outside = root.join(name);
                fs::rename(workspace.join(name), &outside).unwrap();
                symlink(&outside, workspace.join(name)).unwrap();
            }
            "explicit-workspace" => {
                fs::write(
                    root.join("Cargo.toml"),
                    "[workspace]\nmembers=['workspace']\n",
                )
                .unwrap();
                fs::write(
                    workspace.join("Cargo.toml"),
                    "[package]\nname='probe'\nversion='0.2.0'\nworkspace='..'\n",
                )
                .unwrap();
            }
            "discovered-workspace" => {
                fs::write(
                    root.join("Cargo.toml"),
                    "[workspace]\nmembers=['workspace']\n",
                )
                .unwrap();
                fs::write(
                    workspace.join("Cargo.toml"),
                    "[package]\nname='probe'\nversion='0.2.0'\n",
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let manifest = fs::read(workspace.join("Cargo.toml")).unwrap();
        let lock = fs::read(workspace.join("Cargo.lock")).unwrap();
        let mut calls = 0;
        let error = prepare_via(
            &workspace,
            false,
            |path, locked| {
                assert!(locked, "{case}: unsafe unlocked fetch was constructed");
                Ok(command(path, locked))
            },
            |_, _, copy| {
                calls += 1;
                assert!(copy.is_none());
                Err(io::Error::other(LOCK_ERROR))
            },
        )
        .unwrap_err()
        .to_string();
        assert_eq!(calls, 1);
        assert!(error.starts_with(LOCK_ERROR), "{case}: {error}");
        assert!(
            error.contains("\nunlocked Cargo fallback skipped:"),
            "{case}: {error}"
        );
        assert!(
            error.contains(if case.ends_with("symlink") {
                "symlink"
            } else {
                "outside the private copy"
            }),
            "{case}: {error}"
        );
        assert_eq!(fs::read(workspace.join("Cargo.toml")).unwrap(), manifest);
        assert_eq!(fs::read(workspace.join("Cargo.lock")).unwrap(), lock);
        assert!(!fs::read_dir(&*root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cargo-prefetch-")));
    }
}

#[test]
fn real_cargo_stale_lock_populates_cache_without_changing_job_tree() {
    use sha2::{Digest, Sha256};
    let root = ScratchDir::new("cargo-private-real");
    let workspace = workspace(&root);
    let registry = root.join("registry");
    let index = registry.join("index");
    fs::create_dir_all(index.join("pr/ob")).unwrap();
    fs::write(index.join("config.json"), serde_json::json!({"dl": format!("file://{}/{{crate}}-{{version}}.crate", registry.display())}).to_string()).unwrap();
    let mut records = Vec::new();
    let mut old_checksum = String::new();
    for version in ["1.0.0", "2.0.0"] {
        let name = format!("probe-dependency-{version}");
        let package = registry.join(&name);
        fs::create_dir_all(package.join("src")).unwrap();
        fs::write(package.join("src/lib.rs"), "pub fn dependency() {}\n").unwrap();
        fs::write(
            package.join("Cargo.toml"),
            format!("[package]\nname='probe-dependency'\nversion='{version}'\nedition='2021'\n"),
        )
        .unwrap();
        fs::write(
            package.join("build.rs"),
            "fn main() { panic!(\"trusted fetch ran a build script\"); }\n",
        )
        .unwrap();
        let archive = registry.join(format!("{name}.crate"));
        assert!(Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&registry)
            .arg(&name)
            .status()
            .unwrap()
            .success());
        let checksum = format!("{:x}", Sha256::digest(fs::read(archive).unwrap()));
        records.push(serde_json::json!({"name":"probe-dependency", "vers":version, "deps":[], "cksum":checksum, "features":{}, "yanked":false}).to_string());
        if version == "1.0.0" {
            old_checksum = checksum;
        }
    }
    fs::write(
        index.join("pr/ob/probe-dependency"),
        records.join("\n") + "\n",
    )
    .unwrap();
    crate::workspace::git(&index, &["init", "-q"]).unwrap();
    crate::workspace::git(&index, &["add", "."]).unwrap();
    crate::workspace::git(
        &index,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "registry fixture",
        ],
    )
    .unwrap();
    let registry_url = format!("file://{}", index.display());
    fs::write(workspace.join("Cargo.toml"), "[package]\nname='probe'\nversion='0.2.0'\nedition='2021'\n[workspace]\n[dependencies]\nprobe-dependency={version='=2.0.0',registry='fixture'}\n").unwrap();
    let lock = format!("version=4\n[[package]]\nname='probe'\nversion='0.2.0'\ndependencies=['probe-dependency']\n[[package]]\nname='probe-dependency'\nversion='1.0.0'\nsource='registry+{registry_url}'\nchecksum='{old_checksum}'\n");
    fs::write(workspace.join("Cargo.lock"), &lock).unwrap();
    let sibling = root.join("sibling");
    fs::create_dir_all(sibling.join("src")).unwrap();
    fs::write(
        sibling.join("Cargo.toml"),
        "[package]\nname='sibling'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
    )
    .unwrap();
    fs::write(sibling.join("src/lib.rs"), "pub fn sibling() {}\n").unwrap();
    fs::write(
        sibling.join("Cargo.lock"),
        "sibling lock must stay untouched\n",
    )
    .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(workspace.join("Cargo.toml"))
        .unwrap()
        .write_all(b"sibling={path='../sibling'}\n")
        .unwrap();
    fs::create_dir(workspace.join(".cargo")).unwrap();
    let config = "[build]\nrustc-wrapper='/must-not-run-checkout-wrapper'\n[source.crates-io]\nreplace-with='must-not-load-checkout-config'\n";
    fs::write(workspace.join(".cargo/config.toml"), config).unwrap();
    let state = root.join("state");
    fs::create_dir(&state).unwrap();
    let rustup = PathBuf::from(std::env::var_os("RUSTUP_HOME").unwrap());
    for tenant in [false, true] {
        let cargo = root.join(format!("cargo-{tenant}"));
        fs::create_dir(&cargo).unwrap();
        fs::write(
            cargo.join("config.toml"),
            format!("[registries.fixture]\nindex='{registry_url}'\n"),
        )
        .unwrap();
        let mut private = PathBuf::new();
        let mut locked_environment = None;
        let warned = prepare_via(
            &workspace,
            tenant,
            |path, locked| {
                crate::workspace::dependencies::fetch_command_with_lock(
                    &state,
                    path,
                    &BTreeMap::new(),
                    &cargo,
                    &rustup,
                    locked,
                )
            },
            |command, limit, copy| {
                let mut environment: BTreeMap<_, _> = command
                    .get_envs()
                    .map(|(key, value)| (key.to_owned(), value.map(|value| value.to_owned())))
                    .collect();
                environment.remove(OsStr::new("CARGO_NET_OFFLINE"));
                assert_eq!(command.get_current_dir(), Some(state.as_path()));
                if let Some(copy) = copy {
                    private = copy.to_owned();
                    assert_eq!(Some(&environment), locked_environment.as_ref());
                } else {
                    locked_environment = Some(environment);
                }
                bounded(command, limit.unwrap_or(Duration::from_secs(10)))
            },
        )
        .unwrap();
        assert!(warned);
        assert!(!private.as_os_str().is_empty());
        assert!(!private.exists());
        assert_eq!(
            fs::read(workspace.join("Cargo.lock")).unwrap(),
            lock.as_bytes()
        );
        assert_eq!(
            fs::read_to_string(workspace.join(".cargo/config.toml")).unwrap(),
            config
        );
        assert_eq!(
            fs::read_to_string(sibling.join("Cargo.lock")).unwrap(),
            "sibling lock must stay untouched\n"
        );
        assert!(!workspace.join("target").exists());
        assert!(fs::read_dir(cargo.join("registry/cache"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .join("probe-dependency-2.0.0.crate")
                .is_file()));
        // Resolving a second, expendable tree offline proves the new cache is
        // usable by cargo update without letting that check rewrite the job.
        let copy = PrivateCopy::new(&workspace).unwrap();
        let mut offline = crate::workspace::dependencies::fetch_command_with_lock(
            &state,
            &copy.workspace,
            &BTreeMap::new(),
            &cargo,
            &rustup,
            false,
        )
        .unwrap();
        offline.arg("--offline").env("CARGO_NET_OFFLINE", "true");
        bounded(&mut offline, Duration::from_secs(10)).unwrap();
        assert!(fs::read_to_string(copy.workspace.join("Cargo.lock"))
            .unwrap()
            .contains("2.0.0"));
        assert_eq!(
            fs::read(workspace.join("Cargo.lock")).unwrap(),
            lock.as_bytes()
        );
    }
}
