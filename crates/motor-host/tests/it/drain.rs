use super::*;
use serde_json::Value;

#[test]
fn host_drain_heartbeat_body_matches_pinned_cloud_bytes() {
    let vector: Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/host-draining.json"
    ))
    .unwrap();
    let mut bindings = tenants();
    bindings.get_mut("vm").unwrap().vcpu_ceiling = 60;
    let mut h = Host::new(
        bindings,
        Snapshot::default(),
        Box::<MemoryJournal>::default(),
    );
    // Without a bound, full-drain and normal heartbeats retain their original bytes.
    for exchange in vector["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|exchange| {
            exchange["request"]["body"]["warm"] == serde_json::json!([])
                && exchange["request"]["body"].get("accept_until_ms").is_none()
                && exchange["request"]["pathname"] == "/v1/hosts/host-a/heartbeat"
        })
    {
        let expected = &exchange["request"];
        let draining = expected["body"]["draining"].as_bool().unwrap_or(false);
        h.set_draining(draining).unwrap();
        let heartbeat = h.heartbeat(vector["clock_ms"].as_u64().unwrap(), 245760, true);
        let decoded: motor_lease::Heartbeat =
            serde_json::from_str(expected["body_bytes"].as_str().unwrap()).unwrap();
        assert_eq!(heartbeat, decoded);
        // Explicit false is accepted from peers, but hosts omit it to preserve
        // the pre-drain heartbeat bytes when fleet placement is enabled.
        let bytes = if expected["body"]["draining"] == false {
            vector["exchanges"][1]["request"]["body_bytes"]
                .as_str()
                .unwrap()
        } else {
            expected["body_bytes"].as_str().unwrap()
        };
        assert_eq!(serde_json::to_vec(&heartbeat).unwrap(), bytes.as_bytes());
    }
}

#[test]
fn bounded_heartbeat_body_matches_pinned_cloud_bytes_and_omits_cleared_bound() {
    let vector: Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/host-draining.json"
    ))
    .unwrap();
    let exchange = vector["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["request"]["body"]["accept_until_ms"].as_u64().is_some())
        .unwrap();
    let mut bindings = tenants();
    bindings.get_mut("vm").unwrap().vcpu_ceiling = 60;
    let mut h = Host::new(
        bindings,
        Snapshot::default(),
        Box::<MemoryJournal>::default(),
    );
    // The wire vector has no running entries; supply its bound independently
    // of the lifecycle tests that compute and promote real bounded drains.
    h.snapshot.accept_until_ms = exchange["request"]["body"]["accept_until_ms"].as_u64();
    let heartbeat = h.heartbeat(vector["clock_ms"].as_u64().unwrap(), 245760, true);
    assert_eq!(
        serde_json::to_vec(&heartbeat).unwrap(),
        exchange["request"]["body_bytes"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
    h.set_draining(false).unwrap();
    let heartbeat = h.heartbeat(vector["clock_ms"].as_u64().unwrap(), 245760, true);
    assert_eq!(
        serde_json::to_vec(&heartbeat).unwrap(),
        vector["exchanges"][1]["request"]["body_bytes"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
}

/// CLOUD's busy exchanges decode, and a host writes them back byte for byte,
/// except that an explicit `"busy":false` is omitted (absent means false),
/// as with `draining`. Which leases make an entry busy is tested in billing.rs.
#[test]
fn warm_busy_heartbeat_bytes_match_pinned_cloud_vectors() {
    let vector: Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/host-draining.json"
    ))
    .unwrap();
    let mut seen = [false; 2];
    for exchange in vector["exchanges"].as_array().unwrap() {
        let Some(busy) = exchange["request"]["body"]["warm"][0]["busy"].as_bool() else {
            continue;
        };
        seen[usize::from(busy)] = true;
        let bytes = exchange["request"]["body_bytes"].as_str().unwrap();
        let decoded: motor_lease::Heartbeat = serde_json::from_str(bytes).unwrap();
        assert_eq!(decoded.warm[0].busy, busy);
        let expected = if busy {
            bytes.to_owned()
        } else {
            bytes.replace(",\"busy\":false", "")
        };
        assert_eq!(
            String::from_utf8(serde_json::to_vec(&decoded).unwrap()).unwrap(),
            expected
        );
    }
    assert_eq!(seen, [true, true], "both busy exchanges are pinned");
}

#[test]
fn host_drain_persists_across_restart_and_does_not_refuse_binding_leases() {
    let dir = scratch();
    let mut h = disk_host(dir.path());
    assert!(!h.snapshot.draining);
    h.set_draining(true).unwrap();
    h.retention_tick(NOW * 1000).unwrap();
    drop(h);
    let mut h = disk_host(dir.path());
    assert!(h.snapshot.draining);
    assert!(h.heartbeat(NOW * 1000, 245760, true).draining);
    // Admission still honors broker-issued placements, including canaries.
    assert!(h.claim(claims(JOB), NOW * 1000).is_ok());
    h.set_draining(false).unwrap();
    drop(h);
    let h = disk_host(dir.path());
    assert!(!h.snapshot.draining);
    let mut old = serde_json::to_value(&h.snapshot).unwrap();
    old.as_object_mut().unwrap().remove("draining");
    assert!(!serde_json::from_value::<Snapshot>(old).unwrap().draining);
}

#[test]
fn host_drain_is_not_published_when_the_journal_write_fails() {
    struct Full;
    impl Journal for Full {
        fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::other("disk full"))
        }
    }
    let mut h = host();
    h.replace_journal(Box::new(Full));
    assert!(h.set_draining(true).is_err());
    assert!(!h.snapshot.draining);
    assert!(!h.heartbeat(NOW * 1000, 245760, true).draining);
}

