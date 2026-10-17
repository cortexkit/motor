use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

pub(super) struct Directory {
    file: File,
    path: PathBuf,
    owner: u32,
}

impl Directory {
    pub(super) fn open(path: &Path, owner: u32) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| unsafe_path(path, error))?;
        Self::checked(file, path.to_owned(), owner)
    }

    pub(super) fn absolute(path: &Path, owner: u32) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(unsafe_path(path, "directory must be absolute"));
        }
        let mut directory = Self::open(Path::new("/"), owner)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => directory = directory.child(name.as_bytes(), None)?,
                _ => {
                    return Err(unsafe_path(
                        path,
                        "directory must not contain parent components",
                    ))
                }
            }
        }
        Ok(directory)
    }

    fn checked(file: File, path: PathBuf, owner: u32) -> io::Result<Self> {
        let metadata = file.metadata().map_err(|error| unsafe_path(&path, error))?;
        if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
            return Err(unsafe_path(&path, format!("expected a real directory owned by uid {owner} without group/other write permission")));
        }
        Ok(Self { file, path, owner })
    }

    pub(super) fn create(&self, name: &str, mode: libc::mode_t) -> io::Result<Self> {
        self.child(name.as_bytes(), Some(mode))
    }

    fn child(&self, name: &[u8], create: Option<libc::mode_t>) -> io::Result<Self> {
        if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
            return Err(unsafe_path(&self.path, "invalid directory component"));
        }
        let component = CString::new(name).map_err(|error| unsafe_path(&self.path, error))?;
        let path = self.path.join(std::ffi::OsStr::from_bytes(name));
        if let Some(mode) = create {
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), component.as_ptr(), mode) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(unsafe_path(&path, error));
                }
            }
        }
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(unsafe_path(&path, io::Error::last_os_error()));
        }
        use std::os::fd::FromRawFd;
        Self::checked(unsafe { File::from_raw_fd(fd) }, path, self.owner)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn fd_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
    }

    pub(super) fn bind(&self, target: &Self, readonly: bool) -> io::Result<Self> {
        // Mount in this process while both directory fds are open. A child
        // mount command would lose the CLOEXEC fds before resolving these paths.
        let source_fd = CString::new(self.fd_path().as_os_str().as_bytes())?;
        let target_fd = CString::new(target.fd_path().as_os_str().as_bytes())?;
        if unsafe {
            libc::mount(
                source_fd.as_ptr(),
                target_fd.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(unsafe_path(&self.path, io::Error::last_os_error()));
        }
        // Reopen the trusted target to get a descriptor on the new mount,
        // rather than remounting the directory hidden underneath it.
        let mounted = Self::absolute(target.path(), target.owner)?;
        if readonly {
            let mounted_fd = CString::new(mounted.fd_path().as_os_str().as_bytes())?;
            if unsafe {
                libc::mount(
                    std::ptr::null(),
                    mounted_fd.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                    std::ptr::null(),
                )
            } < 0
            {
                return Err(unsafe_path(&target.path, io::Error::last_os_error()));
            }
        }
        Ok(mounted)
    }

    pub(super) fn remove_stale_socket(&self, name: &str) -> io::Result<()> {
        let path = self.fd_path().join(name);
        let display = self.path.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(unsafe_path(&display, error)),
        };
        if !metadata.file_type().is_socket() || metadata.uid() != self.owner {
            return Err(unsafe_path(&display, "expected an owned stale socket node"));
        }
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(_) => return Err(unsafe_path(&display, "socket still has a live listener")),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
            Err(error) => return Err(unsafe_path(&display, error)),
        }
        fs::remove_file(&path).map_err(|error| unsafe_path(&display, error))?;
        self.file.sync_all()
    }
}

fn unsafe_path(path: &Path, reason: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("unsafe tenant path {}: {reason}", path.display()))
}

pub(super) struct DiskLayout {
    pub(super) state: Directory,
    pub(super) runner: Directory,
    pub(super) cargo_bin: Directory,
    pub(super) roots: Vec<Directory>,
}

pub(super) fn prepare_disk_layout(tenant: &Path, owner: u32) -> io::Result<DiskLayout> {
    let tenant = Directory::open(tenant, owner)?;
    let state = tenant.create("state", 0o755)?;
    let runner = state.create("main", 0o700)?;
    let cargo_bin = tenant.create("cargo-home", 0o755)?.create("bin", 0o755)?;
    let roots = tenant.create("roots", 0o755)?;
    let roots = super::ROOTS
        .iter()
        .map(|root| roots.create(root.trim_start_matches('/'), 0o755))
        .collect::<io::Result<_>>()?;
    Ok(DiskLayout {
        state,
        runner,
        cargo_bin,
        roots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::os::unix::{
        fs::{symlink, PermissionsExt},
        net::UnixListener,
    };

    fn uid() -> u32 {
        unsafe { libc::geteuid() }
    }

    #[test]
    fn attach_refuses_persisted_symlink_sources_with_the_path() {
        for source in [
            "roots/Users",
            "roots/home",
            "roots",
            "state",
            "cargo-home/bin",
            "cargo-home",
        ] {
            let root = ScratchDir::new("motor-tenant-symlink");
            let path = root.join(source);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let outside = root.join("outside");
            fs::create_dir(&outside).unwrap();
            symlink(&outside, &path).unwrap();
            let error = prepare_disk_layout(&root, uid())
                .err()
                .expect("attach must reject a symlink source");
            assert!(
                error.to_string().contains(path.to_str().unwrap()),
                "{error}"
            );
            assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
        }
    }

    #[test]
    fn source_fds_stay_pinned_and_check_ownership_and_permissions() {
        let root = ScratchDir::new("motor-tenant-source-fd");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let source = &layout.roots[0];
        fs::write(source.path().join("original"), b"pinned").unwrap();
        fs::rename(source.path(), root.join("old-root")).unwrap();
        symlink("/etc", source.path()).unwrap();
        assert_eq!(
            fs::read(source.fd_path().join("original")).unwrap(),
            b"pinned"
        );
        assert!(Directory::open(&root, uid().wrapping_add(1)).is_err());
        fs::set_permissions(layout.state.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Directory::open(layout.state.path(), uid()).is_err());
    }

    #[test]
    fn attach_removes_only_a_leftover_socket_node() {
        let root = ScratchDir::new("motor-tenant-stale-socket");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let path = layout.runner.fd_path().join("runner.sock");
        let listener = UnixListener::bind(&path).unwrap();
        assert!(layout.runner.remove_stale_socket("runner.sock").is_err());
        drop(listener);
        layout.runner.remove_stale_socket("runner.sock").unwrap();
        assert!(!path.exists());
        fs::write(&path, b"not a socket").unwrap();
        assert!(layout.runner.remove_stale_socket("runner.sock").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"not a socket");
        fs::remove_file(&path).unwrap();
        symlink("/etc/passwd", &path).unwrap();
        assert!(layout.runner.remove_stale_socket("runner.sock").is_err());
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }
}
