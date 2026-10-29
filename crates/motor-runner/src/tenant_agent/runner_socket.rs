use std::{
    fs, io,
    os::fd::AsRawFd,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

const UNIT: &str = "motor-tenant-runner.service";

pub(super) enum Unit {
    Running(u32),
    Inactive,
    Absent,
}

pub(super) fn unit() -> io::Result<Unit> {
    let output = Command::new("systemctl")
        .args(["show", UNIT, "--property=MainPID,ActiveState,LoadState"])
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let property = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or("")
    };
    if property("LoadState=") == "not-found" {
        return Ok(Unit::Absent);
    }
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "cannot read {UNIT} state: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let pid: u32 = property("MainPID=").parse().map_err(io::Error::other)?;
    if pid != 0 && progress(pid)?.is_some() {
        return Ok(Unit::Running(pid));
    }
    match property("ActiveState=") {
        "inactive" | "failed" => Ok(Unit::Inactive),
        state => Err(io::Error::other(format!(
            "tenant_runner_exited: {UNIT} has no live MainPID (state={state})"
        ))),
    }
}

pub(super) fn attach_or_start(
    unit: Unit,
    adopt: impl FnOnce(u32) -> io::Result<()>,
    mut control: impl FnMut(&str) -> io::Result<()>,
    start: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match unit {
        Unit::Running(pid) => adopt(pid),
        Unit::Inactive => {
            // A failed transient unit may still be loaded even after MainPID
            // disappears. Unload it before reusing the systemd-run unit name.
            control("stop")?;
            control("reset-failed")?;
            start()
        }
        Unit::Absent => start(),
    }
}

