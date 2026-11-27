use super::*;
use std::{ffi::CString, io::Write};
mod usage;
pub(super) use usage::exclusive_bytes;
mod fork;
pub(super) use fork::fork_bound;

pub(super) fn validate_registries(root: &Path) -> Result<()> {
    if !root.try_exists()? {
        return Ok(());
    }
    trusted_directory(root)?;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("registry.json");
        if path.try_exists()? {
            let mut file = fs::trusted_file(&path)?;
            decode_registry(
                &entry.file_name().to_string_lossy(),
                &std::io::read_to_string(&mut file)?,
            )?;
        }
    }
    Ok(())
}

fn decode_registry(disk_id: &str, json: &str) -> Result<Workspace> {
    serde_json::from_str(json).map_err(|error| {
        if let Ok(value) = serde_json::from_str::<Value>(json) {
            if let Some(role @ ("fill" | "base")) = value["role"].as_str() {
                return Error(format!("workspace disk {disk_id} has retired role {role}; delete the directory while the helper is stopped"));
            }
        }
        Error(format!("invalid workspace registry for disk {disk_id}: {error}"))
    })
}

#[cfg(test)]
#[test]
fn retired_workspace_roles_name_the_disk_and_require_operator_cleanup() {
    let mut value =
        json!({"account":"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", "workspace_disk_id":"disk-1"});
    assert_eq!(
        decode_registry("disk-1", &value.to_string()).unwrap().role,
        job::WorkspaceRole::Job
    );
    for role in ["fill", "base", "unknown"] {
        value["role"] = json!(role);
        let error = decode_registry("disk-1", &value.to_string()).unwrap_err().0;
        assert!(error.contains("disk-1"));
        if role == "unknown" {
            assert!(error.contains("invalid workspace registry"));
            assert!(!error.contains("delete the directory"));
        } else {
            assert!(error.contains(&format!("retired role {role}")));
            assert!(error.contains("delete the directory while the helper is stopped"));
        }
    }
}

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

pub(super) fn ensure_bound(
    root: &Path,
    request: job::CreateWorkspace,
    grant: Option<&crate::cache_write::Grant>,
) -> Result<()> {
    if root.join(&request.workspace_disk_id).try_exists()? {
        let (registry, disk) = open_disk(root, &request.account, &request.workspace_disk_id)?;
        let meta = disk.metadata()?;
        // A larger request means the host raised its checkout disk cap; the
        // guest grows its filesystem to the new size at its next boot. The
        // host never lowers a disk's cap, so a request smaller than the
        // existing disk is invalid.
        if registry.role != job::WorkspaceRole::Job
            || meta.uid() != 0
            || meta.len() > request.size_bytes
        {
            return Err(Error(
                "workspace role, owner or size mismatch; quarantine required".into(),
            ));
        }
        if meta.len() < request.size_bytes {
            grow_disk(
                root,
                &request.workspace_disk_id,
                (meta.dev(), meta.ino()),
                request.size_bytes,
            )?;
        }
        return Ok(());
    }
    create_bound(root, request, job::WorkspaceRole::Job, grant)
}

