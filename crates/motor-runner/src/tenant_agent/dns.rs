use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::Ipv4Addr,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
};

/// The daemon's private resolver directory, as attachment installs it and as
/// the runner's own files outside this module refer to it.
pub(crate) const DIRECTORY: &str = "/run/motor-tenant-daemon";

/// Root-only backing file for the daemon service's private resolver bind.
/// Updating the same inode also preserves the binding across a future refresh.
pub(crate) fn install(directory: &Path, resolvers: &[String], owner: u32) -> io::Result<()> {
    if resolvers.is_empty() || resolvers.len() > 3 {
        return Err(io::Error::other(
            "daemon DNS needs one to three resolver addresses",
        ));
    }
    let mut contents = String::from("options timeout:2 attempts:1\n");
    for resolver in resolvers {
        let address = resolver.parse::<Ipv4Addr>().map_err(io::Error::other)?;
        contents.push_str(&format!("nameserver {address}\n"));
    }
    match fs::DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other("daemon resolver directory is not private"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join("resolv.conf"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(io::Error::other("daemon resolver file is not private"));
    }
    file.set_len(0)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daemon_resolver_is_owner_only_and_rejects_injection_and_symlinks() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-daemon-dns");
        let directory = root.join("resolver");
        let owner = unsafe { libc::geteuid() };
        install(&directory, &["1.1.1.1".into()], owner).unwrap();
        let file = directory.join("resolv.conf");
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        let metadata = fs::metadata(&file).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), owner);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "options timeout:2 attempts:1\nnameserver 1.1.1.1\n"
        );
        install(&directory, &["8.8.8.8".into()], owner).unwrap();
        assert_eq!(fs::metadata(&file).unwrap().ino(), metadata.ino());
        assert!(install(&directory, &["1.1.1.1\nnameserver 8.8.8.8".into()], owner).is_err());
        assert!(install(&directory, &[], owner).is_err());
        fs::remove_file(&file).unwrap();
        let outside = root.join("outside");
        fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, &file).unwrap();
        assert!(install(&directory, &["1.1.1.1".into()], owner).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged");
    }
}
