use serde::{Deserialize, Serialize};
#[cfg(all(unix, not(target_os = "linux")))]
use std::process::Stdio;
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
    process::{Command, ExitStatus},
    sync::atomic::{AtomicU64, Ordering},
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
    // Computed before the fork because the child may not allocate. The child
    // keeps only the inherited stdin, stdout and stderr (0-2; the job's pipes
    // replace 1 and 2 after the gate), the gate's read end and the job's output
    // write ends. See `close_inherited_descriptors` for why everything else goes.
    let close = descriptor_ranges_to_close(&[
        0,
        1,
        2,
        gate_read.as_raw_fd(),
        stdout_write.as_raw_fd(),
        stderr_write.as_raw_fd(),
    ]);
    let close_fallback_end = descriptor_limit();
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // Everything allocated above the fork. Only async-signal-safe syscalls
        // are used here; allocator/runtime locks can be held by other threads.
        unsafe {
            // This also closes the parent's ends of the three pipes.
            close_inherited_descriptors(&close, close_fallback_end);
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

/// Inclusive descriptor ranges that cover every descriptor number except
/// `keep`, ending at the largest number `close_range` accepts.
#[cfg(any(target_os = "linux", test))]
fn descriptor_ranges_to_close(keep: &[std::os::fd::RawFd]) -> Vec<(u32, u32)> {
    let mut keep: Vec<u32> = keep
        .iter()
        .filter_map(|&fd| u32::try_from(fd).ok())
        .collect();
    keep.sort_unstable();
    keep.dedup();
    let mut ranges = Vec::new();
    let mut next = 0u32;
    for fd in keep {
        if fd > next {
            ranges.push((next, fd - 1));
        }
        // A RawFd is at most i32::MAX, so this cannot overflow.
        next = fd + 1;
    }
    ranges.push((next, u32::MAX));
    ranges
}

/// Bound for the close-one-at-a-time fallback: a process cannot open a
/// descriptor at or above its open-file limit. An unlimited or huge limit is
/// capped at the kernel's default `fs.nr_open` so the fallback stays bounded.
#[cfg(target_os = "linux")]
fn descriptor_limit() -> u32 {
    const DEFAULT_NR_OPEN: u32 = 1 << 20;
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return DEFAULT_NR_OPEN;
    }
    u32::try_from(limit.rlim_cur)
        .unwrap_or(DEFAULT_NR_OPEN)
        .min(DEFAULT_NR_OPEN)
}

/// Close the descriptors in `ranges` in a freshly forked launch child.
///
/// The child waits at the gate until the job's Started record is durable, which
/// can take a long time under load, and until it execs it holds a copy of every
/// descriptor the whole runner had open at the fork, whatever thread opened it.
/// An flock belongs to the open file all those copies share, and a socket or
/// pipe end stays open while any copy exists. Without this, a lock the runner
/// has released by closing its descriptor (the runner state lock, cache locks,
/// store locks) stays held, a listening socket the runner closed stays open and
/// still completes connections, and a reader waits for EOF on another command's
/// output pipe until the job starts.
///
/// Only async-signal-safe system calls: another thread may have held the
/// allocator lock at the fork. `close_range` needs Linux 5.9; when it fails
/// (ENOSYS on older kernels), the range is closed one descriptor at a time,
/// but only below `fallback_end`.
#[cfg(target_os = "linux")]
unsafe fn close_inherited_descriptors(ranges: &[(u32, u32)], fallback_end: u32) {
    for &(first, last) in ranges {
        let closed = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                first as libc::c_uint,
                last as libc::c_uint,
                0 as libc::c_uint,
            )
        };
        if closed != 0 {
            let mut fd = first;
            while fd <= last && fd < fallback_end {
                unsafe { libc::close(fd as libc::c_int) };
                fd += 1;
            }
        }
    }
}

// The portable backend exists for protocol/in-process tests on non-Linux Unix
// development hosts such as macOS. The production crash-safe process-group
// launch backend is Linux-only.
#[cfg(all(unix, not(target_os = "linux")))]
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

#[cfg(windows)]
pub(crate) fn launch(_: &str, _: &Path, _: &[(String, String)]) -> io::Result<Launch> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native Windows hosts only execute inside Hyper-V",
    ))
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
    check_clone_device(source, target)?;
    backend(BackendKind::LinuxReflink)
        .start(source, target)
        .map_err(io::Error::other)
}