const MS: u64 = NOW * 1000;

fn sqlite_host(path: &std::path::Path) -> Host {
    let (journal, snapshot) = motor_host::store::SqliteJournal::open(path).unwrap();
    Host::new(tenants(), snapshot, Box::new(journal))
}

fn admitted(h: &mut Host, job: &str, seq: u64) {
    let (id, _) = h.claim(claims(job), MS).unwrap();
    h.delivered(id).unwrap();
    h.event(
        "tenant",
        &event(seq, TenantEventKind::Admitted, Some(job), None),
        MS,
    )
    .unwrap();
}

#[test]
fn bounded_drain_fixes_latest_started_or_unstarted_finish_and_never_extends() {
    for state in [
        motor_host::state::LeaseState::Reserved,
        motor_host::state::LeaseState::Admitted,
    ] {
        let mut h = host();
        admitted(&mut h, JOB, 1);
        h.event(
            "tenant",
            &event(2, TenantEventKind::Started, Some(JOB), None),
            MS + 123,
        )
        .unwrap();
        let mut queued = claims(JOB2);
        queued.max_s = 40;
        h.claim(queued, MS + 200).unwrap();
        h.snapshot.leases.get_mut(JOB2).unwrap().state = state;
        assert!(h.begin_bounded_drain(MS + 1000).unwrap());
        assert_eq!(h.snapshot.accept_until_ms, Some(MS + 41_000));
        assert!(!h.snapshot.draining);
        assert!(!h.begin_bounded_drain(MS + 90_000).unwrap());
        assert_eq!(h.snapshot.accept_until_ms, Some(MS + 41_000));
    }
    let mut h = host();
    admitted(&mut h, JOB, 1);
    h.event(
        "tenant",
        &event(2, TenantEventKind::Started, Some(JOB), None),
        MS + 123,
    )
    .unwrap();
    h.begin_bounded_drain(MS + 1000).unwrap();
    assert_eq!(h.snapshot.accept_until_ms, Some(MS + 30_123));
    h.cancel_bounded_drain().unwrap();
    h.begin_bounded_drain(MS + 40_000).unwrap();
    assert_eq!(
        h.snapshot.accept_until_ms,
        Some(MS + 40_000),
        "floor at now"
    );
}

#[test]
fn bounded_drain_no_leases_immediately_becomes_full_drain() {
    let mut h = host();
    assert!(h.begin_bounded_drain(MS).unwrap());
    assert!(h.snapshot.draining);
    assert_eq!(h.snapshot.accept_until_ms, None);
    assert!(!h.begin_bounded_drain(MS + 1000).unwrap());
    assert!(!h.cancel_bounded_drain().unwrap());
    assert!(h.snapshot.draining);
    assert!(
        h.claim(claims(JOB), MS).is_ok(),
        "full drain retains canary admission"
    );
}

#[test]
fn full_drain_takes_precedence_over_a_retained_bound() {
    let mut h = host();
    h.snapshot.draining = true;
    h.snapshot.accept_until_ms = Some(MS);
    assert!(h.claim(claims(JOB), MS + 1).is_ok());
    assert_eq!(h.snapshot.accept_until_ms, None);
    assert!(h.snapshot.draining);
}