/// Extends an existing job disk file to `size` bytes. The caller has checked
/// that no VM holds the disk, so no running guest sees its device change.
/// The extension is sparse: nothing the guest wrote is read or moved.
fn grow_disk(root: &Path, disk_id: &str, identity: (u64, u64), size: u64) -> Result<()> {
    let root = File::open(root)?;
    let directory = fs::beneath(
        root.as_raw_fd(),
        disk_id,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    let disk = fs::beneath(
        directory.as_raw_fd(),
        "workspace.img",
        libc::O_RDWR | libc::O_NONBLOCK,
        0,
    )?;
    let meta = disk.metadata()?;
    // The same file open_disk checked, not one swapped in since.
    if !meta.is_file() || meta.uid() != 0 || (meta.dev(), meta.ino()) != identity {
        return Err(Error("workspace disk changed while growing".into()));
    }
    disk.set_len(size)?;
    disk.sync_all()?;
    Ok(())
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
        if registry.account != account || registry.workspace_disk_id != disk_id {
            return Err(Error("workspace deletion identity mismatch".into()));
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

#[cfg(all(test, feature = "linux-root-tests"))]
pub(super) fn create(
    root: &Path,
    request: job::CreateWorkspace,
    role: job::WorkspaceRole,
) -> Result<()> {
    create_bound(root, request, role, None)
}

fn binding(
    account: &str,
    role: job::WorkspaceRole,
    grant: Option<&crate::cache_write::Grant>,
) -> Option<String> {
    grant
        .filter(|g| g.sub == account && role == job::WorkspaceRole::Job)
        .map(|g| g.workspace_key_hash.clone())
}

pub(super) fn create_bound(
    root: &Path,
    request: job::CreateWorkspace,
    role: job::WorkspaceRole,
    grant: Option<&crate::cache_write::Grant>,
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
            workspace_key_hash: binding(&request.account, role, grant),
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

#[cfg(all(test, feature = "linux-root-tests"))]
mod binding_tests;

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
            cache_write_job_id: None,
            account: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            workspace_disk_id: "disk-1".into(),
            size_bytes: 4096,
        };
        create(&root, request(), job::WorkspaceRole::Job).unwrap();
        let registry = root.join("disk-1/registry.json");
        let before = std::fs::read(&registry).unwrap();
        let saved: Workspace = serde_json::from_slice(&before).unwrap();
        assert_eq!(saved.role, job::WorkspaceRole::Job);
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
        assert!(create(&root, request(), job::WorkspaceRole::Job).is_err());
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
            fixed_path_repositories: Default::default(),
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
            fixed_path_repositories: Default::default(),
        });
        let root = jobs.workspaces.clone();
        let mut record = super::super::tests::record();
        record.start.kind = job::VmKind::Recovery;
        let account = record.start.account.clone();
        let disk_id = record.start.workspace_disk_id.clone();
        let request = || job::CreateWorkspace {
            cache_write_job_id: None,
            account: account.clone(),
            workspace_disk_id: disk_id.clone(),
            size_bytes: 4096,
        };
        create(&root, request(), job::WorkspaceRole::Job).unwrap();
        assert!(delete(&root, "other", &disk_id).is_err());
        assert!(delete(&root, &account, "../escape").is_err());
        jobs.insert_record(record.clone());
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
        jobs.registry.lock().clear();
        // Simulate death after the durable rename but before recursive removal.
        std::fs::rename(root.join(&disk_id), root.join(format!(".trash-{disk_id}"))).unwrap();
        finish_deletions(&root).unwrap();
        delete(&root, &account, &disk_id).unwrap();
        assert!(!root.join(&disk_id).exists());
        for role in ["fill", "base"] {
            create(&root, request(), job::WorkspaceRole::Job).unwrap();
            std::fs::write(
                root.join(&disk_id).join("registry.json"),
                json!({
                    "account": account, "workspace_disk_id": disk_id, "role": role
                })
                .to_string(),
            )
            .unwrap();
            assert!(delete(&root, &account, &disk_id).is_err());
            std::fs::remove_dir_all(root.join(&disk_id)).unwrap();
        }
        create(&root, request(), job::WorkspaceRole::Job).unwrap();
        delete(&root, &account, &disk_id).unwrap();
        delete(&root, &account, &disk_id).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_workspace_ensure_enforces_helper_cap_grow_only_and_identity() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let base = super::super::tests::scratch();
        let mut jobs = super::super::tests::machine(&base);
        jobs.settings = Some(JobVms {
            uid_base: 500000,
            uid_count: 1,
            memory_max_bytes: 1048576,
            workspace_disk_max_bytes: Some(8192),
            fixed_path_repositories: Default::default(),
        });
        let account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let op = |size_bytes| Operation::EnsureWorkspace {
            cache_write_job_id: None,
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
        // A raised host cap grows the same file in place, sparsely, and
        // never past the helper's maximum.
        jobs.settings.as_mut().unwrap().workspace_disk_max_bytes = Some(16384);
        assert!(jobs.handle(op(20480), peer).is_err());
        jobs.handle(op(12288), peer).unwrap();
        let grown = std::fs::metadata(&disk).unwrap();
        assert_eq!(
            (grown.ino(), grown.len(), grown.blocks()),
            (before.ino(), 12288, 0)
        );
        assert!(jobs.handle(op(8192), peer).is_err());
        assert_eq!(std::fs::metadata(&disk).unwrap().len(), 12288);
        assert!(jobs
            .handle(
                Operation::EnsureWorkspace {
                    cache_write_job_id: None,
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
}
