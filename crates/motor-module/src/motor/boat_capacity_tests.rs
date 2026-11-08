use super::*;
use crate::hosted::{
    boat_tests::*,
    tests::harness::{Harness, NoSsh},
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn stored_fresh_boat_booking_uses_lease_id_for_status_cancel_attach_and_finish() {
    assert_fresh_boat_cancel_result(Some(Outcome::Exit { code: 23 }), false).await;
}

#[tokio::test]
async fn fresh_boat_cancelled_terminal_finishes_as_cancelled() {
    assert_fresh_boat_cancel_result(Some(Outcome::Cancelled), false).await;
}

#[tokio::test]
async fn fresh_boat_cancel_ack_without_terminal_settles_after_bounded_wait() {
    assert_fresh_boat_cancel_result(None, false).await;
}

#[tokio::test]
async fn fresh_boat_cancel_ack_when_replay_disconnects_still_has_a_bounded_wait() {
    assert_fresh_boat_cancel_result(None, true).await;
}

async fn assert_fresh_boat_cancel_result(real_outcome: Option<Outcome>, disconnect_replay: bool) {
    let boat = Boat::new(0, false).await;
    boat.attach_gate.send_replace(false);
    if let Some(outcome) = &real_outcome {
        *boat.attach_outcome.lock().unwrap() = outcome.clone();
    }
    boat.connect_host_full.store(1, Ordering::SeqCst);
    boat.fresh_no_capacity.store(1, Ordering::SeqCst);
    let (scratch, repo) = committed_repo();
    let root = scratch.0.join("state");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::now_v7();
    storage.create(id, 60, None, None, false).unwrap();
    let route = crate::HostedJobState {
        fleet_routed: true,
        ..crate::HostedJobState::new(&boat.config)
    };
    storage.hosted_route(id, route.clone()).unwrap();
    let (channel, lease) = crate::hosted::Client::new(route)
        .run(id, &Harness::request(&repo), &Arc::new(storage))
        .await
        .unwrap();
    drop(channel);
    let storage = Storage::open(&root).unwrap();
    let booking = Uuid::parse_str(&lease.job_id).unwrap();
    assert_ne!(booking, id);
    assert_eq!(
        boat.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "/v1/leases")
            .count(),
        3
    );
    storage.run_sent(id, true).unwrap();
    storage
        .hosted_update(id, |state| {
            state.started_at_ms = Some(
                crate::hosted::now_s()
                    .saturating_mul(1000)
                    .saturating_sub(2000),
            );
        })
        .unwrap();
    storage.shutdown().unwrap();
    drop(storage);
    let direct = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(boat.config.clone(), &root, direct.clone()).unwrap();
    assert_eq!(motor.storage.runner_id(id), booking);
    assert!(motor.storage.job(id).unwrap().build_server_job_id.is_none());
    assert!(motor.hosted_state(id).is_some());
    let status = motor.job_status(id).await.unwrap();
    assert_eq!(status.job_id, id);
    assert_eq!(status.state, "running");
    tokio::time::timeout(Duration::from_secs(5), async {
        while boat.attaches.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the result owner must hold Attach before cancellation");
    motor.send_cancel(id).await.unwrap();
    boat.disconnect_replay
        .store(disconnect_replay, Ordering::SeqCst);
    if real_outcome.is_some() {
        boat.attach_gate.send_replace(true);
    }
    let stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
    let terminal = if real_outcome.is_none() {
        let mut stream = stream;
        tokio::time::timeout(Duration::from_secs(40), async {
            while let Some(record) = stream.records.recv().await {
                if let StreamRecord::Terminal(terminal) = record.unwrap() {
                    return terminal;
                }
            }
            panic!("cancel collection ended without its unknown summary");
        })
        .await
        .unwrap_or_else(|_| {
            let state = motor.hosted_state(id).unwrap();
            panic!("cancel acknowledgment exceeded the terminal wait: terminal={:?}, pending_finish={:?}, finish_done={}, billing={:?}, attaches={}",
                state.terminal, state.pending_finish, state.finish_done, state.billing, boat.attaches.load(Ordering::SeqCst));
        })
    } else {
        records(stream).await.1
    };
    assert_eq!(terminal.job_id, id);
    assert_eq!(
        terminal.outcome,
        real_outcome.clone().unwrap_or(Outcome::OutcomeUnknown)
    );
    assert_eq!(direct.0.load(Ordering::SeqCst), 0);
    let calls = boat.requests.lock().unwrap().clone();
    assert!(calls
        .iter()
        .filter(|(p, _)| p.ends_with("/reattach"))
        .all(|(p, _)| p == &format!("/v1/leases/{booking}/reattach")));
    let expected_finish = if matches!(real_outcome, Some(Outcome::Exit { .. })) {
        "exit"
    } else {
        "cancelled"
    };
    assert!(calls
        .iter()
        .any(|(p, b)| p == &format!("/v1/leases/{booking}/finish")
            && b["outcome"] == expected_finish));
    if real_outcome.is_none() {
        assert!(
            calls
                .iter()
                .any(|(p, body)| p == &format!("/v1/leases/{booking}/finish")
                    && body["run_ms"]
                        .as_u64()
                        .is_some_and(|runtime| runtime >= 2000)),
            "{calls:?}"
        );
        assert!(terminal.wall_ms >= 2000);
    }
    assert!(boat
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Cancel(c) if c.job_id == booking)));
    assert!(boat
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::Attach(attach) if attach.job_id == booking)));
    motor.close().await;
}

