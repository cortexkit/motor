use base64::{engine::general_purpose::STANDARD, Engine};
use motor_protocol::{
    Frame, ReturnFailureCause as Cause, ReturnPathsFailed, ReturnResult, ReturnedFile, Run,
    MAX_FRAME_BYTES,
};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::{
    ffi::CString,
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

// Resolve symlinks first for an explicit escape result, then walk the resolved
// path through directory descriptors without following replacements. No target
// outside the workspace is opened, even if a command left a racing descendant.
// Follow links through pinned directory descriptors, never through openat.
// Resolving '..' against the descriptor stack prevents a link from leaving the
// workspace, including a dangling link whose outside target does not exist.
#[cfg(unix)]
fn open_file(root: &Path, directory: &File, relative: &Path) -> Result<File, Cause> {
    let mut directories = vec![directory.try_clone().map_err(|_| Cause::Missing)?];
    let mut pending: std::collections::VecDeque<_> = relative
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        if component == "." {
            continue;
        }
        if component == ".." {
            if directories.len() == 1 {
                return Err(Cause::Escape);
            }
            directories.pop();
            continue;
        }
        let name = CString::new(component.as_bytes()).map_err(|_| Cause::Missing)?;
        let parent = directories.last().unwrap();
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            if io::Error::last_os_error().raw_os_error() != Some(libc::ELOOP) {
                return Err(Cause::Missing);
            }
            links += 1;
            if links > 40 {
                return Err(Cause::Missing);
            }
            let mut target = vec![0u8; 4097];
            let size = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    target.as_mut_ptr().cast(),
                    target.len(),
                )
            };
            if size < 0 || size as usize == target.len() {
                return Err(Cause::Missing);
            }
            target.truncate(size as usize);
            let target = Path::new(std::ffi::OsStr::from_bytes(&target));
            let relative = if target.is_absolute() {
                directories.truncate(1);
                target.strip_prefix(root).map_err(|_| Cause::Escape)?
            } else {
                target
            };
            for component in relative.components().rev() {
                pending.push_front(component.as_os_str().to_owned());
            }
            continue;
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|_| Cause::Missing)?;
        if pending.is_empty() {
            return if metadata.is_file() {
                Ok(file)
            } else {
                Err(Cause::NotRegular)
            };
        }
        if !metadata.is_dir() {
            return Err(Cause::NotRegular);
        }
        directories.push(file);
    }
    Err(Cause::NotRegular)
}

#[cfg(unix)]
fn open_directory(path: &Path) -> Result<(File, PathBuf), Cause> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or(Cause::Escape)?;
    let root_name = name.to_owned();
    let parent = fs::canonicalize(parent).map_err(|_| Cause::Escape)?;
    let directory = File::open(&parent).map_err(|_| Cause::Escape)?;
    let name = CString::new(name.as_bytes()).map_err(|_| Cause::Escape)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return Err(Cause::Escape);
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    let root = parent.join(root_name);
    Ok((directory, root))
}

// Windows jobs collect through the guest protocol, never through host paths.
#[cfg(windows)]
fn open_file(_: &Path, _: &File, _: &Path) -> Result<File, Cause> {
    Err(Cause::Escape)
}
#[cfg(windows)]
fn open_directory(_: &Path) -> Result<(File, PathBuf), Cause> {
    Err(Cause::Escape)
}

pub(crate) fn collect(request: &Run, seq: u64) -> ReturnResult {
    collect_after_measure(request, seq, || {})
}

pub(crate) fn set_seq(result: &mut ReturnResult, seq: u64, cap: u64) {
    result.seq = seq;
    if serde_json::to_vec(&Frame::ReturnResult(result.clone()))
        .is_ok_and(|json| json.len() > MAX_FRAME_BYTES)
    {
        let measured_bytes = result.returned_files.as_ref().map_or(0, |files| {
            files
                .iter()
                .map(|file| {
                    // Each padded base64 quartet represents three bytes, less its '=' suffix.
                    let padding = file.data.bytes().rev().take_while(|b| *b == b'=').count();
                    (file.data.len() / 4 * 3 - padding) as u64
                })
                .sum()
        });
        result.returned_files = None;
        result.return_paths_failed = Some(ReturnPathsFailed {
            measured_bytes,
            cap_bytes: cap,
            cause: Cause::FrameLimit,
        });
    }
}

