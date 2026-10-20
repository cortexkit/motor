use motor_protocol::*;
use serde_json::json;

#[test]
fn pinned_tenant_frames_are_inline_and_integer_only() {
    let id = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
    let value = json!({"type":"verified_lease","jti":id,"purpose":"run","sub":"acct:test","vcpus":2,
        "mem_mib":16384,"platform":"linux-x64","net":false,"max_s":30,"exp":1800000000});
    let frame: Frame = serde_json::from_value(value.clone()).unwrap();
    assert!(matches!(frame, Frame::VerifiedLease(_)));
    assert_eq!(serde_json::to_value(&frame).unwrap(), value);
    let mut invalid = value;
    invalid["vcpus"] = json!(2.5);
    assert!(serde_json::from_value::<Frame>(invalid).is_err());
    let event = Frame::Event(TenantEvent {
        event_seq: 1,
        event: TenantEventKind::Refused,
        jti: Some(id),
        cause: Some(RefusedCause::Mismatch),
    });
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({"type":"event","event_seq":1,"event":"refused","jti":id,"cause":"mismatch"})
    );
    let idle = Frame::Event(TenantEvent {
        event_seq: 2,
        event: TenantEventKind::Idle,
        jti: None,
        cause: None,
    });
    assert_eq!(
        serde_json::to_value(idle).unwrap(),
        json!({"type":"event","event_seq":2,"event":"idle"})
    );
    assert_eq!(
        serde_json::to_value(Frame::Ack { event_seq: 2 }).unwrap(),
        json!({"type":"ack","event_seq":2})
    );
}

#[test]
fn pinned_agent_operations_roundtrip_and_preserve_exit_after() {
    let id = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
    for value in [
        json!({"op":"attach_tenant","generation":1,"unix_ms":1800000000000_u64,"seed":"seed","address":"10.200.1.2/30","gateway":"10.200.1.1","resolvers":["1.1.1.1"],"hostname":"tenant-a","machine_id":"0123456789abcdef0123456789abcdef"}),
        json!({"op":"post_restore","unix_ms":1800000000000_u64,"seed":"seed"}),
        json!({"op":"cancel","job_id":id}),
        json!({"op":"drain","deadline_s":10,"exit_after":false}),
        json!({"op":"drain","deadline_s":10,"exit_after":true}),
        json!({"op":"resume"}),
        json!({"op":"status"}),
        json!({"op":"args_status"}),
    ] {
        let request: TenantAgentRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), value);
    }
    assert!(
        serde_json::from_value::<TenantAgentRequest>(json!({"op":"drain","deadline_s":10}))
            .is_err()
    );
    assert!(serde_json::from_value::<TenantAgentRequest>(json!({"op":"unknown"})).is_err());
}

#[test]
fn tenant_snapshot_control_roundtrips_additively() {
    for (wire, expected) in [
        (
            r#"{"op":"prepare_snapshot","kind":"base"}"#,
            TenantAgentRequest::PrepareSnapshot {
                kind: SnapshotKind::Base,
            },
        ),
        (
            r#"{"op":"prepare_snapshot","kind":"suspend"}"#,
            TenantAgentRequest::PrepareSnapshot {
                kind: SnapshotKind::Suspend,
            },
        ),
        (r#"{"op":"thaw"}"#, TenantAgentRequest::Thaw),
        (r#"{"op":"status"}"#, TenantAgentRequest::Status),
    ] {
        let decoded: TenantAgentRequest = serde_json::from_str(wire).unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap()
        );
    }
    assert!(serde_json::from_str::<TenantAgentRequest>(
        r#"{"op":"prepare_snapshot","kind":"unknown"}"#
    )
    .is_err());
    assert!(serde_json::from_str::<TenantAgentRequest>(r#"{"op":"prepare_snapshot"}"#).is_err());
}

#[test]
fn host_attach_deadline_outlasts_guest_runner_readiness_wait() {
    // If the host gave up first, a slow runner start would surface as an
    // anonymous host read timeout instead of the guest's readiness error.
    let margin = TENANT_ATTACH_READ_DEADLINE
        .checked_sub(TENANT_RUNNER_READINESS_WAIT)
        .filter(|margin| !margin.is_zero());
    assert!(
        margin.is_some_and(|margin| margin >= std::time::Duration::from_secs(10)),
        "host attach deadline {TENANT_ATTACH_READ_DEADLINE:?} must exceed the guest readiness wait {TENANT_RUNNER_READINESS_WAIT:?} by at least 10 s"
    );
}
