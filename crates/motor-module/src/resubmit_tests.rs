//! Recovery of persisted hosted jobs and pre-cutover direct resubmissions.
use super::queue_tests::{run_spilled, ResubmitPeer};
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

async fn wait_for_provisioning(
    motor: &Motor,
    id: Uuid,
    requests: &crate::hosted::test_http::Requests,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if requests
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _)| p == "/v1/leases")
            {
                let state = motor.hosted_state(id).unwrap();
                assert!(state.lease_attempted);
                assert!(!state.run_sent);
                assert!(state.placement.is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

// Drive the submission directly because the portable scratch directory can be
// outside /home and /Users, which production hosted eligibility rejects.
fn start_spill(motor: &Motor, repo: &Path, wait: u64) -> JobStream {
    let id = Uuid::now_v7();
    motor
        .storage
        .create(id, wait, None, Some("test".into()), false)
        .unwrap();
    motor
        .storage
        .hosted_route(id, crate::HostedJobState::new(&motor.config))
        .unwrap();
    let path = repo.to_str().unwrap();
    let mut request = RunRequest::new(path, path, path, "exit 7").with_timeout(30);
    request.queue_wait_limit_s = Some(wait);
    let (stream, tx, wake) = motor.stream(id);
    let job = motor.clone();
    let submission = motor.submission(id);
    tokio::spawn(async move {
        let _submission = submission;
        if let Err(error) = job
            .run_job(
                id,
                (request, crate::ReturnOptions::default()),
                Vec::new(),
                (wait, true),
                &tx,
                wake,
            )
            .await
        {
            let _ = tx.send(Err(error)).await;
        }
    });
    stream
}

fn assert_attached_execution(records: &[StreamRecord], id: Uuid, code: i32, output: &[u8]) {
    let bytes: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            StreamRecord::Output(output) => Some(output.bytes.0.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(bytes, output, "{records:?}");
    let terminals: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            StreamRecord::Terminal(terminal) => Some(terminal),
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1, "{records:?}");
    assert_eq!(terminals[0].job_id, id);
    assert_eq!(terminals[0].outcome, Outcome::Exit { code });
    assert_eq!(terminals[0].ran, Some(Ran::Remote));
}

#[tokio::test]
async fn attach_during_spill_provisioning_follows_boat_grant() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    boat.hold_provisioning.store(true, Ordering::SeqCst);
    boat.strict_attach.store(true, Ordering::SeqCst);
    let scratch = ScratchDir::new("spill-attach-boat-grant");
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(boat.config.broker_url.as_ref().unwrap()),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let original = start_spill(&motor, &repo, 30);
    let id = original.job_id;
    wait_for_provisioning(&motor, id, &boat.requests).await;
    drop(original);
    let mut attached = motor.attach(AttachRequest::new(id, 0)).unwrap();
    // Provisioning outlasts the hosted follower's three reattach attempts. No
    // record may claim a terminal while the submission can still send Run.
    assert!(
        tokio::time::timeout(Duration::from_secs(11), attached.records.recv())
            .await
            .is_err()
    );
    assert!(!boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(p, _)| p.ends_with("/reattach")));
    boat.hold_provisioning.store(false, Ordering::SeqCst);
    let records = terminal_of(attached).await;
    assert_attached_execution(&records, id, 23, b"same output\n");
    assert_eq!(boat.runs.load(Ordering::SeqCst), 1);
    assert!(log.lock().unwrap().runs.is_empty());
    assert!(motor.storage.job(id).unwrap().build_server_job_id.is_none());
}

