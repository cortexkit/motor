//! Break tracked hard-link aliases before transferring the checkout to a job.
use super::*;

pub(crate) fn linked_sources(
    config: &Config,
    trees: &crate::workspace::Trees,
    request: &Run,
) -> io::Result<()> {
    let mirror = trees.mirror(paths::request(request).caller_repository_root.text());
    let tree = crate::workspace::snapshot_tree(&mirror, &request.tree_hash)?;
    let plan = plan(config, request, &[], &tree)?;
    refresh(&plan, &tree, |oid| {
        crate::workspace::snapshot_blob(&mirror, oid)
    })
}

fn refresh(plan: &Plan, tree: &[u8], blob: impl Fn(&str) -> io::Result<Vec<u8>>) -> io::Result<()> {
    let root = open_mount(&plan.workspace)?;
    let entries: BTreeMap<_, _> = tree
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let tab = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| io::Error::other("invalid snapshot entry"))?;
            let fields: Vec<_> = std::str::from_utf8(&entry[..tab])
                .map_err(io::Error::other)?
                .split_whitespace()
                .collect();
            if fields.len() != 3 {
                return Err(io::Error::other("invalid snapshot header"));
            }
            Ok((
                PathBuf::from(std::ffi::OsStr::from_bytes(&entry[tab + 1..])),
                (fields[0], fields[2]),
            ))
        })
        .collect::<io::Result<_>>()?;
    for path in &plan.files {
        let pin = pin_source(&root, path)?;
        if pin.metadata()?.nlink() == 1 {
            continue;
        }
        if file_flags(&reopen_regular(&pin)?)? & IMMUTABLE != 0 {
            continue;
        }
        let (mode, oid) = entries
            .get(path)
            .ok_or_else(|| io::Error::other("missing snapshot blob"))?;
        let bytes = blob(oid)?;
        let parent =
            crate::caller_path::owner::directory_at(&root, path.parent().unwrap_or(Path::new("")))?;
        let temporary = CString::new(format!(".motor-source-{}", Uuid::now_v7()))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::mode_t,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| {
            file.write_all(&bytes)?;
            if unsafe { libc::fchmod(fd, if *mode == "100755" { 0o755 } else { 0o644 }) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let name = CString::new(path.file_name().unwrap().as_bytes())?;
            if unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0);
        }
        step(path, "replace hard-linked source inode", result)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tracked_hardlink_is_replaced_from_snapshot_without_touching_alias() {
        let scratch = cortexkit_test_support::ScratchDir::new("source-seal-refresh");
        fs::write(scratch.join("tracked"), b"old").unwrap();
        fs::hard_link(scratch.join("tracked"), scratch.join("untracked")).unwrap();
        let plan = Plan {
            unit: Uuid::now_v7(),
            workspace: scratch.path().into(),
            caller_workspace: scratch.path().into(),
            files: vec!["tracked".into()],
        };
        refresh(&plan, b"100644 blob a\ttracked\0", |_| {
            Ok(b"snapshot".to_vec())
        })
        .unwrap();
        assert_eq!(fs::read(scratch.join("tracked")).unwrap(), b"snapshot");
        assert_eq!(fs::read(scratch.join("untracked")).unwrap(), b"old");
        assert_eq!(fs::metadata(scratch.join("tracked")).unwrap().nlink(), 1);
        assert_ne!(
            fs::metadata(scratch.join("tracked")).unwrap().ino(),
            fs::metadata(scratch.join("untracked")).unwrap().ino()
        );
    }
}
