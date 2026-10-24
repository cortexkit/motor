use std::{
    io::{self, Read},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const ONLINE_TIMEOUT: Duration = Duration::from_secs(120);

/// A fresh Cargo home prevents a warm tenant cache or the offline fallback from
/// hiding a broken online preparation path. The caller runs the production helper.
pub(super) fn check_online_via(
    scratch: &std::path::Path,
    rustup: &std::path::Path,
    run: impl FnOnce(&mut Command, Duration) -> io::Result<()>,
) -> io::Result<()> {
    use std::{collections::BTreeMap, fs};
    fs::create_dir(scratch)?;
    let state = scratch.join("state");
    let workspace = scratch.join("workspace");
    let cargo = scratch.join("cargo-home");
    for path in [&state, &workspace.join("src"), &cargo] {
        fs::create_dir_all(path)?;
    }
    fs::write(
        workspace.join("Cargo.toml"),
        include_str!("../../test-fixtures/cargo-fetch/Cargo.toml"),
    )?;
    fs::write(
        workspace.join("Cargo.lock"),
        include_str!("../../test-fixtures/cargo-fetch/Cargo.lock"),
    )?;
    fs::write(
        workspace.join("src/lib.rs"),
        include_str!("../../test-fixtures/cargo-fetch/src/lib.rs"),
    )?;
    let mut command = crate::workspace::dependencies::fetch_command(
        &state,
        &workspace,
        &BTreeMap::new(),
        &cargo,
        rustup,
    )?;
    command.env("CARGO_NET_OFFLINE", "false");
    run(&mut command, ONLINE_TIMEOUT)?;
    let cache = cargo.join("registry/cache");
    let downloaded = cache.is_dir()
        && fs::read_dir(cache)?
            .any(|entry| entry.is_ok_and(|entry| entry.path().join("itoa-1.0.18.crate").is_file()));
    if !downloaded {
        return Err(io::Error::other(
            "trusted online fetch did not download itoa 1.0.18 into the cold cache",
        ));
    }
    Ok(())
}

/// Runs a tenant runner's root fetch: online first, then an offline fallback
/// that can still use the cache during an outage. `run` runs each attempt
/// under its limit, as a plain bounded child or in a sibling snapshot helper
/// unit.
pub(super) fn prepare_via(
    command: impl FnMut() -> io::Result<Command>,
    run: impl FnMut(&mut Command, Duration) -> io::Result<()>,
) -> io::Result<()> {
    prepare_with_limits_via(command, ONLINE_TIMEOUT, Duration::from_secs(30), run)
}

#[cfg(test)]
fn prepare_with_limits(
    command: impl FnMut() -> io::Result<Command>,
    online: Duration,
    offline: Duration,
) -> io::Result<()> {
    prepare_with_limits_via(command, online, offline, |command, limit| {
        bounded(command, limit)
    })
}

fn prepare_with_limits_via(
    mut command: impl FnMut() -> io::Result<Command>,
    online: Duration,
    offline: Duration,
    mut run: impl FnMut(&mut Command, Duration) -> io::Result<()>,
) -> io::Result<()> {
    let online_result = crate::setup_log::phase("fetch cargo dependencies online", || {
        run(command()?.env("CARGO_NET_OFFLINE", "false"), online)
    });
    let Err(online_error) = online_result else {
        return Ok(());
    };
    crate::setup_log::phase("fetch cargo dependencies offline fallback", || {
        run(command()?.arg("--offline").env("CARGO_NET_OFFLINE", "true"), offline)
    }).map_err(|offline_error| io::Error::new(offline_error.kind(), format!(
        "tenant dependency preparation failed: online fetch: {online_error}; offline fallback could not supply locked sources: {offline_error}; provision the tenant Cargo registry index, crate cache and Git sources before running this locked workspace"
    )))
}

/// Even offline Cargo can wait for a cache lock or a stuck toolchain probe.
/// Bound the entire Cargo fetch, drain diagnostics, and stop its process group
/// before releasing the setup reservation; no job command has started yet.
pub(super) fn bounded(command: &mut Command, timeout: Duration) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let mut stderr = child.stderr.take().unwrap();
    let mut diagnostic = Vec::new();
    let result = (|| {
        let flags = unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(io::Error::last_os_error());
        }
        let started = Instant::now();
        loop {
            drain(&mut stderr, &mut diagnostic)?;
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // Leave an exited leader unreaped until group cleanup is complete so
            // its PID cannot be reused for an unrelated process group.
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { info.si_pid() } != 0 {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return Err(io::Error::new(io::ErrorKind::TimedOut, format!(
                    "cargo dependency fetch exceeded {} ms (cache lock or toolchain/fetch stall)", timeout.as_millis()
                )));
            }
            std::thread::park_timeout(Duration::from_millis(10));
        }
    })();
    if result.as_ref().err().and_then(io::Error::raw_os_error) != Some(libc::ECHILD) {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let status = child.wait();
    let _ = drain(&mut stderr, &mut diagnostic);
    result?;
    let status = status?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "cargo fetch {status}: {}",
            String::from_utf8_lossy(&diagnostic)
        )))
    }
}

