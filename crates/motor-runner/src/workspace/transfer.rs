use std::os::unix::process::CommandExt;
use std::{
    io::{self, Read},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub(crate) const STALL: Duration = Duration::from_secs(60);

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
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(path)
        .args(args)
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
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
    let mut diagnostic = Vec::new();
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "git fetch {status}: {}",
                        String::from_utf8_lossy(&diagnostic)
                    )))
                }
            }
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
                diagnostic.extend_from_slice(
                    &bytes[..bytes.len().min(8192usize.saturating_sub(diagnostic.len()))],
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::park_timeout(Duration::from_millis(25))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    if result.is_err() {
        // index-pack is a child of git; killing only git can leave it writing
        // the mirror and keep the progress-reader pipe open indefinitely.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
    }
    let _ = child.wait();
    drop(rx);
    let _ = reader.join();
    result
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
}