#[tokio::test]
async fn boat_host_full_finishes_lost_then_runs_a_fresh_booking_for_the_same_caller() {
    let boat = Boat::new(0, false).await;
    boat.connect_host_full.store(1, Ordering::SeqCst);
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        Config {
            ..boat.config.clone()
        },
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let mut request = Harness::request(&repo);
    request.submission_key = Some("boat-host-full".into());
    let mut stream = motor
        .submit_agent(request.clone(), None, "test:boat")
        .await
        .unwrap();
    let id = stream.job_id;
    let first = stream.records.recv().await.unwrap().unwrap();
    assert!(
        matches!(&first, StreamRecord::Accepted(a) if a.job_id == id),
        "{first:?}"
    );
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert_eq!(terminal.job_id, id);
    assert_eq!(boat.runs.load(Ordering::SeqCst), 1);
    let calls = boat.requests.lock().unwrap().clone();
    let leases: Vec<_> = calls.iter().filter(|(p, _)| p == "/v1/leases").collect();
    assert_eq!(leases.len(), 2);
    assert_eq!(leases[0].1["job_id"], id.to_string());
    let next_id = leases[1].1["job_id"].as_str().unwrap();
    assert_ne!(next_id, id.to_string());
    let first_finish = calls
        .iter()
        .position(|(p, _)| p == &format!("/v1/leases/{id}/finish"))
        .unwrap();
    assert_eq!(
        calls[first_finish].1,
        serde_json::json!({"outcome":"lost","run_ms":0})
    );
    let next_lease = calls
        .iter()
        .position(|(p, b)| p == "/v1/leases" && b["job_id"] == next_id)
        .unwrap();
    assert!(first_finish < next_lease);
    assert!(calls
        .iter()
        .any(|(p, b)| p == &format!("/v1/leases/{next_id}/finish") && b["outcome"] == "exit"));
    assert!(!calls
        .iter()
        .any(|(p, _)| p.ends_with("/release") || p.ends_with("/replace")));
    let run = boat
        .frames
        .lock()
        .unwrap()
        .iter()
        .find_map(|f| match f {
            Frame::Run(run) => Some(run.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(run.job_id.to_string(), next_id);
    assert_eq!(run.tree_hash, "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    let replay = motor
        .submit_agent(request, None, "test:boat")
        .await
        .unwrap();
    assert_eq!(replay.job_id, id);
    assert_eq!(records(replay).await.1.job_id, id);
    assert_eq!(boat.runs.load(Ordering::SeqCst), 1);
    let job = motor.storage.job(id).unwrap();
    assert!(job.build_server_job_id.is_none());
    assert!(job.hosted.unwrap().finish_done);
}

#[tokio::test]
async fn boat_host_full_stops_after_three_grants_or_the_queue_budget() {
    for budget in [1, 60] {
        let boat = Boat::new(0, false).await;
        boat.connect_host_full.store(10, Ordering::SeqCst);
        let (scratch, repo) = committed_repo();
        let motor = Motor::with_connector(
            Config {
                ..boat.config.clone()
            },
            &scratch.0.join("state"),
            Arc::new(NoSsh(AtomicUsize::new(0))),
        )
        .unwrap();
        let mut request = Harness::request(&repo);
        request.queue_wait_limit_s = Some(budget);
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::RunnerFull
            }
        );
        assert_eq!(terminal.job_id, id);
        assert_eq!(
            terminal.refusal_detail.as_deref(),
            Some("hosted_host_full; spill_attempted=hosted_host_full")
        );
        assert_eq!(boat.runs.load(Ordering::SeqCst), 0);
        let calls = boat.requests.lock().unwrap().clone();
        let grants: Vec<_> = calls.iter().filter(|(p, _)| p == "/v1/leases").collect();
        assert_eq!(grants.len(), if budget == 1 { 1 } else { 3 });
        for (_, grant) in grants {
            let booking = grant["job_id"].as_str().unwrap();
            let finish: Vec<_> = calls
                .iter()
                .filter(|(p, _)| p == &format!("/v1/leases/{booking}/finish"))
                .collect();
            assert_eq!(finish.len(), 1);
            assert_eq!(
                finish[0].1,
                serde_json::json!({"outcome":"lost","run_ms":0})
            );
        }
    }
}
