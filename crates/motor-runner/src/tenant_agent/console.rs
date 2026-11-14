//! Best-effort runner-only exit evidence, independent of the runner process.
use motor_protocol::runner_diagnostics::{
    console_message, CONSOLE_LINE_CHARS, CONSOLE_MAX_BYTES, CONSOLE_MAX_LINES, CONSOLE_TAG,
};
use serde_json::Value;
use std::{collections::VecDeque, io, process::Stdio};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const UNIT: &str = "motor-tenant-runner.service";
const MAX_RECORD_BYTES: usize = 64 * 1024;
const WARNING_LINES: usize = 20;
const DECISION_LINES: usize = 6;

pub(super) async fn watch() -> io::Result<()> {
    let mut command = tokio::process::Command::new("journalctl");
    command.args([
        "--unit=motor-tenant-runner.service",
        "--merge",
        "--follow",
        "--lines=64",
        "--output=json",
        "--no-pager",
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing journal stdout"))?;
    // Write the tag at the beginning of the serial line, without journald's
    // display prefix. Async file I/O cannot stall tenant attachment.
    let console = tokio::fs::OpenOptions::new()
        .write(true)
        .open("/dev/console")
        .await?;
    forward(output, console).await?;
    let _ = child.wait().await?;
    Err(io::Error::other("runner journal follow ended"))
}

#[derive(Default)]
struct Relay {
    warnings: VecDeque<String>,
    panic_body: bool,
    panic_lines: usize,
    lines: usize,
    bytes: usize,
    decisions: usize,
}

impl Relay {
    fn emit(&mut self, message: String, output: &mut Vec<String>) {
        let message: String = message
            .chars()
            .take(CONSOLE_LINE_CHARS)
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        let line = format!("{CONSOLE_TAG}{message}\n");
        if self.lines < CONSOLE_MAX_LINES && self.bytes + line.len() <= CONSOLE_MAX_BYTES {
            self.lines += 1;
            self.bytes += line.len();
            output.push(line);
        }
    }

    fn flush(&mut self, output: &mut Vec<String>) {
        while let Some(message) = self.warnings.pop_front() {
            self.emit(message, output);
        }
    }

    fn entry(&mut self, entry: &Value) -> Vec<String> {
        let mut output = Vec::new();
        // journalctl -u also includes the manager's unit-exit records. Flush
        // the last warnings when it reaps the runner, including signal deaths.
        if entry["_PID"] == "1"
            && entry["UNIT"] == UNIT
            && (entry["EXIT_CODE"].is_string()
                || matches!(
                    entry["MESSAGE_ID"].as_str(),
                    Some(
                        "98e322203f7a4ed290d09fe03c09fe15"
                            | "be02cf6855d2428ba40df7e9d022f03d"
                            | "7ad2d189f7e94e70a38c781354912448"
                    )
                ))
        {
            self.flush(&mut output);
            return output;
        }
        if entry["_SYSTEMD_UNIT"] != UNIT || entry["_COMM"] != "motor-runner" {
            return output;
        }
        let Some(message) = entry["MESSAGE"].as_str() else {
            return output;
        };
        for line in message.lines() {
            let redacted = console_message(line);
            let panic_header = line.starts_with("thread '") && line.contains("' panicked at ");
            if panic_header || self.panic_body {
                self.flush(&mut output);
                if self.panic_lines < 8 {
                    self.panic_lines += 1;
                    self.emit(
                        if redacted == "[message omitted]" {
                            "runner panic cause=[message omitted]".into()
                        } else {
                            redacted
                        },
                        &mut output,
                    );
                }
                self.panic_body = panic_header;
            } else if line.starts_with("[motor-runner] INFO motor_runner::")
                && (redacted.starts_with("build base seed ")
                    || redacted.starts_with("build base published")
                    || redacted.starts_with("Cargo build-folder maintenance ")
                    || redacted.starts_with("runner build base eviction ")
                    || redacted.starts_with("runner admission cleared its own build outputs ")
                    || redacted.starts_with("runner terminal memory wait "))
            {
                // Successful jobs lose their private journal at VM teardown too.
                // Relay only redacted cache and accounting decisions, not
                // arbitrary INFO output.
                if self.decisions < DECISION_LINES {
                    self.decisions += 1;
                    self.emit(redacted, &mut output);
                }
            // The runner's stderr subscriber encodes severity in MESSAGE,
            // not journald's PRIORITY. Never select arbitrary job-unit output.
            } else if line.starts_with("[motor-runner] WARN motor_runner::")
                || line.starts_with("[motor-runner] ERROR motor_runner::")
                || line.starts_with("Error: ")
            {
                if self.warnings.len() == WARNING_LINES {
                    self.warnings.pop_front();
                }
                self.warnings.push_back(redacted);
            }
        }
        output
    }
}

async fn forward(
    mut journal: impl AsyncRead + Unpin,
    mut console: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    let mut relay = Relay::default();
    let mut record = Vec::new();
    let mut oversized = false;
    let mut buffer = [0; 4096];
    loop {
        let count = journal.read(&mut buffer).await?;
        if count == 0 {
            return Err(io::Error::other("journal stream closed"));
        }
        for byte in &buffer[..count] {
            if *byte == b'\n' {
                if !oversized {
                    if let Ok(entry) = serde_json::from_slice(&record) {
                        for line in relay.entry(&entry) {
                            console.write_all(line.as_bytes()).await?;
                        }
                    }
                }
                record.clear();
                oversized = false;
            } else if record.len() < MAX_RECORD_BYTES && !oversized {
                record.push(*byte);
            } else {
                oversized = true;
                record.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn runner(message: &str) -> Value {
        json!({"_SYSTEMD_UNIT":UNIT,"_COMM":"motor-runner","MESSAGE":message})
    }

    fn exited() -> Value {
        json!({"_PID":"1","UNIT":UNIT,"EXIT_CODE":"killed","EXIT_STATUS":"9"})
    }

    #[test]
    fn journal_watcher_keeps_last_warnings_not_job_output_and_reserves_panic_lines() {
        let mut relay = Relay::default();
        for index in 0..25 {
            assert!(relay.entry(&runner(&format!("[motor-runner] WARN motor_runner::server: runner_channel_closed reason=eof bytes={index} error=SECRET"))).is_empty());
        }
        assert!(relay
            .entry(&runner(
                "[motor-runner] INFO motor_runner::server: runner hello written"
            ))
            .is_empty());
        let mut job = runner("thread 'main' panicked at /tenant/SECRET.rs:1:2:");
        job["_SYSTEMD_UNIT"] = json!("motor-job-123.service");
        assert!(relay.entry(&job).is_empty());
        let mut output = relay.entry(&runner(
            "thread 'SECRET' panicked at crates/motor-runner/src/tenant.rs:12:3:",
        ));
        output.extend(relay.entry(&runner(
            "called `Result::unwrap()` on an `Err` value: SECRET",
        )));
        output.extend(relay.entry(&exited()));
        assert_eq!(output.len(), 22);
        assert_eq!(
            output[0],
            format!("{CONSOLE_TAG}runner_channel_closed reason=eof bytes=5\n")
        );
        assert_eq!(
            output[19],
            format!("{CONSOLE_TAG}runner_channel_closed reason=eof bytes=24\n")
        );
        assert!(output[20].contains("runner panic location=crates/motor-runner/src/tenant.rs:12:3"));
        assert!(output[21].contains("runner panic cause=result_unwrap"));
        assert!(!output.concat().contains("SECRET"));
        assert!(!output.concat().contains("runner hello"));
    }

    #[tokio::test]
    async fn journal_stream_recovers_from_oversized_records_and_flushes_error_on_exit() {
        let mut input = vec![b'x'; MAX_RECORD_BYTES + 1];
        input.extend_from_slice(b"\nnot json\n");
        let job = json!({"_SYSTEMD_UNIT":"motor-job-123.service","_COMM":"motor-runner",
            "MESSAGE":"[motor-runner] ERROR motor_runner::server: runner_channel_closed reason=JOB_OUTPUT"});
        for entry in [job, runner("[motor-runner] ERROR motor_runner::server: runner_channel_closed reason=eof error=SECRET"), exited()] {
            input.extend_from_slice(entry.to_string().as_bytes());
            input.push(b'\n');
        }
        let mut output = Vec::new();
        assert!(forward(input.as_slice(), &mut output).await.is_err());
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("{CONSOLE_TAG}runner_channel_closed reason=eof\n")
        );
    }

    #[test]
    fn journal_watcher_has_per_vm_line_and_byte_limits_even_for_repeated_panics() {
        let mut relay = Relay::default();
        let mut output = Vec::new();
        for _ in 0..100 {
            output.extend(relay.entry(&runner("thread 'main' panicked at /tenant/SECRET.rs:1:2:")));
            output.extend(relay.entry(&runner("SECRET")));
            output.extend(relay.entry(&runner(
                "[motor-runner] WARN motor_runner::server: runner_channel_closed reason=eof",
            )));
            output.extend(relay.entry(&exited()));
        }
        assert!(output.len() <= CONSOLE_MAX_LINES);
        assert!(output.concat().len() <= CONSOLE_MAX_BYTES);
        assert!(output.iter().all(|line| line.starts_with(CONSOLE_TAG)));
        assert!(!output.concat().contains("SECRET"));
    }

    #[test]
    fn successful_job_seed_decisions_reach_console_without_a_runner_failure() {
        let mut relay = Relay::default();
        let id = "01a1d8c8-f1ac-7333-b943-a32753fd8936";
        let mut output = relay.entry(&runner(
            "[motor-runner] INFO motor_runner::seed: build base seed refused: record mismatch inputs_match=false times_valid=true workspace=/SECRET"
        ));
        output.extend(relay.entry(&runner(&format!(
            "[motor-runner] INFO motor_runner::seed: build base seed skipped job_id={id} reason=record_mismatch"
        ))));
        assert_eq!(output.len(), 2);
        assert!(output[0].contains("inputs_match=false times_valid=true"));
        assert!(output[1].contains(&format!("job_id={id} reason=record_mismatch")));
        assert!(!output.concat().contains("SECRET"));
        assert!(relay.entry(&exited()).is_empty());
    }

    #[test]
    fn cache_decision_relay_is_bounded_and_excludes_job_units_and_other_info() {
        let mut relay = Relay::default();
        let message =
            "[motor-runner] INFO motor_runner::seed: build base seed skipped reason=no_base";
        let mut job = runner(message);
        job["_SYSTEMD_UNIT"] = json!("motor-job-123.service");
        assert!(relay.entry(&job).is_empty());
        assert!(relay
            .entry(&runner("[motor-runner] INFO motor_runner::seed: SECRET"))
            .is_empty());
        let mut output = Vec::new();
        for _ in 0..100 {
            output.extend(relay.entry(&runner(message)));
        }
        assert_eq!(output.len(), DECISION_LINES);
        assert!(output.concat().len() <= CONSOLE_MAX_BYTES);
    }
}