#[cfg(target_os = "linux")]
fn check_clone_device(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    // Mount tables may contain hidden mounts at the same path. stat follows
    // the effective namespace instead; the ioctl remains the final authority.
    let source_device = std::fs::metadata(source)?.dev();
    let mut parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    loop {
        match std::fs::metadata(parent) {
            Ok(metadata) => {
                let target_device = metadata.dev();
                if source_device != target_device {
                    return Err(io::Error::other(format!(
                        "reflink paths are on different filesystems: {} (device {source_device}) -> {} (existing parent {} device {target_device})",
                        source.display(), target.display(), parent.display()
                    )));
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if parent == Path::new(".") || parent == Path::new("/") {
                    return Err(error);
                }
                parent = parent
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
            }
            Err(error) => return Err(error),
        }
    }
}

static NEXT_CONTROL_FILE_TEMP: AtomicU64 = AtomicU64::new(0);

fn publish_control_file(path: &Path, contents: &[u8], create_if_missing: bool) -> io::Result<()> {
    publish_control_file_with(path, contents, create_if_missing, || Ok(()))
}

fn publish_control_file_with(
    path: &Path,
    contents: &[u8],
    create_if_missing: bool,
    before_rename: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == contents || create_if_missing => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "control file has no file name")
    })?;
    let (temporary_path, mut temporary_file) = loop {
        let sequence = NEXT_CONTROL_FILE_TEMP.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(".{}.{}.tmp", std::process::id(), sequence));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => break (temporary_path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    let write_result = (|| {
        temporary_file.write_all(contents)?;
        temporary_file.sync_all()
    })();
    drop(temporary_file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = before_rename().and_then(|()| fs::rename(&temporary_path, path)) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    #[cfg(unix)]
    crate::filesystem::sync_directory(parent)?;
    Ok(())
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
    crate::setup_log::lock_wait("build_wrapper", || lock.lock_exclusive())?;
    publish_control_file(&slots.join("limit"), limit.to_string().as_bytes(), false)?;
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
    publish_control_file(
        &output.with_extension("root"),
        slots.as_os_str().as_encoded_bytes(),
        false,
    )?;
    publish_control_file(&output.with_extension("cache"), b"", true)?;
    Ok(())
}

/// An open JavaScript cache lock file; lock it with `lock_exclusive` or
/// `try_lock_exclusive`.
///
/// Dropping it releases the lock explicitly before closing the file. A process
/// that another thread forks gets a copy of every open descriptor and keeps it
/// until it execs (or for good, if it never does). An flock belongs to the open
/// file that every copy shares, so closing our descriptor alone would leave the
/// lock held by that copy for a while, and eviction or the next setup step
/// would see the cache entry as still in use. An explicit unlock releases it
/// for every copy at once.
pub(crate) struct JavascriptCacheLock(File);

impl JavascriptCacheLock {
    // Outside tests only the Linux JavaScript install step waits for this
    // lock; eviction only tries it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn lock_exclusive(&self) -> io::Result<()> {
        fs2::FileExt::lock_exclusive(&self.0)
    }

    pub(crate) fn try_lock_exclusive(&self) -> io::Result<()> {
        fs2::FileExt::try_lock_exclusive(&self.0)
    }
}

