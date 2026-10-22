use super::common::*;
use ed25519_dalek::SigningKey;
use motor_lease::fake::{BrokerReply, BrokerRequest, FakeBroker};
use motor_lease::*;
use serde_json::json;

#[test]
fn fake_is_forbidden_for_main_and_requires_worker_bearer() {
    assert!(FakeBroker::new("main", config()).is_err());
    assert!(FakeBroker::new("", config()).is_err());
    let mut b = broker();
    let body = serde_json::to_vec(&lease_request()).unwrap();
    for path in [
        "/v1/leases",
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
    ] {
        let reply = b.handle(
            BrokerRequest {
                content_type: Some("application/json"),
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
            content_type: Some("application/json"),
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
fn keyed_hashes_are_opaque_but_must_be_exactly_43_base64url_characters() {
    for bad in [
        "".into(),
        "a".repeat(42),
        "a".repeat(44),
        format!("{}=", "a".repeat(42)),
        format!("{}+", "a".repeat(42)),
        format!("{}/", "a".repeat(42)),
        format!("{}é", "a".repeat(41)),
    ] {
        for workspace in [true, false] {
            let mut b = broker();
            let mut req = lease_request();
            if workspace {
                req.workspace_key_hash = bad.clone();
            } else {
                req.repo_hash = bad.clone();
            }
            assert_eq!(
                post(&mut b, "/v1/leases", &req, NOW).status,
                400,
                "hash {bad}"
            );
            assert!(b.settlement(&req.job_id).is_none());
        }
    }
    for hash in [
        "a".repeat(43),
        "-".repeat(43),
        "_".repeat(43),
        "z".repeat(43),
    ] {
        let mut b = broker();
        let mut req = lease_request();
        req.workspace_key_hash = hash.clone();
        req.repo_hash = hash;
        issued(&mut b, &req, NOW);
    }
}

#[test]
fn release_requires_a_same_tenant_bearer_an_empty_object_and_a_known_job() {
    let mut b = broker();
    issued(&mut b, &lease_request(), NOW);
    for bearer in [None, Some("another-tenant-token")] {
        let reply = b.handle(
            BrokerRequest {
                content_type: Some("application/json"),
                method: "POST",
                path: "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
                bearer,
                host_headers: None,
                body: b"{}",
            },
            NOW,
        );
        assert_eq!(
            (reply.status, reply.body),
            (401, json!({"error":"unauthorized"}))
        );
    }
    for body in [
        json!(null),
        json!([]),
        json!(""),
        json!({"job_id":"019e4567-89ab-7cde-8012-3456789abcde"}),
        json!({"extra":true}),
    ] {
        let reply = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &body,
            NOW,
        );
        assert_eq!(
            (reply.status, reply.body),
            (400, json!({"error":"bad_request"}))
        );
    }
    let unknown = post(
        &mut b,
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcd1/release",
        &ReleaseRequest {},
        NOW,
    );
    assert_eq!(
        (unknown.status, unknown.body),
        (404, json!({"error":"job_unknown"}))
    );
    assert!(b
        .settlement("019e4567-89ab-7cde-8012-3456789abcde")
        .is_none());
}

#[test]
fn release_is_idempotent_blocks_new_tokens_and_settles_unadmitted_jobs_at_zero() {
    for reserved_heartbeat in [false, true] {
        let mut b = broker();
        let lease = issued(&mut b, &lease_request(), NOW);
        let pending = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {},
            NOW,
        );
        let expected = json!({"job_id":"019e4567-89ab-7cde-8012-3456789abcde", "release_at_ms":lease.token_expires_at_ms + 60000});
        assert_eq!((pending.status, pending.body.clone()), (202, expected));
        assert_eq!(
            post(
                &mut b,
                "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
                &ReleaseRequest {},
                NOW + 1000
            ),
            pending
        );
        assert_eq!(
            post(&mut b, "/v1/leases", &lease_request(), NOW + 1000).body,
            json!({"error":"job_released"})
        );
        let reattach = ReattachRequest {
            requester_jwk: lease_request().requester_jwk,
        };
        let denied = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
            &reattach,
            NOW + 1000,
        );
        assert_eq!(
            (denied.status, denied.body),
            (409, json!({"error":"job_released"}))
        );
        if reserved_heartbeat {
            assert_eq!(
                host_post(
                    &mut b,
                    "heartbeat",
                    &heartbeat(JobState::Reserved, 1234),
                    1,
                    NOW + 1000
                )
                .status,
                200
            );
        }
        let at = lease.token_expires_at_ms + 60000;
        b.settle_due(at - 1);
        assert!(b
            .settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .is_none());
        b.settle_due(at);
        let (settled, reason) = b
            .settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .unwrap();
        assert_eq!(reason, "released");
        assert_eq!(
            settled,
            &UsageReply {
                job_id: "019e4567-89ab-7cde-8012-3456789abcde".into(),
                settled_vcpu_ms: 0,
                released_vcpu_ms: 20000
            }
        );
        assert_eq!(
            post(
                &mut b,
                "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
                &ReleaseRequest {},
                at + 1
            )
            .body,
            json!({"error":"job_finished"})
        );
        let mut retry = lease_request();
        retry.platform = "other-platform".into();
        for now in [at + 1, at + 1_000_000] {
            let reply = post(&mut b, "/v1/leases", &retry, now);
            assert_eq!(
                (reply.status, reply.body),
                (409, json!({"error":"job_released"}))
            );
        }
        let late = host_post(&mut b, "usage", &usage(), 2, at + 1_000_001);
        assert_eq!(
            (late.status, late.body),
            (409, json!({"error":"job_settled"}))
        );
        let ended_attach = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
            &reattach,
            at + 1_000_001,
        );
        assert_eq!(
            (ended_attach.status, ended_attach.body),
            (409, json!({"error":"job_released"}))
        );
        assert_eq!(
            b.settlement("019e4567-89ab-7cde-8012-3456789abcde")
                .unwrap()
                .1,
            "released"
        );
        retry.job_id = "019e4567-89ab-7cde-8012-3456789abcdf".into();
        issued(&mut b, &retry, at + 1_000_001);
    }
}

