use crate::Result;
use std::{
    io::Read,
    os::fd::AsRawFd,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(2);

pub(super) fn output(command: &mut Command) -> Result<Option<Output>> {
    let deadline = Instant::now() + DEADLINE;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = read_output(&mut child, deadline);
    if !matches!(result, Ok(Some(_))) {
        // Neither wait_with_output nor a reader thread bounds pipe reads. Stop
        // reading at the deadline, kill the query, and reap it before returning.
        let _ = child.kill();
        child.wait()?;
    }
    result
}

fn read_output(child: &mut Child, deadline: Instant) -> Result<Option<Output>> {
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_done = false;
    let mut err_done = false;
    let mut status = None;
    loop {
        for (reader, bytes, done) in [
            (&mut stdout as &mut dyn Read, &mut out, &mut out_done),
            (&mut stderr as &mut dyn Read, &mut err, &mut err_done),
        ] {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            if *done {
                continue;
            }
            let mut buffer = [0; 8192];
            match reader.read(&mut buffer) {
                Ok(0) => *done = true,
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if out_done && err_done {
            if let Some(status) = status {
                return Ok(Some(Output {
                    status,
                    stdout: out,
                    stderr: err,
                }));
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: if out_done { -1 } else { stdout.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if err_done { -1 } else { stderr.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Poll in short intervals to notice an exit even after both pipes close.
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(10) as i32;
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, wait) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }
}
