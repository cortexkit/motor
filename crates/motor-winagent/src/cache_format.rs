//! The guest's bounded, non-console invocation of the Windows formatter.

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

fn format_command(volume: &str) -> io::Result<Command> {
    let mut command = Command::new(FORMAT_PROGRAM);
    command.args(crate::cache::format_arguments(volume)?);
    Ok(command)
}

/// Format only the volume selected by the guest's disk identity/layout checks.
/// The caller's deadline includes partition creation and volume discovery.
pub fn run(volume: &str, deadline: Instant) -> io::Result<()> {
    run_command(&mut format_command(volume)?, deadline)
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

    #[test]
    fn exact_guest_format_program_and_arguments() {
        let command =
            format_command("\\\\?\\Volume{0123abcd-0000-4000-8000-00000000cafe}\\").unwrap();
        assert_eq!(command.get_program(), "C:\\Windows\\System32\\format.com");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "\\\\?\\Volume{0123abcd-0000-4000-8000-00000000cafe}\\",
                "/FS:NTFS",
                "/V:motor-target",
                "/Q",
                "/Y",
            ]
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