#[test]
fn release_deadline_uses_latest_run_expiry_and_ignores_attach_expiry() {
    let mut b = broker();
    let first = issued(&mut b, &lease_request(), NOW);
    let latest = issued(&mut b, &lease_request(), NOW + 2000);
    assert!(latest.token_expires_at_ms > first.token_expires_at_ms);
    let attach = post(
        &mut b,
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
        &ReattachRequest {
            requester_jwk: lease_request().requester_jwk,
        },
        NOW + 3000,
    );
    assert!(attach.body["token_expires_at_ms"].as_u64().unwrap() > latest.token_expires_at_ms);
    let release = post(
        &mut b,
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
        &ReleaseRequest {},
        NOW + 4000,
    );
    assert_eq!(
        release.body["release_at_ms"],
        latest.token_expires_at_ms + 60000
    );
    assert_eq!(
        post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {},
            NOW + 10000
        ),
        release
    );
}

#[test]
fn release_then_connect_with_still_valid_run_token_cancels_release_and_bills_normally() {
    for state in [JobState::Admitted, JobState::Started] {
        let mut b = broker();
        let lease = issued(&mut b, &lease_request(), NOW);
        let pending = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {},
            NOW,
        );
        assert_eq!(pending.status, 202);
        let claims = verified(&b, &lease, Purpose::Run, NOW + 1000);
        bind_requester(&claims, &lease_request().requester_jwk).unwrap();
        host_post(&mut b, "heartbeat", &heartbeat(state, 1000), 1, NOW + 1000);
        host_post(
            &mut b,
            "heartbeat",
            &heartbeat(JobState::Reserved, 0),
            2,
            NOW + 2000,
        );
        let admitted = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {},
            NOW + 2000,
        );
        assert_eq!(
            (admitted.status, admitted.body),
            (409, json!({"error":"job_admitted"}))
        );
        let at = pending.body["release_at_ms"].as_u64().unwrap();
        b.settle_due(at);
        assert!(b
            .settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .is_none());
        let attach = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
            &ReattachRequest {
                requester_jwk: lease_request().requester_jwk,
            },
            at,
        );
        assert_eq!(attach.status, 200);
        let reply = host_post(&mut b, "usage", &usage(), 3, at + 1);
        assert_eq!(
            (reply.status, reply.body),
            (
                200,
                json!({"job_id":"019e4567-89ab-7cde-8012-3456789abcde", "settled_vcpu_ms":6000, "released_vcpu_ms":14000})
            )
        );
        assert_eq!(
            b.settlement("019e4567-89ab-7cde-8012-3456789abcde")
                .unwrap()
                .1,
            "usage"
        );
    }
}

