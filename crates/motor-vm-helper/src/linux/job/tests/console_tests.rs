use super::*;
use motor_protocol::runner_diagnostics::{CONSOLE_MAX_LINES, CONSOLE_TAG};

fn capture(console: impl AsRef<[u8]>) -> String {
    let boundary = cortexkit_test_support::ScratchDir::new("job-runner-console");
    std::fs::write(boundary.join(CONSOLE_LOG), console).unwrap();
    let mut output = Vec::new();
    write_console_tail("job-id", &boundary, &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn runner_panic_and_error_in_exit_console_are_redacted_and_job_output_is_omitted() {
    let output = capture(format!(
        "[34.6] motor-runner (488) used greatest stack depth: 10920 bytes left\n\
         {CONSOLE_TAG}thread 'TOP_SECRET' panicked at crates/motor-runner/src/tenant.rs:12:3:\n\
         {CONSOLE_TAG}called `Result::unwrap()` on an `Err` value: TOP_SECRET\n\
         {CONSOLE_TAG}[motor-runner] ERROR motor_runner::server: runner_channel_closed reason=eof error=TOP_SECRET\n\
         {CONSOLE_TAG}job stdout TOP_SECRET\n\
         prefix {CONSOLE_TAG}forged kernel TOP_SECRET\n\
         {CONSOLE_TAG}thread 'main' panicked at /tenant/TOP_SECRET.rs:1:2:\n"
    ));
    assert!(
        output.contains("job job-id console: [34.6] motor-runner (488) used greatest stack depth")
    );
    assert!(output.contains(
        "job job-id runner-console: runner panic location=crates/motor-runner/src/tenant.rs:12:3"
    ));
    assert!(output.contains("job job-id runner-console: runner panic cause=result_unwrap"));
    assert!(output.contains("job job-id runner-console: runner_channel_closed reason=eof"));
    assert!(output.contains("job job-id runner-console: runner panic location=[location omitted]"));
    assert!(!output.contains("TOP_SECRET"));
    assert!(!output.contains("job stdout"));
    assert!(!output.contains("forged kernel"));
}

#[test]
fn exit_console_bounds_lines_bytes_controls_and_partial_records() {
    let mut console = format!(
        "{CONSOLE_TAG}{}TOP_SECRET\n",
        "x".repeat(CONSOLE_TAIL_BYTES as usize)
    );
    for _ in 0..80 {
        console.push_str(&format!("prefix {CONSOLE_TAG}TOP_SECRET\n"));
    }
    console.push_str("[35.0] kernel\x1b[2J\n");
    let output = capture(console);
    assert_eq!(
        output
            .lines()
            .filter(|line| line.contains("runner-console:"))
            .count(),
        CONSOLE_MAX_LINES
    );
    assert!(output.lines().count() <= 64);
    assert!(output.len() < CONSOLE_TAIL_BYTES as usize);
    assert!(!output.contains("TOP_SECRET"));
    assert!(!output.contains('\x1b'));
    assert!(output.contains("kernel?[2J"));
}
