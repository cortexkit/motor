//! Cargo compiler supervisor with machine-wide, launcher-owned advisory slots.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn signal(signal: i32, handler: usize) -> usize;
    fn kill(pid: i32, signal: i32) -> i32;
}

// Linux and macOS use the same cancellation-signal numbers, so one list serves
// both. Core's build.rs compiles this standalone launcher, which cannot use crates.
const CANCEL_SIGNALS: [i32; 7] = [1, 2, 3, 6, 13, 14, 15];
#[cfg(target_os = "macos")]
const USER_SIGNALS: [i32; 2] = [30, 31];
#[cfg(not(target_os = "macos"))]
const USER_SIGNALS: [i32; 2] = [10, 12];
static PENDING_SIGNALS: AtomicU32 = AtomicU32::new(0);

extern "C" fn record_signal(signal: i32) {
    // Only an atomic operation in the handler: forwarding and reaping happen in
    // the supervisor, including signals delivered during the spawn itself.
    PENDING_SIGNALS.fetch_or(1 << signal, Ordering::Relaxed);
}

fn supervise(command: &mut Command) -> io::Result<ExitStatus> {
    for number in CANCEL_SIGNALS.into_iter().chain(USER_SIGNALS) {
        if unsafe { signal(number, record_signal as *const () as usize) } == usize::MAX {
            return Err(io::Error::last_os_error());
        }
    }
    // A private group lets cancellation reach linkers as well as rustc, without
    // signalling Cargo or unrelated compiles in the caller's process group.
    command.process_group(0);
    let mut child = CompileChild(command.spawn()?);
    let mut cancelled_at = None;
    loop {
        let pending = PENDING_SIGNALS.swap(0, Ordering::Relaxed);
        for number in 1..32 {
            if pending & (1 << number) != 0 {
                unsafe { kill(-(child.0.id() as i32), number) };
                cancelled_at.get_or_insert_with(Instant::now);
            }
        }
        if cancelled_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(5)) {
            // A compiler that ignores cancellation must not outlive its launcher.
            unsafe { kill(-(child.0.id() as i32), 9) };
        }
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct CompileChild(Child);

impl Drop for CompileChild {
    fn drop(&mut self) {
        // Even a wait error must not leave the direct child running. Child caches
        // a reaped status, so kill is a no-op after the normal successful wait.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// How long a compile waits for a slot before saying so, and how often it repeats it.
/// A long silent wait reads like a hang to whoever watches the build, so they kill it.
const NOTICE_AFTER: Duration = Duration::from_secs(60);
const NOTICE_EVERY: Duration = Duration::from_secs(5 * 60);

/// Millisecond overrides for the two notice intervals, so tests need not wait minutes.
fn notice_interval(variable: &str, default: Duration) -> Duration {
    std::env::var(variable)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

fn describe_wait(waited: Duration) -> String {
    let seconds = waited.as_secs();
    if seconds >= 60 {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

/// While alive, prints a line to stderr once the compile has waited `NOTICE_AFTER` for a
/// slot and again every `NOTICE_EVERY`. Cargo forwards a compiler's stderr, so the line
/// shows up in the build output. Dropping it (the slot was taken) stops the notices.
struct WaitNotice {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WaitNotice {
    fn start(root: &Path) -> Self {
        let after = notice_interval("PREFRONTAL_BUILD_SLOT_NOTICE_AFTER_MS", NOTICE_AFTER);
        let every = notice_interval("PREFRONTAL_BUILD_SLOT_NOTICE_EVERY_MS", NOTICE_EVERY);
        let root: PathBuf = root.to_path_buf();
        let started = Instant::now();
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mut timeout = after;
            let mut first = true;
            while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(timeout) {
                let slots = match std::fs::read_to_string(root.join("limit")) {
                    Ok(text) if text.trim() == "1" => "1 slot".to_string(),
                    Ok(text) => format!("{} slots", text.trim()),
                    Err(_) => "all slots".to_string(),
                };
                let what = if first {
                    "waiting for a compile slot".to_string()
                } else {
                    format!(
                        "still waiting for a compile slot after {}",
                        describe_wait(started.elapsed())
                    )
                };
                // A closed stderr must not take the compile down with it.
                let _ = writeln!(
                    io::stderr(),
                    "prefrontal-build-slot: {what} ({slots}, all busy; queued behind other \
                     worktrees' builds). This is not a hang."
                );
                first = false;
                timeout = every;
            }
        });
        WaitNotice {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for WaitNotice {
    fn drop(&mut self) {
        // Closing the channel wakes the notice thread at once; wait for it so no notice
        // is printed after the compiler has started.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn acquire(root: &Path) -> io::Result<File> {
    let _notice = WaitNotice::start(root);
    // Waiters queue on one blocking lock so only the head of the queue polls the slots.
    // Every waiter polling at 50 ms put 140+ waiters at about one core of pure wakeups
    // (2026-10-02). Rust opens files close-on-exec, so the queue lock is not inherited
    // by the compiler, and it is released when this function returns.
    let queue = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(root.join("queue.lock"))?;
    // 2 is LOCK_EX without LOCK_NB: block in the kernel until this waiter is at the head.
    while unsafe { flock(queue.as_raw_fd(), 2) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    loop {
        let limit: usize = std::fs::read_to_string(root.join("limit"))?
            .trim()
            .parse()
            .map_err(|_| io::Error::other("invalid build slot limit"))?;
        if limit == 0 {
            return Err(io::Error::other("build slot limit must be positive"));
        }
        for index in 0..limit {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(root.join(format!("{index}.lock")))?;
            // The numeric flock flags request an exclusive, nonblocking lock on Linux and macOS.
            if unsafe { flock(file.as_raw_fd(), 2 | 4) } == 0 {
                // Keep Rust's FD_CLOEXEC: only the supervisor owns this slot.
                // A compiler's background descendants must never pin the pool.
                return Ok(file);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn run() -> io::Result<ExitStatus> {
    let executable = std::env::current_exe()?;
    let root = std::fs::read_to_string(executable.with_extension("root"))?;
    let mut arguments = std::env::args_os().skip(1);
    let compiler = arguments
        .next()
        .ok_or_else(|| io::Error::other("missing compiler"))?;
    let arguments: Vec<_> = arguments.collect();
    let compiling = arguments
        .iter()
        .any(|arg| arg == "--emit" || arg.to_string_lossy().starts_with("--emit="));
    let querying = arguments
        .iter()
        .any(|arg| arg == "--print" || arg.to_string_lossy().starts_with("--print="));
    let _slot = if compiling && !querying {
        Some(acquire(Path::new(&root))?)
    } else {
        None
    };
    // Read the launcher's `.cache` setting after acquiring a slot, so a queued
    // compile uses the program configured when it starts.
    let cache = std::fs::read_to_string(executable.with_extension("cache"))?;
    // This small configuration file is also hand-edited; a trailing newline
    // must not turn the executable path into a nonexistent program name.
    let cache = cache.trim();
    let mut command = if cache.is_empty() {
        Command::new(&compiler)
    } else {
        let mut command = Command::new(cache);
        command.arg(&compiler);
        command
    };
    command.args(arguments);
    supervise(&mut command)
}

fn main() {
    use std::os::unix::process::ExitStatusExt;
    match run() {
        Ok(status) => {
            if let Some(number) = status.signal() {
                // Preserve signal termination, not merely its shell exit-code encoding.
                unsafe {
                    signal(number, 0);
                    kill(std::process::id() as i32, number);
                }
                std::process::exit(128 + number);
            }
            std::process::exit(status.code().unwrap_or(1));
        }
        Err(error) => {
            eprintln!("prefrontal-build-slot: {error}");
            std::process::exit(1);
        }
    }
}
