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
fn installed_units_reserve_host_memory_and_protect_user_runners() {
    let setting = |unit: &str, section: &str, key: &str, expected: &str| {
        let mut current = "";
        let values: Vec<_> = unit
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.starts_with(['#', ';']) {
                    return None;
                }
                if line.starts_with('[') {
                    current = line;
                }
                let (name, value) = line.split_once('=')?;
                (current == section && name == key).then_some(value)
            })
            .collect();
        assert_eq!(values, [expected], "{section} {key} in {unit}");
    };
    let slice = include_str!("../../../script/runner/units/system/motor-jobs.slice");
    setting(slice, "[Slice]", "MemoryHigh", "75%");
    setting(slice, "[Slice]", "MemoryMax", "85%");
    setting(slice, "[Slice]", "MemorySwapMax", "0");
    for unit in [
        include_str!("../../../script/runner/units/motor-runner.service"),
        include_str!("../../../script/runner/units/motor-runner@.service"),
        include_str!("../../../script/runner/units/system/runner-user-manager.conf"),
        include_str!("../../../script/boat-image/motor-boat.service"),
        include_str!("../../../script/tenant-image/guest/motor-tenant-agent.service"),
    ] {
        setting(unit, "[Service]", "OOMScoreAdjust", "-900");
        setting(unit, "[Service]", "OOMPolicy", "continue");
    }
}

#[test]
fn missing_or_unbounded_job_slice_refuses_execution() {
    for (text, budget) in [
        ("", None),
        ("MemoryHigh=infinity\nMemoryMax=infinity\n", None),
        ("MemoryHigh=100\nMemoryMax=100\n", None),
        ("MemoryHigh=0\nMemoryMax=100\n", None),
        (
            "MemoryHigh=75\nMemoryMax=85\n",
            Some(JobMemoryBudget { max: 85, high: 75 }),
        ),
    ] {
        with_process_hook(Box::new(move |_| Ok(output(text))), || {
            assert_eq!(read_job_memory_budget().ok(), budget, "{text}");
        });
    }
}

/// Admission keeps only the remembered ceiling, and the placeholder for "no
/// reading" must never look like a real one.
#[test]
fn a_remembered_slice_ceiling_reads_back_as_a_budget() {
    assert_eq!(
        JobMemoryBudget::from_max(25_769_803_776).map(|budget| budget.max),
        Some(25_769_803_776)
    );
    assert_eq!(JobMemoryBudget::from_max(NO_JOB_MEMORY_BUDGET), None);
}

