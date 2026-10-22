//! A spilled job that never reached a hosted runner goes back to the build
//! server; one whose Run frame may have reached a runner never does, and jobs
//! routed hosted by configuration keep their typed refusals.
use super::queue_tests::{
    assert_one_build_server_job, assert_resubmitted_once, fixture_repo, run_spilled, ResubmitPeer,
};
use super::*;
use crate::hosted::tests::harness::{Behavior, Harness};
use cortexkit_test_support::ScratchDir;
use std::sync::atomic::Ordering;

fn spill_config(broker_url: &str) -> Config {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    Config {
        runner_instance: "test".into(),
        broker_token_source: crate::BrokerTokenSource::Fake,
        broker_url: Some(broker_url.into()),
        spill_to_hosted: true,
        spill_after_s: 0,
        ..Config::default()
    }
}

async fn terminal_of(mut stream: JobStream) -> Vec<StreamRecord> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut records = Vec::new();
        loop {
            let record = stream.records.recv().await.unwrap().unwrap();
            let terminal = matches!(record, StreamRecord::Terminal(_));
            records.push(record);
            if terminal {
                break records;
            }
        }
    })
    .await
    .unwrap()
}

/// A job routed hosted by configuration, not by a spill, goes through the
/// same steps run_snapshot takes after a lease failure: the hosted client
/// (which still retries capacity three times), the spill-return decision
/// (which declines), and today's typed refusal. Driving the steps directly
/// keeps the test independent of where the scratch directory lives; motor.run
/// refuses hosted paths outside /Users and /home before leasing.
#[tokio::test]
async fn configured_hosted_no_capacity_keeps_typed_refusal() {
    let broker = crate::hosted::test_http::Http::new(|path, _| {
        if path == "/v1/leases" {
            (503, serde_json::json!({"error": "no_capacity"}))
        } else {
            (404, serde_json::json!({"error": "job_unknown"}))
        }
    })
    .await;
    let scratch = ScratchDir::new("motor-configured-hosted-no-capacity");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let config = Config {
        hosted_runners: true,
        spill_to_hosted: false,
        ..spill_config(&broker.url)
    };
    let motor =
        Motor::with_connector(config.clone(), &scratch.join("state"), Arc::new(peer)).unwrap();
    let id = Uuid::now_v7();
    motor
        .storage
        .create(id, 30, None, Some("test".into()), false)
        .unwrap();
    motor
        .storage
        .hosted_route(id, crate::HostedJobState::new(&config))
        .unwrap();
    let path = repo.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "true").with_timeout(30);
    let started = tokio::time::Instant::now();
    let error = match motor
        .hosted_client(motor.hosted_state(id).unwrap())
        .run(id, &request, &motor.storage)
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("no_capacity produced a lease"),
    };
    assert_eq!(error, crate::hosted::Error::Capacity);
    assert_eq!(
        broker
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/leases")
            .count(),
        3
    );
    assert!(!motor.return_spill(id, error.code(), started).await.unwrap());
    let (tx, mut rx) = mpsc::channel(4);
    motor
        .refused_with_retry(
            id,
            error.reason(),
            None,
            Some(error.detail()),
            error.retry_after_ms(),
            &tx,
        )
        .await
        .unwrap();
    let Some(Ok(StreamRecord::Terminal(terminal))) = rx.recv().await else {
        panic!("expected a terminal")
    };
    assert_eq!(terminal.job_id, id);
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(terminal
        .refusal_detail
        .as_deref()
        .is_some_and(|d| d.starts_with("hosted_capacity")));
    assert!(log.lock().unwrap().runs.is_empty());
    let state = motor.hosted_state(id).unwrap();
    assert!(state.hosted_no_fallback);
    assert_eq!(state.hosted_code.as_deref(), Some("hosted_capacity"));
    assert!(motor.storage.job(id).unwrap().build_server_job_id.is_none());
}

/// A first-party host received the lease's token but refused it, so no Run
/// frame was ever sent: the spill goes back to the build server, and the lease
/// is released.
#[tokio::test]
async fn spill_refused_by_hosted_runner_before_run_resubmits() {
    let h = Harness::new(Behavior::Refuse("capacity")).await;
    let scratch = ScratchDir::new("motor-spill-host-refused");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(&h.route.broker_url),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(60)).await;
    assert_one_build_server_job(id, &records);
    let resubmitted = assert_resubmitted_once(id, &log.lock().unwrap(), 30);
    assert_eq!(
        motor.storage.job(id).unwrap().build_server_job_id,
        Some(resubmitted)
    );
    assert!(h.tokens.load(Ordering::SeqCst) >= 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    let requests = h.requests.lock().unwrap();
    assert!(
        requests.last().unwrap().ends_with("/release"),
        "{requests:?}"
    );
    assert!(!requests.iter().any(|p| p.ends_with("/reattach")));
}

