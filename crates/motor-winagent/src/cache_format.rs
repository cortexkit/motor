//! The guest's bounded, non-console invocation of the Windows formatter.

use crate::cache_steps::{CacheReporter, CacheStep};
use std::{
    io::{self, Read, Write},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const FORMAT_PROGRAM: &str = "C:\\Windows\\System32\\format.com";
const OUTPUT_LIMIT: usize = 4096;
const DESCRIPTION_LIMIT: usize = 384;
const CONFIRMATION: &[u8] = b"Y\r\n";

fn format_command(mount: &str) -> io::Result<Command> {
    let mut command = Command::new(FORMAT_PROGRAM);
    command.args(crate::cache::format_arguments(mount)?);
    Ok(command)
}

/// Format through the mount folder attached to the newly selected cache volume.
pub fn run(mount: &str, deadline: Instant) -> io::Result<()> {
    run_command(&mut format_command(mount)?, deadline)
}

/// Mount, format and unmount operations that portable tests can replace.
pub trait Formatter {
    fn mount(&mut self, volume: &str, path: &str) -> io::Result<()>;
    fn format(&mut self, path: &str, deadline: Instant) -> io::Result<()>;
    fn unmount(&mut self, path: &str) -> io::Result<()>;
}

struct FormatMount<'a, F: Formatter> {
    formatter: &'a mut F,
    reporter: &'a CacheReporter,
    active: bool,
}
impl<F: Formatter> FormatMount<'_, F> {
    fn unmount(&mut self) -> io::Result<()> {
        self.reporter.cleanup(CacheStep::FormatUnmountPoint, || {
            self.formatter.unmount(crate::CACHE_MOUNT)?;
            self.active = false;
            Ok(())
        })
    }
}
impl<F: Formatter> Drop for FormatMount<'_, F> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.unmount();
        }
    }
}

/// `format.com` requires a mount point even when given a volume GUID path.
/// Detach the first-use mount on both success and failure, before the normal
/// cache validation and mount. The caller's deadline still includes partition
/// creation, discovery, mounting and formatting.
pub fn run_mounted(
    formatter: &mut impl Formatter,
    reporter: &CacheReporter,
    volume: &str,
    deadline: Instant,
) -> io::Result<()> {
    if !crate::cache::is_volume_guid_path(volume) {
        return Err(io::Error::other("invalid cache volume path for format"));
    }
    let mount = crate::CACHE_MOUNT;
    let mut mounted = FormatMount {
        formatter,
        reporter,
        active: false,
    };
    reporter.step(CacheStep::FormatMountPoint, || {
        mounted.formatter.mount(volume, mount)?;
        // Own the mount before reporting completion: the guard must detach it
        // even if the observer disconnects while the native call is running.
        mounted.active = true;
        Ok(())
    })?;
    let result = mounted.formatter.format(mount, deadline);
    // Do not return the format error until the mount has been removed. In
    // particular, process spawn failures and timeouts need the same cleanup.
    let cleanup = mounted.unmount();
    match (result, cleanup) {
        (result, Ok(())) => result,
        (result, Err(cleanup)) => {
            let message = format!("remove cache format mount point failed: {cleanup}");
            match result {
                Ok(()) => Err(io::Error::new(cleanup.kind(), message)),
                Err(error) => Err(io::Error::new(error.kind(), format!("{error}; {message}"))),
            }
        }
    }
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
    complete: bool,
    error: Option<io::Error>,
}

impl Captured {
    fn description(&self) -> String {
        let description = format!(
            "{:?}{}{}",
            String::from_utf8_lossy(&self.bytes),
            if self.complete {
                ""
            } else {
                " [capture incomplete]"
            },
            self.error
                .as_ref()
                .map(|error| format!(" [read error: {error}]"))
                .unwrap_or_default(),
        );
        // Reserve space for both streams inside CacheFailure's 1024-byte wire
        // budget. Keep the tail: format often prints its error after a banner.
        if description.len() > DESCRIPTION_LIMIT {
            let mut start = description.len() - DESCRIPTION_LIMIT;
            while !description.is_char_boundary(start) {
                start += 1;
            }
            format!("\"...{} [truncated]", &description[start..])
        } else if self.truncated {
            format!("{description} [truncated]")
        } else {
            description
        }
    }
}

