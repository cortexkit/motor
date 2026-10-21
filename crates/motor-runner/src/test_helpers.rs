//! Helpers shared by tests that publish and execute temporary programs.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_test_executable(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_test_executable_with(path, contents, |_| Ok(()))
}

fn write_test_executable_with(
    path: &Path,
    contents: &[u8],
    while_unpublished: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "executable has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "executable has no name"))?
        .to_string_lossy();
    let temporary = loop {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(mut file) => {
                let result = (|| {
                    file.write_all(contents)?;
                    while_unpublished(path)
                })();
                drop(file);
                if let Err(error) = result {
                    let _ = fs::remove_file(&temporary);
                    return Err(error);
                }
                break temporary;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    let result = (|| {
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn executable_is_published_only_after_the_writer_is_closed() {
        let scratch = cortexkit_test_support::ScratchDir::new("test-executable-publication");
        let executable = scratch.join("program");
        write_test_executable_with(&executable, b"#!/bin/sh\nexit 0\n", |path| {
            let error = Command::new(path).output().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            Ok(())
        })
        .unwrap();

        assert!(Command::new(executable).status().unwrap().success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_writer_fd_reproduces_exec_text_file_busy() {
        use std::fs::OpenOptions;

        let scratch = cortexkit_test_support::ScratchDir::new("inherited-executable-writer");
        let executable = scratch.join("program");
        write_test_executable(&executable, b"#!/bin/sh\nexit 0\n").unwrap();

        let writer = OpenOptions::new().write(true).open(&executable).unwrap();
        let error = Command::new(&executable).output().unwrap_err();
        eprintln!("fork/exec while writer is inherited: {error}");
        assert_eq!(error.kind(), io::ErrorKind::ExecutableFileBusy);
        drop(writer);
        assert!(Command::new(executable).status().unwrap().success());
    }
}
