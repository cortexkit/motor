use super::*;
use crate::hosted::tests::harness::{Behavior, Harness, NoSsh};
use motor_protocol::{BytePayload, Outcome, Output, OutputStream, Ran};
use std::sync::atomic::AtomicUsize;

async fn detached(h: &Harness, root: &std::path::Path) -> (Motor, Uuid) {
    let motor =
        Motor::with_connector(h.config(), root, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 60, None, None, false).unwrap();
    motor.storage.hosted_route(id, h.route.clone()).unwrap();
    let (channel, lease) = crate::hosted::Client::new(h.route.clone())
        .run(id, &Harness::request(root), &motor.storage)
        .await
        .unwrap();
    drop(channel);
    motor
        .storage
        .hosted_update(id, |state| {
            state.lease(&lease);
            state.run_sent = true;
        })
        .unwrap();
    (motor, id)
}

async fn settled(motor: &Motor, id: Uuid) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while motor.hosted_state(id).unwrap().billing == crate::BillingStatus::Pending {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("settlement must complete");
}

fn fixture() -> (cortexkit_test_support::ScratchDir, Motor, Uuid) {
    let root = cortexkit_test_support::ScratchDir::new("hosted-owner-audit");
    let motor = Motor::with_connector(
        crate::Config::default(),
        &root,
        Arc::new(crate::hosted::tests::harness::NoSsh(
            std::sync::atomic::AtomicUsize::new(0),
        )),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 60, None, None, false).unwrap();
    let mut route = crate::HostedJobState::new(&motor.config);
    route.run_sent = true;
    route.retry_iat = Some(crate::hosted::now_s());
    route.max_runtime_s = Some(60);
    route.placement = Some(motor_lease::Placement {
        host: "first-party-audit".into(),
        vm: None,
    });
    motor.storage.hosted_route(id, route).unwrap();
    (root, motor, id)
}

fn output(seq: u64, bytes: usize) -> StreamRecord {
    StreamRecord::Output(Output::new(
        seq,
        OutputStream::Stdout,
        BytePayload(vec![b'x'; bytes]),
    ))
}

