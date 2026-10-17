use super::common::*;
use ed25519_dalek::SigningKey;
use motor_lease::fake::{BrokerRequest, FakeBroker};
use motor_lease::*;
use serde_json::json;

#[test]
fn fake_is_forbidden_for_main_and_requires_worker_bearer() {
    assert!(FakeBroker::new("main", config()).is_err());
    assert!(FakeBroker::new("", config()).is_err());
    let mut b = broker();
    let body = serde_json::to_vec(&lease_request()).unwrap();
    for path in ["/v1/leases", "/v1/leases/test-job/reattach"] {
        let reply = b.handle(
            BrokerRequest {
                method: "POST",
                path,
                bearer: Some("wrong"),
                host_headers: None,
                body: &body,
            },
            NOW,
        );
        assert_eq!(reply.status, 401);
    }
    let jwks = b.handle(
        BrokerRequest {
            method: "GET",
            path: "/.well-known/jwks.json",
            bearer: None,
            host_headers: None,
            body: &[],
        },
        NOW,
    );
    assert_eq!(jwks.status, 200);
    assert_eq!(jwks.body["keys"].as_array().unwrap().len(), 2);
}

#[test]
fn reservation_floor_fresh_tokens_before_heartbeat_and_while_reserved() {
    let mut b = broker();
    let mut req = lease_request();
    let first = issued(&mut b, &req, NOW);
    assert_eq!(first.reserved_vcpu_ms, 20000);
    let first_claims = verified(&b, &first, NOW);
    assert_eq!(first_claims.purpose, Purpose::Run);
    req.requester_jwk = RequesterJwk::from_key(&SigningKey::from_bytes(&[9; 32]).verifying_key());
    let second = issued(&mut b, &req, NOW);
    assert_eq!(second.lease_id, first.lease_id);
    assert_eq!(second.placement, first.placement);
    assert_eq!(second.reserved_vcpu_ms, first.reserved_vcpu_ms);
    assert_ne!(second.job_token, first.job_token);
    let second_claims = verified(&b, &second, NOW);
    assert!(second_claims.iat > first_claims.iat);
    assert_eq!(
        second_claims.cnf.jkt,
        req.requester_jwk.thumbprint().unwrap()
    );
    assert_eq!(
        host_post(
            &mut b,
            "heartbeat",
            &heartbeat(JobState::Reserved, 0),
            1,
            NOW
        )
        .status,
        200
    );
    let third = issued(&mut b, &req, NOW);
    assert_eq!(third.lease_id, first.lease_id);
    assert!(verified(&b, &third, NOW).iat > second_claims.iat);
}

#[test]
fn rerequest_rejects_every_non_requester_field_change() {
    let mut b = broker();
    issued(&mut b, &lease_request(), NOW);
    for mutate in [
        |r: &mut LeaseRequest| r.vcpus = 3,
        |r: &mut LeaseRequest| r.mem_mib = 1,
        |r: &mut LeaseRequest| r.platform = "windows".into(),
        |r: &mut LeaseRequest| r.net = true,
        |r: &mut LeaseRequest| r.workspace_key_hash = "other".into(),
        |r: &mut LeaseRequest| r.repo_hash = "other".into(),
        |r: &mut LeaseRequest| r.timeout_s = 4,
        |r: &mut LeaseRequest| {
            r.placement_hint = Some(Placement {
                host: HOST.into(),
                vm: "test-vm".into(),
            })
        },
    ] {
        let mut req = lease_request();
        mutate(&mut req);
        let reply = post(&mut b, "/v1/leases", &req, NOW);
        assert_eq!(
            (reply.status, reply.body),
            (409, json!({"error":"job_conflict"}))
        );
    }
}

#[test]
fn admitted_and_started_require_reattach_and_settled_jobs_are_finished() {
    for state in [JobState::Admitted, JobState::Started] {
        let mut b = broker();
        let lease = issued(&mut b, &lease_request(), NOW);
        host_post(&mut b, "heartbeat", &heartbeat(state, 1000), 1, NOW);
        let retry = post(&mut b, "/v1/leases", &lease_request(), NOW);
        assert_eq!(
            (retry.status, retry.body),
            (409, json!({"error":"job_admitted"}))
        );
        let reattach = ReattachRequest {
            requester_jwk: RequesterJwk::from_key(
                &SigningKey::from_bytes(&[9; 32]).verifying_key(),
            ),
        };
        let reply = post(&mut b, "/v1/leases/test-job/reattach", &reattach, NOW);
        assert_eq!(reply.status, 200);
        let attach: LeaseResponse = serde_json::from_value(reply.body).unwrap();
        assert_eq!(attach.placement, lease.placement);
        let c = verified(&b, &attach, NOW);
        assert_eq!(c.purpose, Purpose::Attach);
        assert_eq!(c.cnf.jkt, reattach.requester_jwk.thumbprint().unwrap());
        host_post(&mut b, "usage", &usage(), 2, NOW + 3000);
        for path in ["/v1/leases", "/v1/leases/test-job/reattach"] {
            let reply = if path == "/v1/leases" {
                post(&mut b, path, &lease_request(), NOW + 3000)
            } else {
                post(&mut b, path, &reattach, NOW + 3000)
            };
            assert_eq!(
                (reply.status, reply.body),
                (409, json!({"error":"job_finished"}))
            );
        }
    }
}

