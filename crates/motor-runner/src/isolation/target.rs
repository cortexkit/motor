//! Final directory permissions for an ordinary job's private build output.

use super::checked;
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::Instant,
};

const BATCH: usize = 128;

pub(super) fn restrict_directories(
    root: &Path,
    uid: u32,
    ownership_repaired: bool,
) -> io::Result<()> {
    let started = Instant::now();
    let directories = physical_directories(root)?;
    let mut ownership = Vec::new();
    let mut masks: BTreeMap<String, Vec<&Path>> = BTreeMap::new();
    for (path, metadata) in &directories {
        let owner = if ownership_repaired || metadata.uid() == uid {
            // An already-job-owned directory may deliberately be read-only.
            // A named job ACL never overrides its owner bits; keep that limit.
            (metadata.mode() >> 6) & 7
        } else {
            if metadata.mode() & 0o1000 != 0 {
                // Owning a sticky directory grants unlink rights beyond its
                // named rwx ACL. Refuse rather than expanding the job's access.
                return Err(io::Error::other(format!(
                    "cannot transfer non-job-owned sticky target directory: {}",
                    path.display()
                )));
            }
            ownership.push(path);
            // The preceding recursive grant already gave the job rwx here.
            // Transfer that access to owner bits before masking named writes.
            7
        };
        masks
            .entry(directory_acl(owner, metadata.mode()))
            .or_default()
            .push(path);
    }
    for paths in ownership.chunks(BATCH) {
        checked(
            crate::privilege::command("/usr/bin/chown")
                .args(["-h", "motor-job:motor-job", "--"])
                .args(paths),
        )?;
    }
    for (acl, paths) in masks {
        for paths in paths.chunks(BATCH) {
            // Keep every named entry, including runner cleanup access. Only
            // directories lose effective non-owner write; regular files keep
            // the job write ACL needed for runner-owned cached build outputs.
            checked(
                crate::privilege::command("/usr/bin/setfacl")
                    .args(["-P", "-m", &acl, "--"])
                    .args(paths),
            )?;
        }
    }
    tracing::debug!(
        path = %root.display(),
        directories = directories.len(),
        ownership_repairs = ownership.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "restricted job target directory masks"
    );
    Ok(())
}

fn directory_acl(owner: u32, mode: u32) -> String {
    let permissions = |bits| {
        format!(
            "{}{}{}",
            if bits & 4 != 0 { 'r' } else { '-' },
            if bits & 2 != 0 { 'w' } else { '-' },
            if bits & 1 != 0 { 'x' } else { '-' },
        )
    };
    // Preserve other-class read/search restrictions rather than granting r-x
    // to directories that used to be private.
    format!(
        "u::{},m::r-x,o::{}",
        permissions(owner),
        permissions(mode & 5)
    )
}

fn physical_directories(root: &Path) -> io::Result<Vec<(PathBuf, fs::Metadata)>> {
    let mut pending = vec![root.to_path_buf()];
    let mut directories = Vec::new();
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() {
            return Err(io::Error::other("job target directory is not physical"));
        }
        ensure_runner_search(&path, &metadata)?;
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
        directories.push((path, metadata));
    }
    Ok(directories)
}

fn ensure_runner_search(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let runner = unsafe { libc::geteuid() };
    if runner == 0 {
        return Ok(());
    }
    let name = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: name is a live NUL-terminated pathname; faccessat borrows it.
    if unsafe {
        libc::faccessat(
            libc::AT_FDCWD,
            name.as_ptr(),
            libc::R_OK | libc::X_OK,
            libc::AT_EACCESS,
        )
    } == 0
    {
        return Ok(());
    }
    // Initial chown can leave private directories unreadable to an
    // unprivileged runner. Grant traversal only where the walk needs it;
    // avoid another recursive ACL pass over all cached regular files.
    let acl = if metadata.uid() == runner {
        "u::rwx".to_owned()
    } else {
        format!("u:{runner}:rwx")
    };
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-P", "-m", &acl, "--"])
            .arg(path),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_acl_preserves_owner_and_other_limits_without_mask_write() {
        for owner in 0..8 {
            for other in 0..8 {
                let acl = directory_acl(owner, 0o770 | other);
                assert!(acl.contains("m::r-x"), "{acl}");
                let entries: Vec<_> = acl.split(',').collect();
                let other_permissions = entries[2].strip_prefix("o::").unwrap();
                assert_eq!(other_permissions.as_bytes()[0] == b'r', other & 4 != 0);
                assert_eq!(other_permissions.as_bytes()[1], b'-');
                assert_eq!(other_permissions.as_bytes()[2] == b'x', other & 1 != 0);
                let owner_permissions = entries[0].strip_prefix("u::").unwrap();
                for (index, bit) in [4, 2, 1].into_iter().enumerate() {
                    assert_eq!(
                        owner_permissions.as_bytes()[index] != b'-',
                        owner & bit != 0
                    );
                }
            }
        }
    }

    #[test]
    fn directory_walk_excludes_files_and_does_not_follow_symlinks() {
        let scratch = cortexkit_test_support::ScratchDir::new("target-directory-walk");
        let root = scratch.join("target");
        let outside = scratch.join("outside");
        fs::create_dir_all(root.join("bd/hash/out")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("cached-output"), "retained").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let directories = physical_directories(&root).unwrap();
        assert_eq!(directories.len(), 4);
        assert!(directories.iter().all(|(path, _)| path.starts_with(&root)));
        assert_eq!(
            fs::read_to_string(root.join("cached-output")).unwrap(),
            "retained"
        );
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }
}
