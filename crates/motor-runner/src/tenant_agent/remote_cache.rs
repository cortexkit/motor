//! Credentials arrive only on the host control listener. Stop the idle runner
//! before replacing them so an old fill cannot retain a withdrawn write key.
use super::*;
use std::{
    future::Future,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
};

const DESTINATION: &str = "cache-provisioning";

pub(super) fn parse(value: Value) -> io::Result<RemoteCacheRequest> {
    let request: RemoteCacheRequest = serde_json::from_value(value)
        .map_err(|_| io::Error::other("remote_cache_invalid_request"))?;
    if !request.valid() {
        return Err(io::Error::other("remote_cache_invalid_request"));
    }
    Ok(request)
}

pub(super) async fn apply(request: RemoteCacheRequest) -> io::Result<Value> {
    if snapshot::frozen() || !Path::new("/run/motor-tenant-generation").exists() {
        return Err(io::Error::other("remote_cache_not_attached"));
    }
    activate(super::forward, || async move {
        tokio::task::spawn_blocking(move || {
            let args = RunnerArgs::read(Path::new(ARGS))?;
            command("systemctl", &["stop", "motor-tenant-runner.service"])?;
            let result = (|| {
                let parent = paths::Directory::absolute(Path::new("/tenant/state/main"), 0)?;
                install(&parent, request)
            })();
            // install consumes and drops the credential payload before restart.
            let restart = start_runner(&args, Path::new("/tenant/state/main/runner.sock"));
            result.and(restart)
        })
        .await
        .map_err(io::Error::other)?
    })
    .await
}

async fn activate<F, Fut, A, AFut>(mut control: F, apply: A) -> io::Result<Value>
where
    F: FnMut(Frame) -> Fut,
    Fut: Future<Output = io::Result<Value>>,
    A: FnOnce() -> AFut,
    AFut: Future<Output = io::Result<()>>,
{
    let drained = control(Frame::DrainIfIdle).await;
    let ready = drained.as_ref().is_ok_and(|value| {
        value["ok"] == true
            && value.get("blocked_by") == Some(&Value::Null)
            && value["count"].as_u64() == Some(0)
    });
    if !ready {
        if let Ok(value) = &drained {
            let cause = blocker(&value["blocked_by"]);
            if value["ok"] == true
                && cause != "status_unavailable"
                && value["count"].as_u64().is_some_and(|count| count > 0)
            {
                return Ok(
                    json!({"ok":false,"error":"remote_cache_busy","cause":cause,"count":value["count"].as_u64().unwrap_or(0)}),
                );
            }
        }
        let resumed = control(Frame::Resume)
            .await
            .is_ok_and(|value| value["ok"] == true && value["resumed"] == true);
        return Ok(
            json!({"ok":false,"error":"remote_cache_apply_failed","runner_drained":!resumed}),
        );
    }
    let result = apply().await;
    let healthy = control(Frame::Status)
        .await
        .is_ok_and(|value| value["ok"] == true && value["health"].is_object());
    // On failure before restart the original process may still be alive and
    // drained. Always try to reopen it; report when that cannot be confirmed.
    let resumed = control(Frame::Resume)
        .await
        .is_ok_and(|value| value["ok"] == true && value["resumed"] == true);
    if result.is_err() || !healthy || !resumed {
        return Ok(
            json!({"ok":false,"error":"remote_cache_apply_failed","runner_drained":!resumed}),
        );
    }
    Ok(json!({"ok":true}))
}

fn blocker(value: &Value) -> &'static str {
    match value.as_str() {
        Some("queued") => "queued",
        Some("running") => "running",
        Some("fill") => "fill",
        Some("starting") => "starting",
        Some("reserved") => "reserved",
        Some("preparing") => "preparing",
        Some("admitting") => "admitting",
        Some("admission_save") => "admission_save",
        Some("base_finish") => "base_finish",
        Some("base_settle") => "base_settle",
        Some("finalizing") => "finalizing",
        Some("writer") => "writer",
        _ => "status_unavailable",
    }
}

fn install(parent: &paths::Directory, request: RemoteCacheRequest) -> io::Result<()> {
    install_before_swap(parent, request, || Ok(()))
}