fn real_terminal(id: Uuid) -> TerminalRecord {
    TerminalRecord::new(id, Outcome::Exit { code: 41 }, 125, 0, 0).with_ran(Ran::Remote)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_01_settled_first_party_is_absent_from_recovery_inventory() {
    let (_root, motor, id) = fixture();
    motor
        .storage
        .hosted_update(id, |state| {
            state.terminal = Some(real_terminal(id));
            state.billing = crate::BillingStatus::Available;
        })
        .unwrap();
    assert!(!motor.storage.unsettled_hosted().contains(&id));
    motor.resume_finishes().await;
    assert!(!motor.hosted_results.lock().unwrap().contains_key(&id));
    motor.close().await;
}

#[tokio::test]
async fn audit_04_sandbox_loss_preserves_real_terminal() {
    let (_root, motor, id) = fixture();
    motor
        .storage
        .hosted_update(id, |state| state.terminal = Some(real_terminal(id)))
        .unwrap();
    let (tx, mut records) = mpsc::channel(32);
    motor.lost_sandbox(id, &tx).await.unwrap();
    assert_eq!(
        motor.hosted_state(id).unwrap().terminal.unwrap().outcome,
        Outcome::Exit { code: 41 }
    );
    assert!(
        matches!(records.recv().await.unwrap().unwrap(), StreamRecord::Terminal(t) if t.outcome == Outcome::Exit { code: 41 })
    );
    motor.close().await;
}

#[tokio::test(start_paused = true)]
async fn audit_04_first_durable_boat_terminal_starts_drain_before_transport_progress() {
    let (_root, motor, id) = fixture();
    motor
        .storage
        .hosted_update(id, |state| {
            state.placement.as_mut().unwrap().host = "boat:audit".into()
        })
        .unwrap();
    let owner = motor.reserve_hosted_result(id, u64::MAX);
    owner.state.lock().unwrap().subscribers.insert(7, 9);
    motor
        .persist_hosted_terminal(id, real_terminal(id))
        .unwrap();
    let (deadline, cohort) = owner
        .state
        .lock()
        .unwrap()
        .boat_drain
        .clone()
        .expect("durable probe terminal did not start the end-to-end drain bound");
    assert!(deadline <= tokio::time::Instant::now() + Duration::from_secs(60));
    assert_eq!(cohort, vec![7]);
    tokio::time::advance(Duration::from_secs(30)).await;
    motor
        .persist_hosted_terminal(id, real_terminal(id))
        .unwrap();
    assert_eq!(
        owner.state.lock().unwrap().boat_drain.as_ref().unwrap().0,
        deadline,
        "a replayed terminal extended the original drain bound"
    );
    motor.hosted_results.lock().unwrap().remove(&id);
    motor
        .storage
        .hosted_update(id, |state| {
            state.finish_done = true;
            state.billing = crate::BillingStatus::Unavailable;
        })
        .unwrap();
    motor.close().await;
}

#[tokio::test(start_paused = true)]
async fn audit_05_max_cursor_drain_honors_pending_subscriber_rewind() {
    for rewind in [None, Some(9)] {
        let owner = Owner::new(u64::MAX);
        {
            let mut state = owner.state.lock().unwrap();
            state.subscribers.insert(0, 9);
            state.rewind = rewind;
        }
        assert!(!owner.wait_boat_subscribers(0).await);
        assert_eq!(owner.state.lock().unwrap().rewind, Some(9));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_07_subscribing_to_stopped_owner_never_strands_caller() {
    let (_root, motor, id) = fixture();
    motor
        .storage
        .hosted_update(id, |state| {
            state.terminal = Some(real_terminal(id));
            state.billing = crate::BillingStatus::Available;
        })
        .unwrap();
    let owner = Owner::new(0);
    owner.state.lock().unwrap().finished = true;
    owner.stopped.send_replace(true);
    let (tx, mut records) = mpsc::channel(32);
    motor.subscribe_hosted_result(id, owner, 0, false, tx);
    let record = tokio::time::timeout(Duration::from_secs(1), records.recv())
        .await
        .expect("a stopped owner must not strand a new subscriber")
        .unwrap()
        .unwrap();
    assert!(matches!(record, StreamRecord::Terminal(t) if t.outcome == Outcome::Exit { code: 41 }));
    motor.close().await;
}

#[tokio::test]
async fn audit_14d_drain_error_does_not_rearm_rewind() {
    let (_root, motor, id) = fixture();
    let owner = Owner::new(10);
    owner
        .state
        .lock()
        .unwrap()
        .subscriber_errors
        .insert(0, "subscriber cursor 0 exceeded the drain deadline".into());
    let (tx, mut records) = mpsc::channel(32);
    motor.subscribe_hosted_result(id, owner.clone(), 0, false, tx);
    let error = tokio::time::timeout(Duration::from_secs(1), records.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cursor 0"));
    assert!(owner.state.lock().unwrap().rewind.is_none());
    motor.close().await;
}

#[tokio::test]
async fn audit_14e_oversized_newest_record_is_retained_and_delivered() {
    let (_root, motor, id) = fixture();
    let owner = Owner::new(0);
    let (tx, mut records) = mpsc::channel(32);
    motor.subscribe_hosted_result(id, owner.clone(), 0, false, tx);
    owner.publish(Ok(output(0, 8)));
    assert!(
        matches!(records.recv().await.unwrap().unwrap(), StreamRecord::Output(o) if o.seq == 0)
    );
    let large = output(1, RING_BYTES);
    assert!(serde_json::to_vec(&large).unwrap().len() > RING_BYTES);
    owner.publish(Ok(large));
    assert_eq!(
        owner.state.lock().unwrap().ring.len(),
        1,
        "the newest record must never evict itself"
    );
    let record = tokio::time::timeout(Duration::from_secs(1), records.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(record, StreamRecord::Output(o) if o.seq == 1 && o.bytes.0.len() == RING_BYTES)
    );
    assert_eq!(owner.state.lock().unwrap().ring.len(), 1);
    motor.close().await;
}

#[test]
fn audit_02_ownerless_boat_auth_retirement_releases_registry_before_recovery() {
    // A blocked std mutex must not also block the test's timeout or runtime drop.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (finished, completion) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        let h = Harness::with_host(Behavior::RefuseAttach("auth"), "boat:audit").await;
        let root = cortexkit_test_support::ScratchDir::new("audit-boat-retirement-lock");
        let (motor, id) = detached(&h, &root).await;
        motor
            .storage
            .hosted_update(id, |state| state.retry_iat = Some(0))
            .unwrap();
        let result = motor.open_job_channel_at(id, u64::MAX).await;
        assert!(matches!(result, Err(crate::hosted::Error::AttachAuth)));
        motor.close().await;
        let _ = finished.send(());
    });
    let result = completion.recv_timeout(Duration::from_secs(4));
    runtime.shutdown_timeout(Duration::from_millis(100));
    result.expect("ownerless auth retirement deadlocked on the owner registry");
}

#[tokio::test]
async fn audit_03_queued_old_transport_records_cannot_advance_rewind_cursor() {
    let owner = Owner::new(1_000);
    let (tx, mut records) = mpsc::channel(32);
    emit_follow_record(Some(&owner), &tx, output(1_000, 8))
        .await
        .unwrap();
    owner.state.lock().unwrap().rewind = Some(9);
    assert_eq!(owner.take_rewind(), Some(9));
    owner.publish(records.recv().await.unwrap());
    let (empty, cursor) = {
        let state = owner.state.lock().unwrap();
        (state.ring.is_empty(), state.cursor)
    };
    assert!(
        empty,
        "an old-generation queued record was published into the replay generation"
    );
    assert_eq!(cursor, 9);
    emit_follow_record(Some(&owner), &tx, output(9, 8))
        .await
        .unwrap();
    owner.publish(records.recv().await.unwrap());
    assert_eq!(owner.state.lock().unwrap().cursor, 10);
}

#[tokio::test]
async fn audit_03_ready_replay_batch_cannot_evict_before_subscriber_is_scheduled() {
    let (_root, motor, id) = fixture();
    let owner = motor.reserve_hosted_result(id, 0);
    let (mut caller, caller_tx, _) = motor.stream(id);
    motor.subscribe_hosted_result(id, owner.clone(), 0, false, caller_tx);
    let (source, records) = mpsc::channel(32);
    for seq in 0..32 {
        emit_follow_record(Some(&owner), &source, output(seq, 32 * 1024))
            .await
            .unwrap();
    }
    let producer = tokio::spawn(std::future::pending::<()>());
    motor.collect_hosted_result(id, owner, source, records, producer);
    let record = tokio::time::timeout(Duration::from_secs(1), caller.records.recv())
        .await
        .expect(
            "a ready transport batch evicted the requested record before scheduling its subscriber",
        )
        .unwrap()
        .unwrap();
    assert!(matches!(record, StreamRecord::Output(output) if output.seq == 0));
    drop(caller);
    motor.close().await;
}

#[tokio::test]
async fn audit_14b_probe_terminals_clear_retry_and_pending_cancel() {
    let h = Harness::new(Behavior::StatusTerminalRetry).await;
    let root = cortexkit_test_support::ScratchDir::new("audit-probe-normalize");
    let (motor, id) = detached(&h, &root).await;
    motor.storage.cancel(id, true).unwrap();
    // Usage arrived first; the probed execution terminal completes the pair.
    motor
        .storage
        .hosted_update(id, |state| state.billing = crate::BillingStatus::Available)
        .unwrap();
    let owner = motor.reserve_hosted_result(id, u64::MAX);
    let reply = motor
        .probe_hosted_result(id, crate::hosted::now_s() * 1000, &owner)
        .await
        .unwrap();
    assert_eq!(reply.terminal.unwrap().retry_after_ms, None);
    assert!(!motor.pending(id));
    motor.hosted_results.lock().unwrap().remove(&id);
    motor.close().await;
}

#[tokio::test]
async fn audit_14b_probe_refusal_is_normalized_as_lease_mismatch() {
    let h = Harness::new(Behavior::StatusTerminalRefused).await;
    let root = cortexkit_test_support::ScratchDir::new("audit-probe-refusal");
    let (motor, id) = detached(&h, &root).await;
    let owner = motor.reserve_hosted_result(id, u64::MAX);
    let reply = motor
        .probe_hosted_result(id, crate::hosted::now_s() * 1000, &owner)
        .await
        .unwrap();
    assert!(reply
        .terminal
        .unwrap()
        .refusal_detail
        .unwrap()
        .starts_with("lease_mismatch: "));
    assert_eq!(
        motor.hosted_state(id).unwrap().hosted_code.as_deref(),
        Some("lease_mismatch")
    );
    motor.hosted_results.lock().unwrap().remove(&id);
    motor.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_08_expired_successful_status_probe_still_collects_hosted_terminal() {
    let h = Harness::new(Behavior::StatusTerminal).await;
    h.run_gate.send_replace(false);
    let root = cortexkit_test_support::ScratchDir::new("audit-status-settlement");
    let (motor, id) = detached(&h, &root).await;
    let reply = motor.hosted_result_status(id, u64::MAX).await;
    assert_eq!(reply.terminal.unwrap().outcome, Outcome::Exit { code: 41 });
    h.run_gate.send_replace(true);
    settled(&motor, id).await;
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        crate::BillingStatus::Available
    );
    assert!(h
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::Attach(_))));
    motor.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_10_restart_collects_missing_return_result_without_a_subscriber() {
    let h = Harness::new(Behavior::ReturnCapFailure).await;
    let root = cortexkit_test_support::ScratchDir::new("audit-return-recovery");
    let (motor, id) = detached(&h, &root).await;
    motor
        .storage
        .return_requested(id, vec!["artifact".into()])
        .unwrap();
    motor.resume_finishes().await;
    settled(&motor, id).await;
    assert!(
        motor.storage.job(id).unwrap().return_result.is_some(),
        "MAX recovery skipped the durable ReturnResult"
    );
    motor.close().await;
}

#[tokio::test]
async fn audit_12_cancel_intent_cannot_be_installed_after_durable_completion() {
    for outcome in [Outcome::Exit { code: 41 }, Outcome::OutcomeUnknown] {
        let (_root, motor, id) = fixture();
        motor.storage.cancel(id, true).unwrap();
        let mut terminal = real_terminal(id);
        terminal.outcome = outcome;
        motor.storage.hosted_terminal(id, terminal).unwrap();
        motor
            .storage
            .hosted_update(id, |state| {
                state.billing = crate::BillingStatus::Available;
                state.finish_done = true;
            })
            .unwrap();
        motor.storage.cancel(id, true).unwrap();
        assert!(!motor.pending(id), "completion must fence a late cancel intent and consume an older one in the same store update");
        motor.close().await;
    }
}

#[tokio::test]
async fn audit_12_unfinished_cancel_intent_survives_export_and_reopen() {
    let (root, motor, id) = fixture();
    let state = motor.hosted_state(id).unwrap();
    assert!(state.terminal.is_none());
    assert_eq!(state.billing, crate::BillingStatus::Pending);
    assert!(state.run_sent);
    assert!(!state.finish_done);
    assert!(state.reattach_deadline_s().unwrap() > crate::hosted::now_s());
    motor.storage.cancel(id, true).unwrap();
    assert!(motor.pending(id));
    motor.close().await;
    let exported: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("state.json")).unwrap()).unwrap();
    assert_eq!(exported["jobs"][id.to_string()]["pending_cancel"], true);
    let reopened = crate::storage::Storage::open(&root).unwrap();
    assert!(reopened.job(id).unwrap().pending_cancel);
    assert!(reopened.job(id).unwrap().hosted.unwrap().terminal.is_none());
    reopened.shutdown().unwrap();
}

