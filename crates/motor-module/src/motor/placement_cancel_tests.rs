use super::repository_routing_tests::{request, until, HeldClock};
use super::*;
use crate::hosted::{
    boat_tests::committed_repo,
    tests::harness::{Behavior, Harness, NoSsh},
};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn terminal(mut stream: JobStream) -> TerminalRecord {
    while let Some(record) = stream.records.recv().await {
        match record.unwrap() {
            StreamRecord::Terminal(terminal) => return terminal,
            StreamRecord::Accepted(_) => panic!("a placement-cancelled job must not be accepted"),
            _ => {}
        }
    }
    panic!("missing terminal");
}

fn motor(h: &Harness, root: &Path, repo: &Path) -> Motor {
    Motor::with_connector(
        Config {
            hosted_repositories: vec![repo.to_str().unwrap().into()],
            ..h.config()
        },
        root,
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap()
}

async fn assert_cancelled(motor: &Motor, h: &Harness, id: Uuid, stream: JobStream) {
    let task = tokio::spawn(terminal(stream));
    until(
        "placement cancellation terminal without advancing the clock",
        || task.is_finished(),
    )
    .await;
    let terminal = task.await.unwrap();
    assert_eq!(terminal.outcome, Outcome::Cancelled);
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert!(!motor.hosted_state(id).unwrap().run_sent);
    until("release acknowledged", || {
        motor.hosted_state(id).unwrap().finish_done
    })
    .await;
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.ends_with("/release"))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_during_capacity_retry_stops_without_waiting_for_retry() {
    let _clock = HeldClock::new();
    let logs = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(logs.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"no_capacity"}),
        "30",
    ));
    let (scratch, repo) = committed_repo();
    let motor = motor(&h, &scratch.0.join("state"), &repo);
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    until("capacity retry sleep", || {
        logs.text().contains("hosted broker retry wait")
    })
    .await;
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    assert_cancelled(&motor, &h, id, stream).await;
    assert_eq!(h.lease_bodies.lock().unwrap().len(), 1);
    motor.close().await;
}

async fn cancel_in_flight(grant: bool) {
    let _clock = HeldClock::new();
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_reply_gate.send_replace(false);
    if !grant {
        h.lease_replies.lock().unwrap().push_back((
            202,
            serde_json::json!({"status":"provisioning", "retry_after_ms":30000}),
            "",
        ));
    }
    let (scratch, repo) = committed_repo();
    let motor = motor(&h, &scratch.0.join("state"), &repo);
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    until("broker decision before its reply", || {
        h.timeline.snapshot().contains("lease reply ready")
    })
    .await;
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    tokio::task::yield_now().await;
    assert!(
        motor.hosted_state(id).unwrap().terminal.is_none(),
        "the reply must be drained before settlement"
    );
    h.lease_reply_gate.send_replace(true);
    assert_cancelled(&motor, &h, id, stream).await;
    assert_eq!(motor.hosted_state(id).unwrap().placement.is_some(), grant);
    assert_eq!(h.lease_bodies.lock().unwrap().len(), 1);
    motor.close().await;
}

#[tokio::test(start_paused = true)]
async fn cancel_with_lease_request_in_flight_drains_and_releases_grant() {
    cancel_in_flight(true).await;
}

#[tokio::test(start_paused = true)]
async fn cancel_with_provisioning_request_in_flight_does_not_retry() {
    cancel_in_flight(false).await;
}

async fn cancel_after_grant(replacement_retry: bool) {
    let _clock = HeldClock::new();
    let logs = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(logs.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    h.host_refusals.lock().unwrap().push_back("host_full");
    h.replace_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error": if replacement_retry { "replace_pending" } else { "no_capacity" }}),
        "30",
    ));
    let (scratch, repo) = committed_repo();
    let motor = motor(&h, &scratch.0.join("state"), &repo);
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    until("replacement retry with a booked lease", || {
        logs.text().contains("hosted broker retry wait")
    })
    .await;
    assert!(motor.hosted_state(id).unwrap().placement.is_some());
    assert_eq!(
        h.requests.lock().unwrap().as_slice(),
        ["/v1/leases", &format!("/v1/leases/{id}/replace")]
    );
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    assert_cancelled(&motor, &h, id, stream).await;
    motor.close().await;
}

#[tokio::test(start_paused = true)]
async fn cancel_after_grant_before_run_releases_the_booked_lease() {
    cancel_after_grant(false).await;
}

#[tokio::test(start_paused = true)]
async fn cancel_during_replacement_retry_releases_the_booked_lease() {
    cancel_after_grant(true).await;
}