#[test]
fn bounded_drain_persists_promotes_atomically_and_keeps_canary_admission() {
    let dir = scratch();
    let mut h = sqlite_host(dir.path());
    admitted(&mut h, JOB, 1);
    h.begin_bounded_drain(MS).unwrap();
    drop(h);
    let mut h = sqlite_host(dir.path());
    assert_eq!(h.snapshot.accept_until_ms, Some(MS + 30_000));
    assert!(!h.snapshot.draining);
    assert_eq!(
        h.heartbeat(MS + 10, 0, true).accept_until_ms,
        Some(MS + 30_000)
    );
    let stored = Arc::new(Mutex::new(Vec::new()));
    h.replace_journal(Box::new(MemoryJournal(stored.clone())));
    h.event(
        "tenant",
        &event(2, TenantEventKind::Terminal, Some(JOB), None),
        MS + 1000,
    )
    .unwrap();
    let writes = stored.lock().unwrap();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].leases[JOB].state,
        motor_host::state::LeaseState::Ended
    );
    assert!(writes[0].draining);
    assert_eq!(writes[0].accept_until_ms, None);
    drop(writes);
    // Persist the same transition through SQLite and reopen the actual journal.
    drop(h);
    let mut h = sqlite_host(dir.path());
    h.event(
        "tenant",
        &event(2, TenantEventKind::Terminal, Some(JOB), None),
        MS + 1000,
    )
    .unwrap();
    drop(h);
    let mut h = sqlite_host(dir.path());
    assert!(h.snapshot.draining);
    assert_eq!(h.snapshot.accept_until_ms, None);
    assert!(!h.cancel_bounded_drain().unwrap());
    assert!(h.claim(claims(JOB2), MS + 1001).is_ok());
}

#[test]
fn bounded_drain_refuses_just_past_deadline_accepts_inside_and_allows_attach() {
    for settings in host_settings() {
        for delta in [-1i64, 0, 1] {
            let mut h = host();
            h.settings = settings.clone();
            if matches!(settings, motor_host::config::HostSettings::PerJob(_)) {
                h.record_host_resources(motor_host::state::jobs::HostResources {
                    at_ms: MS,
                    available_mem_mib: 100_000,
                    vcpu_ceiling: 2,
                    permit_budget: 4,
                });
            }
            h.claim(claims(JOB), MS).unwrap();
            h.begin_bounded_drain(MS).unwrap();
            let mut short = claims(JOB2);
            short.max_s = 29;
            let result = h.claim(short, (MS as i64 + 1000 + delta) as u64);
            if delta > 0 {
                assert_eq!(
                    result.unwrap_err().refusal(),
                    motor_host::state::Refusal::Capacity
                );
                assert!(!h.snapshot.leases.contains_key(JOB2));
            } else {
                assert!(result.is_ok(), "{result:?}");
            }
        }
    }
    let mut h = host();
    admitted(&mut h, JOB, 1);
    h.close(1).unwrap();
    h.begin_bounded_drain(MS).unwrap();
    let mut attach = claims(JOB);
    attach.purpose = Purpose::Attach;
    assert!(h.claim(attach, MS + 60_000).is_ok());
}

#[test]
fn bounded_drain_cancel_and_hard_drain_clear_bound_durably() {
    let dir = scratch();
    let mut h = sqlite_host(dir.path());
    admitted(&mut h, JOB, 1);
    h.begin_bounded_drain(MS).unwrap();
    assert!(h.cancel_bounded_drain().unwrap());
    drop(h);
    let mut h = sqlite_host(dir.path());
    assert_eq!(h.snapshot.accept_until_ms, None);
    assert!(!h.snapshot.draining);
    assert_eq!(h.heartbeat(MS, 0, true).accept_until_ms, None);
    h.begin_bounded_drain(MS).unwrap();
    h.set_draining(true).unwrap();
    drop(h);
    let mut h = sqlite_host(dir.path());
    assert!(h.snapshot.draining);
    assert_eq!(h.snapshot.accept_until_ms, None);
    assert!(!h.cancel_bounded_drain().unwrap());
    h.set_draining(false).unwrap();
    assert!(!h.snapshot.draining);
}

#[test]
fn bounded_drain_and_promotion_are_not_published_if_the_journal_fails() {
    struct Full;
    impl Journal for Full {
        fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::other("disk full"))
        }
    }
    let mut h = host();
    admitted(&mut h, JOB, 1);
    let original = h.snapshot.clone();
    h.replace_journal(Box::new(Full));
    assert!(h.begin_bounded_drain(MS).is_err());
    assert_eq!(h.snapshot, original);
    h.replace_journal(Box::<MemoryJournal>::default());
    h.begin_bounded_drain(MS).unwrap();
    let bounded = h.snapshot.clone();
    h.replace_journal(Box::new(Full));
    assert!(h
        .event(
            "tenant",
            &event(2, TenantEventKind::Terminal, Some(JOB), None),
            MS + 1
        )
        .is_err());
    assert_eq!(h.snapshot, bounded);
    assert!(!h.snapshot.draining);
    assert!(h.cancel_bounded_drain().is_err());
    assert_eq!(h.snapshot, bounded);
}