#[tokio::test]
async fn audit_12_hosted_terminal_before_runner_terminal_keeps_cancel_pending() {
    let (_root, motor, id) = fixture();
    motor.storage.cancel(id, true).unwrap();
    motor
        .storage
        .hosted_update(id, |state| state.billing = crate::BillingStatus::Available)
        .unwrap();
    motor.reserve_hosted_result(id, 0);
    let (tx, _records) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    let _ = tokio::time::timeout(
        Duration::from_millis(50),
        motor.follow_inner(id, 0, None, &tx, wake, None),
    )
    .await;
    assert!(
        motor.pending(id),
        "HostedTerminal metadata consumed cancel before a real runner terminal was stored"
    );
    motor.hosted_results.lock().unwrap().remove(&id);
    motor.close().await;
}

#[tokio::test]
async fn audit_12_unknown_metadata_completion_does_not_end_unconfirmed_cancel_recovery() {
    let (_root, motor, id) = fixture();
    motor.storage.cancel(id, true).unwrap();
    motor
        .storage
        .hosted_update(id, |state| {
            state.terminal = Some(
                TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
                    .with_refusal_detail("hosted_cancel_unconfirmed: no real terminal received"),
            );
            state.finish_done = true;
            state.billing = crate::BillingStatus::Available;
        })
        .unwrap();
    let owner = motor.reserve_hosted_result(id, u64::MAX);
    let (tx, _records) = mpsc::channel(32);
    let _ = tokio::time::timeout(
        Duration::from_millis(50),
        motor.recover_recorded_unknown(id, &owner, &tx),
    )
    .await;
    assert!(
        motor.pending(id),
        "metadata ended recovery without confirming cancel or storing a real terminal"
    );
    motor.hosted_results.lock().unwrap().remove(&id);
    motor.close().await;
}

