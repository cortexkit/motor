//! A hosted job that ends with an unknown outcome always logs why: the
//! runner's own detail when it reported the unknown outcome, or the reason
//! ck-motor gave up when no terminal arrived.
use super::*;
use crate::hosted::{
    boat_tests::{committed_repo, records},
    tests::harness::{Behavior, Harness, NoSsh, UNKNOWN_DETAIL, UNKNOWN_WALL_MS},
};
use std::sync::atomic::AtomicUsize;

/// The runner ends a running job as outcome_unknown on a live connection: no
/// disconnect or reattach happens, so the terminal's own detail is the only
/// cause there is, and it must reach the log before the lease is settled.
#[tokio::test]
async fn runner_reported_outcome_unknown_logs_its_cause() {
    let h = Harness::new(Behavior::UnknownTerminal).await;
    let (scratch, repo) = committed_repo();
    let config = Config {
        hosted_repositories: vec![repo.to_str().unwrap().into()],
        ..h.config()
    };
    let motor = Motor::with_connector(
        config,
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let captured = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(captured.clone());
    let args = crate::cli::RunArgs {
        cwd: Some(repo.clone()),
        command: vec!["true".into()],
        timeout: 30,
        ..crate::cli::RunArgs::default()
    };
    let stream = motor
        .run(crate::cli::build_request(&args).unwrap())
        .unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    let text = captured.text();
    let job = format!("job_id={id}");
    let reported = text
        .lines()
        .find(|line| line.contains(&job) && line.contains("hosted runner reported outcome unknown"))
        .unwrap_or_else(|| panic!("no runner terminal line: {text}"));
    assert!(reported.contains("WARN"), "{reported}");
    assert!(reported.contains(UNKNOWN_DETAIL), "{reported}");
    assert!(
        reported.contains(&format!("wall_ms={UNKNOWN_WALL_MS}")),
        "{reported}"
    );
    let settled = text
        .lines()
        .find(|line| line.contains(&job) && line.contains("hosted job outcome unknown"))
        .unwrap_or_else(|| panic!("no outcome-unknown line: {text}"));
    assert!(settled.contains(UNKNOWN_DETAIL), "{settled}");
}

/// ck-motor ends a hosted job itself when no terminal ever arrived; the log
/// says so rather than leaving only the settlement line.
#[tokio::test]
async fn outcome_unknown_without_a_runner_terminal_logs_that_none_arrived() {
    let scratch = cortexkit_test_support::ScratchDir::new("motor-outcome-unknown-no-terminal");
    let config = Config {
        hosted_runners: true,
        ..Config::default()
    };
    let motor = Motor::with_connector(
        config.clone(),
        &scratch.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor
        .storage
        .create(id, 30, None, Some("test".into()), false)
        .unwrap();
    motor
        .storage
        .hosted_route(id, crate::HostedJobState::new(&config))
        .unwrap();
    let captured = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(captured.clone());
    let (tx, mut rx) = mpsc::channel(4);
    motor.finish_hosted(id, false, &tx).await.unwrap();
    let Some(Ok(StreamRecord::Terminal(terminal))) = rx.recv().await else {
        panic!("expected a terminal")
    };
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    let text = captured.text();
    assert!(
        text.lines()
            .any(|line| line.contains(&format!("job_id={id}"))
                && line.contains("hosted job outcome unknown")
                && line.contains("no terminal received from the hosted runner")),
        "{text}"
    );
}