pub(super) fn control(operation: &str) -> io::Result<()> {
    let output = Command::new("systemctl").args([operation, UNIT]).output()?;
    if output.status.success() || matches!(unit()?, Unit::Absent) {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "systemctl {operation} {UNIT}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

struct Progress {
    state: String,
    wchan: String,
}

fn progress(pid: u32) -> io::Result<Option<Progress>> {
    let directory = std::path::PathBuf::from(format!("/proc/{pid}"));
    let stat = match super::diagnostics::bounded(&directory.join("stat"), 4096) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let process = super::diagnostics::process(pid, &String::from_utf8_lossy(&stat))
        .ok_or_else(|| io::Error::other("invalid runner process stat"))?;
    if matches!(process.state.as_str(), "Z" | "X" | "x") {
        return Ok(None);
    }
    let wait = super::diagnostics::bounded(&directory.join("wchan"), 128).unwrap_or_default();
    let wchan = String::from_utf8_lossy(&wait)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(80)
        .collect();
    Ok(Some(Progress {
        state: process.state,
        wchan,
    }))
}

pub(super) fn main_pid() -> io::Result<Option<u32>> {
    let output = Command::new("systemctl")
        .args(["show", UNIT, "--property=MainPID", "--value"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "cannot read {UNIT} MainPID: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let pid: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(io::Error::other)?;
    Ok((pid != 0).then_some(pid))
}

pub(super) fn peer_is_current(
    socket: &impl AsRawFd,
    expected: u32,
    current_pid: &mut impl FnMut() -> io::Result<Option<u32>>,
) -> io::Result<bool> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of_val(&credentials) || credentials.pid <= 0 {
        return Err(io::Error::other("invalid runner socket peer credentials"));
    }
    // Query MainPID again after connecting, so a unit restart between lookup
    // and connect cannot authenticate the previous runner's listener.
    Ok(credentials.pid as u32 == expected && current_pid()? == Some(expected))
}

fn ready(
    path: &Path,
    current_pid: &mut impl FnMut() -> io::Result<Option<u32>>,
) -> io::Result<bool> {
    let Some(expected) = current_pid()? else {
        return Ok(false);
    };
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::other(format!(
                    "unsafe tenant path {}: expected an owned socket node",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let socket = match std::os::unix::net::UnixStream::connect(path) {
        Ok(socket) => socket,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    if !peer_is_current(&socket, expected, current_pid)? {
        return Err(io::Error::other(format!(
            "unsafe tenant path {}: socket listener is not the current {UNIT} MainPID",
            path.display()
        )));
    }
    Ok(true)
}

pub(super) fn wait(
    path: &Path,
    expected: u32,
    current_pid: impl FnMut() -> io::Result<Option<u32>>,
) -> io::Result<()> {
    let started = Instant::now();
    wait_with(
        path,
        expected,
        current_pid,
        || started.elapsed(),
        std::thread::sleep,
        |elapsed, progress| {
            tracing::info!(
                pid = expected,
                elapsed_s = elapsed.as_secs(),
                state = progress.state,
                wchan = progress.wchan,
                "tenant_runner_starting"
            );
        },
    )
}

fn wait_with(
    path: &Path,
    expected: u32,
    mut current_pid: impl FnMut() -> io::Result<Option<u32>>,
    elapsed: impl Fn() -> Duration,
    mut sleep: impl FnMut(Duration),
    mut report: impl FnMut(Duration, &Progress),
) -> io::Result<()> {
    let timeout = motor_protocol::TENANT_RUNNER_READINESS_WAIT;
    let mut next_report = Duration::from_secs(30);
    loop {
        let progress = progress(expected)?;
        if current_pid()? != Some(expected) || progress.is_none() {
            return Err(io::Error::other(format!(
                "tenant_runner_exited: {UNIT} MainPID {expected} exited or changed after {} ms",
                elapsed().as_millis()
            )));
        }
        if ready(path, &mut current_pid)? {
            return Ok(());
        }
        let elapsed = elapsed();
        if elapsed >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "tenant_runner_readiness_timeout: after {} ms runner socket {} is not listening for {UNIT} MainPID {expected}",
                    timeout.as_millis(),
                    path.display()
                ),
            ));
        }
        if elapsed >= next_report {
            report(elapsed, progress.as_ref().expect("live runner"));
            next_report = elapsed + Duration::from_secs(30);
        }
        // State cleanup can block on XFS filesystem I/O while other threads do the work.
        // Liveness remains progress until the cap: the main thread staying
        // in uninterruptible disk sleep (process state D) is not a failure.
        sleep(Duration::from_secs(1).min(timeout.saturating_sub(elapsed)));
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::{fs, os::unix::net::UnixListener};

    /// A socket node no process listens on, like one left by a previous boot.
    /// Binding and dropping a listener isn't reliable in a multithreaded test
    /// process: a child another test is forking holds a copy of the listening
    /// fd until it execs, so connects can still succeed after the drop.
    pub(in crate::tenant_agent) fn leftover_socket_node(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mode = libc::S_IFSOCK | 0o600;
        assert_eq!(
            unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, 0) },
            0,
            "mknod {}: {}",
            path.display(),
            io::Error::last_os_error()
        );
    }

    #[test]
    fn leftover_socket_is_not_ready_until_current_main_pid_listens() {
        let root = ScratchDir::new("motor-tenant-socket-ready");
        let path = root.join("runner.sock");
        leftover_socket_node(&path);
        let pid = std::process::id();
        assert!(!ready(&path, &mut || Ok(Some(pid))).unwrap());
        fs::remove_file(&path).unwrap();
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(ready(&path, &mut || Ok(Some(pid))).unwrap());
        assert!(ready(&path, &mut || Ok(Some(pid + 1))).is_err());
        let mut calls = 0;
        assert!(ready(&path, &mut || {
            calls += 1;
            Ok(Some(if calls == 1 { pid } else { pid + 1 }))
        })
        .is_err());
        wait(&path, pid, || Ok(Some(pid))).unwrap();
    }

    struct Clock(std::cell::Cell<Duration>);

    impl Clock {
        fn new() -> Self {
            Self(std::cell::Cell::new(Duration::ZERO))
        }
        fn elapsed(&self) -> Duration {
            self.0.get()
        }
        fn advance(&self, duration: Duration) {
            self.0.set(self.0.get() + duration);
        }
    }

    #[test]
    fn readiness_slower_than_thirty_seconds_succeeds_and_reports_progress() {
        let root = ScratchDir::new("motor-tenant-slow-ready");
        let path = root.join("runner.sock");
        let pid = std::process::id();
        let clock = Clock::new();
        let mut listener = None;
        let mut reports = Vec::new();
        wait_with(
            &path,
            pid,
            || Ok(Some(pid)),
            || clock.elapsed(),
            |duration| {
                clock.advance(duration);
                if clock.elapsed() == Duration::from_secs(61) {
                    listener = Some(UnixListener::bind(&path).unwrap());
                }
            },
            |elapsed, progress| {
                assert!(!progress.state.is_empty());
                reports.push(elapsed.as_secs());
            },
        )
        .unwrap();
        assert_eq!(clock.elapsed(), Duration::from_secs(61));
        assert_eq!(reports, [30, 60]);
    }

    #[test]
    fn attach_adopts_running_listening_runner_without_initialization() {
        let root = ScratchDir::new("motor-tenant-adopt-ready");
        let path = root.join("runner.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let pid = std::process::id();
        attach_or_start(
            Unit::Running(pid),
            |expected| wait(&path, expected, || Ok(Some(pid))),
            |_| panic!("must not reset a running unit"),
            || panic!("must not initialize or start a second runner"),
        )
        .unwrap();
        assert!(path.exists());
    }

    #[test]
    fn attach_waits_for_starting_runner_without_reset_or_restart() {
        let root = ScratchDir::new("motor-tenant-adopt-starting");
        let path = root.join("runner.sock");
        let pid = std::process::id();
        let clock = Clock::new();
        let mut listener = None;
        attach_or_start(
            Unit::Running(pid),
            |expected| {
                wait_with(
                    &path,
                    expected,
                    || Ok(Some(pid)),
                    || clock.elapsed(),
                    |duration| {
                        clock.advance(duration);
                        listener = Some(UnixListener::bind(&path).unwrap());
                    },
                    |_, _| {},
                )
            },
            |_| panic!("must not reset a starting unit"),
            || panic!("must not initialize or start a second runner"),
        )
        .unwrap();
        assert_eq!(clock.elapsed(), Duration::from_secs(1));
    }

    #[test]
    fn attach_resets_failed_unit_before_restarting() {
        let calls = std::cell::RefCell::new(Vec::new());
        attach_or_start(
            Unit::Inactive,
            |_| panic!("failed unit cannot be adopted"),
            |operation| {
                calls.borrow_mut().push(operation.to_owned());
                Ok(())
            },
            || {
                calls.borrow_mut().push("start".to_owned());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["stop", "reset-failed", "start"]);
    }

    #[test]
    fn adoption_refuses_symlink_even_when_target_has_current_main_pid_listener() {
        let root = ScratchDir::new("motor-tenant-adopt-symlink");
        let target = root.join("target.sock");
        let path = root.join("runner.sock");
        let _listener = UnixListener::bind(&target).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let pid = std::process::id();
        let error = wait(&path, pid, || Ok(Some(pid))).unwrap_err();
        assert!(error.to_string().contains("expected an owned socket node"));
        assert!(target.exists());
    }

    #[test]
    fn readiness_fails_promptly_when_unit_exits_or_main_pid_changes() {
        let root = ScratchDir::new("motor-tenant-runner-exited");
        let path = root.join("runner.sock");
        let pid = std::process::id();
        for current in [None, Some(pid + 1)] {
            let error = wait_with(
                &path,
                pid,
                || Ok(current),
                || Duration::ZERO,
                |_| panic!("must not wait after unit exits"),
                |_, _| {},
            )
            .unwrap_err();
            assert!(error.to_string().starts_with("tenant_runner_exited:"));
        }
    }

    #[test]
    fn readiness_cap_has_named_error_even_while_process_is_alive() {
        let root = ScratchDir::new("motor-tenant-readiness-cap");
        let path = root.join("runner.sock");
        let pid = std::process::id();
        let clock = Clock::new();
        let error = wait_with(
            &path,
            pid,
            || Ok(Some(pid)),
            || clock.elapsed(),
            |duration| clock.advance(duration),
            |_, _| {},
        )
        .unwrap_err();
        assert_eq!(clock.elapsed(), Duration::from_secs(900));
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error
            .to_string()
            .starts_with("tenant_runner_readiness_timeout:"));
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }
}