impl Drop for JavascriptCacheLock {
    fn drop(&mut self) {
        // When this open file holds no lock (a failed try), unlocking is a
        // no-op: flock state belongs to the open file, not to the path.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// Keep cache-use locks outside evictable entries so a rename or deletion cannot
/// give another setup step a different lock for the same install key.
pub(crate) fn javascript_cache_lock(state: &Path, key: &str) -> io::Result<JavascriptCacheLock> {
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
        .map(JavascriptCacheLock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    #[cfg(target_os = "linux")]
    fn clone_target_checks_effective_devices_before_creating_destination() {
        use std::os::unix::fs::MetadataExt;

        let scratch = cortexkit_test_support::ScratchDir::new("motor-clone-devices");
        assert_ne!(
            std::fs::metadata(&scratch).unwrap().dev(),
            std::fs::metadata("/proc").unwrap().dev()
        );
        let target = Path::new("/proc/motor-reflink-device-test/missing/clone");
        let error = clone_target(&scratch, target).unwrap_err().to_string();
        assert!(error.contains("different filesystems"), "{error}");
        assert!(error.contains("existing parent /proc device"), "{error}");
        assert!(!target.exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn clone_target_follows_same_device_alias_and_missing_parent() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-clone-device-alias");
        let source = scratch.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("bytes"), "clone bytes").unwrap();
        let alias = scratch.join("alias");
        std::os::unix::fs::symlink(&scratch, &alias).unwrap();
        let target = alias.join("missing/clone");
        clone_target(&source, &target).unwrap();
        assert_eq!(std::fs::read(target.join("bytes")).unwrap(), b"clone bytes");
    }

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

    #[test]
    fn descriptor_ranges_cover_everything_but_kept_descriptors() {
        assert_eq!(
            descriptor_ranges_to_close(&[0, 1, 2, 7, 5, 5, -1]),
            vec![(3, 4), (6, 6), (8, u32::MAX)]
        );
        assert_eq!(descriptor_ranges_to_close(&[1, 0]), vec![(2, u32::MAX)]);
        assert_eq!(
            descriptor_ranges_to_close(&[3]),
            vec![(0, 2), (4, u32::MAX)]
        );
    }

    /// A launch child parked at the gate must not keep the runner's locks and
    /// sockets alive after the runner closes them.
    #[test]
    #[cfg(target_os = "linux")]
    fn parked_launch_child_does_not_hold_parent_locks_or_sockets() {
        use std::time::{Duration, Instant};

        let scratch = cortexkit_test_support::ScratchDir::new("motor-parked-child-descriptors");
        let lock_path = scratch.join("lock");
        let lock = File::create(&lock_path).unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let mut launch = launch("echo out; echo err >&2", &scratch, &[]).unwrap();
        let pid = launch.identity.pid;
        // Closing the only descriptor releases an flock and stops a listener,
        // unless a copy of the descriptor lives on in the parked child.
        drop(lock);
        drop(listener);

        let probe = File::open(&lock_path).unwrap();
        // The child closes its copies right after the fork, which may run a
        // little after `launch` returns; a parked child holding them never does.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let lock_released = fs2::FileExt::try_lock_exclusive(&probe).is_ok();
            let listener_closed = matches!(
                std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)),
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused
            );
            if lock_released && listener_closed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "parked child still holds parent descriptors: \
                 lock_released={lock_released} listener_closed={listener_closed}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // Still parked: the child has not exec'd bash yet.
        assert_eq!(
            std::fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
            std::fs::read_link("/proc/self/exe").unwrap()
        );
        // Stdin, stdout, stderr, the gate and the job's two output pipes.
        let open: Vec<_> = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(open.len() <= 6, "parked child has descriptors {open:?}");

        // The descriptors the job needs survived.
        launch.release().unwrap();
        let mut stdout = String::new();
        launch.stdout.read_to_string(&mut stdout).unwrap();
        let mut stderr = String::new();
        launch.stderr.read_to_string(&mut stderr).unwrap();
        assert!(launch.wait().unwrap().success());
        launch.reap();
        assert_eq!((stdout.as_str(), stderr.as_str()), ("out\n", "err\n"));
    }

    #[test]
    fn control_file_publication_keeps_old_contents_until_rename() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-control-file-atomic");
        let path = scratch.join("limit");
        std::fs::write(&path, b"old").unwrap();

        let result = publish_control_file_with(&path, b"new", false, || {
            assert_eq!(std::fs::read(&path).unwrap(), b"old");
            Err(io::Error::other("injected publication failure"))
        });

        assert_eq!(
            result.unwrap_err().to_string(),
            "injected publication failure"
        );
        assert_eq!(std::fs::read(path).unwrap(), b"old");
    }

    #[test]
    fn unchanged_control_file_publication_preserves_mtime() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-control-file-mtime");
        let path = scratch.join("limit");
        std::fs::write(&path, b"4").unwrap();
        let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        publish_control_file(&path, b"4", false).unwrap();

        let after = std::fs::metadata(path).unwrap().modified().unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn build_wrapper_preserves_existing_cache_setting() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-wrapper-cache");
        let source = scratch.join("supervisor.rs");
        let output = scratch.join("wrapper");
        let slots = scratch.join("slots");
        std::fs::create_dir(&slots).unwrap();
        std::fs::write(&output, b"already built").unwrap();
        let cache = output.with_extension("cache");
        std::fs::write(&cache, b"/usr/bin/sccache\\n").unwrap();

        build_wrapper(&source, &output, &slots, 2).unwrap();

        assert_eq!(std::fs::read(cache).unwrap(), b"/usr/bin/sccache\\n");
    }
}