#[tokio::test]
async fn audit_12_real_terminal_keeps_cancel_pending_until_settlement_is_recorded() {
    let (_root, motor, id) = fixture();
    motor.storage.cancel(id, true).unwrap();
    motor
        .persist_hosted_terminal(id, real_terminal(id))
        .unwrap();
    assert!(
        motor.pending(id),
        "a real terminal consumed cancel before billing or finish was settled"
    );
    motor
        .storage
        .hosted_update(id, |state| state.billing = crate::BillingStatus::Available)
        .unwrap();
    motor.storage.cancel(id, true).unwrap();
    assert!(!motor.pending(id));
    motor.close().await;
}

#[tokio::test]
async fn audit_12_shutdown_fallback_cannot_turn_missing_terminal_into_cancel_completion() {
    let (_root, motor, id) = fixture();
    motor.storage.cancel(id, true).unwrap();
    motor
        .storage
        .hosted_update(id, |state| state.billing = crate::BillingStatus::Available)
        .unwrap();
    motor.hosted_cleanup.shutdown.send_replace(true);
    let (tx, _records) = mpsc::channel(32);
    motor.finish_hosted(id, false, &tx).await.unwrap();
    assert!(motor.pending(id), "shutdown synthesized a detail-free unknown and consumed unconfirmed intent as if it came from the runner");
    assert!(!motor.hosted_state(id).unwrap().has_real_terminal());
    motor.close().await;
}