#[test]
fn delayed_reserved_heartbeat_cannot_reopen_admission() {
    let mut b = broker();
    issued(&mut b, &lease_request(), NOW);
    host_post(
        &mut b,
        "heartbeat",
        &heartbeat(JobState::Started, 1000),
        1,
        NOW,
    );
    host_post(
        &mut b,
        "heartbeat",
        &heartbeat(JobState::Reserved, 0),
        2,
        NOW,
    );
    assert_eq!(
        post(&mut b, "/v1/leases", &lease_request(), NOW).body,
        json!({"error":"job_admitted"})
    );
}

#[test]
fn host_message_vectors_get_exact_401_errors_for_replay_skew_and_unregistered_host() {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../contracts/vectors/interim.json")).unwrap();
    let headers: HostHeaders = serde_json::from_value(v["host_headers"].clone()).unwrap();
    let body = v["host_body"].as_str().unwrap().as_bytes();
    let path = "/v1/hosts/test-host/heartbeat";
    let mut b = broker();
    let handle = |b: &mut FakeBroker, h: &HostHeaders, now| {
        b.handle(
            BrokerRequest {
                method: "POST",
                path,
                bearer: None,
                host_headers: Some(h),
                body,
            },
            now,
        )
    };
    let accepted = handle(&mut b, &headers, NOW);
    assert_eq!(accepted.status, 200);
    let replay = handle(&mut b, &headers, NOW);
    assert_eq!(
        (replay.status, replay.body),
        (401, json!({"error":"host_auth_failed"}))
    );
    for skew in [60001i64, -60001] {
        let mut fresh = broker();
        let reply = handle(&mut fresh, &headers, (NOW as i64 + skew) as u64);
        assert_eq!(
            (reply.status, reply.body),
            (401, json!({"error":"host_auth_failed"}))
        );
    }
    let mut unregistered = FakeBroker::new("lease-test", config()).unwrap();
    let reply = handle(&mut unregistered, &headers, NOW);
    assert_eq!(
        (reply.status, reply.body),
        (401, json!({"error":"host_auth_failed"}))
    );
    let low = HostHeaders::sign(
        HOST,
        "POST",
        path,
        body,
        NOW,
        1,
        &SigningKey::from_bytes(&[4; 32]),
    )
    .unwrap();
    let reply = handle(&mut b, &low, NOW);
    assert_eq!(
        (reply.status, reply.body),
        (401, json!({"error":"host_auth_failed"}))
    );
}

#[test]
fn usage_resend_conflict_unknown_and_reservation_cap() {
    let mut b = broker();
    issued(&mut b, &lease_request(), NOW);
    let first = host_post(&mut b, "usage", &usage(), 1, NOW + 3000);
    assert_eq!(
        (first.status, first.body.clone()),
        (
            200,
            json!({"job_id":"test-job","settled_vcpu_ms":6000,"released_vcpu_ms":14000})
        )
    );
    assert_eq!(host_post(&mut b, "usage", &usage(), 2, NOW + 3000), first);
    let mut changed = usage();
    changed.egress_bytes = Some(1);
    let conflict = host_post(&mut b, "usage", &changed, 3, NOW + 3000);
    assert_eq!(
        (conflict.status, conflict.body),
        (409, json!({"error":"usage_conflict"}))
    );
    changed.job_id = "unknown".into();
    let unknown = host_post(&mut b, "usage", &changed, 4, NOW + 3000);
    assert_eq!(
        (unknown.status, unknown.body),
        (404, json!({"error":"job_unknown"}))
    );
    let mut b = broker();
    issued(&mut b, &lease_request(), NOW);
    changed = usage();
    changed.ended_at_ms = NOW + 100000;
    let capped = host_post(&mut b, "usage", &changed, 1, NOW + 100000);
    assert_eq!(
        capped.body,
        json!({"job_id":"test-job","settled_vcpu_ms":20000,"released_vcpu_ms":0})
    );
}