fn collect_after_measure(request: &Run, seq: u64, before_read: impl FnOnce()) -> ReturnResult {
    let mut result = ReturnResult {
        job_id: request.job_id,
        seq,
        returned_files: None,
        return_paths_failed: None,
    };
    let mut measured = 0u64;
    let collected = (|| -> Result<Vec<ReturnedFile>, Cause> {
        let (directory, root) = open_directory(Path::new(&request.workspace_key))?;
        let mut files = Vec::new();
        for path in &request.return_paths {
            let relative = Path::new(path)
                .strip_prefix(&request.workspace_key)
                .map_err(|_| Cause::Escape)?;
            let file = open_file(&root, &directory, relative)?;
            measured = measured.saturating_add(file.metadata().map_err(|_| Cause::Missing)?.len());
            files.push(file);
        }
        if measured > request.return_max_bytes {
            return Err(Cause::OverCap);
        }
        before_read();
        let mut returned = Vec::new();
        let mut read_bytes = 0u64;
        for (path, mut file) in request.return_paths.iter().zip(&files) {
            let mut bytes = Vec::new();
            (&mut file)
                .take(request.return_max_bytes - read_bytes)
                .read_to_end(&mut bytes)
                .map_err(|_| Cause::Missing)?;
            read_bytes += bytes.len() as u64;
            returned.push(ReturnedFile {
                path: path.clone(),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                data: STANDARD.encode(bytes),
            });
        }
        // Recheck the open files, not their pathnames, to detect growth while
        // reading without ever reading a byte beyond the caller's total cap.
        measured = files.iter().try_fold(0u64, |sum, file| {
            file.metadata()
                .map(|m| sum.saturating_add(m.len()))
                .map_err(|_| Cause::Missing)
        })?;
        if measured > request.return_max_bytes {
            return Err(Cause::OverCap);
        }
        result.returned_files = Some(returned.clone());
        if serde_json::to_vec(&Frame::ReturnResult(result.clone()))
            .map_err(|_| Cause::FrameLimit)?
            .len()
            > MAX_FRAME_BYTES
        {
            return Err(Cause::FrameLimit);
        }
        Ok(returned)
    })();
    match collected {
        Ok(files) => result.returned_files = Some(files),
        Err(cause) => {
            result.returned_files = None;
            result.return_paths_failed = Some(ReturnPathsFailed {
                measured_bytes: measured,
                cap_bytes: request.return_max_bytes,
                cause,
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    fn collect(request: &Run) -> ReturnResult {
        super::collect(request, 0)
    }

    fn request(root: &Path, paths: &[&Path], cap: u64) -> Run {
        let mut run: Run = match serde_json::from_str::<Frame>(include_str!(
            "../../../test-vectors/exec-remote-v1/frames/run.json"
        ))
        .unwrap()
        {
            Frame::Run(run) => run,
            _ => unreachable!(),
        };
        run.workspace_key = root.to_str().unwrap().into();
        run.return_paths = paths.iter().map(|p| p.to_str().unwrap().into()).collect();
        run.return_max_bytes = cap;
        run
    }

    fn failed(result: ReturnResult, cause: Cause, measured: u64, cap: u64) {
        assert_eq!(result.returned_files, None);
        assert_eq!(
            result.return_paths_failed,
            Some(ReturnPathsFailed {
                cause,
                measured_bytes: measured,
                cap_bytes: cap
            })
        );
    }

    #[test]
    fn return_paths_one_byte_has_sha256_and_base64() {
        let dir = ScratchDir::new("return-paths");
        let file = dir.join("one");
        fs::write(&file, b"x").unwrap();
        let result = collect(&request(dir.path(), &[&file], 1));
        assert_eq!(result.return_paths_failed, None);
        assert_eq!(
            result.returned_files.unwrap(),
            vec![ReturnedFile {
                path: file.to_str().unwrap().into(),
                sha256: "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881".into(),
                data: "eA==".into(),
            }]
        );
    }

    #[test]
    fn return_paths_measure_all_files_before_reading_and_keep_no_partial_bytes() {
        let dir = ScratchDir::new("return-paths");
        let one = dir.join("one");
        let two = dir.join("two");
        fs::write(&one, b"x").unwrap();
        fs::write(&two, b"xy").unwrap();
        failed(
            collect(&request(dir.path(), &[&one, &two], 2)),
            Cause::OverCap,
            3,
            2,
        );
        failed(
            collect(&request(dir.path(), &[&two], 1)),
            Cause::OverCap,
            2,
            1,
        );
        failed(
            collect(&request(dir.path(), &[&dir.join("missing")], 1)),
            Cause::Missing,
            0,
            1,
        );
        failed(
            collect(&request(dir.path(), &[dir.path()], 1)),
            Cause::NotRegular,
            0,
            1,
        );
    }

    #[test]
    fn return_paths_detect_growth_between_measurement_and_read() {
        let dir = ScratchDir::new("return-paths");
        let file = dir.join("growing");
        fs::write(&file, b"x").unwrap();
        failed(
            collect_after_measure(&request(dir.path(), &[&file], 1), 0, || {
                fs::write(&file, b"xy").unwrap()
            }),
            Cause::OverCap,
            2,
            1,
        );
    }

    #[test]
    fn return_paths_escape_and_replaced_symlink_never_return_target_bytes() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDir::new("return-paths");
        let root = dir.join("workspace");
        fs::create_dir(&root).unwrap();
        let secret = dir.join("secret");
        fs::write(&secret, b"private").unwrap();
        let link = root.join("link");
        symlink(&secret, &link).unwrap();
        failed(
            collect(&request(&root, &[&link], 100)),
            Cause::Escape,
            0,
            100,
        );
        fs::remove_file(&secret).unwrap();
        failed(
            collect(&request(&root, &[&link], 100)),
            Cause::Escape,
            0,
            100,
        );
        fs::write(&secret, b"private").unwrap();
        fs::remove_file(&link).unwrap();
        fs::write(&link, b"x").unwrap();
        let result = collect_after_measure(&request(&root, &[&link], 1), 0, || {
            fs::remove_file(&link).unwrap();
            symlink(&secret, &link).unwrap();
        });
        assert_eq!(result.returned_files.unwrap()[0].data, "eA==");
    }

    // This fixture needs Linux's longer filesystem paths to exceed the JSON frame cap.
    #[cfg(target_os = "linux")]
    #[test]
    fn return_paths_frame_limit_accounts_for_json_path_escaping() {
        let dir = ScratchDir::new("return-paths");
        let mut path = dir.path().to_path_buf();
        for _ in 0..34 {
            path.push("\u{1}".repeat(100));
        }
        fs::create_dir_all(&path).unwrap();
        path.push("out");
        fs::write(&path, b"x").unwrap();
        let run = request(
            dir.path(),
            &vec![path.as_path(); 64],
            motor_protocol::RETURN_MAX_BYTES,
        );
        assert!(motor_protocol::validate_return_paths(
            &run.workspace_key,
            &run.return_paths,
            run.return_max_bytes
        )
        .is_ok());
        failed(
            collect(&run),
            Cause::FrameLimit,
            64,
            motor_protocol::RETURN_MAX_BYTES,
        );
    }

    #[test]
    fn return_paths_frame_size_uses_final_sequence_and_escaped_paths() {
        let mut result = ReturnResult {
            job_id: motor_protocol::Uuid::now_v7(),
            seq: 0,
            returned_files: Some(vec![
                ReturnedFile {
                    path: format!("/workspace/{}", "\u{1}".repeat(3500)),
                    sha256: "a".repeat(64),
                    data: "eA==".into()
                };
                64
            ]),
            return_paths_failed: None,
        };
        set_seq(&mut result, u64::MAX, 64);
        assert_eq!(result.seq, u64::MAX);
        failed(result, Cause::FrameLimit, 64, 64);
    }

    #[test]
    fn return_paths_follow_internal_links_but_refuse_replaced_workspace_root() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDir::new("return-paths");
        let canonical = fs::canonicalize(dir.path()).unwrap();
        let root = canonical.join("workspace");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("one"), b"x").unwrap();
        symlink("../one", root.join("sub/link")).unwrap();
        symlink(root.join("one"), root.join("absolute")).unwrap();
        let run = request(&root, &[&root.join("sub/link"), &root.join("absolute")], 2);
        let result = collect(&run);
        assert_eq!(result.return_paths_failed, None);
        assert!(result
            .returned_files
            .unwrap()
            .iter()
            .all(|f| f.data == "eA=="));
        fs::rename(&root, canonical.join("old-workspace")).unwrap();
        symlink(canonical.join("old-workspace"), &root).unwrap();
        failed(collect(&run), Cause::Escape, 0, 2);
    }

    #[test]
    fn return_paths_allow_a_symlinked_workspace_ancestor() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDir::new("return-paths");
        let base = fs::canonicalize(dir.path()).unwrap();
        let actual_parent = base.join("actual-parent");
        let linked_parent = base.join("linked-parent");
        fs::create_dir_all(actual_parent.join("workspace")).unwrap();
        symlink(&actual_parent, &linked_parent).unwrap();
        let root = linked_parent.join("workspace");
        let file = root.join("returned");
        fs::write(&file, b"x").unwrap();

        let result = collect(&request(&root, &[&file], 1));

        assert_eq!(result.return_paths_failed, None);
        assert_eq!(result.returned_files.unwrap()[0].data, "eA==");
    }
}