#[tokio::test]
async fn audit_12_actual_unknown_terminal_is_proof_independent_of_diagnostic_text() {
    let (_root, motor, id) = fixture();
    motor.storage.cancel(id, true).unwrap();
    motor
        .storage
        .hosted_update(id, |state| state.billing = crate::BillingStatus::Available)
        .unwrap();
    motor
        .storage
        .hosted_terminal(
            id,
            TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
                .with_refusal_detail("runner supplied diagnostic"),
        )
        .unwrap();
    assert!(motor
        .storage
        .job(id)
        .unwrap()
        .terminal_observed_at_ms
        .is_some());
    assert!(!motor.pending(id), "an actually observed terminal with settled usage must not be rejected because it has diagnostic text");
    motor.close().await;
}

#[tokio::test]
async fn audit_13_expired_unknown_cancel_recovery_reaches_a_terminal_state() {
    let (_root, motor, id) = fixture();
    motor
        .storage
        .hosted_update(id, |state| {
            state.retry_iat = Some(0);
            state.broker_url = "http://127.0.0.1:1".into();
            state.terminal = Some(
                TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
                    .with_refusal_detail("hosted_cancel_unconfirmed: connection lost"),
            );
        })
        .unwrap();
    motor.storage.cancel(id, true).unwrap();
    let owner = motor.reserve_hosted_result(id, u64::MAX);
    let (tx, _records) = mpsc::channel(32);
    assert!(tokio::time::timeout(
        Duration::from_secs(1),
        motor.recover_recorded_unknown(id, &owner, &tx)
    )
    .await
    .expect("expired unknown cancellation retried forever")
    .unwrap());
    assert!(motor.storage.job(id).unwrap().hosted_complete());
    motor.hosted_results.lock().unwrap().remove(&id);
    motor.close().await;
}

