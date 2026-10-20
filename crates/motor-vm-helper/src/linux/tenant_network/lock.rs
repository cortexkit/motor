use crate::{Error, Result};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

pub(super) fn acquire() -> Result<File> {
    acquire_at(Path::new("/run/motor-network"), 0)
}

fn acquire_at(directory: &Path, owner: u32) -> Result<File> {
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
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(file)
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