/// Once the Run frame may have reached a hosted runner, a lost acknowledgement
/// is followed by reattaching to that runner, never by a second submission.
#[tokio::test]
async fn spill_whose_run_reached_hosted_runner_reattaches_not_resubmits() {
    let h = Harness::new(Behavior::DropRunAck).await;
    h.broker.lock().unwrap().knobs.max_s = Some(30);
    let scratch = ScratchDir::new("motor-spill-run-reached");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(&h.route.broker_url),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(60)).await;
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(t)) if t.job_id == id && t.outcome == Outcome::Exit { code: 23 }),
        "{records:?}"
    );
    assert_eq!(log.lock().unwrap().runs, vec![(id, 0)]);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert!(motor.storage.job(id).unwrap().build_server_job_id.is_none());
    let requests = h.requests.lock().unwrap();
    assert!(requests.iter().any(|p| p.ends_with("/reattach")));
    assert!(!requests.iter().any(|p| p.ends_with("/release")));
}

/// ck-motor stores the resubmission's build-server ID before sending its Run
/// frame. A restart in between attaches to that ID: the runner, which never
/// admitted it, answers with a retry-safe refused-before-start terminal, and
/// nothing is submitted. A restart after the Run frame picks up the real
/// output under the same ID. Either way the caller sees its own job ID.
#[tokio::test]
async fn restart_between_resubmit_and_run_attaches_build_server_id() {
    let scratch = ScratchDir::new("motor-resubmit-restart");
    let config = spill_config("http://127.0.0.1:1");
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let (before_run, after_run) = (Uuid::now_v7(), Uuid::now_v7());
    let (before_id, after_id) = (Uuid::now_v7(), Uuid::now_v7());
    {
        let motor =
            Motor::with_connector(config.clone(), &scratch.join("state"), Arc::new(peer)).unwrap();
        for (id, build_server_id) in [(before_id, before_run), (after_id, after_run)] {
            motor
                .storage
                .create(id, 30, None, Some("test".into()), false)
                .unwrap();
            let mut route = crate::HostedJobState::new(&config);
            route.spill_wait_ms = Some(45_000);
            route.lease_attempted = true;
            route.placement_attempts = 1;
            motor.storage.hosted_route(id, route).unwrap();
            motor
                .storage
                .resubmitted(id, build_server_id, "hosted_capacity")
                .unwrap();
        }
    }
    // The second job's Run frame reached the build server before the restart.
    log.lock().unwrap().completed.insert(after_run);
    let motor = Motor::with_connector(
        config,
        &scratch.join("state"),
        Arc::new(ResubmitPeer { log: log.clone() }),
    )
    .unwrap();
    assert!(motor.hosted_state(before_id).is_none());

    let records = terminal_of(motor.attach(AttachRequest::new(before_id, 0)).unwrap()).await;
    let [StreamRecord::Terminal(terminal)] = records.as_slice() else {
        panic!("{records:?}")
    };
    assert_eq!(terminal.job_id, before_id);
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TransferInterrupted
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));

    let records = terminal_of(motor.attach(AttachRequest::new(after_id, 0)).unwrap()).await;
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(t)) if t.job_id == after_id && t.outcome == Outcome::Exit { code: 7 }),
        "{records:?}"
    );
    assert!(records
        .iter()
        .any(|r| matches!(r, StreamRecord::Output(o) if o.seq == 1)));

    let log = log.lock().unwrap();
    assert_eq!(log.attaches, vec![before_run, after_run]);
    assert!(log.runs.is_empty(), "a restart never submits again");
}

/// A spilled job whose lease CLOUD refuses with 403 `account_closed` is not
/// sent back to the build server: a closed account must not run anything
/// anywhere. The caller gets one typed refusal that says not to repeat the
/// request, and nothing falls back. (`no_capacity` still sends a spill back:
/// `spilled_no_capacity_resubmits_to_build_server_as_one_stream`.)
#[tokio::test]
async fn spilled_account_closed_is_refused_and_never_resubmitted() {
    let (status, body) = crate::hosted::tests::cloud_account_closed_reply();
    let broker = crate::hosted::test_http::Http::new(move |path, _| {
        if path == "/v1/leases" {
            (status, body.clone())
        } else {
            (404, serde_json::json!({"error": "job_unknown"}))
        }
    })
    .await;
    let scratch = ScratchDir::new("motor-spill-account-closed");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(&broker.url),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(30)).await;
    let Some(StreamRecord::Terminal(terminal)) = records.last() else {
        panic!("{records:?}")
    };
    assert_eq!(terminal.job_id, id);
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.retry_after_ms, None);
    assert_eq!(
        terminal.refusal_detail.as_deref(),
        Some("hosted_account_closed: hosted account closed")
    );
    // The build server saw only the first submission, which the spill
    // withdrew; nothing was submitted again.
    let log = log.lock().unwrap();
    assert_eq!(log.runs, vec![(id, 0)]);
    assert_eq!(log.withdraws, vec![id]);
    let requests = broker.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(path, _)| path == "/v1/leases")
            .count(),
        1,
        "{requests:?}"
    );
    assert!(!requests
        .iter()
        .any(|(path, _)| path.ends_with("/reattach") || path.ends_with("/finish")));
    assert!(motor.storage.job(id).unwrap().build_server_job_id.is_none());
    let state = motor.hosted_state(id).unwrap();
    assert!(state.hosted_no_fallback);
    assert_eq!(state.hosted_code.as_deref(), Some("hosted_account_closed"));
}
