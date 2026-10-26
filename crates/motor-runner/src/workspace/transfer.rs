#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    collections::VecDeque,
    io::{self, Read},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub(crate) const STALL: Duration = Duration::from_secs(60);
const DIAGNOSTIC_PART: usize = 8192;
const ELISION: &str = "\n...[git stderr omitted]...\n";

#[derive(Default)]
struct Diagnostic {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    seen: usize,
}

impl Diagnostic {
    fn observe(&mut self, bytes: &[u8]) {
        self.seen = self.seen.saturating_add(bytes.len());
        self.head
            .extend_from_slice(&bytes[..bytes.len().min(DIAGNOSTIC_PART - self.head.len())]);
        if bytes.len() >= DIAGNOSTIC_PART {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - DIAGNOSTIC_PART..]);
        } else {
            let discard = (self.tail.len() + bytes.len()).saturating_sub(DIAGNOSTIC_PART);
            self.tail.drain(..discard);
            self.tail.extend(bytes);
        }
    }

    fn text(&self) -> String {
        let tail: Vec<_> = self.tail.iter().copied().collect();
        let mut bytes = self.head.clone();
        if self.seen > DIAGNOSTIC_PART * 2 {
            bytes.extend_from_slice(ELISION.as_bytes());
        }
        let overlap = (self.head.len() + tail.len()).saturating_sub(self.seen);
        bytes.extend_from_slice(&tail[overlap..]);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Bound the redacted wire detail without losing Git's final fatal diagnostic.
pub(crate) fn bounded_fetch_detail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let marker = if limit >= ELISION.len() { ELISION } else { "" };
    let head_budget = (limit - marker.len()) / 3;
    let tail_budget = limit - marker.len() - head_budget;
    let mut head = head_budget;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - tail_budget;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{}{}", &text[..head], marker, &text[tail..])
}

/// Git's forced progress reports byte/object counts while indexing a local
/// bundle. Identical repeated lines and unrelated diagnostics are not progress.
struct Progress {
    last: Instant,
    line: Vec<u8>,
    previous: Vec<u8>,
}
impl Progress {
    fn new(now: Instant) -> Self {
        Self {
            last: now,
            line: Vec::new(),
            previous: Vec::new(),
        }
    }
    fn observe(&mut self, bytes: &[u8], now: Instant) {
        for byte in bytes {
            if matches!(byte, b'\r' | b'\n') {
                let text = String::from_utf8_lossy(&self.line);
                if [
                    "Receiving objects:",
                    "Resolving deltas:",
                    "Counting objects:",
                    "Compressing objects:",
                    "Unpacking objects:",
                ]
                .iter()
                .any(|phase| text.contains(phase))
                    && self.line != self.previous
                {
                    self.last = now;
                    self.previous.clone_from(&self.line);
                }
                self.line.clear();
            } else if self.line.len() < 8192 {
                self.line.push(*byte);
            }
        }
    }
    fn remaining(&self, now: Instant) -> Duration {
        STALL.saturating_sub(now.saturating_duration_since(self.last))
    }
}

pub(crate) fn fetch(path: &Path, args: &[&str]) -> io::Result<()> {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(path)
        .args(args)
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0");
    run(command)
}