#[tokio::test]
async fn audit_13_legacy_max_or_missing_deadline_unknown_retires_at_mint_horizon() {
    for explicit_max in [true, false] {
        let (root, motor, template) = fixture();
        let config = motor.config.clone();
        let minted = crate::hosted::now_s().saturating_sub(49 * 60 * 60);
        let id = Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, minted, 0));
        motor.storage.create(id, 60, None, None, false).unwrap();
        let mut state = motor.hosted_state(template).unwrap();
        state.retry_iat = None;
        state.max_runtime_s = None;
        state.broker_url = "http://127.0.0.1:1".into();
        state.terminal = Some(TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0));
        motor.storage.hosted_route(id, state).unwrap();
        if explicit_max {
            motor.storage.health_deadline(id, None).unwrap();
        }
        motor.storage.cancel(id, true).unwrap();
        motor.close().await;
        let motor = Motor::with_connector(
            config,
            &root,
            Arc::new(crate::hosted::tests::harness::NoSsh(
                std::sync::atomic::AtomicUsize::new(0),
            )),
        )
        .unwrap();
        assert!(motor.pending(id));
        let owner = motor.reserve_hosted_result(id, u64::MAX);
        let (tx, _records) = mpsc::channel(32);
        assert!(tokio::time::timeout(
            Duration::from_secs(1),
            motor.recover_recorded_unknown(id, &owner, &tx)
        )
        .await
        .expect("legacy cancellation recovery ignored its finite mint horizon")
        .unwrap());
        assert!(!motor.pending(id));
        assert!(motor.storage.job(id).unwrap().hosted_complete());
        assert!(!motor.storage.unsettled_hosted().contains(&id));
        motor.hosted_results.lock().unwrap().remove(&id);
        motor.close().await;
    }
}

#[tokio::test]
async fn audit_13_undated_legacy_unknown_gets_one_durable_recovery_horizon() {
    let (root, motor, template) = fixture();
    let config = motor.config.clone();
    let id = Uuid::from_u128(1);
    assert!(id.get_timestamp().is_none());
    motor.storage.create(id, 60, None, None, false).unwrap();
    let mut state = motor.hosted_state(template).unwrap();
    state.retry_iat = None;
    state.max_runtime_s = None;
    state.terminal = Some(TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0));
    motor.storage.hosted_route(id, state).unwrap();
    motor.storage.health_deadline(id, None).unwrap();
    motor.storage.cancel(id, true).unwrap();
    let first = motor.hosted_result_deadline(id).unwrap();
    assert_ne!(
        first,
        u64::MAX,
        "a legacy ID without mint time left cancellation recovery unbounded"
    );
    let anchor = motor
        .storage
        .job(id)
        .unwrap()
        .terminal_observed_at_ms
        .unwrap();
    assert_eq!(first, anchor + 48 * 60 * 60 * 1000);
    motor.close().await;
    let motor = Motor::with_connector(
        config,
        &root,
        Arc::new(crate::hosted::tests::harness::NoSsh(
            std::sync::atomic::AtomicUsize::new(0),
        )),
    )
    .unwrap();
    assert!(motor.pending(id));
    assert!(!motor.hosted_state(id).unwrap().has_real_terminal());
    assert_eq!(
        motor.hosted_result_deadline(id).unwrap(),
        first,
        "restart renewed the legacy recovery horizon"
    );
    motor.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_14a_pending_settlement_attach_preserves_requested_output_cursor() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    h.run_gate.send_replace(false);
    let root = cortexkit_test_support::ScratchDir::new("audit-pending-attach-cursor");
    let (motor, id) = detached(&h, &root).await;
    motor
        .storage
        .hosted_update(id, |state| state.terminal = Some(real_terminal(id)))
        .unwrap();
    let mut caller = motor
        .attach(motor_protocol::AttachRequest::new(id, 9))
        .unwrap();
    let record = tokio::time::timeout(Duration::from_secs(2), caller.records.recv())
        .await
        .expect("pending-settlement attach discarded its replay cursor")
        .unwrap()
        .unwrap();
    assert!(matches!(record, StreamRecord::Output(o) if o.seq == 9));
    drop(caller);
    h.run_gate.send_replace(true);
    settled(&motor, id).await;
    motor.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_14c_status_during_shutdown_cannot_wait_for_an_unelected_owner() {
    let (_root, motor, id) = fixture();
    motor.hosted_cleanup.shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(1), motor.job_status(id))
        .await
        .expect("status hung after shutdown")
        .unwrap();
    motor.close().await;
}

