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
        self.bind_with_remount(
            target,
            readonly.then_some(libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY),
        )
    }

    pub(super) fn bind_toolchain(&self, target: &Self) -> io::Result<Self> {
        self.bind_with_remount(target, Some(TOOLCHAIN_BIND_FLAGS))
    }

    fn bind_with_remount(&self, target: &Self, remount: Option<libc::c_ulong>) -> io::Result<Self> {
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
        if let Some(flags) = remount {
            let mounted_fd = CString::new(mounted.fd_path().as_os_str().as_bytes())?;
            if unsafe {
                libc::mount(
                    std::ptr::null(),
                    mounted_fd.as_ptr(),
                    std::ptr::null(),
                    flags,
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

const TOOLCHAIN_BIND_FLAGS: libc::c_ulong =
    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV;

fn unsafe_path(path: &Path, reason: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("unsafe tenant path {}: {reason}", path.display()))
}

pub(super) struct DiskLayout {
    pub(super) state: Directory,
    pub(super) runner: Directory,
    pub(super) cargo_bin: Directory,
    pub(super) roots: Vec<Directory>,
}

impl DiskLayout {
    pub(super) fn initialize_job_history(
        &self,
        boot_marker: &Path,
        forked: bool,
    ) -> io::Result<()> {
        match fs::symlink_metadata(boot_marker) {
            Ok(metadata) => {
                if !metadata.is_file()
                    || metadata.uid() != self.runner.owner
                    || metadata.mode() & 0o022 != 0
                {
                    return Err(unsafe_path(boot_marker, "expected an owned boot marker"));
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // Source-seal journal recovery repairs inode flags and ownership, without replaying or settling jobs.
        self.clear_execution_history()?;
        if forked {
            for name in ["mirror-reports.json", "quarantine", "job-staging", "spools"] {
                self.remove_history(&self.runner.fd_path().join(name))?;
            }
            for entry in fs::read_dir(self.runner.fd_path())? {
                let entry = entry?;
                if entry.file_name().to_str().is_some_and(|name| {
                    name.starts_with("mirror-reports.invalid-") && name.ends_with(".json")
                }) {
                    self.remove_history(&entry.path())?;
                }
            }
            self.runner.file.sync_all()?;
        }
        // The marker lives on /run, not the workspace disk. It is created before
        // starting a runner, so a restart in this boot cannot erase a live job.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(boot_marker)?
            .sync_all()
    }

    // Execution pages and returned-file collections are both below jobs/.
    // Keep the catalog, mirror refs and retirement records beside them intact.
    fn execution_history(&self) -> [PathBuf; 2] {
        ["jobs", "tenant-events.json"].map(|name| self.runner.fd_path().join(name))
    }

    pub(super) fn clear_execution_history(&self) -> io::Result<()> {
        let roots: Vec<_> = super::ROOTS
            .iter()
            .zip(&self.roots)
            .map(|(guest, disk)| (Path::new(guest), disk.path()))
            .collect();
        crate::source_seal::recovery::stopped_disk(self.runner.path(), &roots)?;
        for path in self.execution_history() {
            self.remove_history(&path)?;
        }
        self.runner.file.sync_all()
    }

    fn remove_history(&self, path: &Path) -> io::Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.is_dir() {
            fs::remove_dir_all(path)
        } else {
            // Unlink a symlink itself, never the object it names.
            fs::remove_file(path)
        }
    }
}

pub(super) fn prepare_disk_layout(tenant: &Path, owner: u32) -> io::Result<DiskLayout> {
    let tenant = Directory::open(tenant, owner)?;
    let state = tenant.create("state", 0o755)?;
    let runner = state.create("main", 0o700)?;
    let cargo = tenant.create("cargo-home", 0o755)?;
    let cargo_bin = cargo.create("bin", 0o755)?;
    // Even a cold tenant binds both shared source trees read-only into jobs.
    cargo.create("registry", 0o755)?;
    cargo.create("git", 0o755)?;
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
    fn toolchain_bind_is_readonly_nosuid_nodev_but_executable() {
        assert_ne!(TOOLCHAIN_BIND_FLAGS & libc::MS_BIND, 0);
        assert_ne!(TOOLCHAIN_BIND_FLAGS & libc::MS_REMOUNT, 0);
        assert_ne!(TOOLCHAIN_BIND_FLAGS & libc::MS_RDONLY, 0);
        assert_ne!(TOOLCHAIN_BIND_FLAGS & libc::MS_NOSUID, 0);
        assert_ne!(TOOLCHAIN_BIND_FLAGS & libc::MS_NODEV, 0);
        assert_eq!(TOOLCHAIN_BIND_FLAGS & libc::MS_NOEXEC, 0);
    }

    /// A tenant root shaped like a fresh tenant disk: mode 0755 and no default
    /// ACL. Scratch space can carry a group-writable default ACL (the build
    /// server's /srv/build/tmp does), which new directories would inherit and
    /// which the ownership checks rightly refuse.
    fn tenant_root(name: &str) -> (ScratchDir, PathBuf) {
        let scratch = ScratchDir::new(name);
        let root = scratch.join("tenant");
        fs::create_dir(&root).unwrap();
        #[cfg(target_os = "linux")]
        match std::process::Command::new("setfacl")
            .args(["-b", "-k"])
            .arg(&root)
            .status()
        {
            Ok(status) => assert!(
                status.success(),
                "setfacl -b -k failed on {}",
                root.display()
            ),
            // Without setfacl the filesystem carries no ACL for us to clear.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("setfacl: {error}"),
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        (scratch, root)
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
            let (scratch, root) = tenant_root("motor-tenant-symlink");
            let path = root.join(source);
            // Explicit 0755: the default 0777 would leave parents group-writable
            // under a 002 umask, and attach would refuse them before the symlink.
            std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o755)
                .create(path.parent().unwrap())
                .unwrap();
            let outside = scratch.join("outside");
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
        let (_scratch, root) = tenant_root("motor-tenant-source-fd");
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
        let (_scratch, root) = tenant_root("motor-tenant-stale-socket");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let path = layout.runner.fd_path().join("runner.sock");
        let listener = UnixListener::bind(&path).unwrap();
        assert!(layout.runner.remove_stale_socket("runner.sock").is_err());
        drop(listener);
        // Recreate the node so nothing can be listening on it; see
        // `leftover_socket_node` for why the dropped listener isn't enough.
        fs::remove_file(&path).unwrap();
        super::super::runner_socket::tests::leftover_socket_node(&path);
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

#[cfg(test)]
mod cargo_layout_tests {
    use super::*;
    #[test]
    fn cold_tenant_creates_shared_source_roots_for_read_only_job_binds() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-cargo-layout");
        let owner = unsafe { libc::geteuid() };
        prepare_disk_layout(&root, owner).unwrap();
        for name in ["registry", "git"] {
            let metadata = std::fs::metadata(root.join("cargo-home").join(name)).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.uid(), owner);
            assert_eq!(metadata.mode() & 0o777, 0o755);
        }
    }
}
#[cfg(test)]
mod job_layout_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn uid() -> u32 {
        unsafe { libc::geteuid() }
    }
    fn tenant_root(name: &str) -> (cortexkit_test_support::ScratchDir, PathBuf) {
        let scratch = cortexkit_test_support::ScratchDir::new(name);
        let root = scratch.join("tenant");
        fs::create_dir(&root).unwrap();
        (scratch, root)
    }

    #[test]
    fn job_vm_cleanup_pins_only_execution_history_and_preserves_warm_state() {
        let (_scratch, root) = tenant_root("motor-job-history");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let relative: Vec<_> = layout
            .execution_history()
            .iter()
            .map(|p| p.strip_prefix(layout.runner.fd_path()).unwrap().to_owned())
            .collect();
        assert_eq!(
            relative,
            [PathBuf::from("jobs"), PathBuf::from("tenant-events.json")]
        );
        let state = root.join("state/main");
        let job = state.join("jobs/old-job");
        fs::create_dir_all(job.join("returned-collection")).unwrap();
        fs::write(job.join("record.json"), b"old job").unwrap();
        fs::write(job.join("0.chunk"), b"output page").unwrap();
        fs::write(job.join("returned-collection/0"), b"returned page").unwrap();
        fs::write(state.join("tenant-events.json"), b"old events").unwrap();
        let preserved = [
            "state/main/mirrors/repo/refs/motor/snapshots/pin",
            "roots/home/workspace/target/object",
            "cargo-home/registry/source",
        ];
        for name in preserved {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, name.as_bytes()).unwrap();
        }
        let mut catalog = crate::storage::Catalog::default();
        catalog.workspaces.insert(
            "/home/workspace".into(),
            crate::storage::Workspace {
                repository: "/home/repo".into(),
                snapshot: "snapshot-pin".into(),
                ..Default::default()
            },
        );
        catalog.mirror_used_ms.insert("repo".into(), 42);
        catalog.legacy_checkouts.insert(
            "/home/retiring-checkout".into(),
            crate::storage::LegacyCheckout {
                repository: "/home/repo".into(),
                tip: Some("retiring-tip".into()),
            },
        );
        let catalog_bytes = serde_json::to_vec(&catalog).unwrap();
        fs::write(state.join("catalog.json"), &catalog_bytes).unwrap();
        layout.clear_execution_history().unwrap();
        layout.clear_execution_history().unwrap();
        assert!(!state.join("jobs").exists());
        assert!(!state.join("tenant-events.json").exists());
        let kept_bytes = fs::read(state.join("catalog.json")).unwrap();
        assert_eq!(kept_bytes, catalog_bytes);
        let kept: crate::storage::Catalog = serde_json::from_slice(&kept_bytes).unwrap();
        assert_eq!(kept.workspaces["/home/workspace"].snapshot, "snapshot-pin");
        assert_eq!(kept.mirror_used_ms["repo"], 42);
        assert_eq!(
            kept.legacy_checkouts["/home/retiring-checkout"]
                .tip
                .as_deref(),
            Some("retiring-tip")
        );
        for name in preserved {
            assert_eq!(fs::read(root.join(name)).unwrap(), name.as_bytes());
        }
    }

    #[test]
    fn job_vm_cleanup_never_follows_history_symlinks() {
        let (scratch, root) = tenant_root("motor-job-history-symlink");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let kept = scratch.join("kept");
        fs::create_dir(&kept).unwrap();
        fs::write(kept.join("page"), b"keep").unwrap();
        symlink(&kept, layout.runner.path().join("jobs")).unwrap();
        symlink(
            kept.join("page"),
            layout.runner.path().join("tenant-events.json"),
        )
        .unwrap();
        layout.clear_execution_history().unwrap();
        assert_eq!(fs::read(kept.join("page")).unwrap(), b"keep");
        symlink(&kept, layout.runner.path().join("quarantine")).unwrap();
        symlink(
            kept.join("page"),
            layout
                .runner
                .path()
                .join("mirror-reports.invalid-donor.json"),
        )
        .unwrap();
        layout
            .initialize_job_history(&scratch.join("boot-marker"), true)
            .unwrap();
        assert_eq!(fs::read(kept.join("page")).unwrap(), b"keep");
    }

    #[test]
    fn job_vm_cleanup_retains_unrecoverable_seals_and_execution_history() {
        let (scratch, root) = tenant_root("motor-job-history-seal-error");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let job = layout
            .runner
            .path()
            .join("jobs")
            .join(motor_protocol::Uuid::now_v7().to_string());
        fs::create_dir_all(&job).unwrap();
        fs::write(job.join("seal"), b"invalid seal record").unwrap();
        fs::write(job.join("record.json"), b"previous execution").unwrap();
        let marker = scratch.join("boot-marker");
        let error = layout.initialize_job_history(&marker, false).unwrap_err();
        assert!(
            error.to_string().contains("leftover source seals"),
            "{error}"
        );
        assert!(job.join("seal").exists());
        assert!(job.join("record.json").exists());
        assert!(!marker.exists());
    }

    #[test]
    #[ignore = "requires root, file handles and CAP_LINUX_IMMUTABLE"]
    fn isolated_next_job_boot_replays_seals_before_erasing_history() {
        assert_eq!(uid(), 0);
        let (scratch, root) = tenant_root("motor-job-history-seal-replay");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let workspace = root.join("roots/home/workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("source"), b"tracked source").unwrap();
        fs::write(workspace.join("old-snapshot-only"), b"old tracked source").unwrap();
        let id = motor_protocol::Uuid::now_v7();
        let state = layout.runner.path();
        let job = state.join("jobs").join(id.to_string());
        fs::create_dir_all(&job).unwrap();
        let plan = serde_json::from_value(serde_json::json!({
            "unit": id, "workspace": workspace, "caller_workspace": "/home/workspace",
            "files": ["c291cmNl", "b2xkLXNuYXBzaG90LW9ubHk="]
        }))
        .unwrap();
        let seals = crate::source_seal::Runtime::default();
        seals.seal(state, id, plan).unwrap();
        // A real job sees this inode tree through the /home bind. Model the
        // bind's spelling in the durable record without mounting over /home
        // on the machine running the test.
        let record_path = crate::source_seal::record_path(state, id);
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        record["plan"]["workspace"] = "/home/workspace".into();
        fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
        drop(seals);
        drop(layout);
        let next = prepare_disk_layout(&root, uid()).unwrap();
        let result = next.initialize_job_history(&scratch.join("next-boot-marker"), false);
        // Clear flags even on assertion failure so the scratch tree is removable.
        let mut leftovers = Vec::new();
        for name in ["source", "old-snapshot-only"] {
            use std::os::fd::AsRawFd;
            let file = File::open(workspace.join(name)).unwrap();
            let mut flags: libc::c_int = 0;
            assert_eq!(
                unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) },
                0
            );
            if flags & 0x10 != 0 {
                leftovers.push(name);
            }
            flags &= !0x10;
            assert_eq!(
                unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &flags) },
                0
            );
        }
        result.unwrap();
        assert!(leftovers.is_empty(), "leftover seals on {leftovers:?}");
        for name in ["source", "old-snapshot-only"] {
            fs::write(workspace.join(name), b"writable again").unwrap();
        }
        assert!(!record_path.exists());
        crate::isolation::restore_runner_access(&workspace).unwrap();
    }

    #[test]
    fn forked_boot_never_replays_donor_jobs_or_pending_reports() {
        use crate::{
            engine::{Config, Runner},
            storage::{atomic_job_json, Job},
        };
        use motor_protocol::{Frame, MirrorRef, Outcome, RefusalReason, Uuid};

        let (scratch, root) = tenant_root("motor-forked-history");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let state = layout.runner.path();
        let id = Uuid::now_v7();
        let mut donor = Job::new(id, None, 1, 1, 2);
        donor.terminal_ended(Outcome::Exit { code: 0 }, 2);
        let directory = state.join("jobs").join(id.to_string());
        fs::create_dir_all(&directory).unwrap();
        atomic_job_json(&directory.join("record.json"), &donor).unwrap();
        fs::create_dir(directory.join("returned-donor")).unwrap();
        fs::write(directory.join("returned-donor/chunk-0"), b"returned bytes").unwrap();
        fs::write(directory.join("exit"), b"0").unwrap();
        let log = crate::tenant::EventLog::open_for_mode(state, true).unwrap();
        log.admit(id).unwrap();
        log.observe(&donor).unwrap();
        log.mirror_refs(
            true,
            &state.join("mirrors/donor"),
            vec![MirrorRef {
                repository: Some("/home/donor".into()),
                dependency: None,
                reference: "refs/motor/snapshots/donor".into(),
                commit: "a".repeat(40),
            }],
        )
        .unwrap();
        assert!(log.first().is_some());
        assert!(state.join("mirror-reports.json").exists());
        drop(log);

        let forked_marker = scratch.join("motor-workspace-forked");
        layout
            .initialize_job_history(&scratch.join("boot-marker"), true)
            .unwrap();
        super::super::mark_workspace_forked(&forked_marker, 7, true).unwrap();
        let mut config = Config::new(root.join("state"), "main");
        config.tenant_vm = true;
        config.per_job = true;
        config.manual_tick = true;
        config.fixture_workspace_forked_marker = Some(forked_marker);
        let runner = Runner::open(config).unwrap();
        assert!(!runner.has_job_record(id));
        assert!(!directory.exists());
        assert!(matches!(runner.replay(id, 0).unwrap().as_slice(),
            [Frame::Terminal(terminal)] if matches!(terminal.outcome,
                Outcome::RefusedBeforeStart { reason: RefusalReason::TransferInterrupted })));
        let log = runner.tenant_events().unwrap();
        log.enable_reports(true, true).unwrap();
        while let Some(event) = log.first() {
            assert_ne!(event.jti, Some(id), "donor lifecycle was reported");
            assert!(
                event.mirror_refs.is_none(),
                "donor mirror report was replayed"
            );
            log.ack(event.event_seq).unwrap();
        }
    }

    #[test]
    fn fork_only_history_is_preserved_on_ordinary_boot_and_same_boot_restart() {
        let (scratch, root) = tenant_root("motor-fork-only-history");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let state = layout.runner.path();
        let leftovers = [
            "mirror-reports.json",
            "mirror-reports.invalid-donor.json",
            "quarantine/donor.json",
            "job-staging/donor/record.json",
            "spools/donor/bundle",
        ];
        for name in leftovers {
            let path = state.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"donor evidence").unwrap();
        }
        layout
            .initialize_job_history(&scratch.join("ordinary-boot"), false)
            .unwrap();
        for name in leftovers {
            assert!(state.join(name).exists(), "ordinary boot removed {name}");
        }
        let forked_boot = scratch.join("forked-boot");
        layout.initialize_job_history(&forked_boot, true).unwrap();
        for name in leftovers {
            assert!(!state.join(name).exists(), "fork kept {name}");
        }
        fs::write(state.join("mirror-reports.json"), b"this boot's report").unwrap();
        layout.initialize_job_history(&forked_boot, true).unwrap();
        assert_eq!(
            fs::read(state.join("mirror-reports.json")).unwrap(),
            b"this boot's report"
        );
    }

    #[test]
    fn second_initialization_in_one_boot_keeps_job_records() {
        let (scratch, root) = tenant_root("motor-job-boot-history");
        let layout = prepare_disk_layout(&root, uid()).unwrap();
        let marker = scratch.join("boot-marker");
        let jobs = layout.runner.path().join("jobs");
        fs::create_dir(&jobs).unwrap();
        fs::write(jobs.join("old-record"), b"previous boot").unwrap();
        layout.initialize_job_history(&marker, false).unwrap();
        assert!(!jobs.exists());
        fs::create_dir(&jobs).unwrap();
        let record = jobs.join("record.json");
        fs::write(&record, b"this boot's execution").unwrap();
        drop(layout);
        let restarted = prepare_disk_layout(&root, uid()).unwrap();
        restarted.initialize_job_history(&marker, false).unwrap();
        assert_eq!(fs::read(&record).unwrap(), b"this boot's execution");
        fs::remove_file(marker).unwrap();
        restarted
            .initialize_job_history(&scratch.join("boot-marker"), false)
            .unwrap();
        assert!(!jobs.exists());
    }
}
