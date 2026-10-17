use std::{
    io,
    os::fd::AsRawFd,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

const UNIT: &str = "motor-tenant-runner.service";

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
    peer_is_current(&socket, expected, current_pid)
}

pub(super) fn wait(
    path: &Path,
    timeout: Duration,
    mut current_pid: impl FnMut() -> io::Result<Option<u32>>,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if ready(path, &mut current_pid)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "runner socket {} is not listening for the current {UNIT} MainPID",
                    path.display()
                ),
            ));
        }
        std::thread::park_timeout(
            Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::{fs, os::unix::net::UnixListener};

    #[test]
    fn leftover_socket_is_not_ready_until_current_main_pid_listens() {
        let root = ScratchDir::new("motor-tenant-socket-ready");
        let path = root.join("runner.sock");
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        let pid = std::process::id();
        assert!(!ready(&path, &mut || Ok(Some(pid))).unwrap());
        fs::remove_file(&path).unwrap();
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(ready(&path, &mut || Ok(Some(pid))).unwrap());
        assert!(!ready(&path, &mut || Ok(Some(pid + 1))).unwrap());
        let mut calls = 0;
        assert!(!ready(&path, &mut || {
            calls += 1;
            Ok(Some(if calls == 1 { pid } else { pid + 1 }))
        })
        .unwrap());
        wait(&path, Duration::ZERO, || Ok(Some(pid))).unwrap();
        let error = wait(&path, Duration::ZERO, || Ok(None)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }
}
