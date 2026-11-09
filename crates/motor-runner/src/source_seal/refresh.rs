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
    refresh(&plan, &tree, false, |oid| {
        crate::workspace::snapshot_blob(&mirror, oid)
    })
}

/// Break hard links that workspace preparation created after the checkout was
/// transferred. Bun installs a `file:` dependency by hard-linking its tracked
/// files into `node_modules`, and a sealed inode would freeze that name too, so
/// each sealed path gets its own inode first. `node_modules` keeps the old one.
pub(crate) fn planned_sources(
    plan: &Plan,
    tree: &[u8],
    mirror: &Path,
    preserve_times: bool,
) -> io::Result<()> {
    refresh(plan, tree, preserve_times, |oid| {
        crate::workspace::snapshot_blob(mirror, oid)
    })
}

fn refresh(
    plan: &Plan,
    tree: &[u8],
    preserve_times: bool,
    blob: impl Fn(&str) -> io::Result<Vec<u8>>,
) -> io::Result<()> {
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
        let saved_times = if preserve_times {
            Some((pin.metadata()?.modified()?, parent.metadata()?.modified()?))
        } else {
            None
        };
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
            if let Some((mtime, _)) = saved_times {
                file.set_times(fs::FileTimes::new().set_modified(mtime))?;
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
        // Creating and renaming the temporary inode changes its directory too.
        // Restore through the held descriptor only after the temporary is gone.
        if let Some((_, mtime)) = saved_times {
            parent.set_times(fs::FileTimes::new().set_modified(mtime))?;
        }
        step(path, "replace hard-linked source inode", result)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rematerialize_times(preserve_times: bool) {
        use std::time::{Duration, UNIX_EPOCH};
        let scratch = cortexkit_test_support::ScratchDir::new("source-seal-refresh-times");
        fs::create_dir(scratch.join("watched")).unwrap();
        fs::create_dir(scratch.join("node_modules")).unwrap();
        let file_time = UNIX_EPOCH + Duration::new(1_000_000_000, 123_456_789);
        let directory_time = UNIX_EPOCH + Duration::new(1_000_000_100, 987_654_321);
        for name in ["first", "second"] {
            let path = scratch.join("watched").join(name);
            fs::write(&path, "snapshot").unwrap();
            fs::hard_link(&path, scratch.join("node_modules").join(name)).unwrap();
            File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(file_time))
                .unwrap();
        }
        File::open(scratch.join("watched"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(directory_time))
            .unwrap();
        let plan = Plan {
            unit: Uuid::now_v7(),
            workspace: scratch.path().into(),
            caller_workspace: scratch.path().into(),
            files: vec!["watched/first".into(), "watched/second".into()],
        };
        refresh(
            &plan,
            b"100644 blob a\twatched/first\0\
                         100644 blob a\twatched/second\0",
            preserve_times,
            |_| Ok(b"snapshot".to_vec()),
        )
        .unwrap();
        let parent = fs::metadata(scratch.join("watched"))
            .unwrap()
            .modified()
            .unwrap();
        if preserve_times {
            assert_eq!(parent, directory_time);
        } else {
            assert!(parent > directory_time);
        }
        for name in ["first", "second"] {
            let new = fs::metadata(scratch.join("watched").join(name)).unwrap();
            let alias = fs::metadata(scratch.join("node_modules").join(name)).unwrap();
            assert_ne!(new.ino(), alias.ino());
            assert_eq!(new.nlink(), 1);
            assert_eq!(alias.modified().unwrap(), file_time);
            if preserve_times {
                assert_eq!(new.modified().unwrap(), file_time);
            } else {
                assert!(new.modified().unwrap() > file_time);
            }
        }
        let names = fs::read_dir(scratch.join("watched"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2, "temporary source names must be removed");
    }

    #[test]
    fn seeded_rematerialization_preserves_file_and_parent_mtimes() {
        rematerialize_times(true);
    }

    #[test]
    fn ordinary_rematerialization_retains_fresh_file_and_parent_mtimes() {
        rematerialize_times(false);
    }

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
        refresh(&plan, b"100644 blob a\ttracked\0", false, |_| {
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