#[tokio::test]
async fn attach_unknown_outcome_reports_unconfirmed_cancel_and_retries() {
    let broker = crate::hosted::test_http::Http::new(|_, _| {
        (404, serde_json::json!({"error":"job_unknown"}))
    })
    .await;
    let scratch = ScratchDir::new("spill-unknown-cancel-unacknowledged");
    let config = spill_config(&broker.url);
    let motor = Motor::with_connector(
        config.clone(),
        &scratch.join("state"),
        Arc::new(ResubmitPeer::new()),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 30, None, None, false).unwrap();
    let mut route = crate::HostedJobState::new(&config);
    route.run_sent = true;
    route.terminal = Some(
        TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
            .with_refusal_detail("hosted stream disconnected before terminal"),
    );
    motor.storage.hosted_route(id, route).unwrap();
    let records = terminal_of(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    let [StreamRecord::Terminal(terminal)] = records.as_slice() else {
        panic!("expected an outcome-unknown terminal, got {records:?}")
    };
    assert_eq!(terminal.job_id, id);
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    assert!(terminal
        .refusal_detail
        .as_deref()
        .unwrap()
        .contains("hosted_cancel_unconfirmed"));
    assert!(terminal
        .refusal_detail
        .as_deref()
        .unwrap()
        .contains("hosted stream disconnected before terminal"));
    assert!(motor.pending(id));
    assert_eq!(
        motor.hosted_state(id).unwrap().terminal.as_ref(),
        Some(terminal)
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while broker
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.ends_with("/reattach"))
            .count()
            < 5
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // Stop the retry task after proving it runs independently of the stream.
    motor.storage.cancel(id, false).unwrap();
}

#[tokio::test]
async fn unknown_outcome_defers_boat_finish_until_cancel_retry_is_acknowledged() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    let scratch = ScratchDir::new("spill-unknown-cancel-retry-boat");
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
    let config = spill_config(boat.config.broker_url.as_ref().unwrap());
    let motor = Motor::with_connector(
        config.clone(),
        &scratch.join("state"),
        Arc::new(ResubmitPeer::new()),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 30, None, None, false).unwrap();
    motor
        .storage
        .hosted_route(id, crate::HostedJobState::new(&config))
        .unwrap();
    let path = repo.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "exit 7").with_timeout(30);
    let (channel, _) = motor
        .hosted_client(motor.hosted_state(id).unwrap())
        .run(id, &request, &motor.storage)
        .await
        .unwrap();
    drop(channel);
    motor
        .storage
        .hosted_update(id, |s| s.run_sent = true)
        .unwrap();
    boat.reattach_unavailable.store(true, Ordering::SeqCst);
    let (tx, mut rx) = mpsc::channel(4);
    motor.finish_hosted(id, false, &tx).await.unwrap();
    assert!(
        matches!(rx.recv().await.unwrap().unwrap(), StreamRecord::Terminal(t) if t.outcome == Outcome::OutcomeUnknown && t.refusal_detail.as_deref().unwrap().contains("hosted_cancel_unconfirmed"))
    );
    assert!(motor.pending(id));
    assert!(!boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(p, _)| p.ends_with("/finish")));
    boat.reattach_unavailable.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(30), async {
        while motor.pending(id) || !motor.hosted_state(id).unwrap().finish_done {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(boat
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Cancel(c) if c.job_id == id)));
    assert!(boat
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .0
        .ends_with("/finish"));
}

#[tokio::test]
async fn restart_retries_unknown_hosted_cancel_before_boat_finish() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    let scratch = ScratchDir::new("spill-unknown-cancel-restart");
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
    let config = spill_config(boat.config.broker_url.as_ref().unwrap());
    let motor = Motor::with_connector(
        config.clone(),
        &scratch.join("state"),
        Arc::new(ResubmitPeer::new()),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 30, None, None, false).unwrap();
    motor
        .storage
        .hosted_route(id, crate::HostedJobState::new(&config))
        .unwrap();
    let path = repo.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "exit 7").with_timeout(30);
    let (channel, _) = motor
        .hosted_client(motor.hosted_state(id).unwrap())
        .run(id, &request, &motor.storage)
        .await
        .unwrap();
    drop(channel);
    // Persist the state left when an unknown terminal was delivered before a
    // cancellation acknowledgement, then reopen without an in-memory worker.
    let terminal = TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
        .with_refusal_detail("hosted_cancel_unconfirmed: cancellation intent retained for retry");
    motor
        .storage
        .hosted_update(id, |s| {
            s.run_sent = true;
            s.terminal = Some(terminal.clone());
        })
        .unwrap();
    motor.storage.cancel(id, true).unwrap();
    motor.storage.terminal_observed(id).unwrap();
    motor.close().await;
    boat.reattach_unavailable.store(true, Ordering::SeqCst);
    let motor = Motor::with_connector(
        config,
        &scratch.join("state"),
        Arc::new(ResubmitPeer::new()),
    )
    .unwrap();
    assert!(motor.pending(id));
    let (_stream, _tx, _wake) = motor.stream(id);
    motor.resume_finishes().await;
    assert!(motor.pending(id));
    assert!(!boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(p, _)| p.ends_with("/finish")));
    boat.reattach_unavailable.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(30), async {
        while motor.pending(id) || !motor.hosted_state(id).unwrap().finish_done {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(motor.hosted_state(id).unwrap().terminal, Some(terminal));
    assert!(boat.requests.lock().unwrap().iter().any(|(path, body)| {
        path == &format!("/v1/leases/{id}/finish") && body["outcome"] == "cancelled"
    }));
    assert!(boat
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Cancel(c) if c.job_id == id)));
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
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let config = Config {
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

/// Once the Run frame may have reached a hosted runner, a lost acknowledgement
/// is followed by reattaching to that runner, never by a second submission.
#[tokio::test]
async fn spill_whose_run_reached_hosted_runner_reattaches_not_resubmits() {
    let h = Harness::new(Behavior::DropRunAck).await;
    h.broker.lock().unwrap().knobs.max_s = Some(30);
    let scratch = ScratchDir::new("motor-spill-run-reached");
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
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
    assert!(log.lock().unwrap().runs.is_empty());
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
                .resubmitted(id, build_server_id, "hosted_capacity", None)
                .unwrap();
        }
    }
    // The second job's Run frame reached the build server before the restart.
    log.lock().unwrap().completed.insert(after_run);
    let motor = Motor::with_connector(
        config,
        &scratch.join("state"),
        Arc::new(ResubmitPeer {
            log: log.clone(),
            end: super::queue_tests::ResubmitEnd::Exit,
        }),
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
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
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
    assert!(log.runs.is_empty());
    assert!(log.withdraws.is_empty());
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
