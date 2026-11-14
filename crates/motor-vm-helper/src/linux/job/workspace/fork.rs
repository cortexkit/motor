use super::*;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};

// Keep the caller's order, including ties. u128 also covers st_blocks * 512
// and the weighted comparison without overflowing at u64::MAX blocks.
fn choose(sizes: impl IntoIterator<Item = u128>) -> Option<usize> {
    let mut sizes = sizes.into_iter().enumerate();
    let (_, first) = sizes.next()?;
    let (mut largest_index, mut largest) = (0, first);
    for (index, size) in sizes {
        if size > largest {
            largest_index = index;
            largest = size;
        }
    }
    Some(if 5 * first < 4 * largest {
        largest_index
    } else {
        0
    })
}

pub(in crate::linux::job) fn fork_bound(
    root: &Path,
    request: &job::Fork,
    grant: Option<&crate::cache_write::Grant>,
    available: impl Fn(&str, &std::fs::Metadata) -> Result<()>,
) -> Result<job::Forked> {
    request.validate()?;
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error("workspace fork requires root".into()));
    }
    trusted_directory(root)?;
    // symlink_metadata also refuses dangling symlinks and partial directories.
    match std::fs::symlink_metadata(root.join(&request.disk_id)) {
        Ok(_) => return Err(Error("fork destination already exists".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut donors = Vec::new();
    for id in &request.donor_ids {
        match std::fs::symlink_metadata(root.join(id)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let (registry, disk) = open_disk(root, &request.account, id)?;
        let meta = disk.metadata()?;
        if registry.role != job::WorkspaceRole::Job || meta.uid() != 0 {
            return Err(Error("fork donor must be a root-owned Job disk".into()));
        }
        available(id, &meta)?;
        // File::metadata uses fstat on the opened disk, never guest content.
        donors.push((id, disk, u128::from(meta.blocks()) * 512));
    }
    let index = choose(donors.iter().map(|(_, _, size)| *size))
        .ok_or_else(|| Error("no fork donors exist".into()))?;
    let (id, source, _) = &donors[index];
    let forked = job::Forked {
        donor_id: (*id).clone(),
    };
    let staging = stage(root, request, &forked, source, grant)?;
    let result = publish(root, &staging, &request.disk_id);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(root.join(&staging));
    }
    result?;
    Ok(forked)
}

fn stage(
    root: &Path,
    request: &job::Fork,
    forked: &job::Forked,
    source: &File,
    grant: Option<&crate::cache_write::Grant>,
) -> Result<String> {
    let mut random = [0u8; 16];
    if unsafe { libc::getrandom(random.as_mut_ptr().cast(), random.len(), 0) }
        != random.len() as isize
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let staging = format!(".seed-{:032x}", u128::from_ne_bytes(random));
    let path = root.join(&staging);
    std::fs::DirBuilder::new().mode(0o700).create(&path)?;
    let result = (|| -> Result<()> {
        let disk = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join("workspace.img"))?;
        fs::reflink(source, &disk)?;
        disk.sync_all()?;
        tenant::durable_write(
            &path.join("fork.json"),
            &serde_json::to_vec(forked).map_err(|e| Error(e.to_string()))?,
        )?;
        tenant::durable_write(
            &path.join("registry.json"),
            &serde_json::to_vec(&Workspace {
                workspace_key_hash: binding(&request.account, job::WorkspaceRole::Job, grant),
                account: request.account.clone(),
                workspace_disk_id: request.disk_id.clone(),
                role: job::WorkspaceRole::Job,
            })
            .map_err(|e| Error(e.to_string()))?,
        )?;
        File::open(&path)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(path);
    }
    result?;
    Ok(staging)
}

fn publish(root: &Path, staging: &str, id: &str) -> Result<()> {
    let root = File::open(root)?;
    let staging = CString::new(staging).unwrap();
    let id = CString::new(id).unwrap();
    // Admission serializes helper operations; NOREPLACE also preserves any
    // destination created outside the helper while the clone was in progress.
    if unsafe {
        libc::renameat2(
            root.as_raw_fd(),
            staging.as_ptr(),
            root.as_raw_fd(),
            id.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    root.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_choice_keeps_first_at_exactly_eighty_percent() {
        assert_eq!(choose([80, 100]), Some(0));
        assert_eq!(choose([79, 100]), Some(1));
        assert_eq!(choose([81, 100]), Some(0));
    }

    #[test]
    fn size_choice_breaks_largest_ties_by_request_order() {
        assert_eq!(choose([1, 10, 10]), Some(1));
        assert_eq!(choose([10, 10, 1]), Some(0));
    }

    #[test]
    fn size_choice_handles_zero_and_maximum_block_counts() {
        let max = u128::from(u64::MAX);
        assert_eq!(choose([]), None);
        assert_eq!(choose([0]), Some(0));
        assert_eq!(choose([0, 0]), Some(0));
        assert_eq!(choose([0, max]), Some(1));
        assert_eq!(choose([max, max]), Some(0));
        assert_eq!(choose([0, max * 512]), Some(1));
        assert_eq!(choose([max * 512, max * 512]), Some(0));
        assert_eq!(choose([max * 512 * 4 / 5, max * 512]), Some(0));
    }
}

#[cfg(all(test, feature = "linux-root-tests"))]
mod root_tests;