fn run(mut command: Command) -> io::Result<()> {
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(unix)]
    let mut child = command.spawn()?;
    #[cfg(windows)]
    let mut child = crate::native_process::Child::spawn(&mut command)?;
    let mut stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::sync_channel(8);
    let reader = std::thread::spawn(move || {
        let mut bytes = [0; 4096];
        while let Ok(n) = stderr.read(&mut bytes) {
            if n == 0 || tx.send(bytes[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut progress = Progress::new(Instant::now());
    let mut diagnostic = Diagnostic::default();
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error),
            Ok(None) => {}
        }
        let remaining = progress.remaining(Instant::now());
        if remaining.is_zero() {
            break Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "transfer_stalled: git fetch made no progress for 60 seconds",
            ));
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(25))) {
            Ok(bytes) => {
                progress.observe(&bytes, Instant::now());
                diagnostic.observe(&bytes);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::park_timeout(Duration::from_millis(25))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    if !matches!(&result, Ok(status) if status.success()) {
        // index-pack is a child of git; killing only git can leave it writing
        // the mirror and keep the progress-reader pipe open indefinitely.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
    }
    let _ = child.wait();
    // Exit status can arrive before the reader has delivered the last stderr
    // chunks. Drain through EOF before formatting the error, not just until exit.
    for bytes in rx {
        diagnostic.observe(&bytes);
    }
    let _ = reader.join();
    result.and_then(|status| {
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "git fetch {status}: {}",
                diagnostic.text()
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn git_fetch_progress_has_no_total_duration_limit() {
        let start = Instant::now();
        let mut progress = Progress::new(start);
        for second in (30..=600).step_by(30) {
            let now = start + Duration::from_secs(second);
            assert!(!progress.remaining(now).is_zero());
            progress.observe(
                format!("Receiving objects: {} KiB\r", second).as_bytes(),
                now,
            );
        }
        assert!(!progress
            .remaining(start + Duration::from_secs(650))
            .is_zero());
        assert!(progress
            .remaining(start + Duration::from_secs(660))
            .is_zero());
    }
    #[test]
    fn repeated_git_progress_and_diagnostics_do_not_hide_a_stall() {
        let start = Instant::now();
        let mut progress = Progress::new(start);
        progress.observe(b"Receiving objects: 1 KiB\r", start);
        for second in 1..=60 {
            progress.observe(
                b"Receiving objects: 1 KiB\rdiagnostic\n",
                start + Duration::from_secs(second),
            );
        }
        assert!(progress.remaining(start + STALL).is_zero());
    }

    #[test]
    fn diagnostic_capture_is_bounded_and_wire_truncation_keeps_both_ends() {
        let across_boundary = format!("{}é", "x".repeat(DIAGNOSTIC_PART - 1));
        let mut complete = Diagnostic::default();
        complete.observe(across_boundary.as_bytes());
        assert_eq!(complete.text(), across_boundary);
        let mut diagnostic = Diagnostic::default();
        diagnostic.observe(b"head marker\n");
        for _ in 0..100 {
            diagnostic.observe(&[b'x'; 4096]);
        }
        diagnostic.observe("\nfatal: final error é\n".as_bytes());
        assert_eq!(diagnostic.head.len(), DIAGNOSTIC_PART);
        assert_eq!(diagnostic.tail.len(), DIAGNOSTIC_PART);
        let text = diagnostic.text();
        assert!(text.starts_with("head marker"));
        assert!(text.ends_with("fatal: final error é\n"));
        for limit in [0, 1, 20, 1024] {
            let bounded = bounded_fetch_detail(&text, limit);
            assert!(bounded.len() <= limit);
            if limit == 1024 {
                assert!(bounded.starts_with("head marker"));
                assert!(bounded.ends_with("fatal: final error é\n"));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn fetch_stderr_child() {
        use std::io::Write;
        let Some(mode) = std::env::var_os("MOTOR_TEST_FETCH_STDERR_CHILD") else {
            return;
        };
        let mut stderr = std::io::stderr().lock();
        if mode == "long" {
            stderr.write_all(b"git diagnostic head\n").unwrap();
            stderr.write_all(&vec![b'p'; 128 * 1024]).unwrap();
        }
        stderr
            .write_all(b"\nfatal: final-error-sentinel sensitive-value\n")
            .unwrap();
        stderr.flush().unwrap();
        std::process::exit(128);
    }

    #[cfg(unix)]
    #[test]
    fn failed_fetch_drains_stderr_and_keeps_final_error_in_refusal_detail() {
        let scratch = cortexkit_test_support::ScratchDir::new("fetch-diagnostic");
        let mut config = crate::Config::new(scratch.join("state"), "fetch-diagnostic");
        config.free_space_floor_bytes = 0;
        config.manual_tick = true;
        config.owned_workspace_roots = vec![scratch.path().to_path_buf()];
        let runner = crate::Runner::open(config).unwrap();
        for mode in ["short", "long"] {
            // Use the same process/pipe handling as Git without replacing PATH
            // or relying on a particular Git release's progress formatting.
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "workspace::transfer::tests::fetch_stderr_child",
                    "--nocapture",
                ])
                .env("MOTOR_TEST_FETCH_STDERR_CHILD", mode);
            let error = run(command).unwrap_err();
            let raw = error.to_string();
            assert!(raw.contains("fatal: final-error-sentinel"), "{raw}");
            assert!(raw.len() <= DIAGNOSTIC_PART * 2 + ELISION.len() + 128);
            let detail = runner.safe_refusal_detail(error, ["sensitive-value".to_owned()]);
            assert!(detail.len() <= motor_protocol::REFUSAL_DETAIL_MAX_BYTES);
            assert!(detail.contains("fatal: final-error-sentinel"), "{detail}");
            assert!(!detail.contains("sensitive-value"), "{detail}");
            if mode == "long" {
                assert!(detail.contains("git diagnostic head"), "{detail}");
            }
        }
    }
}
