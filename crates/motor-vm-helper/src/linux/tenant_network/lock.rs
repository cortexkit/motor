use crate::{Error, Result};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

pub(in crate::linux) struct Guard {
    _file: File,
    _held: crate::lock_order::Held,
}

pub(in crate::linux) fn acquire() -> Result<Guard> {
    acquire_at(Path::new(DIRECTORY), 0)
}

/// Takes the flock only if nobody holds it; None means it is busy.
pub(in crate::linux) fn try_acquire() -> Result<Option<Guard>> {
    try_acquire_at(Path::new(DIRECTORY), 0)
}

const DIRECTORY: &str = "/run/motor-network";

pub(in crate::linux) fn acquire_at(directory: &Path, owner: u32) -> Result<Guard> {
    lock_at(directory, owner, true)?
        .ok_or_else(|| Error("blocking network lock returned busy".into()))
}

pub(in crate::linux) fn try_acquire_at(directory: &Path, owner: u32) -> Result<Option<Guard>> {
    lock_at(directory, owner, false)
}

fn lock_at(directory: &Path, owner: u32, wait: bool) -> Result<Option<Guard>> {
    // Track acquisition as well as ownership: waiting for the flock while a
    // later lock is held is already an inversion, even before flock succeeds.
    let held = crate::lock_order::enter(crate::lock_order::Lock::N);
    match fs::DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o777 != 0o700 {
        return Err(Error("unsafe motor network lock directory".into()));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join("lock"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(Error("unsafe motor network lock file".into()));
    }
    let operation = if wait {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        let error = std::io::Error::last_os_error();
        if !wait && error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(Guard {
        _file: file,
        _held: held,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_mutation_lock_is_private_and_excludes_another_open_description() {
        let root = cortexkit_test_support::ScratchDir::new("network-lock");
        let directory = root.join("network");
        let _held = acquire_at(&directory, unsafe { libc::geteuid() }).unwrap();
        let other = File::open(directory.join("lock")).unwrap();
        assert_ne!(
            unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(other.metadata().unwrap().mode() & 0o777, 0o600);
    }
}
