use super::*;

use crate::backend::ExecutionOutcome;
use motor_protocol::Outcome;
use std::os::unix::process::ExitStatusExt;

fn output(text: &str) -> Output {
    Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: text.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

#[test]
fn retained_main_status_preserves_exit_signal_and_core_dump() {
    for (code, status, expected) in [
        (1, 0, Outcome::Exit { code: 0 }),
        (1, 42, Outcome::Exit { code: 42 }),
        (1, 255, Outcome::Exit { code: 255 }),
        (2, 9, Outcome::Signal { signal: 9 }),
        (2, 15, Outcome::Signal { signal: 15 }),
        (3, 11, Outcome::Signal { signal: 11 }),
    ] {
        let exit = exit_status("job.service", |args| {
            assert_eq!(
                args,
                [
                    "show",
                    "--property=ExecMainCode",
                    "--property=ExecMainStatus",
                    "job.service"
                ]
            );
            Ok(output(&format!(
                "ExecMainCode={code}\nExecMainStatus={status}\n"
            )))
        })
        .unwrap();
        assert_eq!(ExecutionOutcome::Unix(exit).wire(), expected);
        assert_eq!(exit.core_dumped(), code == 3);
    }
}

#[test]
fn missing_or_invalid_main_status_never_becomes_success() {
    for text in [
        "",
        "ExecMainCode=\nExecMainStatus=\n",
        "ExecMainCode=2\n",
        "ExecMainStatus=9\n",
        "ExecMainCode=0\nExecMainStatus=0\n",
        "ExecMainCode=1\nExecMainStatus=256\n",
        "ExecMainCode=1\nExecMainStatus=-1\n",
        "ExecMainCode=2\nExecMainStatus=0\n",
        "ExecMainCode=3\nExecMainStatus=128\n",
        "ExecMainCode=4\nExecMainStatus=9\n",
        "ExecMainCode=invalid\nExecMainStatus=9\n",
    ] {
        assert!(
            exit_status("job.service", |_| Ok(output(text))).is_err(),
            "{text:?}"
        );
    }
    let error = exit_status("job.service", |_| {
        Ok(Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: b"ExecMainCode=1\nExecMainStatus=0\n".to_vec(),
            stderr: b"unit not loaded".to_vec(),
        })
    })
    .unwrap_err();
    assert!(error.to_string().contains("unit not loaded"));
}
