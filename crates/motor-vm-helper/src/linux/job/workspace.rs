use super::*;
use std::{ffi::CString, io::Write};
mod usage;
pub(super) use usage::exclusive_bytes;

pub(super) fn open_disk(root: &Path, account: &str, disk_id: &str) -> Result<(Workspace, File)> {
    job::validate_account(account)?;
    job::validate_id(disk_id)?;
    for ancestor in root.ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
    }
    let root = File::open(root)?;
    let directory = fs::beneath(
        root.as_raw_fd(),
        disk_id,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    let meta = directory.metadata()?;
    if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err(Error("insecure workspace directory".into()));
    }
    let registry = fs::beneath(directory.as_raw_fd(), "registry.json", libc::O_RDONLY, 0)?;
    let meta = registry.metadata()?;
    if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err(Error("insecure workspace registry".into()));
    }
    let registry: Workspace =
        serde_json::from_reader(registry).map_err(|e| Error(e.to_string()))?;
    if registry.account != account || registry.workspace_disk_id != disk_id {
        return Err(Error("workspace identity mismatch".into()));
    }
    let disk = fs::beneath(
        directory.as_raw_fd(),
        "workspace.img",
        libc::O_RDONLY | libc::O_NONBLOCK,
        0,
    )?;
    if !disk.metadata()?.is_file() {
        return Err(Error("workspace disk must be regular".into()));
    }
    Ok((registry, disk))
}

pub(super) fn ensure(root: &Path, request: job::CreateWorkspace) -> Result<()> {
    if root.join(&request.workspace_disk_id).try_exists()? {
        let (registry, disk) = open_disk(root, &request.account, &request.workspace_disk_id)?;
        if registry.role != job::WorkspaceRole::Job
            || disk.metadata()?.uid() != 0
            || disk.metadata()?.len() != request.size_bytes
        {
            return Err(Error(
                "workspace role, owner or size mismatch; quarantine required".into(),
            ));
        }
        return Ok(());
    }
    create(root, request, job::WorkspaceRole::Job)
}