#[tokio::test]
async fn audit_04_broker_refusal_preserves_a_real_terminal_without_cancelling() {
    let h = Harness::new(Behavior::BrokerRefuseAttach).await;
    let root = cortexkit_test_support::ScratchDir::new("audit-broker-refused-terminal");
    let (motor, id) = detached(&h, &root).await;
    motor
        .storage
        .hosted_update(id, |state| state.terminal = Some(real_terminal(id)))
        .unwrap();
    let owner = motor.reserve_hosted_result(id, u64::MAX);
    let (tx, mut records) = mpsc::channel(32);
    motor
        .follow(id, u64::MAX, None, &tx, owner.wake.subscribe())
        .await
        .unwrap();
    assert!(
        matches!(records.recv().await.unwrap().unwrap(), StreamRecord::Terminal(t) if t.outcome == Outcome::Exit { code: 41 })
    );
    assert_eq!(
        motor.hosted_state(id).unwrap().terminal.unwrap().outcome,
        Outcome::Exit { code: 41 }
    );
    assert!(!h
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::Cancel(_))));
    motor.hosted_results.lock().unwrap().remove(&id);
    motor.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_11_shared_cancel_watch_cannot_block_result_collection_forever() {
    let h = Harness::new(Behavior::Normal).await;
    let root = cortexkit_test_support::ScratchDir::new("audit-held-cancel-watch");
    let (motor, id) = detached(&h, &root).await;
    let (_held, delivery) = watch::channel(None);
    motor.hosted_cancels.lock().unwrap().insert(id, delivery);
    let owner = motor.ensure_hosted_result(id, None);
    let mut stopped = owner.stopped.subscribe();
    tokio::time::timeout(Duration::from_secs(7), stopped.wait_for(|done| *done))
        .await
        .expect("a held cancel watch prevented collection")
        .unwrap();
    assert_eq!(
        motor.hosted_state(id).unwrap().terminal.unwrap().outcome,
        Outcome::Exit { code: 23 }
    );
    motor.close().await;
}

struct SilentStatus;

struct TerminalBeforeUsage(tokio::sync::watch::Receiver<bool>);
impl crate::Connector for TerminalBeforeUsage {
    fn connect(
        &self,
        id: Uuid,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = io::Result<Box<dyn crate::transport::Duplex>>>
                + Send
                + '_,
        >,
    > {
        let mut release = self.0.clone();
        Box::pin(async move {
            let (client, mut server) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let _ = motor_protocol::read_frame_async(&mut server).await;
                motor_protocol::write_frame_async(
                    &mut server,
                    &Frame::Hello {
                        protocol_version: motor_protocol::PROTOCOL_VERSION,
                        started: true,
                        gate_jobs: true,
                        platforms: vec!["linux".into()],
                        default_branch_tip: false,
                        return_paths: false,
                        return_chunked_max_bytes: None,
                        return_chunked: false,
                        sibling_paths: false,
                    },
                )
                .await
                .unwrap();
                motor_protocol::write_frame_async(&mut server, &Frame::Terminal(real_terminal(id)))
                    .await
                    .unwrap();
                // A subsequent frame makes the terminal handler's completion
                // observable while the usage record remains withheld.
                motor_protocol::write_frame_async(
                    &mut server,
                    &Frame::Output(motor_protocol::Output::new(
                        1,
                        motor_protocol::OutputStream::Stdout,
                        motor_protocol::BytePayload(b"barrier".to_vec()),
                    )),
                )
                .await
                .unwrap();
                release.wait_for(|ready| *ready).await.unwrap();
                let _ = motor_protocol::write_frame_async(
                    &mut server,
                    &Frame::HostedTerminal(motor_protocol::HostedTerminal {
                        job_id: id,
                        placement: "first_party/first-party-audit".into(),
                        billed_vcpu_ms: 1,
                        max_runtime_s: 60,
                    }),
                )
                .await;
            });
            Ok(Box::new(client) as Box<dyn crate::transport::Duplex>)
        })
    }
}

#[tokio::test]
async fn audit_12_streamed_terminal_waits_for_usage_before_consuming_cancel() {
    let (_root, motor, id) = fixture();
    motor.storage.cancel(id, true).unwrap();
    let (release, ready) = tokio::sync::watch::channel(false);
    let channel = Channel::open(&TerminalBeforeUsage(ready), id)
        .await
        .unwrap();
    let (tx, mut records) = mpsc::channel(32);
    let follower = {
        let motor = motor.clone();
        tokio::spawn(async move {
            motor
                .follow(id, 0, Some(channel), &tx, watch::channel(false).1)
                .await
        })
    };
    let record = tokio::time::timeout(Duration::from_secs(3), records.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(record, StreamRecord::Output(_)));
    assert!(
        motor.pending(id),
        "the streamed terminal consumed cancel before the withheld usage record arrived"
    );
    release.send_replace(true);
    follower.await.unwrap().unwrap();
    assert!(!motor.pending(id));
    motor.close().await;
}