#[test]
fn release_refuses_previously_admitted_or_otherwise_finished_jobs() {
    for state in [JobState::Admitted, JobState::Started] {
        let mut b = broker();
        issued(&mut b, &lease_request(), NOW);
        host_post(&mut b, "heartbeat", &heartbeat(state, 0), 1, NOW);
        let admitted = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {},
            NOW,
        );
        assert_eq!(
            (admitted.status, admitted.body),
            (409, json!({"error":"job_admitted"}))
        );
        host_post(&mut b, "usage", &usage(), 2, NOW + 3000);
        let finished = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {},
            NOW + 3000,
        );
        assert_eq!(
            (finished.status, finished.body),
            (409, json!({"error":"job_finished"}))
        );
    }
    let mut b = broker();
    issued(&mut b, &lease_request(), NOW);
    let finished = post(
        &mut b,
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
        &ReleaseRequest {},
        NOW + 611000,
    );
    assert_eq!(
        (finished.status, finished.body),
        (409, json!({"error":"job_finished"}))
    );
}

#[test]
fn release_is_not_cancelled_by_another_job_or_another_hosts_heartbeat() {
    let mut b = broker();
    let lease = issued(&mut b, &lease_request(), NOW);
    post(
        &mut b,
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
        &ReleaseRequest {},
        NOW,
    );
    let mut other_job = heartbeat(JobState::Started, 1000);
    other_job.running[0].job_id = "019e4567-89ab-7cde-8012-3456789abcdf".into();
    assert_eq!(
        host_post(&mut b, "heartbeat", &other_job, 1, NOW).body,
        json!({"ignored":["019e4567-89ab-7cde-8012-3456789abcdf"],"closed_subjects":[]})
    );
    let key = SigningKey::from_bytes(&[9; 32]);
    b.register_host("other-host".into(), key.verifying_key());
    let path = "/v1/hosts/other-host/heartbeat";
    let body = serde_json::to_vec(&heartbeat(JobState::Started, 1000)).unwrap();
    let headers = HostHeaders::sign("other-host", "POST", path, &body, NOW, 1, &key).unwrap();
    let reply = b.handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path,
            bearer: None,
            host_headers: Some(&headers),
            body: &body,
        },
        NOW,
    );
    assert_eq!(
        reply.body,
        json!({"ignored":["019e4567-89ab-7cde-8012-3456789abcde"],"closed_subjects":[]})
    );
    b.settle_due(lease.token_expires_at_ms + 60000);
    assert_eq!(
        b.settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .unwrap()
            .1,
        "released"
    );
    assert_eq!(
        b.settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .unwrap()
            .0
            .settled_vcpu_ms,
        0
    );
}

#[test]
fn reservation_floor_fresh_tokens_before_heartbeat_and_while_reserved() {
    let mut b = broker();
    let mut req = lease_request();
    let first = issued(&mut b, &req, NOW);
    assert_eq!(first.reserved_vcpu_ms, 20000);
    let first_claims = verified(&b, &first, Purpose::Run, NOW);
    assert_eq!(first_claims.purpose, Purpose::Run);
    req.requester_jwk = RequesterJwk::from_key(&SigningKey::from_bytes(&[9; 32]).verifying_key());
    let second = issued(&mut b, &req, NOW + 1000);
    assert_eq!(second.lease_id, first.lease_id);
    assert_eq!(second.placement, first.placement);
    assert_eq!(second.reserved_vcpu_ms, first.reserved_vcpu_ms);
    assert_ne!(second.job_token, first.job_token);
    let second_claims = verified(&b, &second, Purpose::Run, NOW);
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
    let third = issued(&mut b, &req, NOW + 2000);
    assert_eq!(third.lease_id, first.lease_id);
    assert!(verified(&b, &third, Purpose::Run, NOW).iat > second_claims.iat);
}

