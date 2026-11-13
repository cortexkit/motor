use super::*;
use std::time::{Duration, Instant};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::mpsc,
};

pub(in crate::linux) struct StalledCounter {
    pub command: PathBuf,
    pub pid_file: PathBuf,
    cancel: mpsc::Sender<()>,
    watchdog: Option<std::thread::JoinHandle<()>>,
}

impl StalledCounter {
    pub fn new(root: &Path) -> Self {
        let command = root.join("stalled-counter");
        let pid_file = root.join("stalled-counter.pid");
        std::fs::write(&command, "#!/bin/sh\necho $$ > \"$0.pid\"\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (cancel, cancelled) = mpsc::channel();
        let path = pid_file.clone();
        // Keep a broken unbounded implementation from hanging the test runner.
        // This fallback is later than the assertion's 2.5 s limit.
        let watchdog = std::thread::spawn(move || {
            if cancelled.recv_timeout(Duration::from_secs(4))
                == Err(mpsc::RecvTimeoutError::Timeout)
            {
                let pid: i32 = std::fs::read_to_string(path)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        });
        Self {
            command,
            pid_file,
            cancel,
            watchdog: Some(watchdog),
        }
    }
}

impl Drop for StalledCounter {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        self.watchdog.take().unwrap().join().unwrap();
    }
}

#[test]
fn stalled_counter_read_is_unmeasured_within_deadline_and_reaps_child() {
    let root = cortexkit_test_support::ScratchDir::new("counter-deadline");
    let stalled = StalledCounter::new(&root);
    let started = Instant::now();
    let bytes = optional_egress_bytes_with_command(
        &Tap {
            tap_index: 17,
            uid: 500000,
        },
        &mut Command::new(&stalled.command),
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(2500));
    assert_eq!(bytes, None);
    let pid: i32 = std::fs::read_to_string(&stalled.pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "counter process survived"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn bounded_counter_read_preserves_zero_missing_query_and_parse_results() {
    let tap = Tap {
        tap_index: 17,
        uid: 500000,
    };
    for (text, expected) in [
        (r#"{"nftables":[]}"#, None),
        (
            r#"{"nftables":[{"counter":{"family":"inet","table":"motor_tenant","name":"tenant_mtap17_egress","bytes":0}}]}"#,
            Some(0),
        ),
    ] {
        assert_eq!(
            optional_egress_bytes_with_command(
                &tap,
                Command::new("/bin/printf").args(["%s", text])
            )
            .unwrap(),
            expected
        );
    }
    for script in [
        "echo query-failed >&2; exit 1",
        "echo invalid-json",
        "echo '{}'",
    ] {
        assert!(optional_egress_bytes_with_command(
            &tap,
            Command::new("/bin/sh").args(["-c", script])
        )
        .is_err());
    }
}

#[test]
fn query_surviving_its_kill_is_reaped_in_the_background_and_reapers_are_capped() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static REAPERS: AtomicUsize = AtomicUsize::new(0);
    let root = cortexkit_test_support::ScratchDir::new("counter-reaper");
    let stalled = StalledCounter::new(&root);
    let (handed, handoff) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let started = Instant::now();
    // The seam models a query stuck in the kernel: its kill has not taken
    // effect within the grace period, and its reaper cannot finish yet.
    let output = counter::output_with(
        &mut Command::new(&stalled.command),
        &REAPERS,
        |_, _| Ok(None),
        move |mut child| {
            handed.send(child.id()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(10));
            let _ = child.wait();
        },
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(2500));
    assert!(output.is_none());
    handoff
        .recv_timeout(Duration::from_secs(2))
        .expect("the unreaped query was not handed to a reaper");
    assert_eq!(REAPERS.load(Ordering::Acquire), 1);
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while REAPERS.load(Ordering::Acquire) != 0 {
        assert!(Instant::now() < deadline, "reaper never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    // With every reaper slot taken, a query is not started at all.
    REAPERS.store(counter::MAX_REAPERS, Ordering::Release);
    let marker = root.join("started");
    let skipped = counter::output_with(
        Command::new("/bin/sh").args(["-c", &format!("touch {}", marker.display())]),
        &REAPERS,
        |_, _| Ok(None),
        |_| {},
    )
    .unwrap();
    REAPERS.store(0, Ordering::Release);
    assert!(skipped.is_none());
    assert!(!marker.exists(), "query started past the reaper cap");
}