fn drain(stderr: &mut impl Read, diagnostic: &mut Vec<u8>) -> io::Result<()> {
    let mut bytes = [0; 4096];
    // Bound each pass even if a child writes continuously, so output cannot
    // starve the deadline. Continue draining after the retained prefix is full.
    for _ in 0..16 {
        match stderr.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => diagnostic.extend_from_slice(&bytes[..n.min(8192 - diagnostic.len())]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_online_qualification_requires_a_new_cache_and_a_downloaded_crate() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-fetch-qualification");
        let scratch = root.join("probe");
        let rustup = root.join("rustup");
        let error = check_online_via(&scratch, &rustup, |command, limit| {
            assert_eq!(limit, Duration::from_secs(120));
            let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
            assert_eq!(
                env[std::ffi::OsStr::new("CARGO_NET_OFFLINE")],
                Some(std::ffi::OsStr::new("false"))
            );
            assert_eq!(
                env[std::ffi::OsStr::new("CARGO_HOME")],
                Some(scratch.join("cargo-home").as_os_str())
            );
            assert!(!scratch.join("cargo-home/registry").exists());
            assert!(command.get_args().any(|arg| arg == "--locked"));
            assert!(!command.get_args().any(|arg| arg == "--offline"));
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("did not download itoa 1.0.18"));
        let error =
            check_online_via(&scratch, &rustup, |_, _| panic!("reused a warm cache")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        let fresh = root.join("fresh");
        check_online_via(&fresh, &rustup, |_, _| {
            let cache = fresh.join("cargo-home/registry/cache/index.crates.io-test");
            std::fs::create_dir_all(&cache)?;
            std::fs::write(cache.join("itoa-1.0.18.crate"), "downloaded")
        })
        .unwrap();
        let error = check_online_via(&root.join("failed"), &rustup, |_, _| {
            Err(io::Error::other("online DNS failure"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "online DNS failure");
    }

    #[test]
    fn tenant_online_fetch_uses_persistent_cache_and_skips_fallback_on_success() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-fetch-online");
        let calls = std::cell::Cell::new(0);
        prepare_with_limits(|| {
            calls.set(calls.get() + 1);
            let mut command = Command::new("sh");
            command.args(["-c", "test \"$CARGO_NET_OFFLINE\" = false && test $# = 0 && touch \"$CARGO_HOME/cached\"", "sh"])
                .env("CARGO_HOME", &*root);
            Ok(command)
        }, Duration::from_secs(2), Duration::from_secs(2)).unwrap();
        assert_eq!(calls.get(), 1);
        assert!(root.join("cached").is_file());
    }

    #[test]
    fn tenant_network_failure_falls_back_to_existing_offline_cache() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-fetch-fallback");
        std::fs::write(root.join("cached"), "verified source").unwrap();
        let calls = std::cell::Cell::new(0);
        prepare_with_limits(|| {
            let mut command = Command::new("sh");
            command.args(["-c", if calls.get() == 0 {
                "test \"$CARGO_NET_OFFLINE\" = false || exit 99; exit 76"
            } else {
                "test \"$CARGO_NET_OFFLINE\" = true && test \"$1\" = --offline && test -f \"$CARGO_HOME/cached\""
            }, "sh"]).env("CARGO_HOME", &*root);
            calls.set(calls.get() + 1);
            Ok(command)
        }, Duration::from_secs(2), Duration::from_secs(2)).unwrap();
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn tenant_fetch_deadline_kills_a_stuck_fetch_and_its_child() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-fetch-deadline");
        let pidfile = root.join("child-pid");
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & echo $! > \"$1\"; wait", "sh"])
            .arg(&pidfile);
        let started = Instant::now();
        let error = bounded(&mut command, Duration::from_millis(200)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error
            .to_string()
            .contains("cache lock or toolchain/fetch stall"));
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: u32 = std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let live = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .is_some_and(|stat| {
                    !matches!(
                        stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
                        Some("Z" | "X")
                    )
                });
            if !live {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fetch descendant survived the setup deadline"
            );
            std::thread::park_timeout(Duration::from_millis(5));
        }
    }

    #[test]
    fn tenant_fetch_drains_and_caps_subprocess_diagnostics() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 65536 /dev/zero >&2; exit 7"]);
        let error = bounded(&mut command, Duration::from_secs(5)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("exit status: 7"));
        assert!(error.to_string().len() <= 8300);
        assert!(bounded(
            Command::new("true").args([] as [&str; 0]),
            Duration::from_secs(5)
        )
        .is_ok());
    }
}

