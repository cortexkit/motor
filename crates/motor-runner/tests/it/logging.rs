//! motor-runner's own diagnostics reach its stderr, which the systemd user
//! unit sends to the journal. Without an installed subscriber every
//! `tracing` event the runner emits is silently discarded. The test spawns the
//! real binary, so the subscriber is installed in that child process, never in
//! this shared test process.
use super::process::ChildGuard;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

#[cfg_attr(
    target_os = "linux",
    ignore = "requires the trusted build-server runner account; see motor-runner README"
)]
#[test]
fn serve_writes_its_tracing_events_to_stderr() {
    let scratch = cortexkit_test_support::ScratchDir::new("motor-runner-logging");
    // The socket lives under the state root, and macOS limits Unix socket
    // paths to about a hundred bytes, so keep the root short: a sibling of
    // the shared scratch root, removed again below.
    let state = cortexkit_test_support::scratch_root()
        .parent()
        .unwrap()
        .join(format!("mrl{}", std::process::id()));
    let mut child = ChildGuard::spawn(
        Command::new(env!("CARGO_BIN_EXE_motor-runner"))
            .args(["serve", "--instance", "logging", "--state-root"])
            .arg(&state)
            .arg("--build-slot-root")
            .arg(scratch.join("slots"))
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )
    .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::new();
    let found = loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line)
                if line.starts_with("[motor-runner] INFO motor_runner: motor-runner serving") =>
            {
                break Some(line)
            }
            Ok(line) => seen.push(line),
            Err(_) => break None,
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&state);
    let line = found.unwrap_or_else(|| panic!("no serving line on stderr; saw {seen:?}"));
    assert!(line.contains("instance=logging"), "{line}");
    assert!(
        line.contains(&format!("version={}", env!("CARGO_PKG_VERSION"))),
        "{line}"
    );
}
