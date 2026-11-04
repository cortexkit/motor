use super::*;
use crate::hosted::{
    account::tests::{token, FakeVault},
    tests::harness::{Behavior, Harness},
};
use std::sync::atomic::Ordering;

fn assert_broker_released(
    broker: &Arc<std::sync::Mutex<motor_lease::fake::FakeBroker>>,
    body: &LeaseRequest,
) {
    let path = format!("/v1/leases/{}/reattach", body.job_id);
    let body = serde_json::to_vec(&motor_lease::ReattachRequest {
        requester_jwk: body.requester_jwk.clone(),
    })
    .unwrap();
    let reply = broker.lock().unwrap().handle(
        motor_lease::fake::BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path: &path,
            bearer: Some("hosted-test-bearer"),
            host_headers: None,
            body: &body,
        },
        crate::hosted::now_s() * 1000,
    );
    assert_eq!(reply.status, 409);
    assert_eq!(reply.body["error"], "job_released");
}

fn config(h: &Harness) -> Config {
    Config {
        hosted_repositories_placement_hint: Some(Placement {
            host: "configured-canary-host".into(),
            vm: Some("test-vm".into()),
        }),
        broker_token_source: crate::config::BrokerTokenSource::CkAccount,
        ..h.config()
    }
}

fn request(mode: Mode) -> Request {
    Request {
        mode,
        host: "test-host".into(),
    }
}

fn tokens() -> Arc<AccountTokens> {
    FakeVault::new(vec![Ok(token("hosted-test-bearer"))]).client()
}

fn assert_released(h: &Harness, id: Uuid) {
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.ends_with("/release"))
            .count(),
        1,
        "every attempted booking needs exactly one explicit cleanup release"
    );
    let body: LeaseRequest = serde_json::from_slice(&h.lease_bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body.job_id, id.to_string());
    assert_broker_released(&h.broker, &body);
    assert_eq!(
        h.tokens.load(Ordering::SeqCst),
        0,
        "must never connect to a host"
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn release_once_without_connecting_uses_probe_request_and_redacts_tokens() {
    let h = Harness::new(Behavior::ConnectFailure).await;
    let report = run(config(&h), Some(tokens()), request(Mode::Release))
        .await
        .unwrap();
    assert_eq!(report.status, "ok");
    assert_eq!(report.placement.as_ref().unwrap().host, "test-host");
    assert!(report.replacement_placement.is_none());
    assert_eq!(report.replace_pending, 0);
    assert_eq!(
        report.replies.lease,
        vec![Reply {
            status: 201,
            error: None
        }]
    );
    assert_eq!(
        report.replies.release,
        vec![Reply {
            status: 202,
            error: None
        }]
    );
    assert_released(&h, report.job_id);
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let body: LeaseRequest = serde_json::from_slice(&bodies[0]).unwrap();
    assert_eq!(body.job_id, report.job_id.to_string());
    assert_eq!(body.vcpus, 2);
    assert_eq!(body.platform, "linux-x64");
    assert_eq!(body.timeout_s, 10);
    assert_eq!(
        body.placement_hint,
        Some(Placement {
            host: "test-host".into(),
            vm: Some("test-vm".into())
        })
    );
    assert!(!body.net);
    assert_eq!(body.allow_spill, Some(false));
    assert!(body.avoid_host.is_none());
    assert_eq!(
        body.workspace_key_hash,
        "4LA6aAC6Fo6VVjGrc3JmzF4CH_92gwPYSPUUNY69pz8"
    );
    let output = serde_json::to_value(&report).unwrap();
    for phase in ["lease", "replace", "release"] {
        for reply in output["replies"][phase].as_array().unwrap() {
            let keys: Vec<_> = reply
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, ["error", "status"]);
        }
    }
    let output = output.to_string();
    for secret in [
        "hosted-test-bearer",
        "run_token",
        "attach_token",
        "requester_jwk",
        "endpoint",
    ] {
        assert!(!output.contains(secret), "leaked {secret}");
    }
}

