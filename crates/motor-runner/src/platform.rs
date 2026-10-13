use serde::{Deserialize, Serialize};
#[cfg(not(target_os = "linux"))]
use std::process::Stdio;
use std::{
    fs::File,
    io,
    path::Path,
    process::{Command, ExitStatus},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity {
    pub pid: i32,
    pub start: u64,
    /// Transient service identity survives daemon restart; old records have none.
    #[serde(default)]
    pub unit: Option<uuid::Uuid>,
}

pub(crate) struct Launch {
    #[cfg(test)]
    pub(crate) _test_guard: crate::test_cleanup::LaunchGuard,
    pub durable_output: bool,
    pub identity: ProcessIdentity,
    pub stdout: File,
    pub stderr: File,
    #[cfg(target_os = "linux")]
    pub(crate) gate: File,
    #[cfg(target_os = "linux")]
    pub(crate) runtime: Option<std::sync::Arc<crate::isolation::RuntimeDir>>,
    #[cfg(not(target_os = "linux"))]
    child: std::process::Child,
}

#[cfg(target_os = "linux")]
pub(crate) fn identity(pid: i32) -> Option<ProcessIdentity> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let start = text
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    Some(ProcessIdentity {
        pid,
        start,
        unit: None,
    })
}

/// Only signal a recorded process group if its leader still has the recorded
/// kernel start time. A PID reused after a crash belongs to somebody else.
#[cfg(target_os = "linux")]
pub(crate) fn signal(process: ProcessIdentity, hard: bool) {
    if let Some(id) = process.unit {
        crate::isolation::signal(id, hard);
        return;
    }
    if identity(process.pid).is_some_and(|p| p.start == process.start) {
        unsafe {
            libc::kill(
                -process.pid,
                if hard { libc::SIGKILL } else { libc::SIGTERM },
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn signal(_process: ProcessIdentity, _hard: bool) {}

#[cfg(target_os = "linux")]
pub(crate) fn launch(command: &str, cwd: &Path, env: &[(String, String)]) -> io::Result<Launch> {
    use std::{
        ffi::CString,
        os::{fd::FromRawFd, unix::ffi::OsStrExt},
    };
    let shell = CString::new("/bin/bash").unwrap();
    let args = [
        CString::new("bash").unwrap(),
        CString::new("-c").unwrap(),
        CString::new(command)?,
    ];
    let mut argv: Vec<_> = args.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let strings: Vec<_> = env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect::<Result<_, _>>()?;
    let mut envp: Vec<_> = strings.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());
    let cwd = CString::new(cwd.as_os_str().as_bytes())?;
    let pipe = || -> io::Result<(File, File)> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
    };
    use std::os::fd::AsRawFd;
    let (gate_read, gate_write) = pipe()?;
    let (stdout_read, stdout_write) = pipe()?;
    let (stderr_read, stderr_write) = pipe()?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // Everything allocated above the fork. Only async-signal-safe syscalls
        // are used here; allocator/runtime locks can be held by other threads.
        unsafe {
            libc::close(gate_write.as_raw_fd());
            libc::close(stdout_read.as_raw_fd());
            libc::close(stderr_read.as_raw_fd());
            if libc::setpgid(0, 0) != 0 {
                libc::_exit(126);
            }
            // Job-created private directories must not inherit a group-writable
            // mask from the daemon (security checks expect mode 0755).
            libc::umask(0o022);
            let mut byte = 0u8;
            loop {
                let n = libc::read(gate_read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
                if n == 1 {
                    break;
                }
                if n == -1 && *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                libc::_exit(125);
            }
            libc::close(gate_read.as_raw_fd());
            if libc::chdir(cwd.as_ptr()) != 0 {
                libc::_exit(126);
            }
            libc::dup2(stdout_write.as_raw_fd(), 1);
            libc::dup2(stderr_write.as_raw_fd(), 2);
            libc::execve(shell.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    drop(gate_read);
    drop(stdout_write);
    drop(stderr_write);
    unsafe {
        libc::setpgid(pid, pid);
    }
    let identity = identity(pid).ok_or_else(|| io::Error::other("child vanished before launch"))?;
    Ok(Launch {
        #[cfg(test)]
        _test_guard: crate::test_cleanup::LaunchGuard(identity),
        durable_output: false,
        identity,
        stdout: stdout_read,
        stderr: stderr_read,
        gate: gate_write,
        runtime: None,
    })
}

// The portable backend exists for protocol/in-process tests on the Mac. The
// production crash-safe process-group launch backend is Linux-only.
#[cfg(not(target_os = "linux"))]
pub(crate) fn launch(command: &str, cwd: &Path, env: &[(String, String)]) -> io::Result<Launch> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::process::CommandExt;
    let mut executable = Command::new("/bin/bash");
    executable
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().cloned())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    executable.process_group(0);
    // Only the async-signal-safe syscall is used between fork and exec.
    unsafe {
        executable.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    let mut child = executable.spawn()?;
    let stdout = unsafe { File::from_raw_fd(child.stdout.take().unwrap().into_raw_fd()) };
    let stderr = unsafe { File::from_raw_fd(child.stderr.take().unwrap().into_raw_fd()) };
    let identity = ProcessIdentity {
        pid: child.id() as i32,
        start: 0,
        unit: None,
    };
    Ok(Launch {
        #[cfg(test)]
        _test_guard: crate::test_cleanup::LaunchGuard(identity),
        durable_output: false,
        identity,
        stdout,
        stderr,
        child,
    })
}

pub(crate) fn unit_alive(process: Option<ProcessIdentity>) -> io::Result<Option<bool>> {
    #[cfg(target_os = "linux")]
    if let Some(id) = process.and_then(|p| p.unit) {
        return crate::isolation::unit_alive(id).map(Some);
    }
    let _ = process;
    Ok(None)
}

impl Launch {
    pub fn release(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::io::Write;
            self.gate.write_all(&[1])?;
        }
        Ok(())
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::ExitStatusExt;
            let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
            loop {
                if unsafe {
                    libc::waitid(
                        libc::P_PID,
                        self.identity.pid as u32,
                        &mut status,
                        libc::WEXITED | libc::WNOWAIT,
                    )
                } >= 0
                {
                    let code = unsafe { status.si_status() };
                    let raw = if status.si_code == libc::CLD_EXITED {
                        code << 8
                    } else {
                        code
                    };
                    return Ok(ExitStatus::from_raw(raw));
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.child.wait()
        }
    }
    pub fn reap(&self) {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::waitpid(self.identity.pid, std::ptr::null_mut(), 0);
        }
        #[cfg(target_os = "linux")]
        let _ = &self.runtime;
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn clone_target(source: &Path, target: &Path) -> io::Result<()> {
    use cortexkit_cow::{backend, BackendKind};
    backend(BackendKind::LinuxReflink)
        .start(source, target)
        .map_err(io::Error::other)
}

/// Instances use different wrappers but the same slot root and limit file.
pub(crate) fn build_wrapper(
    source: &Path,
    output: &Path,
    slots: &Path,
    limit: u32,
) -> io::Result<()> {
    use fs2::FileExt;
    std::fs::create_dir_all(slots)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(slots.join("configure.lock"))?;
    lock.lock_exclusive()?;
    std::fs::write(slots.join("limit"), limit.to_string())?;
    if !output.exists() {
        let status = Command::new("rustc")
            .arg(source)
            // This is a standalone rustc invocation, not a Cargo build. Match
            // prefrontal-build-slot's edition instead of rustc's 2015 default.
            .args(["--edition=2021", "-O", "-o"])
            .arg(output)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("build-slot compilation failed"));
        }
    }
    std::fs::write(
        output.with_extension("root"),
        slots.as_os_str().as_encoded_bytes(),
    )?;
    std::fs::write(output.with_extension("cache"), "")?;
    Ok(())
}

/// Keep cache-use locks outside evictable entries so a rename or deletion cannot
/// give another setup step a different lock for the same install key.
pub(crate) fn javascript_cache_lock(state: &Path, key: &str) -> io::Result<File> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::other("invalid JavaScript cache key"));
    }
    let locks = state.join("js-install-locks");
    std::fs::create_dir_all(&locks)?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(locks.join(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    struct ParentMask(libc::mode_t);
    impl Drop for ParentMask {
        fn drop(&mut self) {
            unsafe {
                libc::umask(self.0);
            }
        }
    }

    #[test]
    fn job_process_sets_022_umask_independently_of_daemon() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-job-umask");
        // Nextest runs each test in a separate process. Restore the parent's
        // mask as well, so the test does not leave global process state changed.
        let _parent = ParentMask(unsafe { libc::umask(0o002) });
        let stat = if cfg!(target_os = "linux") {
            "stat -c %a d"
        } else {
            "stat -f %Lp d"
        };
        let mut launch = launch(
            &format!("set -eu; umask; mkdir d; {stat}"),
            &scratch,
            &[("PATH".into(), "/usr/bin:/bin".into())],
        )
        .unwrap();
        launch.release().unwrap();
        let mut stdout = String::new();
        launch.stdout.read_to_string(&mut stdout).unwrap();
        assert!(launch.wait().unwrap().success());
        launch.reap();
        assert_eq!(stdout, "0022\n755\n");
    }
}
