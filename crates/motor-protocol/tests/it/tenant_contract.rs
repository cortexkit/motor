use motor_protocol::*;
use serde_json::json;

#[test]
fn idle_drain_frames_and_bounded_non_debug_cache_request_roundtrip() {
    for value in [
        json!({"type":"drain_if_idle"}),
        json!({"type":"drain_if_idle_reply","blocked_by":null,"count":0}),
        json!({"type":"drain_if_idle_reply","blocked_by":"finalizing","count":2}),
    ] {
        let frame: Frame = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(frame).unwrap(), value);
    }
    let value = json!({"op":"set_remote_cache","config":"{}","read_key":"{}"});
    let request: RemoteCacheRequest = serde_json::from_value(value.clone()).unwrap();
    assert!(request.valid());
    assert_eq!(serde_json::to_value(request).unwrap(), value);
    let request: RemoteCacheRequest = serde_json::from_value(
        json!({"op":"set_remote_cache","config":"{}","read_key":"x".repeat(65537)}),
    )
    .unwrap();
    assert!(!request.valid());
}

#[test]
fn terminal_job_peak_is_an_optional_addition_to_lifecycle_events() {
    for peak in [None, Some(481)] {
        let mut value = json!({"type":"event","event_seq":3,"event":"terminal","jti":"00000000-0000-4000-8000-000000000001"});
        if let Some(peak) = peak {
            value["mem_peak_mib"] = json!(peak);
        }
        let Frame::Event(event) = serde_json::from_value::<Frame>(value.clone()).unwrap() else {
            panic!("expected event")
        };
        assert_eq!(event.mem_peak_mib, peak);
        assert_eq!(serde_json::to_value(Frame::Event(event)).unwrap(), value);
    }
}

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
        setup: None,
        mem_peak_mib: None,
        mem_working_peak_mib: None,
        remote_cache_totals: None,
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
        setup: None,
        mem_peak_mib: None,
        mem_working_peak_mib: None,
        remote_cache_totals: None,
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
fn working_peak_is_additive_for_old_lifecycle_readers() {
    #[derive(serde::Deserialize)]
    struct OldEvent {
        event_seq: u64,
        event: TenantEventKind,
        jti: Option<Uuid>,
        cause: Option<RefusedCause>,
        setup: Option<TenantSetupPhase>,
        mem_peak_mib: Option<u64>,
    }
    for peak in [None, Some(513)] {
        let mut value = json!({"type":"event","event_seq":3,"event":"terminal","jti":"00000000-0000-4000-8000-000000000001","mem_peak_mib":1600});
        if let Some(peak) = peak {
            value["mem_working_peak_mib"] = json!(peak);
        }
        let old: OldEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(old.event_seq, 3);
        assert_eq!(old.event, TenantEventKind::Terminal);
        assert!(old.jti.is_some());
        assert_eq!(old.cause, None);
        assert_eq!(old.setup, None);
        assert_eq!(old.mem_peak_mib, Some(1600));
        let Frame::Event(new) = serde_json::from_value::<Frame>(value.clone()).unwrap() else {
            panic!("event expected")
        };
        assert_eq!(new.mem_working_peak_mib, peak);
        assert_eq!(serde_json::to_value(Frame::Event(new)).unwrap(), value);
    }
}

#[test]
fn remote_cache_totals_are_additive_for_old_lifecycle_readers() {
    #[derive(serde::Deserialize)]
    struct OldEvent {
        event_seq: u64,
        event: TenantEventKind,
        jti: Option<Uuid>,
        cause: Option<RefusedCause>,
        setup: Option<TenantSetupPhase>,
        mem_peak_mib: Option<u64>,
        mem_working_peak_mib: Option<u64>,
    }
    for totals in [
        None,
        Some(motor_protocol::RemoteCacheTotals {
            hits: 7,
            misses: 3,
            errors: u64::MAX,
        }),
    ] {
        let mut value = json!({"type":"event","event_seq":3,"event":"terminal","jti":"00000000-0000-4000-8000-000000000001","mem_peak_mib":1600,"mem_working_peak_mib":513});
        if let Some(totals) = totals {
            value["remote_cache_totals"] = json!({"hits": 7, "misses": 3, "errors": u64::MAX});
            assert_eq!(
                serde_json::to_value(totals).unwrap(),
                value["remote_cache_totals"]
            );
        }
        // The host decodes the event object directly, ignoring unknown fields.
        let old: OldEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(old.event_seq, 3);
        assert_eq!(old.event, TenantEventKind::Terminal);
        assert!(old.jti.is_some());
        assert_eq!(old.cause, None);
        assert_eq!(old.setup, None);
        assert_eq!(old.mem_peak_mib, Some(1600));
        assert_eq!(old.mem_working_peak_mib, Some(513));
        let direct: motor_protocol::TenantEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(direct.remote_cache_totals, totals);
        let Frame::Event(new) = serde_json::from_value::<Frame>(value.clone()).unwrap() else {
            panic!("event expected")
        };
        assert_eq!(new, direct);
        assert_eq!(serde_json::to_value(Frame::Event(new)).unwrap(), value);
    }
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
        json!({"op":"diagnostics"}),
        json!({"op":"diagnostics","https":true}),
        json!({"op":"diagnostics","since_unix_s":1800000000,"until_unix_s":1800000600,
            "jti":"00000000-0000-7000-8000-000000000001"}),
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
