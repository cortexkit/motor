//! Portable process-argument parsing and job attribution for macOS qualification tests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use std::{
    io,
    path::{Path, PathBuf},
};

pub fn procargs(bytes: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let invalid = || io::Error::other("invalid KERN_PROCARGS2 response");
    let count = i32::from_ne_bytes(bytes.get(..4).ok_or_else(invalid)?.try_into().unwrap());
    if count <= 0 {
        return Err(invalid());
    }
    let mut rest = &bytes[4..];
    let executable = rest.iter().position(|b| *b == 0).ok_or_else(invalid)?;
    rest = &rest[executable + 1..];
    while rest.first() == Some(&0) {
        rest = &rest[1..];
    }
    let mut args = Vec::new();
    for _ in 0..count {
        let end = rest.iter().position(|b| *b == 0).ok_or_else(invalid)?;
        args.push(rest[..end].to_vec());
        rest = &rest[end + 1..];
    }
    Ok(args)
}

pub fn vm_config_argument(args: &[Vec<u8>]) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let values: Vec<_> = args
        .windows(2)
        .filter(|pair| pair[0] == b"--vm-config")
        .map(|pair| PathBuf::from(std::ffi::OsString::from_vec(pair[1].clone())))
        .collect();
    if values.len() != 1 || !values[0].is_absolute() {
        return Err(io::Error::other(
            "expected one absolute daemon --vm-config argument",
        ));
    }
    Ok(values[0].clone())
}

pub fn distinct_workspaces(first: &Path, second: &Path) -> io::Result<()> {
    let root = |path: &Path| -> io::Result<PathBuf> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "qualification workspace is not a git checkout",
            ));
        }
        std::fs::canonicalize(Path::new(
            std::str::from_utf8(&output.stdout)
                .map_err(io::Error::other)?
                .trim(),
        ))
    };
    if root(first)? == root(second)? {
        return Err(io::Error::other(
            "concurrent jobs require distinct workspace keys (git roots)",
        ));
    }
    Ok(())
}

pub fn attributed_services<T: Copy + Eq>(
    job_ids: &[uuid::Uuid],
    records: &[(uuid::Uuid, Option<T>, bool)],
    alive: impl Fn(T) -> bool,
) -> Vec<T> {
    let mut services = Vec::new();
    for (id, service, open) in records {
        if job_ids.contains(id) && *open {
            if let Some(service) = service.filter(|s| alive(*s)) {
                if !services.contains(&service) {
                    services.push(service);
                }
            }
        }
    }
    services
}

pub fn entries(path: &Path) -> io::Result<Vec<PathBuf>> {
    match std::fs::read_dir(path) {
        Ok(entries) => entries.map(|entry| entry.map(|e| e.path())).collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub fn final_record(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "json")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok())
}