#[test]
fn rerequest_rejects_every_non_requester_field_change() {
    let mut b = broker();
    issued(&mut b, &lease_request(), NOW);
    for mutate in [
        |r: &mut LeaseRequest| r.vcpus = 3,
        |r: &mut LeaseRequest| r.platform = "windows".into(),
        |r: &mut LeaseRequest| r.net = true,
        |r: &mut LeaseRequest| r.workspace_key_hash = base64url(&[7; 32]),
        |r: &mut LeaseRequest| r.repo_hash = base64url(&[7; 32]),
        |r: &mut LeaseRequest| r.timeout_s = 11,
        |r: &mut LeaseRequest| {
            r.placement_hint = Some(Placement {
                host: HOST.into(),
                vm: Some("test-vm".into()),
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
        let reply = post(
            &mut b,
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
            &reattach,
            NOW,
        );
        assert_eq!(reply.status, 200);
        let attach: LeaseResponse = serde_json::from_value(reply.body).unwrap();
        assert_eq!(attach.placement, lease.placement);
        let c = verified(&b, &attach, Purpose::Attach, NOW);
        assert_eq!(c.purpose, Purpose::Attach);
        assert_eq!(c.cnf.jkt, reattach.requester_jwk.thumbprint().unwrap());
        host_post(&mut b, "usage", &usage(), 2, NOW + 3000);
        // A settled job never gets another run token.
        let retry = post(&mut b, "/v1/leases", &lease_request(), NOW + 3000);
        assert_eq!(
            (retry.status, retry.body),
            (409, json!({"error":"job_finished"}))
        );
    }
}

/// A job its host settled by usage report can be reattached for 86400 s after
/// the settlement, so the host can replay output the client lost. The attach
/// reply repeats the lease unchanged and settles nothing; at the end of the
/// window the reply is 409 `job_finished` again.
#[test]
fn usage_settled_job_reattaches_for_a_day_without_changing_its_settlement() {
    let mut b = broker();
    let lease = issued(&mut b, &lease_request(), NOW);
    host_post(
        &mut b,
        "heartbeat",
        &heartbeat(JobState::Started, 1000),
        1,
        NOW,
    );
    let settled_at = NOW + 3000;
    let settled = host_post(&mut b, "usage", &usage(), 2, settled_at);
    assert_eq!(settled.status, 200);
    let reattach = ReattachRequest {
        requester_jwk: RequesterJwk::from_key(&SigningKey::from_bytes(&[9; 32]).verifying_key()),
    };
    let path = "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach";
    let last = settled_at + motor_lease::fake::SETTLED_REATTACH_WINDOW_MS - 1;
    for now in [settled_at, settled_at + 600_000, last] {
        let reply = post(&mut b, path, &reattach, now);
        assert_eq!(reply.status, 200, "{}", reply.body);
        let attach: LeaseResponse = serde_json::from_value(reply.body).unwrap();
        assert_eq!(
            (
                &attach.lease_id,
                &attach.placement,
                &attach.endpoint,
                &attach.endpoint_spki_sha256,
                attach.max_s,
                attach.reserved_vcpu_ms,
            ),
            (
                &lease.lease_id,
                &lease.placement,
                &lease.endpoint,
                &lease.endpoint_spki_sha256,
                lease.max_s,
                lease.reserved_vcpu_ms,
            )
        );
        let claims = verified(&b, &attach, Purpose::Attach, now);
        assert_eq!(claims.iat, now / 1000);
        assert_eq!(claims.cnf.jkt, reattach.requester_jwk.thumbprint().unwrap());
        let (reply, reason) = b
            .settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .unwrap();
        assert_eq!(
            (reply, reason),
            (
                &serde_json::from_value(settled.body.clone()).unwrap(),
                "usage"
            )
        );
    }
    // The host's resent report still answers with the original settlement.
    assert_eq!(host_post(&mut b, "usage", &usage(), 3, last), settled);
    let expired = post(&mut b, path, &reattach, last + 1);
    assert_eq!(
        (expired.status, expired.body),
        (409, json!({"error":"job_finished"}))
    );
}

/// Only a usage-report settlement opens the reattach window: a job settled
/// because its host was lost, or a released one, keeps its refusal.
#[test]
fn reattach_window_excludes_host_lost_and_released_jobs() {
    let reattach = ReattachRequest {
        requester_jwk: RequesterJwk::from_key(&SigningKey::from_bytes(&[9; 32]).verifying_key()),
    };
    let path = "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach";
    let mut lost = broker();
    let lease = issued(&mut lost, &lease_request(), NOW);
    let deadline = NOW + (lease.max_s + 600) * 1000;
    lost.settle_due(deadline);
    assert_eq!(
        lost.settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .unwrap()
            .1,
        "host_lost"
    );
    let reply = post(&mut lost, path, &reattach, deadline);
    assert_eq!(
        (reply.status, reply.body),
        (409, json!({"error":"job_finished"}))
    );
    let mut released = broker();
    issued(&mut released, &lease_request(), NOW);
    let release = post(
        &mut released,
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
        &ReleaseRequest {},
        NOW,
    );
    let release_at = release.body["release_at_ms"].as_u64().unwrap();
    released.settle_due(release_at);
    let reply = post(&mut released, path, &reattach, release_at);
    assert_eq!(
        (reply.status, reply.body),
        (409, json!({"error":"job_released"}))
    );
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
    let body = serde_json::to_vec(&heartbeat(JobState::Reserved, 0)).unwrap();
    let body = body.as_slice();
    let path = "/v1/hosts/test-host/heartbeat";
    let headers = HostHeaders::sign(
        HOST,
        "POST",
        path,
        body,
        NOW,
        NOW * 1000,
        &SigningKey::from_bytes(&[4; 32]),
    )
    .unwrap();
    let mut b = broker();
    let handle = |b: &mut FakeBroker, h: &HostHeaders, now| {
        b.handle(
            BrokerRequest {
                content_type: Some("application/json"),
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
            json!({"job_id":"019e4567-89ab-7cde-8012-3456789abcde","settled_vcpu_ms":6000,"released_vcpu_ms":14000})
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
    changed.job_id = "019e4567-89ab-7cde-8012-3456789abcd1".into();
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
        json!({"job_id":"019e4567-89ab-7cde-8012-3456789abcde","settled_vcpu_ms":20000,"released_vcpu_ms":0})
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
            json!({"ignored":["019e4567-89ab-7cde-8012-3456789abcde"],"closed_subjects":[]}),
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
                content_type: Some("application/json"),
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
    hb.running[0].job_id = "019e4567-89ab-7cde-8012-3456789abcd0".into();
    assert_eq!(
        host_post(&mut b, "heartbeat", &hb, 1, NOW).body,
        json!({"ignored":["019e4567-89ab-7cde-8012-3456789abcd0"],"closed_subjects":[]})
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
        b.settle_due(deadline - 1);
        assert!(b
            .settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .is_none());
        b.settle_due(deadline);
        let (settled, reason) = b
            .settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .unwrap();
        assert_eq!(reason, "host_lost");
        assert_eq!(settled.settled_vcpu_ms, 2 * elapsed.unwrap_or(0));
        let reply = host_post(&mut b, "usage", &usage(), 2, deadline);
        assert_eq!(
            (reply.status, reply.body),
            (409, json!({"error":"job_settled"}))
        );
        b.settle_due(deadline + 1000);
        assert_eq!(
            b.settlement("019e4567-89ab-7cde-8012-3456789abcde")
                .unwrap()
                .0
                .settled_vcpu_ms,
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
        req.job_id = format!("019e4567-89ab-7cde-8012-{max_s:012x}");
        req.timeout_s = timeout_s;
        b.knobs.max_s = Some(max_s);
        let lease = issued(&mut b, &req, NOW);
        assert_eq!(lease.max_s, max_s);
        assert_eq!(verified(&b, &lease, Purpose::Run, NOW).max_s, max_s);
        assert_eq!(lease.reserved_vcpu_ms, 2 * max_s * 1000);
        b.settle_due(NOW + (max_s + 600) * 1000);
        assert_eq!(b.settlement(&req.job_id).unwrap().0.settled_vcpu_ms, 0);
    }
}

/// A lease for more vCPUs than any host holds is refused with 422
/// `no_host_fits` naming the largest count, and books nothing: once the limit
/// allows it, the same job ID gets a fresh lease rather than a conflict.
#[test]
fn max_vcpus_knob_answers_no_host_fits_without_reserving() {
    let mut b = broker();
    b.knobs.max_vcpus = Some(8);
    let mut req = lease_request();
    req.vcpus = 16;
    let refusal = post(&mut b, "/v1/leases", &req, NOW);
    assert_eq!(
        (refusal.status, refusal.body),
        (422, json!({"error":"no_host_fits","max_vcpus":8}))
    );
    assert!(b.settlement(&req.job_id).is_none());
    let mut fitting = lease_request();
    fitting.job_id = "019e4567-89ab-7cde-8012-000000000008".into();
    fitting.vcpus = 8;
    assert_eq!(
        issued(&mut b, &fitting, NOW).reserved_vcpu_ms,
        8 * fitting.timeout_s * 1000
    );
    b.knobs.max_vcpus = None;
    assert_eq!(
        issued(&mut b, &req, NOW).reserved_vcpu_ms,
        16 * req.timeout_s * 1000
    );
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
        assert!(b
            .settlement("019e4567-89ab-7cde-8012-3456789abcde")
            .is_none());
    }
}

#[test]
fn heartbeat_elapsed_is_monotonic_capped_and_refreshes_do_not_move_loss_deadline() {
    let mut b = broker();
    let request = lease_request();
    let lease = issued(&mut b, &request, NOW);
    assert_eq!(
        host_post(
            &mut b,
            "heartbeat",
            &heartbeat(JobState::Started, 5000),
            1,
            NOW
        )
        .status,
        200
    );
    host_post(
        &mut b,
        "heartbeat",
        &heartbeat(JobState::Reserved, 0),
        2,
        NOW + 1000,
    );
    assert_eq!(
        post(&mut b, "/v1/leases", &request, NOW + 1000).body,
        json!({"error":"job_admitted"})
    );
    host_post(
        &mut b,
        "heartbeat",
        &heartbeat(JobState::Started, 999999),
        3,
        NOW + 2000,
    );
    let deadline = NOW + (lease.max_s + 600) * 1000;
    b.settle_due(deadline);
    assert_eq!(
        b.settlement(&request.job_id).unwrap().0.settled_vcpu_ms,
        20000
    );
    assert_eq!(
        host_post(
            &mut b,
            "heartbeat",
            &heartbeat(JobState::Started, 1),
            4,
            deadline
        )
        .body,
        json!({"ignored":[],"closed_subjects":[]})
    );
    let mut b = broker();
    issued(&mut b, &request, NOW);
    issued(&mut b, &request, NOW + 20000);
    let attach = ReattachRequest {
        requester_jwk: request.requester_jwk.clone(),
    };
    post(
        &mut b,
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
        &attach,
        NOW + 40000,
    );
    b.settle_due(deadline);
    assert_eq!(b.settlement(&request.job_id).unwrap().1, "host_lost");
}

#[test]
fn avoid_host_is_required_validated_never_placed_on_and_part_of_the_lease_hash() {
    // A body without the key is refused, as is a value outside the grammar.
    let mut missing = serde_json::to_value(lease_request()).unwrap();
    missing.as_object_mut().unwrap().remove("avoid_host");
    assert_eq!(
        post(&mut broker(), "/v1/leases", &missing, NOW).body,
        json!({"error":"bad_request"})
    );
    for bad in [
        "".to_string(),
        "a".repeat(65),
        "boat:test".into(),
        "a b".into(),
    ] {
        let mut request = lease_request();
        request.avoid_host = Some(bad.clone());
        assert_eq!(
            post(&mut broker(), "/v1/leases", &request, NOW).status,
            400,
            "{bad:?}"
        );
    }
    // The avoided host is not selectable even when the hint names it; the
    // fake has no other host, so the lease has no capacity.
    let mut b = broker();
    let mut request = lease_request();
    request.placement_hint = Some(config().placement);
    request.avoid_host = Some(config().placement.host);
    let refused = post(&mut b, "/v1/leases", &request, NOW);
    assert_eq!(
        (refused.status, refused.body),
        (503, json!({"error":"no_capacity"}))
    );
    // A host the fake does not run changes nothing, and a re-request must
    // repeat the value it was first sent with.
    request.avoid_host = Some("build-server-1".into());
    issued(&mut b, &request, NOW);
    issued(&mut b, &request, NOW + 1000);
    request.avoid_host = None;
    assert_eq!(
        post(&mut b, "/v1/leases", &request, NOW + 2000).body,
        json!({"error":"job_conflict"})
    );
}

#[test]
fn unusable_placement_hint_is_a_hint_and_finite_budget_uses_granted_runtime() {
    let mut b = broker();
    let mut request = lease_request();
    request.placement_hint = Some(Placement {
        host: "unavailable-host".into(),
        vm: Some("unavailable-vm".into()),
    });
    let lease = issued(&mut b, &request, NOW);
    assert_eq!(lease.placement, config().placement);
    let mut b = broker();
    b.knobs.allowance_vcpu_ms = Some(25000);
    request.timeout_s = 100;
    let lease = issued(&mut b, &request, NOW);
    assert_eq!((lease.max_s, lease.reserved_vcpu_ms), (12, 24000));
    let mut b = broker();
    b.knobs.allowance_vcpu_ms = Some(19999);
    assert_eq!(
        post(&mut b, "/v1/leases", &request, NOW).body,
        json!({"error":"budget_exhausted"})
    );
    assert!(b.settlement(&request.job_id).is_none());
}

#[test]
fn released_admission_report_must_win_before_zero_settlement() {
    let mut b = broker();
    let request = lease_request();
    let lease = issued(&mut b, &request, NOW);
    post(
        &mut b,
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
        &ReleaseRequest::default(),
        NOW,
    );
    let at = lease.token_expires_at_ms + 60000;
    host_post(
        &mut b,
        "heartbeat",
        &heartbeat(JobState::Admitted, 0),
        1,
        at,
    );
    assert_eq!(b.settlement(&request.job_id).unwrap().1, "released");
    assert_eq!(
        host_post(&mut b, "usage", &usage(), 2, at).body,
        json!({"error":"job_settled"})
    );
}

#[test]
fn routing_authentication_and_media_type_checks_have_contract_precedence() {
    let mut b = broker();
    let request = |path, bearer, content_type| BrokerRequest {
        method: "POST",
        path,
        bearer,
        content_type,
        host_headers: None,
        body: b"not-json",
    };
    assert_eq!(
        b.handle(request("/v1/unknown", None, None), NOW).body,
        json!({"error":"not_found"})
    );
    assert_eq!(
        b.handle(request("/v1/leases", None, None), NOW).body,
        json!({"error":"unauthorized"})
    );
    assert_eq!(
        b.handle(request("/v1/leases", Some(BEARER), None), NOW)
            .body,
        json!({"error":"bad_request"})
    );
    assert_eq!(
        b.handle(request("/v1/hosts/test-host/unknown", None, None), NOW)
            .body,
        json!({"error":"not_found"})
    );
    assert_eq!(
        b.handle(
            request(
                "/v1/hosts/test-host/heartbeat?extra=1",
                None,
                Some("application/json")
            ),
            NOW
        )
        .body,
        json!({"error":"bad_request"})
    );
}

#[test]
fn allow_spill_projection_distinguishes_false_and_true_but_not_requester_rotation() {
    for initial in [false, true] {
        let mut b = broker();
        let mut request = lease_request();
        request.allow_spill = Some(initial);
        issued(&mut b, &request, NOW);
        request.requester_jwk =
            RequesterJwk::from_key(&SigningKey::from_bytes(&[9; 32]).verifying_key());
        issued(&mut b, &request, NOW + 1000);
        request.allow_spill = Some(!initial);
        assert_eq!(
            post(&mut b, "/v1/leases", &request, NOW + 2000).body,
            json!({"error":"job_conflict"}),
            "{initial} -> {}",
            !initial
        );
        // Dropping the member on a re-request is a malformed body, refused
        // before the existing job is consulted.
        request.allow_spill = None;
        assert_eq!(
            post(&mut b, "/v1/leases", &request, NOW + 2000),
            BrokerReply::error(400, "bad_request"),
            "{initial} -> absent"
        );
    }
}

/// The broker requires `allow_spill` on every lease body. `LeaseRequest` still
/// deserializes a body without it, so lease requests ck-motor stored before it
/// always sent the member still load, but the fake answers such a body 400
/// `bad_request` and books nothing, as it does for any non-boolean value.
#[test]
fn allow_spill_is_required_and_must_be_boolean() {
    let mut body = serde_json::to_value(lease_request()).unwrap();
    body.as_object_mut().unwrap().remove("allow_spill");
    let parsed: LeaseRequest = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(parsed.allow_spill, None);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), body);
    let mut b = broker();
    assert_eq!(
        post(&mut b, "/v1/leases", &body, NOW),
        BrokerReply::error(400, "bad_request")
    );
    // Nothing was claimed: the job is unknown to reattach.
    assert_eq!(
        post(
            &mut b,
            &format!("/v1/leases/{}/reattach", parsed.job_id),
            &ReattachRequest {
                requester_jwk: parsed.requester_jwk.clone(),
            },
            NOW,
        ),
        BrokerReply::error(404, "job_unknown")
    );
    body["allow_spill"] = json!(true);
    assert_eq!(post(&mut b, "/v1/leases", &body, NOW).status, 201);
    for bad in [serde_json::Value::Null, json!(0), json!("false")] {
        body["allow_spill"] = bad;
        assert_eq!(
            post(&mut broker(), "/v1/leases", &body, NOW).body,
            json!({"error":"bad_request"})
        );
    }
}
