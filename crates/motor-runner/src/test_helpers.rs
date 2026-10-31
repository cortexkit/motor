//! Helpers shared by tests that publish and execute temporary programs.
use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Keep scratch ownership while exposing a path accepted by no-follow staging.
/// macOS's default scratch base contains the /tmp or /var symlink; resolving
/// only this trusted fixture root leaves staging's own symlink checks intact.
pub(crate) struct CanonicalScratchDir {
    _owned: cortexkit_test_support::ScratchDir,
    path: std::path::PathBuf,
}

impl CanonicalScratchDir {
    pub(crate) fn new(prefix: &str) -> Self {
        let owned = cortexkit_test_support::ScratchDir::new(prefix);
        let path = fs::canonicalize(owned.path()).expect("canonical scratch root");
        Self {
            _owned: owned,
            path,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for CanonicalScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.path()
    }
}

impl AsRef<Path> for CanonicalScratchDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

fn wait_until_stopped(child: &mut Child) -> io::Result<()> {
    let pid = child.id() as libc::pid_t;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if result == pid {
            return if libc::WIFSTOPPED(status) {
                Ok(())
            } else {
                Err(io::Error::other(
                    "executable writer exited before its staging barrier",
                ))
            };
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        } else if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "executable writer did not reach its staging barrier",
            ));
        }
        thread::sleep(Duration::from_millis(1));
    }
}

pub(crate) fn write_test_executable(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_test_executable_with(path, contents, |_| Ok(()))
}

/// A child process forked from the test process and stopped before `exec`,
/// holding a copy of every descriptor the test process had open at the fork.
///
/// Tests run on many threads, and other threads spawn processes all the time.
/// Each such child briefly shares every open descriptor, so a lock, socket or
/// file a test closes may stay open in another process for a moment. This
/// makes that moment last until the guard is dropped, so a test can prove its
/// release does not depend on it. Dropping the guard lets the child exec and
/// waits for it.
pub(crate) struct ForkedBeforeExec {
    release: std::os::unix::net::UnixStream,
    child: Option<thread::JoinHandle<io::Result<std::process::ExitStatus>>>,
}