fn capture(mut pipe: impl Read + Send + 'static) -> Arc<Mutex<Captured>> {
    let output = Arc::new(Mutex::new(Captured::default()));
    let shared = Arc::clone(&output);
    thread::spawn(move || {
        let mut buffer = [0; 1024];
        loop {
            let result = pipe.read(&mut buffer);
            let mut output = shared.lock().unwrap();
            match result {
                Ok(0) => break,
                Ok(count) => {
                    let discard = (output.bytes.len() + count).saturating_sub(OUTPUT_LIMIT);
                    output.bytes.drain(..discard);
                    output.bytes.extend_from_slice(&buffer[..count]);
                    output.truncated |= discard != 0;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    output.error = Some(error);
                    break;
                }
            }
            // Keep draining after the buffer fills, or a full pipe could block format.
        }
        shared.lock().unwrap().complete = true;
    });
    output
}

fn failure(
    kind: io::ErrorKind,
    reason: impl std::fmt::Display,
    stdout: &Captured,
    stderr: &Captured,
) -> io::Error {
    io::Error::new(
        kind,
        format!(
            "cache format {reason}; stdout={}; stderr={}",
            stdout.description(),
            stderr.description()
        ),
    )
}

fn run_command(command: &mut Command, deadline: Instant) -> io::Result<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            io::Error::new(error.kind(), format!("cache format spawn failed: {error}"))
        })?;
    let stdout = capture(child.stdout.take().unwrap());
    let stderr = capture(child.stderr.take().unwrap());
    // SYSTEM in session 0 has no interactive console. Supply the documented
    // hard-disk confirmation, then EOF; /V supplies the label separately.
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(CONFIRMATION);
    }
    let mut status = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(failure(
                        error.kind(),
                        format!("wait failed: {error}"),
                        &stdout.lock().unwrap(),
                        &stderr.lock().unwrap(),
                    ));
                }
            }
        }
        {
            let stdout = stdout.lock().unwrap();
            let stderr = stderr.lock().unwrap();
            if let Some(status) = status
                && stdout.complete
                && stderr.complete
            {
                return if status.success() {
                    Ok(())
                } else {
                    Err(failure(
                        io::ErrorKind::Other,
                        format!("failed: {status}"),
                        &stdout,
                        &stderr,
                    ))
                };
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // Never join a pipe reader past the deadline, even if an inherited
            // pipe handle remains open. Its bounded tail is still diagnostic.
            return Err(failure(
                io::ErrorKind::TimedOut,
                "timed out",
                &stdout.lock().unwrap(),
                &stderr.lock().unwrap(),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOLUME: &str = "\\\\?\\Volume{0123abcd-0000-4000-8000-00000000cafe}\\";

    #[derive(Default)]
    struct FakeFormatter {
        calls: Vec<&'static str>,
        volume: String,
        mount_path: String,
        format_path: String,
        unmount_path: String,
        deadline: Option<Instant>,
        mount_error: bool,
        format_error: Option<io::ErrorKind>,
        unmount_error: bool,
        disconnect_on_mount: Option<std::sync::mpsc::Receiver<crate::cache_steps::StepEvent>>,
    }

    impl Formatter for FakeFormatter {
        fn mount(&mut self, volume: &str, path: &str) -> io::Result<()> {
            self.calls.push("mount");
            self.volume = volume.to_owned();
            self.mount_path = path.to_owned();
            drop(self.disconnect_on_mount.take());
            if self.mount_error {
                return Err(io::Error::other("mount failed"));
            }
            Ok(())
        }
        fn format(&mut self, path: &str, deadline: Instant) -> io::Result<()> {
            self.calls.push("format");
            self.format_path = path.to_owned();
            self.deadline = Some(deadline);
            if let Some(kind) = self.format_error {
                return Err(io::Error::new(kind, "format failed"));
            }
            Ok(())
        }
        fn unmount(&mut self, path: &str) -> io::Result<()> {
            self.calls.push("unmount");
            self.unmount_path = path.to_owned();
            if self.unmount_error {
                return Err(io::Error::other("unmount failed"));
            }
            Ok(())
        }
    }

    fn exercise_mounted(
        formatter: &mut impl Formatter,
        volume: &str,
        deadline: Instant,
    ) -> io::Result<()> {
        let (tx, _rx) = std::sync::mpsc::channel();
        run_mounted(formatter, &CacheReporter::new(tx), volume, deadline)
    }

    #[test]
    fn mount_is_set_before_format_and_removed_after_success() {
        let mut formatter = FakeFormatter::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        exercise_mounted(&mut formatter, VOLUME, deadline).unwrap();
        assert_eq!(formatter.calls, ["mount", "format", "unmount"]);
        assert_eq!(formatter.volume, VOLUME);
        assert_eq!(formatter.mount_path, "C:\\t\\");
        assert_eq!(formatter.unmount_path, "C:\\t\\");
        assert_eq!(formatter.deadline, Some(deadline));
    }

    #[test]
    fn format_receives_the_mount_path_not_the_volume_guid() {
        let mut formatter = FakeFormatter::default();
        exercise_mounted(&mut formatter, VOLUME, Instant::now()).unwrap();
        assert_eq!(formatter.format_path, "C:\\t\\");
    }

    #[test]
    fn mount_is_removed_on_every_format_error() {
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::Other,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Interrupted,
        ] {
            let mut formatter = FakeFormatter {
                format_error: Some(kind),
                ..Default::default()
            };
            let error = exercise_mounted(&mut formatter, VOLUME, Instant::now()).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), "format failed");
            assert_eq!(formatter.calls, ["mount", "format", "unmount"]);
            assert_eq!(formatter.unmount_path, "C:\\t\\");
        }
    }

    #[test]
    fn mount_failure_never_formats_or_removes_an_unowned_mount() {
        let mut formatter = FakeFormatter {
            mount_error: true,
            ..Default::default()
        };
        assert!(exercise_mounted(&mut formatter, VOLUME, Instant::now()).is_err());
        assert_eq!(formatter.calls, ["mount"]);
    }

    #[test]
    fn unmount_failure_is_reported_alongside_any_format_failure() {
        for format_error in [None, Some(io::ErrorKind::TimedOut)] {
            let mut formatter = FakeFormatter {
                format_error,
                unmount_error: true,
                ..Default::default()
            };
            let error = exercise_mounted(&mut formatter, VOLUME, Instant::now()).unwrap_err();
            assert!(error.to_string().contains(
                "remove cache format mount point failed: FormatUnmountPoint: unmount failed"
            ));
            assert_eq!(formatter.calls, ["mount", "format", "unmount", "unmount"]);
            if let Some(kind) = format_error {
                assert_eq!(error.kind(), kind);
                assert!(error.to_string().contains("format failed"));
            }
        }
    }

    #[test]
    fn invalid_volume_identity_is_rejected_before_mounting() {
        for bad in [
            "C:",
            "C:\\t\\",
            "\\\\.\\PhysicalDrive1",
            "\\\\?\\Volume{not-a-guid}\\",
        ] {
            let mut formatter = FakeFormatter::default();
            assert!(exercise_mounted(&mut formatter, bad, Instant::now()).is_err());
            assert!(formatter.calls.is_empty(), "{bad}");
        }
    }

    #[test]
    fn observer_disconnect_before_mount_does_not_attach_or_format() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let mut formatter = FakeFormatter::default();
        let error = run_mounted(
            &mut formatter,
            &CacheReporter::new(tx),
            VOLUME,
            Instant::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cache observer stopped"));
        assert!(formatter.calls.is_empty());
    }

    #[test]
    fn observer_disconnect_after_mount_still_removes_the_mount() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut formatter = FakeFormatter {
            disconnect_on_mount: Some(rx),
            ..Default::default()
        };
        let error = run_mounted(
            &mut formatter,
            &CacheReporter::new(tx),
            VOLUME,
            Instant::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cache observer stopped"));
        assert_eq!(formatter.calls, ["mount", "unmount"]);
        assert_eq!(formatter.unmount_path, "C:\\t\\");
    }

    #[test]
    fn exact_guest_format_program_and_arguments() {
        let command = format_command("C:\\t\\").unwrap();
        assert_eq!(command.get_program(), "C:\\Windows\\System32\\format.com");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["C:\\t\\", "/FS:NTFS", "/V:motor-target", "/Q", "/Y",]
        );
        assert!(format_command("C:").is_err());
    }

    #[cfg(unix)]
    fn shell(script: &str, timeout: Duration) -> io::Result<()> {
        run_command(
            Command::new("sh").args(["-c", script]),
            Instant::now() + timeout,
        )
    }

    #[cfg(unix)]
    #[test]
    fn formatter_receives_confirmation_and_eof_without_a_console() {
        shell(
            "IFS= read -r answer; [ \"$answer\" = \"$(printf 'Y\\r')\" ] && ! IFS= read -r extra",
            Duration::from_secs(5),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn format_failure_carries_exit_status_stdout_and_stderr() {
        let error = shell(
            "printf 'format prompt'; printf 'Invalid parameter' >&2; exit 1",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        let message = error.to_string();
        assert!(message.contains("exit status: 1"), "{message}");
        assert!(message.contains("stdout=\"format prompt\""), "{message}");
        assert!(
            message.contains("stderr=\"Invalid parameter\""),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn format_output_is_bounded_but_pipes_are_drained() {
        let error = shell(
            "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; exit 1",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        let message = error.to_string();
        assert_eq!(message.matches("[truncated]").count(), 2, "{message}");
        assert!(message.len() <= 1024, "{} bytes", message.len());
    }

    #[cfg(unix)]
    #[test]
    fn format_output_tails_survive_the_cache_failure_wire_budget() {
        use crate::cache_steps::{CacheLimits, CacheReporter, CacheStep, watch_cache};
        use std::sync::mpsc;

        let (events_tx, events_rx) = mpsc::channel();
        let reporter = CacheReporter::new(events_tx);
        let result = reporter.step(CacheStep::InitializeRaw, || {
            shell(
                "head -c 131072 /dev/zero; printf STDOUT_END; head -c 131072 /dev/zero >&2; printf STDERR_END >&2; exit 1",
                Duration::from_secs(5),
            )
        });
        let (result_tx, result_rx) = mpsc::channel();
        result_tx.send(result).unwrap();
        let failure = watch_cache(
            events_rx,
            result_rx,
            |_, _| Ok(()),
            || panic!("completed format must not be cancelled"),
            CacheLimits::default(),
        )
        .unwrap_err();
        assert_eq!(failure.step, CacheStep::InitializeRaw);
        failure.validate().unwrap();
        assert!(failure.message.contains("exit status: 1"), "{failure}");
        assert!(failure.message.contains("STDOUT_END"), "{failure}");
        assert!(failure.message.contains("STDERR_END"), "{failure}");
        let decoded: crate::cache_steps::CacheFailure =
            serde_json::from_str(&serde_json::to_string(&failure).unwrap()).unwrap();
        assert_eq!(decoded, failure);
    }

    #[cfg(unix)]
    #[test]
    fn format_deadline_kills_process_and_preserves_output() {
        let started = Instant::now();
        let error = shell(
            "printf 'waiting'; printf 'busy' >&2; exec sleep 10",
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        let message = error.to_string();
        assert!(message.contains("stdout=\"waiting\""), "{message}");
        assert!(message.contains("stderr=\"busy\""), "{message}");
    }
}