fn install_before_swap(
    parent: &paths::Directory,
    request: RemoteCacheRequest,
    before_swap: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let root = parent.fd_path();
    let destination = root.join(DESTINATION);
    let existing = match fs::symlink_metadata(&destination) {
        Ok(_) => Some(paths::Directory::open(&destination, unsafe {
            libc::geteuid()
        })?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    // Unique directories also leave crash debris unreferenced. Never reuse a
    // partially written set. Only the canonical name is read after restart.
    let temporary = root.join(format!(".cache-provisioning-{}", uuid::Uuid::now_v7()));
    fs::DirBuilder::new().mode(0o700).create(&temporary)?;
    let result = (|| {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        own(&directory)?;
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        for (name, bytes, mode) in [
            ("config.json", Some(request.config), 0o444),
            ("read-key", Some(request.read_key), 0o400),
            ("write-key", request.write_key, 0o400),
        ] {
            if let Some(bytes) = bytes {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(temporary.join(name))?;
                own(&file)?;
                file.set_permissions(fs::Permissions::from_mode(mode))?;
                file.write_all(bytes.as_bytes())?;
                file.sync_all()?;
            }
        }
        directory.sync_all()?;
        // Persist the new name before exchange, so either complete set survives
        // a crash. renameat2 exchanges nonempty directories in one operation.
        fs::File::open(&root)?.sync_all()?;
        before_swap()?;
        if existing.is_some() {
            exchange(&temporary, &destination)?;
        } else {
            fs::rename(&temporary, &destination)?;
        }
        fs::File::open(&root)?.sync_all()
    })();
    // After exchange this is the old directory; before exchange it is only the
    // incomplete new one. remove_dir_all does not traverse symlinks in either.
    if temporary.exists() {
        let _ = fs::remove_dir_all(&temporary);
        let _ = fs::File::open(&root).and_then(|file| file.sync_all());
    }
    result
}

fn own(file: &fs::File) -> io::Result<()> {
    // The agent runs as root: set both owner and group explicitly, rather than
    // inheriting a group from a setgid directory on the tenant disk.
    if unsafe { libc::fchown(file.as_raw_fd(), libc::geteuid(), libc::getegid()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn exchange(left: &Path, right: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let left = CString::new(left.as_os_str().as_bytes())?;
    let right = CString::new(right.as_os_str().as_bytes())?;
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::os::unix::fs::{symlink, MetadataExt};

    fn request(write: bool) -> RemoteCacheRequest {
        RemoteCacheRequest {
            op: "set_remote_cache".into(),
            config: "configuration".into(),
            read_key: "private-read-credential".into(),
            write_key: write.then(|| "private-write-credential".into()),
        }
    }

    fn parent(scratch: &ScratchDir) -> paths::Directory {
        let root = scratch.join("root");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        paths::Directory::open(&root, unsafe { libc::geteuid() }).unwrap()
    }

    #[test]
    fn remote_cache_replaces_whole_directory_with_exact_modes_and_withdraws_write_key() {
        let scratch = ScratchDir::new("remote-cache-replace");
        let parent = parent(&scratch);
        install(&parent, request(true)).unwrap();
        let destination = parent.fd_path().join(DESTINATION);
        let old = fs::File::open(destination.join("read-key")).unwrap();
        let old_inode = old.metadata().unwrap().ino();
        let mut next = request(false);
        next.read_key = "rotated".into();
        install(&parent, next).unwrap();
        assert_ne!(
            old_inode,
            fs::metadata(destination.join("read-key")).unwrap().ino()
        );
        assert_eq!(fs::read(destination.join("read-key")).unwrap(), b"rotated");
        assert!(!destination.join("write-key").exists());
        for (name, mode) in [("", 0o700), ("config.json", 0o444), ("read-key", 0o400)] {
            let metadata = fs::symlink_metadata(destination.join(name)).unwrap();
            assert_eq!(metadata.mode() & 0o7777, mode);
            assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
            assert_eq!(metadata.gid(), unsafe { libc::getegid() });
        }
    }

    #[test]
    fn remote_cache_refuses_links_and_non_directories_without_touching_target() {
        for link in [true, false] {
            let scratch = ScratchDir::new("remote-cache-links");
            let parent = parent(&scratch);
            let target = parent.fd_path().join("outside");
            fs::write(&target, "untouched").unwrap();
            let destination = parent.fd_path().join(DESTINATION);
            if link {
                symlink(&target, &destination).unwrap();
            } else {
                fs::write(&destination, "untouched").unwrap();
            }
            assert!(install(&parent, request(true)).is_err());
            assert_eq!(fs::read(&target).unwrap(), b"untouched");
        }
    }

    #[test]
    fn remote_cache_failed_swap_keeps_the_complete_old_set() {
        let scratch = ScratchDir::new("remote-cache-failed-swap");
        let parent = parent(&scratch);
        install(&parent, request(true)).unwrap();
        let destination = parent.fd_path().join(DESTINATION);
        let old_config = fs::read(destination.join("config.json")).unwrap();
        let mut next = request(false);
        next.config = "new-configuration".into();
        assert!(
            install_before_swap(&parent, next, || Err(io::Error::other("injected failure")))
                .is_err()
        );
        assert_eq!(
            fs::read(destination.join("config.json")).unwrap(),
            old_config
        );
        assert!(destination.join("write-key").exists());
        assert_eq!(fs::read_dir(parent.fd_path()).unwrap().count(), 1);
    }

    #[test]
    fn remote_cache_bounds_and_parse_errors_never_quote_credentials() {
        for value in [
            json!({"op":"set_remote_cache","config":"ok","read_key":{"private-read-credential":true}}),
            json!({"op":"set_remote_cache","config":"ok","read_key":"x".repeat(65537)}),
            json!({"op":"set_remote_cache","config":"ok","read_key":"ok","private-write-credential":true}),
        ] {
            let error = parse(value).err().unwrap().to_string();
            assert_eq!(error, "remote_cache_invalid_request");
        }
        assert!(parse(serde_json::to_value(request(true)).unwrap()).is_ok());
    }

    #[tokio::test]
    async fn remote_cache_probe_is_an_agent_extension_and_invalid_credentials_are_redacted() {
        let lock = tokio::sync::Mutex::new(());
        assert_eq!(
            dispatch_value(json!({"op":"remote_cache_probe"}), &lock)
                .await
                .unwrap(),
            json!({"ok":true,"remote_cache_provisioning":1})
        );
        let result = dispatch_value(json!({"op":"set_remote_cache","config":"ok","read_key":{"READ-SECRET":"WRITE-SECRET"}}), &lock).await;
        assert_eq!(
            agent_result(result),
            json!({"ok":false,"error":"remote_cache_invalid_request"})
        );
    }

    #[tokio::test]
    async fn remote_cache_busy_refuses_immediately_and_failures_reopen_admission() {
        for cause in [
            "queued",
            "starting",
            "running",
            "finalizing",
            "fill",
            "admission_save",
        ] {
            let reply = activate(
                |frame| {
                    assert_eq!(frame, Frame::DrainIfIdle);
                    std::future::ready(Ok(json!({"ok":true,"blocked_by":cause,"count":2})))
                },
                || async { panic!("busy runner must never be stopped") },
            )
            .await
            .unwrap();
            assert_eq!(
                reply,
                json!({"ok":false,"error":"remote_cache_busy","cause":cause,"count":2})
            );
        }
        let mut calls = Vec::new();
        let reply = activate(
            |frame| {
                calls.push(frame.clone());
                std::future::ready(Ok(match frame {
                    Frame::DrainIfIdle => json!({"ok":true,"blocked_by":null,"count":0}),
                    Frame::Status => json!({"ok":true,"health":{}}),
                    Frame::Resume => json!({"ok":true,"resumed":true}),
                    _ => unreachable!(),
                }))
            },
            || async { Err(io::Error::other("READ-SECRET WRITE-SECRET")) },
        )
        .await
        .unwrap();
        assert_eq!(
            reply,
            json!({"ok":false,"error":"remote_cache_apply_failed","runner_drained":false})
        );
        assert_eq!(calls, [Frame::DrainIfIdle, Frame::Status, Frame::Resume]);
    }

    #[tokio::test]
    async fn remote_cache_lost_idle_drain_reply_attempts_resume_and_reports_when_still_drained() {
        for resumed in [false, true] {
            let mut calls = Vec::new();
            let reply = activate(
                |frame| {
                    calls.push(frame.clone());
                    std::future::ready(match frame {
                        Frame::DrainIfIdle => Err(io::Error::other("READ-SECRET")),
                        Frame::Resume => Ok(json!({"ok":true,"resumed":resumed})),
                        _ => unreachable!(),
                    })
                },
                || async { panic!("no credentials may be written without confirmed idle drain") },
            )
            .await
            .unwrap();
            assert_eq!(
                reply,
                json!({"ok":false,"error":"remote_cache_apply_failed","runner_drained":!resumed})
            );
            assert_eq!(calls, [Frame::DrainIfIdle, Frame::Resume]);
        }
    }
}