pub fn collect_capture(root: &Path, relative: &Path, evidence: &Path) -> io::Result<bool> {
    use std::{
        ffi::CString,
        fs::File,
        io::Write,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
    };
    let mut directory = File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    // The job controls every descendant. Walk by directory descriptors so a
    // guest symlink or rename cannot redirect evidence reads or the acknowledgement.
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::other(
                "capture path must be normalized and relative",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(false);
            }
            return Err(error);
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    let regular = |name: &str| -> io::Result<File> {
        let name = CString::new(name).unwrap();
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("capture must be a regular file"));
        }
        Ok(file)
    };
    match regular("complete") {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    for name in ["guest.stdout", "guest.stderr", "guest.status.json"] {
        let mut source = regular(name)?;
        let mut destination = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(evidence.join(name))?;
        io::copy(&mut source, &mut destination)?;
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"collected".as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { File::from_raw_fd(fd) }.write_all(b"collected\n")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_config_argument_preserves_spaces_and_excludes_environment() {
        let mut bytes = 4i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/Library/Application Support/Motor/runner\0\0runner\0serve\0--vm-config\0/Library/Application Support/Motor/macos/vm.toml\0SECRET=not-an-argument\0");
        let args = procargs(&bytes).unwrap();
        assert_eq!(args.len(), 4);
        assert_eq!(
            vm_config_argument(&args).unwrap(),
            Path::new("/Library/Application Support/Motor/macos/vm.toml")
        );
        assert!(procargs(&bytes[..20]).is_err());
        assert!(vm_config_argument(&[b"--vm-config".to_vec()]).is_err());
    }
    #[test]
    fn ambient_or_stale_vm_cannot_supply_second_qualification_service() {
        let a = uuid::Uuid::now_v7();
        let b = uuid::Uuid::now_v7();
        let ambient = uuid::Uuid::now_v7();
        let records = [
            (a, Some(11), true),
            (b, Some(12), true),
            (ambient, Some(13), true),
        ];
        assert_eq!(
            attributed_services(&[a, b], &records, |pid| pid != 12),
            vec![11]
        );
        assert_eq!(
            attributed_services(&[a, b], &records, |_| true),
            vec![11, 12]
        );
        assert!(attributed_services::<i32>(&[], &records, |_| true).is_empty());
    }
    #[test]
    fn concurrent_workspace_keys_reject_same_checkout_and_subdirectory() {
        let scratch = cortexkit_test_support::ScratchDir::new("qualification-workspaces");
        for name in ["a", "b"] {
            let status = std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(scratch.join(name))
                .status()
                .unwrap();
            assert!(status.success());
        }
        std::fs::create_dir(scratch.join("a/sub")).unwrap();
        assert!(distinct_workspaces(&scratch.join("a"), &scratch.join("a/sub")).is_err());
        assert!(distinct_workspaces(&scratch.join("a"), &scratch.join("b")).is_ok());
    }
    #[test]
    fn cleanup_inventory_does_not_treat_unreadable_directory_as_empty() {
        let scratch = cortexkit_test_support::ScratchDir::new("qualification-inventory");
        std::fs::write(scratch.join("not-directory"), b"x").unwrap();
        assert!(entries(&scratch.join("not-directory")).is_err());
        assert!(entries(&scratch.join("missing")).unwrap().is_empty());
    }
    #[test]
    fn record_polling_ignores_incomplete_atomic_publication() {
        let id = uuid::Uuid::now_v7();
        assert!(final_record(Path::new(&format!("{id}.json"))));
        assert!(!final_record(Path::new(&format!("{id}.tmp"))));
        assert!(!final_record(Path::new("unexpected.json")));
    }
    #[test]
    fn capture_collection_never_follows_guest_links_or_clobbers_host_files() {
        let scratch = cortexkit_test_support::ScratchDir::new("qualification-capture-links");
        let root = scratch.join("state");
        let capture = root.join("jobs/job/workspace/capture");
        std::fs::create_dir_all(&capture).unwrap();
        let private = scratch.join("host-private");
        std::fs::write(&private, b"must not be copied or overwritten").unwrap();
        std::fs::write(capture.join("complete"), b"").unwrap();
        std::fs::write(capture.join("guest.stderr"), b"err").unwrap();
        std::fs::write(capture.join("guest.status.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(&private, capture.join("guest.stdout")).unwrap();
        let relative = Path::new("jobs/job/workspace/capture");
        let first = scratch.join("first");
        std::fs::create_dir(&first).unwrap();
        assert!(collect_capture(&root, relative, &first).is_err());
        assert!(!first.join("guest.stdout").exists());
        assert!(!capture.join("collected").exists());
        std::fs::remove_file(capture.join("guest.stdout")).unwrap();
        std::fs::write(capture.join("guest.stdout"), b"out").unwrap();
        std::os::unix::fs::symlink(&private, capture.join("collected")).unwrap();
        let second = scratch.join("second");
        std::fs::create_dir(&second).unwrap();
        assert!(collect_capture(&root, relative, &second).is_err());
        assert_eq!(
            std::fs::read(&private).unwrap(),
            b"must not be copied or overwritten"
        );
        std::fs::remove_file(capture.join("collected")).unwrap();
        std::os::unix::fs::symlink(&capture, root.join("alias")).unwrap();
        assert!(collect_capture(&root, Path::new("alias"), &second).is_err());
        let third = scratch.join("third");
        std::fs::create_dir(&third).unwrap();
        assert!(collect_capture(&root, relative, &third).unwrap());
        assert_eq!(std::fs::read(third.join("guest.stdout")).unwrap(), b"out");
        assert!(capture.join("collected").is_file());
    }
}
