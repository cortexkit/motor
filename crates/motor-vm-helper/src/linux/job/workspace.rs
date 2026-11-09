use super::*;
use std::{ffi::CString, io::Write};

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
        if registry.account != account
            || registry.workspace_disk_id != disk_id
            || registry.role != job::WorkspaceRole::Job
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

// Fill/base creation is deliberately unexposed to host requests. A future
// root-controlled fill provisioner must select the account/repository's trusted
// reusable checkout for cache fills and base builds, not accept a disk name
// from the host. Ordinary job workspaces must never supply that checkout.
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
        let jobs = super::super::tests::machine(&base);
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
}