pub(super) fn base_lease(
    root: &Path,
    account: &str,
    disk_id: &str,
    exclusive: bool,
) -> Result<File> {
    let (registry, disk) = open_disk(root, account, disk_id)?;
    if registry.role != job::WorkspaceRole::Base || disk.metadata()?.uid() != 0 {
        return Err(Error("base lease requires a root-owned base".into()));
    }
    let mode = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if unsafe { libc::flock(disk.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
        return Err(Error("base is held by a seed or clone".into()));
    }
    Ok(disk)
}

// Only helper-created trash names are visited. A synced rename is the deletion
// decision; startup can finish removing its contents without opening guest data.
pub(super) fn finish_deletions(root: &Path) -> Result<()> {
    if !root.try_exists()? {
        return Ok(());
    }
    trusted_directory(root)?;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|n| n.starts_with(".seed-")) {
            trusted_directory(&entry.path())?;
            std::fs::remove_dir_all(entry.path())?;
            continue;
        }
        let Some(id) = name.to_str().and_then(|n| n.strip_prefix(".trash-")) else {
            continue;
        };
        job::validate_id(id)?;
        trusted_directory(&entry.path())?;
        std::fs::remove_dir_all(entry.path())?;
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

pub(super) fn delete(root: &Path, account: &str, disk_id: &str) -> Result<()> {
    delete_role(root, account, disk_id, job::WorkspaceRole::Job)
}

pub(super) fn delete_role(
    root: &Path,
    account: &str,
    disk_id: &str,
    role: job::WorkspaceRole,
) -> Result<()> {
    job::validate_account(account)?;
    job::validate_id(disk_id)?;
    trusted_directory(root)?;
    let path = root.join(disk_id);
    let trash = root.join(format!(".trash-{disk_id}"));
    if path.try_exists()? {
        trusted_directory(&path)?;
        let registry: Workspace =
            serde_json::from_reader(fs::trusted_file(&path.join("registry.json"))?)
                .map_err(|e| Error(e.to_string()))?;
        if registry.account != account
            || registry.workspace_disk_id != disk_id
            || registry.role != role
        {
            return Err(Error("workspace deletion identity or role mismatch".into()));
        }
        if trash.try_exists()? {
            return Err(Error("workspace trash collision".into()));
        }
        std::fs::rename(&path, &trash)?;
        File::open(root)?.sync_all()?;
    }
    if trash.try_exists()? {
        trusted_directory(&trash)?;
        std::fs::remove_dir_all(trash)?;
        File::open(root)?.sync_all()?;
    }
    Ok(())
}

// The role is chosen by the helper's provisioning operation, never by Start.
pub(super) fn create(
    root: &Path,
    request: job::CreateWorkspace,
    role: job::WorkspaceRole,
) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error("workspace provisioning requires root".into()));
    }
    job::validate_account(&request.account)?;
    job::validate_id(&request.workspace_disk_id)?;
    if request.size_bytes == 0 || !request.size_bytes.is_multiple_of(512) {
        return Err(Error(
            "workspace size must be a positive multiple of 512".into(),
        ));
    }
    trusted_directory(root)?;
    let parent = File::open(root)?;
    let name = CString::new(request.workspace_disk_id.as_str()).unwrap();
    // Never repair or overwrite an existing/partial directory: its role may
    // already have protected a disk used by a different class of VM.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    parent.sync_all()?;
    let dir = fs::beneath(
        parent.as_raw_fd(),
        &request.workspace_disk_id,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    let disk = fs::beneath(
        dir.as_raw_fd(),
        "workspace.img",
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    disk.set_len(request.size_bytes)?;
    disk.sync_all()?;
    let mut registry = fs::beneath(
        dir.as_raw_fd(),
        "registry.json",
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    registry.write_all(
        &serde_json::to_vec(&Workspace {
            account: request.account,
            workspace_disk_id: request.workspace_disk_id,
            role,
        })
        .map_err(|e| Error(e.to_string()))?,
    )?;
    registry.sync_all()?;
    dir.sync_all()?;
    parent.sync_all()?;
    Ok(())
}

pub(super) fn prepare_fill(root: &Path, fill: &job::Fill) -> Result<()> {
    fill.validate()?;
    let id = fill.disk_id();
    if root.join(&id).try_exists()? {
        let (registry, _) = open_disk(root, &fill.account, &id)?;
        if registry.role != job::WorkspaceRole::Fill || read_fill(root, &id)? != *fill {
            return Err(Error("fill provenance mismatch".into()));
        }
        return Ok(());
    }
    create(
        root,
        job::CreateWorkspace {
            account: fill.account.clone(),
            workspace_disk_id: id.clone(),
            size_bytes: fill.size_bytes,
        },
        job::WorkspaceRole::Fill,
    )?;
    tenant::durable_write(
        &root.join(&id).join("fill.json"),
        &serde_json::to_vec(fill).map_err(|e| Error(e.to_string()))?,
    )
}

pub(super) fn read_fill(root: &Path, id: &str) -> Result<job::Fill> {
    serde_json::from_reader(fs::trusted_file(&root.join(id).join("fill.json"))?)
        .map_err(|e| Error(e.to_string()))
}

pub(super) fn promote(root: &Path, fill: &job::Fill) -> Result<()> {
    let id = fill.disk_id();
    let (mut registry, disk) = open_disk(root, &fill.account, &id)?;
    if read_fill(root, &id)? != *fill
        || disk.metadata()?.uid() != 0
        || !matches!(
            registry.role,
            job::WorkspaceRole::Fill | job::WorkspaceRole::Base
        )
    {
        return Err(Error("base publication provenance mismatch".into()));
    }
    disk.sync_all()?;
    registry.role = job::WorkspaceRole::Base;
    // The synced atomic registry replacement is the publication point. A crash
    // leaves an unpublishable Fill or a complete, immutable Base.
    tenant::durable_write(
        &root.join(id).join("registry.json"),
        &serde_json::to_vec(&registry).map_err(|e| Error(e.to_string()))?,
    )
}

pub(super) fn seed(root: &Path, seed: &job::Seed) -> Result<()> {
    let id = &seed.disk_id;
    if root.join(id).try_exists()? {
        let (registry, disk) = open_disk(root, &seed.account, id)?;
        let previous: job::Seed =
            serde_json::from_reader(fs::trusted_file(&root.join(id).join("seed.json"))?)
                .map_err(|e| Error(e.to_string()))?;
        if registry.role != job::WorkspaceRole::Job
            || disk.metadata()?.uid() != 0
            || previous != *seed
        {
            return Err(Error("seed identity mismatch".into()));
        }
        return Ok(());
    }
    let base = base_lease(root, &seed.account, &seed.base_id, false)?;
    let provenance = read_fill(root, &seed.base_id)?;
    if provenance.account != seed.account || provenance.repository_key != seed.repository_key {
        return Err(Error("base repository or account mismatch".into()));
    }
    // Publish the new Job registry only after cloning and syncing. Incomplete
    // directories have no usable registry and cannot be attached after a crash.
    let mut random = [0u8; 16];
    if unsafe { libc::getrandom(random.as_mut_ptr().cast(), random.len(), 0) }
        != random.len() as isize
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let staging = format!(".seed-{:032x}", u128::from_ne_bytes(random));
    let path = root.join(&staging);
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(&path)?;
    let result = (|| {
        let disk = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join("workspace.img"))?;
        fs::reflink(&base, &disk)?;
        disk.sync_all()?;
        tenant::durable_write(
            &path.join("seed.json"),
            &serde_json::to_vec(seed).map_err(|e| Error(e.to_string()))?,
        )?;
        tenant::durable_write(
            &path.join("registry.json"),
            &serde_json::to_vec(&Workspace {
                account: seed.account.clone(),
                workspace_disk_id: id.clone(),
                role: job::WorkspaceRole::Job,
            })
            .map_err(|e| Error(e.to_string()))?,
        )?;
        std::fs::rename(&path, root.join(id))?;
        File::open(root)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(path);
    }
    result
}

#[cfg(all(test, feature = "linux-root-tests"))]
mod fill_tests;

#[cfg(all(test, feature = "linux-root-tests"))]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn isolated_workspace_creation_sets_role_once_and_refuses_partial_directories() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let base = super::super::tests::scratch();
        let root = base.join("workspaces");
        let request = || job::CreateWorkspace {
            account: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            workspace_disk_id: "disk-1".into(),
            size_bytes: 4096,
        };
        create(&root, request(), job::WorkspaceRole::Fill).unwrap();
        let registry = root.join("disk-1/registry.json");
        let before = std::fs::read(&registry).unwrap();
        let saved: Workspace = serde_json::from_slice(&before).unwrap();
        assert_eq!(saved.role, job::WorkspaceRole::Fill);
        assert_eq!(saved.account, request().account);
        assert_eq!(
            std::fs::metadata(root.join("disk-1/workspace.img"))
                .unwrap()
                .len(),
            4096
        );
        assert!(create(&root, request(), job::WorkspaceRole::Job).is_err());
        assert_eq!(std::fs::read(&registry).unwrap(), before);
        std::fs::remove_file(&registry).unwrap();
        assert!(create(&root, request(), job::WorkspaceRole::Base).is_err());
        assert!(!registry.exists());
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("disk-2"))
            .unwrap();
        let mut partial = request();
        partial.workspace_disk_id = "disk-2".into();
        assert!(create(&root, partial, job::WorkspaceRole::Job).is_err());
        let mut jobs = super::super::tests::machine(&base);
        jobs.settings = Some(JobVms {
            uid_base: 500000,
            uid_count: 1,
            memory_max_bytes: 1048576,
            workspace_disk_max_bytes: Some(4096),
        });
        let mut host = request();
        host.workspace_disk_id = "disk-3".into();
        jobs.create_workspace(host).unwrap();
        let disk = root.join("disk-3/workspace.img");
        assert_eq!(std::fs::read(&disk).unwrap(), vec![0; 4096]);
        assert_eq!(std::fs::metadata(&disk).unwrap().blocks(), 0);
        let host: Workspace =
            serde_json::from_slice(&std::fs::read(root.join("disk-3/registry.json")).unwrap())
                .unwrap();
        assert_eq!(host.role, job::WorkspaceRole::Job);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_workspace_deletion_refuses_attached_recovery_foreign_and_fill_disks_and_resumes_trash(
    ) {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let base = super::super::tests::scratch();
        let mut jobs = super::super::tests::machine(&base);
        jobs.settings = Some(JobVms {
            uid_base: 500000,
            uid_count: 1,
            memory_max_bytes: 1048576,
            workspace_disk_max_bytes: Some(4096),
        });
        let root = jobs.workspaces.clone();
        let mut record = super::super::tests::record();
        record.start.kind = job::VmKind::Recovery;
        let account = record.start.account.clone();
        let disk_id = record.start.workspace_disk_id.clone();
        let request = || job::CreateWorkspace {
            account: account.clone(),
            workspace_disk_id: disk_id.clone(),
            size_bytes: 4096,
        };
        create(&root, request(), job::WorkspaceRole::Job).unwrap();
        assert!(delete(&root, "other", &disk_id).is_err());
        assert!(delete(&root, &account, "../escape").is_err());
        jobs.records
            .insert(record.start.job_id.clone(), record.clone());
        assert!(jobs
            .handle(
                Operation::DeleteWorkspace {
                    account: account.clone(),
                    disk_id: disk_id.clone()
                },
                Peer { uid: 0, gid: 0 }
            )
            .is_err());
        assert!(root.join(&disk_id).exists());
        jobs.records.clear();
        // Simulate death after the durable rename but before recursive removal.
        std::fs::rename(root.join(&disk_id), root.join(format!(".trash-{disk_id}"))).unwrap();
        finish_deletions(&root).unwrap();
        delete(&root, &account, &disk_id).unwrap();
        assert!(!root.join(&disk_id).exists());
        for role in [job::WorkspaceRole::Fill, job::WorkspaceRole::Base] {
            create(&root, request(), role).unwrap();
            assert!(delete(&root, &account, &disk_id).is_err());
            std::fs::remove_dir_all(root.join(&disk_id)).unwrap();
        }
        create(&root, request(), job::WorkspaceRole::Job).unwrap();
        delete(&root, &account, &disk_id).unwrap();
        delete(&root, &account, &disk_id).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_workspace_ensure_enforces_helper_cap_exact_size_and_identity() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let base = super::super::tests::scratch();
        let mut jobs = super::super::tests::machine(&base);
        jobs.settings = Some(JobVms {
            uid_base: 500000,
            uid_count: 1,
            memory_max_bytes: 1048576,
            workspace_disk_max_bytes: Some(8192),
        });
        let account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let op = |size_bytes| Operation::EnsureWorkspace {
            account: account.into(),
            disk_id: "disk-1".into(),
            size_bytes,
        };
        let peer = Peer { uid: 0, gid: 0 };
        assert!(jobs.handle(op(12288), peer).is_err());
        assert!(!jobs.workspaces.join("disk-1").exists());
        jobs.handle(op(8192), peer).unwrap();
        let disk = jobs.workspaces.join("disk-1/workspace.img");
        let before = std::fs::metadata(&disk).unwrap();
        assert_eq!(before.len(), 8192);
        assert_eq!(before.blocks(), 0);
        jobs.handle(op(8192), peer).unwrap();
        assert_eq!(std::fs::metadata(&disk).unwrap().ino(), before.ino());
        assert!(jobs.handle(op(4096), peer).is_err());
        assert_eq!(std::fs::metadata(&disk).unwrap().len(), 8192);
        assert!(jobs
            .handle(
                Operation::EnsureWorkspace {
                    account: "other".into(),
                    disk_id: "disk-1".into(),
                    size_bytes: 8192
                },
                peer
            )
            .is_err());
        let reply = jobs
            .handle(
                Operation::WorkspaceUsage {
                    account: account.into(),
                    disk_id: "disk-1".into(),
                },
                peer,
            )
            .unwrap();
        assert!(matches!(
            reply.reply,
            Reply::WorkspaceUsage {
                exclusive_bytes: Some(0)
            }
        ));
        for (owner, id) in [(account, "missing"), ("other", "disk-1")] {
            let reply = jobs
                .handle(
                    Operation::WorkspaceUsage {
                        account: owner.into(),
                        disk_id: id.into(),
                    },
                    peer,
                )
                .unwrap();
            assert!(matches!(
                reply.reply,
                Reply::WorkspaceUsage {
                    exclusive_bytes: None
                }
            ));
        }
        std::fs::remove_file(&disk).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &disk).unwrap();
        let reply = jobs
            .handle(
                Operation::WorkspaceUsage {
                    account: account.into(),
                    disk_id: "disk-1".into(),
                },
                peer,
            )
            .unwrap();
        assert!(matches!(
            reply.reply,
            Reply::WorkspaceUsage {
                exclusive_bytes: None
            }
        ));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_base_eviction_refuses_vm_seed_clone_job_and_fill_holders() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let base = super::super::tests::scratch();
        let mut jobs = super::super::tests::machine(&base);
        let account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let peer = Peer { uid: 0, gid: 0 };
        let op = || Operation::EvictBase {
            account: account.into(),
            disk_id: "disk-1".into(),
        };
        let request = || job::CreateWorkspace {
            account: account.into(),
            workspace_disk_id: "disk-1".into(),
            size_bytes: 8192,
        };
        for role in [job::WorkspaceRole::Job, job::WorkspaceRole::Fill] {
            create(&jobs.workspaces, request(), role).unwrap();
            assert!(jobs.handle(op(), peer).is_err());
            std::fs::remove_dir_all(jobs.workspaces.join("disk-1")).unwrap();
        }
        create(&jobs.workspaces, request(), job::WorkspaceRole::Base).unwrap();
        let seed = jobs.lease_base(account, "disk-1").unwrap();
        let clone = jobs.lease_base(account, "disk-1").unwrap();
        assert!(jobs.handle(op(), peer).is_err());
        drop(seed);
        assert!(jobs.handle(op(), peer).is_err());
        drop(clone);
        let mut record = super::super::tests::record();
        record.start.workspace_disk_id = "disk-1".into();
        record.start.kind = job::VmKind::BaseBuild;
        jobs.records.insert(record.start.job_id.clone(), record);
        assert!(jobs.handle(op(), peer).is_err());
        jobs.records.clear();
        jobs.handle(op(), peer).unwrap();
        jobs.handle(op(), peer).unwrap();
        assert!(!jobs.workspaces.join("disk-1").exists());
        std::fs::remove_dir_all(base).unwrap();
    }
}
