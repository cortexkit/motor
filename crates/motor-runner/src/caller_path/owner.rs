//! Runner startup checks existing ownership before creating its state directory.
//! Empty roots are claimed only after catalog mappings and filesystem probes pass.
use super::{startup, CallerRoot};
use crate::{storage::invalid, Config};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::{fs, io, path::PathBuf};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Owner {
    instance: String,
    state_dir: PathBuf,
}

fn expected(config: &Config) -> io::Result<Owner> {
    Ok(Owner {
        instance: config.instance.clone(),
        state_dir: startup::canonical_future(&config.state_dir())?,
    })
}

fn refusal(root: &CallerRoot, owner: &Owner, reason: impl std::fmt::Display) -> io::Error {
    invalid(format!(
        "caller root {} marker {}: expected instance {:?}, state directory {}; {reason}",
        root.host_root.display(),
        root.host_root.join(".motor-owner").display(),
        owner.instance,
        owner.state_dir.display()
    ))
}

pub(crate) fn check(config: &Config) -> io::Result<()> {
    for root in &config.caller_roots {
        check_root(config, root)?;
    }
    Ok(())
}

pub(crate) fn check_root(config: &Config, root: &CallerRoot) -> io::Result<()> {
    CallerRoot::parse(&format!(
        "{}={}",
        root.caller_prefix,
        root.host_root.display()
    ))?;
    let owner = expected(config)?;
    check_inner(root, &owner)
        .and_then(|directory| {
            #[cfg(target_os = "linux")]
            if let Some(directory) = directory {
                let name = PathBuf::from(format!(".motor-trash-{}", config.instance));
                match directory_at(&directory, &name) {
                    Ok(trash) => super::trash::validate(&trash)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            #[cfg(not(target_os = "linux"))]
            let _ = directory;
            Ok(())
        })
        .map_err(|e| refusal(root, &owner, e))
}

#[cfg(target_os = "linux")]
pub(crate) fn directory(path: &Path) -> io::Result<fs::File> {
    directory_at(
        &fs::File::open("/")?,
        path.strip_prefix("/").map_err(io::Error::other)?,
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn directory_at(root: &fs::File, path: &Path) -> io::Result<fs::File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    let mut directory = root.try_clone()?;
    for component in path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(invalid("expected a relative directory path"));
        }
        let name = CString::new(component.as_os_str().as_bytes())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        directory = unsafe { fs::File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn check_inner(root: &CallerRoot, owner: &Owner) -> io::Result<Option<fs::File>> {
    use std::{
        io::Read,
        os::fd::{AsRawFd, FromRawFd},
    };
    let directory = directory(&root.host_root)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".motor-owner".as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            // Enumerate the opened directory, not its mutable pathname.
            if fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?
                .next()
                .is_none()
            {
                return Ok(Some(directory));
            }
            return Err(invalid("marker is absent but the host root is not empty"));
        }
        return Err(error);
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(invalid("marker is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    let found: Owner = serde_json::from_slice(&bytes)
        .map_err(|e| invalid(format!("malformed owner marker: {e}")))?;
    if found.instance != owner.instance
        || found.state_dir.as_os_str() != owner.state_dir.as_os_str()
    {
        return Err(invalid(format!(
            "found instance {:?}, state directory {}",
            found.instance,
            found.state_dir.display()
        )));
    }
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
fn check_inner(_: &CallerRoot, _: &Owner) -> io::Result<Option<fs::File>> {
    Err(invalid("caller roots require Linux"))
}

pub(crate) fn claim(config: &Config) -> io::Result<()> {
    for root in &config.caller_roots {
        let owner = expected(config)?;
        claim_inner(root, &owner, |_| Ok(())).map_err(|e| refusal(root, &owner, e))?;
        trash(root, &config.instance).map_err(|e| refusal(root, &owner, e))?;
    }
    Ok(())
}

/// Startup can find regular files and symlinks as well as detached trees in
/// caller-root trash. Keep the trash descriptor open throughout the purge so
/// replacing an ancestor cannot redirect a removal outside the host root.
pub(crate) fn purge_trash(config: &Config) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        for entry in super::trash::entries(config)? {
            entry.remove(config.isolate_jobs, config.boat_profile)?;
        }
    }
    #[cfg(not(target_os = "linux"))]
    if !config.caller_roots.is_empty() {
        return Err(invalid("caller roots require Linux"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn trash(root: &CallerRoot, instance: &str) -> io::Result<()> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };
    let directory = directory(&root.host_root)?;
    let name = CString::new(format!(".motor-trash-{instance}"))?;
    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0
        && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
    {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let trash = unsafe { fs::File::from_raw_fd(fd) };
    super::trash::validate(&trash)?;
    directory.sync_all()
}

#[cfg(not(target_os = "linux"))]
fn trash(_: &CallerRoot, _: &str) -> io::Result<()> {
    Err(invalid("caller roots require Linux"))
}

#[cfg(target_os = "linux")]
fn claim_inner(
    root: &CallerRoot,
    owner: &Owner,
    mut checkpoint: impl FnMut(bool) -> io::Result<()>,
) -> io::Result<()> {
    use std::{
        ffi::CString,
        io::Write,
        os::fd::{AsRawFd, FromRawFd},
    };
    let Some(directory) = check_inner(root, owner)? else {
        return Ok(());
    };
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    file.write_all(&serde_json::to_vec(owner).map_err(invalid)?)?;
    file.sync_all()?;
    checkpoint(false)?;
    let linked = unsafe {
        libc::linkat(
            file.as_raw_fd(),
            c"".as_ptr(),
            directory.as_raw_fd(),
            c".motor-owner".as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if linked != 0 {
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EPERM | libc::ENOENT)) {
            return Err(error);
        }
        // Unprivileged Linux can publish an O_TMPFILE through its own proc fd.
        let source = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        if unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                source.as_ptr(),
                directory.as_raw_fd(),
                c".motor-owner".as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    directory.sync_all()?;
    checkpoint(true)?;
    if check_inner(root, owner)?.is_some() {
        return Err(invalid("owner marker disappeared after publication"));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn claim_inner(_: &CallerRoot, _: &Owner, _: impl FnMut(bool) -> io::Result<()>) -> io::Result<()> {
    Err(invalid("caller roots require Linux"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::symlink},
        sync::{Arc, Barrier},
    };

    fn config(scratch: &Path) -> Config {
        let mut config = Config::new(scratch.join("state"), "main");
        let host = scratch.join("home");
        fs::create_dir_all(&host).unwrap();
        config
            .caller_roots
            .push(CallerRoot::parse(&format!("/home={}", host.display())).unwrap());
        config
    }

    #[test]
    fn marker_claim_is_durable_and_names_instance_and_state() {
        let scratch = ScratchDir::new("caller-owner");
        let config = config(&scratch);
        check(&config).unwrap();
        assert!(!config.state_dir().exists());
        claim(&config).unwrap();
        check(&config).unwrap();
        let host = &config.caller_roots[0].host_root;
        let marker: Owner = crate::storage::read_json(&host.join(".motor-owner")).unwrap();
        assert_eq!(marker, expected(&config).unwrap());
        assert!(host.join(".motor-trash-main").is_dir());
        assert_eq!(fs::read_dir(host).unwrap().count(), 2);
    }

    #[test]
    fn absent_nonempty_malformed_and_nonregular_markers_refuse_without_state() {
        for kind in ["absent", "malformed", "directory", "fifo"] {
            let scratch = ScratchDir::new("caller-owner-invalid");
            let config = config(&scratch);
            let host = &config.caller_roots[0].host_root;
            let marker = host.join(".motor-owner");
            match kind {
                "absent" => fs::write(host.join("leftover"), b"secret").unwrap(),
                "malformed" => fs::write(&marker, b"not json").unwrap(),
                "directory" => fs::create_dir(&marker).unwrap(),
                "fifo" => {
                    let name = CString::new(marker.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                _ => unreachable!(),
            }
            let error = check(&config).unwrap_err().to_string();
            assert!(
                error.contains(host.to_str().unwrap()) && error.contains(marker.to_str().unwrap()),
                "{error}"
            );
            assert!(!config.state_dir().exists());
            assert_eq!(fs::read_dir(host).unwrap().count(), 1);
        }
    }

    #[test]
    fn symlink_marker_and_ancestor_never_open_outside_fifo() {
        let scratch = ScratchDir::new("caller-owner-no-follow");
        let mut config = config(&scratch);
        let outside = scratch.join("outside");
        fs::create_dir(&outside).unwrap();
        let fifo = outside.join(".motor-owner");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        let watcher = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(watcher >= 0);
        let mut watcher = unsafe { fs::File::from_raw_fd(watcher) };
        let directory_name = CString::new(outside.as_os_str().as_bytes()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(watcher.as_raw_fd(), directory_name.as_ptr(), libc::IN_OPEN)
            } >= 0
        );
        let marker = config.caller_roots[0].host_root.join(".motor-owner");
        symlink(&fifo, &marker).unwrap();
        assert!(check(&config).is_err());
        assert!(check_root(&config, &config.caller_roots[0]).is_err());
        fs::remove_file(marker).unwrap();
        let ancestor = scratch.join("link");
        symlink(&outside, &ancestor).unwrap();
        config.caller_roots[0].host_root = ancestor;
        assert!(check(&config).is_err());
        assert!(check_root(&config, &config.caller_roots[0]).is_err());
        assert!(!config.state_dir().exists());
        let mut events = [0u8; 4096];
        let observed = watcher.read(&mut events);
        assert!(
            matches!(observed, Err(ref e) if e.kind() == io::ErrorKind::WouldBlock),
            "outside directory or FIFO was opened: {observed:?}"
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn owner_mismatch_refuses_before_creating_attempted_state() {
        let scratch = ScratchDir::new("caller-owner-mismatch");
        let mut config = config(&scratch);
        claim(&config).unwrap();
        for other_instance in [false, true] {
            if other_instance {
                config.instance = "other".into();
            } else {
                config.state_root = scratch.join("different-state");
            }
            let error = check(&config).unwrap_err().to_string();
            assert!(
                error.contains("expected instance")
                    && error.contains("found instance")
                    && error.contains("state directory"),
                "{error}"
            );
            assert!(!config.state_dir().exists());
        }
    }

    #[test]
    fn unreadable_marker_refuses_without_creating_state() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = ScratchDir::new("caller-owner-unreadable");
        let config = config(&scratch);
        claim(&config).unwrap();
        let marker = config.caller_roots[0].host_root.join(".motor-owner");
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o0)).unwrap();
        // Root can read mode-000 files; only an unprivileged process exercises
        // the kernel's permission refusal.
        if unsafe { libc::geteuid() } != 0 {
            assert!(check(&config).is_err());
        }
        assert!(!config.state_dir().exists());
        fs::set_permissions(marker, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn concurrent_first_claims_have_exactly_one_winner() {
        let scratch = ScratchDir::new("caller-owner-race");
        let a = config(&scratch);
        let mut b = config(&scratch);
        b.state_root = scratch.join("other-state");
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [a, b]
            .into_iter()
            .map(|config| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let owner = expected(&config).unwrap();
                    claim_inner(&config.caller_roots[0], &owner, |linked| {
                        if !linked {
                            barrier.wait();
                        }
                        Ok(())
                    })
                })
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .filter(Result::is_ok)
                .count(),
            1
        );
        assert_eq!(fs::read_dir(scratch.join("home")).unwrap().count(), 1);
        assert!(!scratch.join("state").exists());
        assert!(!scratch.join("other-state").exists());
    }

    #[test]
    fn marker_crashes_before_link_leave_no_entry_and_after_link_can_resume() {
        for after_link in [false, true] {
            let scratch = ScratchDir::new("caller-owner-crash");
            let config = config(&scratch);
            let root = &config.caller_roots[0];
            let result = claim_inner(root, &expected(&config).unwrap(), |linked| {
                if linked == after_link {
                    Err(io::Error::other("simulated process death"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());
            assert_eq!(
                fs::read_dir(&root.host_root).unwrap().count(),
                usize::from(after_link)
            );
            claim(&config).unwrap();
            check(&config).unwrap();
            assert_eq!(fs::read_dir(&root.host_root).unwrap().count(), 2);
        }
    }

    #[test]
    fn marker_disappearing_before_readback_is_not_a_successful_claim() {
        let scratch = ScratchDir::new("caller-owner-readback");
        let config = config(&scratch);
        let root = &config.caller_roots[0];
        let result = claim_inner(root, &expected(&config).unwrap(), |linked| {
            if linked {
                fs::remove_file(root.host_root.join(".motor-owner"))?;
            }
            Ok(())
        });
        assert!(result.unwrap_err().to_string().contains("disappeared"));
        assert_eq!(fs::read_dir(&root.host_root).unwrap().count(), 0);
    }

    #[test]
    fn caller_trash_purge_removes_files_trees_and_links_not_their_targets() {
        let scratch = ScratchDir::new("caller-trash-purge");
        let config = config(&scratch);
        claim(&config).unwrap();
        let trash = config.caller_roots[0].host_root.join(".motor-trash-main");
        let outside = scratch.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"unchanged").unwrap();
        fs::write(trash.join("file"), b"retired").unwrap();
        fs::create_dir(trash.join("tree")).unwrap();
        fs::write(trash.join("tree/file"), b"retired").unwrap();
        symlink(&outside, trash.join("link")).unwrap();
        symlink(&outside, trash.join("tree/link")).unwrap();
        purge_trash(&config).unwrap();
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"unchanged");
        check(&config).unwrap();
    }
}
