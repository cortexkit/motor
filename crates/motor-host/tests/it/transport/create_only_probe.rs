use super::*;
use motor_protocol::{CreateOnlyProbeDetail as Detail, CreateOnlyProbeReport};

#[test]
fn probe_requires_idle_host_and_bound_account_then_forwards_reports_without_recording_them() {
    let (runtime, fake, _, _, _) = fixture();
    let request = json!({"op":"remote_cache_create_only_probe","account":"tenant"});
    let busy = runtime.admin("tenant", &request).unwrap();
    assert_eq!(busy["error"], "remote_cache_host_not_idle");
    assert_eq!(busy["exit_code"], 1);
    assert!(fake.calls.lock().unwrap().is_empty());
    runtime.host.lock().unwrap().set_draining(true).unwrap();
    assert_eq!(
        runtime
            .admin(
                "tenant",
                &json!({"op":"remote_cache_create_only_probe","account":"other"})
            )
            .unwrap()["error"],
        "remote_cache_invalid_request"
    );
    assert!(fake.calls.lock().unwrap().is_empty());
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
    let reply = runtime
        .admin(
            "tenant",
            &json!({"op":"remote_cache_create_only_probe","account":"tenant"}),
        )
        .unwrap();
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
        let reply = runtime
            .admin(
                "tenant",
                &json!({"op":"remote_cache_create_only_probe","account":"tenant"}),
            )
            .unwrap();
        assert_eq!(reply["error"], "remote_cache_create_only_probe_failed");
        assert_eq!(reply["exit_code"], 1);
        assert!(!reply.to_string().contains("PRIVATE-KEY"));
    }
}