#[tokio::test(start_paused = true)]
async fn first_party_result_is_published_while_same_owner_collects_late_usage() {
    let (_root, motor, id) = fixture();
    let (release, ready) = tokio::sync::watch::channel(false);
    let channel = Channel::open(&TerminalBeforeUsage(ready), id)
        .await
        .unwrap();
    let owner = motor.reserve_hosted_result(id, 0);
    let (mut caller, caller_tx, _) = motor.stream(id);
    motor.subscribe_hosted_result(id, owner.clone(), 0, false, caller_tx);
    let (tx, records) = mpsc::channel(32);
    let producer = {
        let motor = motor.clone();
        let owner = owner.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            motor
                .follow(id, 0, Some(channel), &tx, owner.wake.subscribe())
                .await
                .unwrap();
        })
    };
    motor.collect_hosted_result(id, owner.clone(), tx, records, producer);
    let terminal = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if let StreamRecord::Terminal(terminal) = caller.records.recv().await.unwrap().unwrap()
            {
                break terminal;
            }
        }
    })
    .await
    .expect("real terminal publication must not wait for billing");
    assert_eq!(terminal, real_terminal(id));
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        crate::BillingStatus::Pending
    );
    assert!(!*owner.stopped.borrow());
    assert!(
        owner.state.lock().unwrap().live,
        "publication must retain the live usage connection"
    );
    drop(caller);
    release.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), owner.wait_stopped())
        .await
        .unwrap();
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        crate::BillingStatus::Available
    );
    assert_eq!(motor.hosted_state(id).unwrap().billed_vcpu_ms, Some(1));
    motor.close().await;
}

#[tokio::test]
async fn late_cancel_ack_cannot_replace_an_already_observed_exit_finish() {
    let (_root, motor, id) = fixture();
    motor
        .storage
        .hosted_update(id, |state| {
            state.placement.as_mut().unwrap().host = "boat:test".into();
        })
        .unwrap();
    motor
        .persist_hosted_terminal(id, real_terminal(id))
        .unwrap();
    motor
        .storage
        .cancel_acknowledged_with_owner(id, true)
        .unwrap();
    let state = motor.hosted_state(id).unwrap();
    assert_eq!(state.terminal.unwrap(), real_terminal(id));
    assert!(
        state.pending_finish.is_none(),
        "a late acknowledgment must not install a cancelled finish over the real exit"
    );
    motor.close().await;
}

impl crate::Connector for SilentStatus {
    fn connect(
        &self,
        _: Uuid,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = io::Result<Box<dyn crate::transport::Duplex>>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async {
            let (client, mut server) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let _ = motor_protocol::read_frame_async(&mut server).await;
                motor_protocol::write_frame_async(
                    &mut server,
                    &Frame::Hello {
                        protocol_version: motor_protocol::PROTOCOL_VERSION,
                        started: true,
                        gate_jobs: true,
                        platforms: vec!["linux".into()],
                        default_branch_tip: false,
                        return_paths: false,
                        return_chunked_max_bytes: None,
                        return_chunked: false,
                        sibling_paths: false,
                    },
                )
                .await
                .unwrap();
                assert!(matches!(
                    motor_protocol::read_frame_async(&mut server).await.unwrap(),
                    Some(Frame::JobStatus { .. })
                ));
                let _keep_open = server;
                std::future::pending::<()>().await;
            });
            Ok(Box::new(client) as Box<dyn crate::transport::Duplex>)
        })
    }
}

#[tokio::test]
async fn audit_14c_explicit_status_times_out_a_silent_runner() {
    let root = cortexkit_test_support::ScratchDir::new("audit-silent-status");
    let motor = Motor::with_connector(
        crate::Config {
            runner_instance: "test".into(),
            ..crate::Config::default()
        },
        &root,
        Arc::new(SilentStatus),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor
        .storage
        .create(id, 60, None, Some("test".into()), false)
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(7), motor.job_status(id))
        .await
        .expect("explicit status waited forever for the runner");
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    motor.close().await;
}