pub(crate) fn fork_before_exec() -> io::Result<ForkedBeforeExec> {
    use std::os::{fd::AsRawFd, unix::net::UnixStream, unix::process::CommandExt};
    let (mut parent, child) = UnixStream::pair()?;
    parent.set_read_timeout(Some(Duration::from_secs(30)))?;
    let child_fd = child.as_raw_fd();
    let forked = thread::spawn(move || {
        let _child_channel = child;
        let mut command = Command::new("/bin/sh");
        command.args(["-c", ":"]);
        // SAFETY: between fork and exec the closure only calls write and read,
        // which are async-signal-safe, on a descriptor the child inherited,
        // and on failure reads errno, which does not allocate.
        unsafe {
            command.pre_exec(move || {
                let ready = [1u8];
                if libc::write(child_fd, ready.as_ptr().cast(), ready.len()) != 1 {
                    return Err(io::Error::last_os_error());
                }
                let mut release = [0u8];
                if libc::read(child_fd, release.as_mut_ptr().cast(), release.len()) != 1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.status()
    });
    let mut ready = [0u8];
    if let Err(error) = parent.read_exact(&mut ready) {
        // Release a child that is parked but answered too late, then reap it.
        let _ = parent.write_all(&[1]);
        let _ = forked.join();
        return Err(io::Error::other(format!(
            "forked child did not reach pre-exec: {error}"
        )));
    }
    Ok(ForkedBeforeExec {
        release: parent,
        child: Some(forked),
    })
}

impl Drop for ForkedBeforeExec {
    fn drop(&mut self) {
        let _ = self.release.write_all(&[1]);
        if let Some(child) = self.child.take() {
            let _ = child.join();
        }
    }
}

fn write_test_executable_with(
    path: &Path,
    contents: &[u8],
    while_unpublished: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "executable has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "executable has no name"))?
        .to_string_lossy();
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));

    // Keep the writable file handle in a helper process so subprocesses launched by other test threads cannot inherit it.
    let mut writer = Command::new("/bin/sh")
        .arg("-c")
        .arg("umask 022; set -C; exec 3>\"$2\"; /bin/cat >&3 || exit; printf 'ready\\n'; kill -STOP $$; exec 3>&-; /bin/chmod 755 \"$2\" && /bin/mv -f \"$2\" \"$1\"")
        .arg("sh")
        .arg(path)
        .arg(&temporary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let write_result = writer.stdin.as_mut().unwrap().write_all(contents);
    drop(writer.stdin.take());
    if let Err(error) = write_result {
        let _ = writer.kill();
        let _ = writer.wait();
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    let mut ready = [0; 6];
    if let Err(error) = writer.stdout.as_mut().unwrap().read_exact(&mut ready) {
        let _ = writer.kill();
        let _ = writer.wait();
        return Err(error);
    }
    if &ready != b"ready\n" {
        let _ = writer.kill();
        let _ = writer.wait();
        return Err(io::Error::other(
            "executable writer did not hold its staging file",
        ));
    }

    // Only the fixed readiness report matters. A concurrent pre-exec fork can
    // retain the pipe's write end even after the writer exits, so publication
    // must depend on this child's exit rather than pipe EOF.
    drop(writer.stdout.take());

    if let Err(error) = wait_until_stopped(&mut writer) {
        let _ = writer.kill();
        let _ = writer.wait();
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    let hook_result = while_unpublished(path);
    if unsafe { libc::kill(writer.id() as i32, libc::SIGCONT) } != 0 {
        let error = io::Error::last_os_error();
        let _ = writer.kill();
        let _ = writer.wait();
        return Err(error);
    }
    if let Err(error) = wait_for_executable_writer(writer) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    hook_result
}

fn wait_for_executable_writer(mut writer: Child) -> io::Result<()> {
    let status = writer.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "executable writer failed: {status}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod pipe_tests {
    use super::*;
    use std::{
        os::{fd::OwnedFd, unix::net::UnixStream},
        process::ChildStdout,
        sync::mpsc,
    };

    #[test]
    fn executable_writer_reaping_does_not_wait_for_inherited_report_pipe() {
        let mut writer = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let (report, inherited_writer) = UnixStream::pair().unwrap();
        // Model a report pipe whose writer survived in an unrelated fork. The
        // actual child exits immediately, but EOF cannot arrive until release.
        writer.stdout = Some(ChildStdout::from(OwnedFd::from(report)));
        let (finished, result) = mpsc::channel();
        let waiter = thread::spawn(move || {
            finished.send(wait_for_executable_writer(writer)).unwrap();
        });
        let before_release = result.recv_timeout(Duration::from_secs(2));
        drop(inherited_writer);
        waiter.join().unwrap();
        before_release
            .expect("writer exit must not depend on report-pipe EOF")
            .unwrap();
    }
}

// The race it proves needs Linux's ETXTBSY behaviour, so the module is Linux-only.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream, unix::process::CommandExt},
        process::Command,
        thread,
    };

    #[test]
    fn concurrent_fork_during_executable_write_cannot_inherit_its_writer_fd() {
        let scratch = cortexkit_test_support::ScratchDir::new("concurrent-executable-publication");
        let executable = scratch.join("program");
        let mut pending_exec = None;

        write_test_executable_with(&executable, b"#!/bin/sh\nexit 0\n", |path| {
            let (mut parent, child) = UnixStream::pair()?;
            parent.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
            child.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
            let child_fd = child.as_raw_fd();
            let executable = path.to_owned();
            let forked_exec = thread::spawn(move || {
                let _child_channel = child;
                let mut command = Command::new(executable);
                unsafe {
                    command.pre_exec(move || {
                        let ready = [1u8];
                        if libc::write(child_fd, ready.as_ptr().cast(), ready.len()) != 1 {
                            return Err(io::Error::last_os_error());
                        }
                        let mut release = [0u8];
                        if libc::read(child_fd, release.as_mut_ptr().cast(), release.len()) != 1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                command.status()
            });

            let mut ready = [0u8];
            parent.read_exact(&mut ready).map_err(|error| {
                io::Error::other(format!("forked child did not signal pre-exec: {error}"))
            })?;
            if ready != [1] {
                return Err(io::Error::other("forked child did not reach pre-exec"));
            }
            pending_exec = Some((parent, forked_exec));
            Ok(())
        })
        .unwrap();

        let (mut release, forked_exec) = pending_exec.unwrap();
        release.write_all(&[1]).unwrap();
        let status = forked_exec.join().unwrap().unwrap();
        assert!(status.success());
    }
}
