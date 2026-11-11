use super::*;
use motor_protocol::{CreateOnlyProbeDetail as Detail, CreateOnlyProbeReport};

fn serving_host_with_active_lease() -> (Arc<Runtime>, Arc<Fake>) {
    let (runtime, fake, _, _, claims) = fixture();
    let now = unix_ms();
    {
        let mut host = runtime.host.lock().unwrap();
        host.guest_ready("tenant", 1).unwrap();
        let (id, _) = host.claim(claims, now).unwrap();
        host.delivered(id).unwrap();
        host.event(
            "tenant",
            &event(1, TenantEventKind::Admitted, Some(JOB), None),
            now,
        )
        .unwrap();
        assert!(!host.snapshot.draining);
    }
    assert_eq!(
        runtime.host_admin(&json!({"op":"host_status"})).unwrap()["active_leases"],
        1
    );
    fake.calls.lock().unwrap().clear();
    (runtime, fake)
}

/// The probe names the tenant's account subject, which differs from the VM's
/// tenant name in production (ovh-1: tenant-1, acct:01KX...).
fn probe(account: &str) -> serde_json::Value {
    json!({"op":"remote_cache_create_only_probe","account":account})
}

#[test]
fn probe_runs_with_an_active_lease_and_bound_account_without_recording_reports() {
    let (runtime, fake) = serving_host_with_active_lease();
    let request = probe(&claims(JOB).sub);
    let failed = CreateOnlyProbeReport::refused(Detail::UnexpectedStatusOrBytes);
    *fake.admin_reply.lock().unwrap() = Some(serde_json::to_value(&failed).unwrap());
    let before = runtime.host.lock().unwrap().snapshot.clone();
    assert_eq!(
        runtime.admin("tenant", &request).unwrap(),
        serde_json::to_value(&failed).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&runtime.host.lock().unwrap().snapshot).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert_eq!(
        fake.calls.lock().unwrap().last().map(String::as_str),
        Some("admin:remote_cache_create_only_probe")
    );
    let mut cases = failed.cases;
    for case in &mut cases {
        case.ok = true;
        case.detail = Detail::Passed;
    }
    let passed = CreateOnlyProbeReport::new(cases);
    *fake.admin_reply.lock().unwrap() = Some(serde_json::to_value(&passed).unwrap());
    assert_eq!(runtime.admin("tenant", &request).unwrap()["exit_code"], 0);
    fake.calls.lock().unwrap().clear();
    // Neither another account nor the VM's tenant name reaches the guest.
    for account in ["acct:01ARZ3NDEKTSV4RRFFQ69G5FAW", "tenant"] {
        assert_eq!(
            runtime.admin("tenant", &probe(account)).unwrap()["error"],
            "remote_cache_invalid_request"
        );
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn active_leases_still_refuse_provisioning_and_activation_even_on_a_drained_host() {
    let (runtime, fake) = serving_host_with_active_lease();
    for draining in [false, true] {
        runtime.host.lock().unwrap().set_draining(draining).unwrap();
        for request in [
            json!({"op":"set_remote_cache","config":"configuration","read_key":"read-key"}),
            json!({"op":"activate_runner_args"}),
        ] {
            assert_eq!(
                runtime.admin("tenant", &request).unwrap()["error"],
                "remote_cache_host_not_idle"
            );
        }
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn emergency_block_refuses_probe_by_name_before_waking_or_forwarding() {
    let (runtime, fake, _, _, _) = fixture();
    runtime
        .host
        .lock()
        .unwrap()
        .set_remote_cache_write_block(true)
        .unwrap();
    let reply = runtime.admin("tenant", &probe(&claims(JOB).sub)).unwrap();
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["exit_code"], 1);
    assert_eq!(reply["cases"][0]["detail"], "remote_cache_write_blocked");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn arbitrary_agent_details_and_inconsistent_reports_are_never_echoed() {
    let (runtime, fake, _, _, _) = fixture();
    runtime.host.lock().unwrap().set_draining(true).unwrap();
    let failed = serde_json::to_value(CreateOnlyProbeReport::refused(
        Detail::UnexpectedStatusOrBytes,
    ))
    .unwrap();
    for reply in [
        json!({"ok":false,"error":"PRIVATE-KEY"}),
        {
            let mut reply = failed.clone();
            reply["cases"][0]["detail"] = "PRIVATE-KEY".into();
            reply
        },
        {
            let mut reply = failed;
            reply["exit_code"] = 0.into();
            reply
        },
    ] {
        *fake.admin_reply.lock().unwrap() = Some(reply);
        let reply = runtime.admin("tenant", &probe(&claims(JOB).sub)).unwrap();
        assert_eq!(reply["error"], "remote_cache_create_only_probe_failed");
        assert_eq!(reply["exit_code"], 1);
        assert!(!reply.to_string().contains("PRIVATE-KEY"));
    }
}