#[tokio::test]
async fn replace_waits_through_pending_then_releases_replacement_without_connecting() {
    let first = Harness::new(Behavior::ConnectFailure).await;
    let second = Harness::with_host(Behavior::ConnectFailure, "second-host").await;
    first.replace_replies.lock().unwrap().extend([
        (503, serde_json::json!({"error":"replace_pending"}), "1"),
        (503, serde_json::json!({"error":"replace_pending"}), "1"),
    ]);
    first
        .replacement_hosts
        .lock()
        .unwrap()
        .push_back(second.broker.clone());
    let start = tokio::time::Instant::now();
    let report = run(config(&first), Some(tokens()), request(Mode::Replace))
        .await
        .unwrap();
    assert_eq!(report.status, "ok");
    assert!(
        start.elapsed() >= Duration::from_secs(2),
        "Retry-After must be respected"
    );
    assert_eq!(report.placement.as_ref().unwrap().host, "test-host");
    assert_eq!(
        report.replacement_placement.as_ref().unwrap().host,
        "second-host"
    );
    assert_eq!(report.replace_pending, 2);
    assert_eq!(
        report.replies.replace,
        vec![
            Reply {
                status: 503,
                error: Some("replace_pending".into())
            },
            Reply {
                status: 503,
                error: Some("replace_pending".into())
            },
            Reply {
                status: 201,
                error: None
            },
        ]
    );
    assert_eq!(
        report.replies.release,
        vec![Reply {
            status: 202,
            error: None
        }]
    );
    assert_eq!(first.lease_bodies.lock().unwrap().len(), 1);
    assert_eq!(
        first.requests.lock().unwrap().last().unwrap(),
        &format!("/v1/leases/{}/release", report.job_id)
    );
    let body: LeaseRequest =
        serde_json::from_slice(&first.lease_bodies.lock().unwrap()[0]).unwrap();
    assert_broker_released(&second.broker, &body);
    assert_eq!(
        first.tokens.load(Ordering::SeqCst) + second.tokens.load(Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn error_after_booking_still_releases_original() {
    let h = Harness::new(Behavior::ConnectFailure).await;
    h.replace_replies.lock().unwrap().push_back((
        500,
        serde_json::json!({"error":"broker_failed", "token":"private-response-text"}),
        "",
    ));
    let report = run(config(&h), Some(tokens()), request(Mode::Replace))
        .await
        .unwrap();
    assert_eq!(report.status, "refused");
    assert!(report.error.is_some());
    assert_eq!(
        report.replies.replace,
        vec![Reply {
            status: 500,
            error: Some("broker_failed".into())
        }]
    );
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("private-response-text"));
    assert_released(&h, report.job_id);
}

#[tokio::test]
async fn lost_lease_reply_still_releases_booking() {
    let h = Harness::new(Behavior::ConnectFailure).await;
    h.drop_lease.store(true, Ordering::SeqCst);
    let report = run(config(&h), Some(tokens()), request(Mode::Release))
        .await
        .unwrap();
    assert_eq!(report.status, "refused");
    assert!(report.placement.is_none());
    assert_released(&h, report.job_id);
}

#[tokio::test]
async fn pending_deadline_still_releases_original() {
    let h = Harness::new(Behavior::ConnectFailure).await;
    h.replace_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"replace_pending"}),
        "180",
    ));
    let report = probe(
        config(&h),
        Some(tokens()),
        request(Mode::Replace),
        Duration::from_millis(500),
    )
    .await;
    assert_eq!(report.status, "refused");
    assert_eq!(report.replace_pending, 1);
    assert!(report.replacement_placement.is_none());
    assert_released(&h, report.job_id);
}

#[tokio::test]
async fn invalid_vm_refuses_without_booking() {
    let h = Harness::new(Behavior::ConnectFailure).await;
    let mut config = config(&h);
    config.hosted_repositories_placement_hint = None;
    let report = run(config, Some(tokens()), request(Mode::Release))
        .await
        .unwrap();
    assert_eq!(report.status, "refused");
    assert!(h.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn disconnected_operator_does_not_cancel_cleanup() {
    let h = Harness::new(Behavior::ConnectFailure).await;
    h.replace_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"replace_pending"}),
        "1",
    ));
    let caller = tokio::spawn(run(config(&h), Some(tokens()), request(Mode::Replace)));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !h
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.ends_with("/replace"))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let released = h
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.ends_with("/release"));
            if released {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let body: LeaseRequest = serde_json::from_slice(&h.lease_bodies.lock().unwrap()[0]).unwrap();
    assert_released(&h, Uuid::parse_str(&body.job_id).unwrap());
}