#[cfg(test)]
mod offline_tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    #[test]
    fn tenant_fetch_with_empty_registry_fails_clearly_offline_without_starting_a_build() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-fetch-offline");
        let state = root.join("state");
        let workspace = root.join("workspace");
        let cargo = root.join("cargo-home");
        for path in [&state, &workspace.join("src"), &cargo] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(workspace.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
        fs::write(workspace.join("Cargo.toml"), "[package]\nname='tenant-cache-probe'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nmotor_missing_cache_probe='=0.1.0'\n").unwrap();
        let lock = "version = 4\n[[package]]\nname = 'tenant-cache-probe'\nversion = '0.1.0'\ndependencies = ['motor_missing_cache_probe']\n[[package]]\nname = 'motor_missing_cache_probe'\nversion = '0.1.0'\nsource = 'registry+https://github.com/rust-lang/crates.io-index'\nchecksum = '0000000000000000000000000000000000000000000000000000000000000000'\n";
        fs::write(workspace.join("Cargo.lock"), lock).unwrap();
        let rustup = PathBuf::from(std::env::var_os("RUSTUP_HOME").unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".rustup")
                .into_os_string()
        }));
        let calls = std::cell::Cell::new(0);
        let error = super::prepare_with_limits(
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    return Ok(std::process::Command::new("false"));
                }
                crate::workspace::dependencies::fetch_command(
                    &state,
                    &workspace,
                    &BTreeMap::new(),
                    &cargo,
                    &rustup,
                )
            },
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no matching package named"), "{error}");
        assert!(error.contains("offline"), "{error}");
        assert!(
            error.contains("offline fallback could not supply locked sources"),
            "{error}"
        );
        assert!(
            error.contains("provision the tenant Cargo registry index"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("Cargo.lock")).unwrap(),
            lock
        );
        assert!(!workspace.join("target").exists());
        assert!(!cargo.join("registry/index").exists());
    }
}
