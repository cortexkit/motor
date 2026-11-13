use crate::Result;
use std::{
    io::Read,
    os::fd::AsRawFd,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(2);
// A killed query normally exits within milliseconds and is reaped here.
const KILL_GRACE: Duration = Duration::from_millis(200);
// Each background reaper owns a query that ignored SIGKILL, such as one stuck
// in the kernel. Past this many, new queries are not started at all.
pub(super) const MAX_REAPERS: usize = 4;
static REAPERS: AtomicUsize = AtomicUsize::new(0);

pub(super) fn output(command: &mut Command) -> Result<Option<Output>> {
    output_with(command, &REAPERS, crate::linux::process::wait_child, |mut child| {
        let _ = child.wait();
    })
}

/// Runs `command` for at most DEADLINE plus KILL_GRACE. A query that `settle`
/// has not reaped within KILL_GRACE of its kill goes to `reap` on a background
/// thread, and the result is unmeasured: the caller is holding a job's exit
/// evidence open meanwhile.
pub(super) fn output_with(
    command: &mut Command,
    reapers: &'static AtomicUsize,
    settle: impl FnOnce(&mut Child, Duration) -> std::io::Result<Option<std::process::ExitStatus>>,
    reap: impl FnOnce(Child) + Send + 'static,
) -> Result<Option<Output>> {
    if reapers.load(Ordering::Acquire) >= MAX_REAPERS {
        eprintln!("egress counter query skipped: {MAX_REAPERS} earlier queries are still exiting");
        return Ok(None);
    }
    let deadline = Instant::now() + DEADLINE;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = read_output(&mut child, deadline);
    if !matches!(result, Ok(Some(_))) {
        // Neither wait_with_output nor a reader thread bounds pipe reads. Stop
        // reading at the deadline and kill the query. Reap it here if it dies
        // promptly; never wait without a bound on the caller's thread.
        let _ = child.kill();
        if settle(&mut child, KILL_GRACE)?.is_none() {
            reapers.fetch_add(1, Ordering::AcqRel);
            let spawned = std::thread::Builder::new()
                .name("counter-reaper".into())
                .spawn(move || {
                    reap(child);
                    reapers.fetch_sub(1, Ordering::AcqRel);
                });
            if let Err(error) = spawned {
                // The closure, and with it the unreaped child, is dropped.
                reapers.fetch_sub(1, Ordering::AcqRel);
                eprintln!("egress counter reaper spawn failed: {error}");
            }
        }
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
