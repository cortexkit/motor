//! Replay seals before a fresh VM discards the previous execution's history.
use super::*;

/// Only the guest boot/recovery path calls this, before starting any runner.
/// The previous VM is gone, so none of its job units can still be using a seal.
pub(crate) fn stopped_disk(state: &Path, roots: &[(&Path, &Path)]) -> io::Result<()> {
    let jobs = state.join("jobs");
    match fs::symlink_metadata(&jobs) {
        Ok(metadata) if !metadata.is_dir() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let pinned = open_mount(&jobs)?;
    for entry in fs::read_dir(format!("/proc/self/fd/{}", pinned.as_raw_fd()))? {
        let entry = entry?;
        let Ok(id) = entry.file_name().to_string_lossy().parse::<Uuid>() else {
            continue;
        };
        let recover = || {
            let directory = JobDirectory::open(state, id)?;
            let _lock = directory.lock()?;
            if !directory.exists(c"seal")? && !directory.exists(c"seal-intent")? {
                return Ok(());
            }
            if unsafe { libc::geteuid() } != 0 {
                return Err(io::Error::other("disk seal recovery requires root"));
            }
            unseal_locked_at(&directory, id, None, Owners::DirectRoot, |workspace| {
                // The guest's /Users and /home binds do not exist yet. Resolve
                // only beneath the corresponding validated on-disk root.
                for (guest, disk) in roots {
                    if let Ok(relative) = workspace.strip_prefix(guest) {
                        validate_relative(relative)?;
                        crate::permissions::physical_root(disk)?;
                        return Ok(disk.join(relative));
                    }
                }
                Err(io::Error::other("sealed workspace is outside guest roots"))
            })
        };
        recover().map_err(|error| {
            io::Error::other(format!("leftover source seals for job {id}: {error}"))
        })?;
    }
    Ok(())
}

/// Repair flags orphaned by older per-job images that erased seal records.
/// Jobs lack CAP_LINUX_IMMUTABLE, so they cannot set this flag; fills never
/// seal, and the host only handles an opaque disk image. Only in this mode
/// can an unrecorded flag be attributed to the runner rather than to another
/// writer of the checkout filesystem.
pub(crate) fn checkout(config: &Config, path: &Path) -> io::Result<()> {
    if !config.per_job || !config.tenant_vm || !config.isolate_jobs || config.fill_vm {
        return Ok(());
    }
    config.source_seals.check_host(path)?;
    crate::workspace::validate_destinations(
        &config.owned_workspace_roots,
        paths::host(path),
        None,
        &[],
    )?;
    if !path.try_exists()? {
        return Ok(());
    }
    let recover = || {
        let root = open_mount(path)?;
        let device = root.metadata()?.dev();
        if device != fs::metadata(config.state_dir())?.dev() || device == fs::metadata("/")?.dev() {
            return Err(io::Error::other("checkout is not on the job disk"));
        }
        let count = clear_tree(&root)?;
        if count != 0 {
            tracing::warn!(
                count,
                reason = "leftover seals without a record",
                "recovered checkout source seals"
            );
        }
        Ok(())
    };
    recover().map_err(|error| {
        io::Error::other(format!(
            "leftover source seals in {}: {error}",
            path.display()
        ))
    })
}

fn clear_tree(directory: &File) -> io::Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
        let name = CString::new(entry?.file_name().as_bytes())?;
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64;
        how.resolve = 0x08 | 0x04 | 0x01; // BENEATH | NO_SYMLINKS | NO_XDEV
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                directory.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of_val(&how),
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            // A nested mount is not part of the checkout's filesystem.
            if error.raw_os_error() == Some(libc::EXDEV) {
                continue;
            }
            return Err(error);
        }
        let pin = unsafe { File::from_raw_fd(fd as i32) };
        let metadata = pin.metadata()?;
        if metadata.is_dir() {
            count += clear_tree(&pin)?;
        } else if metadata.is_file() {
            let file = reopen_regular(&pin)?;
            if file_flags(&file)? & IMMUTABLE != 0 {
                // A sealed regular inode cannot acquire new hard links. Refuse
                // an unexpected alias rather than changing an outside inode.
                if metadata.nlink() != 1 {
                    return Err(io::Error::other("immutable inode has multiple hard links"));
                }
                immutable(&file, false)?;
                file.sync_all()?;
                count += 1;
            }
        }
        // Symlinks and special files are never reopened or followed.
    }
    Ok(count)
}

pub(crate) fn access_error(error: io::Error) -> io::Error {
    io::Error::other(format!("restore workspace access (possible leftover source seals; immutable flags require a trusted seal record outside per-job VMs): {error}"))
}