#[test]
fn dead_launcher_never_counts_as_a_started_job_even_if_unit_is_active() {
    with_process_hook(Box::new(|_| Ok(output(""))), || {
        assert!(!wait_until_active(
            "motor-job-test.service",
            || Ok(true),
            |_| panic!("must not wait")
        )
        .unwrap());
        let calls = std::cell::Cell::new(0);
        assert!(!wait_until_active(
            "motor-job-test.service",
            || {
                calls.set(calls.get() + 1);
                Ok(calls.get() == 2)
            },
            |_| panic!("must not wait")
        )
        .unwrap());
    });
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

#[test]
fn queries_are_unprivileged_and_mutations_keep_sudo() {
    with_process_hook(
        Box::new(|command| {
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            if command.get_program() == "/usr/bin/sudo" {
                assert_eq!(&args[..2], &["-n", "/usr/bin/systemctl"]);
                assert!([
                    "start",
                    "stop",
                    "kill",
                    "reset-failed",
                    "restart",
                    "unknown-verb"
                ]
                .contains(&args[2]));
            } else {
                assert_eq!(command.get_program(), "/usr/bin/systemctl");
                assert!([
                    "show",
                    "is-active",
                    "is-failed",
                    "list-units",
                    "list-unit-files",
                    "status",
                    "cat",
                    "help",
                    "list-jobs",
                    "show-environment",
                    "list-dependencies",
                    "is-enabled",
                    "get-default",
                    "is-system-running"
                ]
                .contains(&args[0]));
            }
            Ok(output(""))
        }),
        || {
            let mut config = crate::engine::Config::new("unused", "main");
            config.isolate_jobs = true;
            for verb in [
                "show",
                "is-active",
                "is-failed",
                "list-units",
                "list-unit-files",
                "status",
                "cat",
                "help",
                "list-jobs",
                "show-environment",
                "list-dependencies",
                "is-enabled",
                "get-default",
                "is-system-running",
                "start",
                "stop",
                "kill",
                "reset-failed",
                "restart",
                "unknown-verb",
            ] {
                crate::backend::systemctl(&config, &[verb, "motor-job-test.service"]).unwrap();
            }
            config.systemd_command = Some(std::sync::Arc::new(|args| {
                assert_eq!(args, ["show", "motor-job-test.service"]);
                Ok(output("hook"))
            }));
            assert_eq!(
                crate::backend::systemctl(&config, &["show", "motor-job-test.service"])
                    .unwrap()
                    .stdout,
                b"hook"
            );
        },
    );
}

#[test]
fn activation_backoff_preserves_fast_starts_and_caps_slow_polling() {
    let mut backoff = PollBackoff::activation();
    let delays: Vec<_> = (0..20).map(|_| backoff.next_delay().as_millis()).collect();
    assert_eq!(
        &delays[..11],
        &[10, 10, 10, 10, 10, 10, 20, 40, 80, 160, 200]
    );
    assert!(delays[11..].iter().all(|delay| *delay == 200));
    let latency = |ready_ms| {
        let elapsed = std::rc::Rc::new(std::cell::Cell::new(0u128));
        let clock = elapsed.clone();
        with_process_hook(
            Box::new(move |command| {
                assert_eq!(command.get_program(), "/usr/bin/systemctl");
                let mut reply = output("");
                if clock.get() < ready_ms {
                    reply.status = std::process::ExitStatus::from_raw(3 << 8);
                }
                Ok(reply)
            }),
            || {
                assert!(wait_until_active(
                    "motor-job-test.service",
                    || Ok(false),
                    |delay| elapsed.set(elapsed.get() + delay.as_millis())
                )
                .unwrap());
            },
        );
        elapsed.get()
    };
    for ready in [0u128, 1, 10, 11, 20, 21, 23, 30, 40, 50] {
        assert_eq!(ready.div_ceil(10) * 10, latency(ready));
    }
    assert_eq!(latency(300), 360);
    println!("activation latency with a virtual clock and zero-cost command seam: ready=0ms before=0ms after=0ms; ready=23ms before=30ms after=30ms; ready=50ms before=50ms after=50ms; ready=300ms before=300ms after=360ms");
}

#[test]
fn slow_activation_timeout_bounds_query_count() {
    let elapsed = std::cell::Cell::new(Duration::ZERO);
    let queries = std::rc::Rc::new(std::cell::Cell::new(0));
    let count = queries.clone();
    with_process_hook(
        Box::new(move |command| {
            assert_eq!(command.get_program(), "/usr/bin/systemctl");
            count.set(count.get() + 1);
            let mut reply = output("");
            reply.status = std::process::ExitStatus::from_raw(3 << 8);
            Ok(reply)
        }),
        || {
            assert!(!wait_until_active(
                "motor-job-test.service",
                || Ok(elapsed.get() >= Duration::from_secs(15)),
                |delay| elapsed.set(elapsed.get() + delay)
            )
            .unwrap());
        },
    );
    assert!((15_000..=15_200).contains(&elapsed.get().as_millis()));
    assert!(queries.get() < 90, "{} activation queries", queries.get());
}

#[test]
fn monitoring_backoff_caps_queries_and_preserves_gone_unit_detection() {
    let mut polling = PollBackoff::monitoring();
    let delays: Vec<_> = (0..12).map(|_| polling.next_delay().as_millis()).collect();
    assert_eq!(&delays[..6], &[50, 100, 200, 400, 800, 1000]);
    assert!(delays[6..].iter().all(|delay| *delay == 1000));
    with_process_hook(
        Box::new(|command| {
            assert_eq!(command.get_program(), "/usr/bin/systemctl");
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert_eq!(
                args,
                [
                    "show",
                    "--property=LoadState",
                    "--property=ActiveState",
                    "motor-job-test.service"
                ]
            );
            Ok(Output {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: b"LoadState=not-found\n".to_vec(),
                stderr: Vec::new(),
            })
        }),
        || assert!(!alive("motor-job-test.service", |args| systemctl(args, true)).unwrap()),
    );
}