#[test]
fn usage_from_another_host_is_unknown_and_heartbeat_jobs_are_ignored() {
    let mut b = broker();
    issued(&mut b, &lease_request(), NOW);
    b.register_host(
        "other-host".into(),
        SigningKey::from_bytes(&[9; 32]).verifying_key(),
    );
    for (route, body, expected) in [
        (
            "usage",
            serde_json::to_vec(&usage()).unwrap(),
            json!({"error":"job_unknown"}),
        ),
        (
            "heartbeat",
            serde_json::to_vec(&heartbeat(JobState::Started, 999)).unwrap(),
            json!({"ignored":["test-job"]}),
        ),
    ] {
        let path = format!("/v1/hosts/other-host/{route}");
        let h = HostHeaders::sign(
            "other-host",
            "POST",
            &path,
            &body,
            NOW,
            if route == "usage" { 1 } else { 2 },
            &SigningKey::from_bytes(&[9; 32]),
        )
        .unwrap();
        let reply = b.handle(
            BrokerRequest {
                method: "POST",
                path: &path,
                bearer: None,
                host_headers: Some(&h),
                body: &body,
            },
            NOW,
        );
        assert_eq!(reply.body, expected);
    }
    let mut hb = heartbeat(JobState::Started, 10);
    hb.running[0].job_id = "not-placed".into();
    assert_eq!(
        host_post(&mut b, "heartbeat", &hb, 1, NOW).body,
        json!({"ignored":["not-placed"]})
    );
}

#[test]
fn lost_host_settlement_uses_last_elapsed_or_zero_and_blocks_late_usage() {
    for elapsed in [None, Some(1234)] {
        let mut b = broker();
        let lease = issued(&mut b, &lease_request(), NOW);
        if let Some(elapsed) = elapsed {
            host_post(
                &mut b,
                "heartbeat",
                &heartbeat(JobState::Started, elapsed),
                1,
                NOW + 1000,
            );
        }
        let deadline = NOW + (lease.max_s + 600) * 1000;
        b.settle_host_lost(deadline - 1);
        assert!(b.settlement("test-job").is_none());
        b.settle_host_lost(deadline);
        let (settled, reason) = b.settlement("test-job").unwrap();
        assert_eq!(reason, "host_lost");
        assert_eq!(settled.settled_vcpu_ms, 2 * elapsed.unwrap_or(0));
        let reply = host_post(&mut b, "usage", &usage(), 2, deadline);
        assert_eq!(
            (reply.status, reply.body),
            (409, json!({"error":"job_settled"}))
        );
        b.settle_host_lost(deadline + 1000);
        assert_eq!(
            b.settlement("test-job").unwrap().0.settled_vcpu_ms,
            2 * elapsed.unwrap_or(0)
        );
    }
}

#[test]
fn refusal_and_budget_shortening_knobs_leave_reservations_correct() {
    let mut b = broker();
    b.knobs.refusal = Some((403, "capacity".into()));
    let refusal = post(&mut b, "/v1/leases", &lease_request(), NOW);
    assert_eq!(
        (refusal.status, refusal.body),
        (403, json!({"error":"capacity"}))
    );
    b.knobs.refusal = None;
    for (max_s, timeout_s) in [(30, 100), (5, 100)] {
        let mut req = lease_request();
        req.job_id = format!("budget-{max_s}");
        req.timeout_s = timeout_s;
        b.knobs.max_s = Some(max_s);
        let lease = issued(&mut b, &req, NOW);
        assert_eq!(lease.max_s, max_s);
        assert_eq!(verified(&b, &lease, NOW).max_s, max_s);
        assert_eq!(lease.reserved_vcpu_ms, 200000);
        b.settle_host_lost(NOW + (max_s + 600) * 1000);
        assert_eq!(b.settlement(&req.job_id).unwrap().0.settled_vcpu_ms, 0);
    }
}

#[test]
fn invalid_usage_and_resource_overflow_do_not_settle_or_panic() {
    let mut b = broker();
    let mut req = lease_request();
    req.timeout_s = u64::MAX;
    assert_eq!(post(&mut b, "/v1/leases", &req, NOW).status, 400);
    issued(&mut b, &lease_request(), NOW);
    for mutate in [
        |u: &mut Usage| u.vcpus = 1,
        |u: &mut Usage| u.ended_at_ms = NOW - 1,
    ] {
        let mut u = usage();
        mutate(&mut u);
        assert_eq!(
            host_post(&mut b, "usage", &u, if u.vcpus == 1 { 1 } else { 2 }, NOW).status,
            400
        );
        assert!(b.settlement("test-job").is_none());
    }
}
