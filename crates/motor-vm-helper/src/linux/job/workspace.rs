use super::*;
use std::{ffi::CString, io::Write};

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
        let host: Workspace =
            serde_json::from_slice(&std::fs::read(root.join("disk-3/registry.json")).unwrap())
                .unwrap();
        assert_eq!(host.role, job::WorkspaceRole::Job);
        std::fs::remove_dir_all(base).unwrap();
    }
}
