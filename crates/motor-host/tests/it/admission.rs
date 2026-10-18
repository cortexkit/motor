use super::*;
use motor_host::state::{Admission, Error, LeaseState, Refusal};

fn refused<T: std::fmt::Debug>(result: Result<T, Error>, reason: Refusal) {
    assert!(
        matches!(result, Err(Error::Refused(r)) if r == reason),
        "{result:?}"
    );
}
#[test]
fn binding_refuses_unknown_vm_cross_tenant_and_changed_job_binding_without_mutation() {
    let mut h = host();
    let mut bad = claims(JOB);
    bad.placement.vm = Some("unknown".into());
    refused(h.claim(bad, NOW * 1000), Refusal::Auth);
    let mut bad = claims(JOB);
    bad.sub = "org:01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
    refused(h.claim(bad, NOW * 1000), Refusal::Auth);
    assert!(h.snapshot.leases.is_empty());
    h.claim(claims(JOB), NOW * 1000).unwrap();
    let mut changed = claims(JOB);
    changed.placement.vm = Some("other".into());
    h.tenants.insert(
        "other".into(),
        Tenant {
            tenant: "other".into(),
            ..tenants()["vm"].clone()
        },
    );
    refused(h.claim(changed, NOW * 1000), Refusal::Auth);
}
#[test]
fn every_run_checks_host_full_without_summing_memory_caps_and_attach_is_free() {
    let mut h = host();
    h.guest_ready("tenant", 1).unwrap();
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.delivered(id).unwrap();
    h.event(
        "tenant",
        &event(1, TenantEventKind::Admitted, Some(JOB), None),
        NOW * 1000,
    )
    .unwrap();
    h.claim(claims(JOB2), NOW * 1000).unwrap();
    assert_eq!(h.quota("tenant", NOW * 1000), 2);
    refused(h.claim(claims(JOB3), NOW * 1000), Refusal::HostFull);
    assert_eq!(h.quota("tenant", NOW * 1000), 2);
    assert_eq!(h.snapshot.leases.len(), 2);
    let mut attach = claims(JOB);
    attach.purpose = Purpose::Attach;
    let (attach_id, _) = h.claim(attach.clone(), NOW * 1000).unwrap();
    refused(h.claim(attach, NOW * 1000), Refusal::Auth);
    h.close(attach_id).unwrap();
    assert!(h.snapshot.zero_usage.is_empty());
    assert_eq!(h.share("tenant", NOW * 1000), 2);
}
#[test]
fn strictly_newer_replacement_waits_for_refused_and_counts_share_once() {
    let mut h = host();
    let (old, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.delivered(old).unwrap();
    refused(h.claim(claims(JOB), NOW * 1000), Refusal::Auth);
    assert!(h.connection_live(old));
    let mut newer = claims(JOB);
    newer.iat += 1;
    newer.exp += 1;
    let (new, status) = h.claim(newer.clone(), NOW * 1000).unwrap();
    assert_eq!(status, Admission::Waiting);
    assert!(!h.connection_live(old));
    h.close(old).unwrap();
    assert_eq!(h.share("tenant", NOW * 1000), 1);
    refused(h.claim(claims(JOB), NOW * 1000), Refusal::Auth);
    h.event(
        "tenant",
        &event(
            1,
            TenantEventKind::Refused,
            Some(JOB),
            Some(RefusedCause::Disconnect),
        ),
        NOW * 1000,
    )
    .unwrap();
    assert_eq!(h.pending(JOB).unwrap(), Admission::Ready);
    h.delivered(new).unwrap();
    h.event(
        "tenant",
        &event(2, TenantEventKind::Admitted, Some(JOB), None),
        NOW * 1000,
    )
    .unwrap();
    newer.iat += 1;
    newer.exp += 1;
    refused(h.claim(newer, NOW * 1000), Refusal::Auth);
    assert!(h.snapshot.zero_usage.is_empty());
}
#[test]
fn delayed_admission_wins_over_replacement_disconnect_and_expiry() {
    let mut h = host();
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.delivered(id).unwrap();
    let mut newer = claims(JOB);
    newer.iat += 1;
    newer.exp += 1;
    h.claim(newer, NOW * 1000).unwrap();
    h.events_connected("tenant", Some(NOW * 1000));
    h.expire((NOW + 301) * 1000).unwrap();
    assert_eq!(h.share("tenant", (NOW + 301) * 1000), 1);
    h.event(
        "tenant",
        &event(1, TenantEventKind::Admitted, Some(JOB), None),
        (NOW + 302) * 1000,
    )
    .unwrap();
    h.expire((NOW + 359) * 1000).unwrap();
    assert_eq!(h.snapshot.leases[JOB].state, LeaseState::Admitted);
    assert!(h.snapshot.zero_usage.is_empty());
    refused(h.pending(JOB), Refusal::Auth);
}
#[test]
fn delivered_disconnect_requires_matching_refused_and_reconnect_does_not_settle_zero() {
    let mut h = host();
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.delivered(id).unwrap();
    h.close(id).unwrap();
    h.event(
        "tenant",
        &event(
            1,
            TenantEventKind::Refused,
            Some(JOB2),
            Some(RefusedCause::Disconnect),
        ),
        NOW * 1000,
    )
    .unwrap();
    assert_eq!(h.share("tenant", NOW * 1000), 1);
    h.event(
        "tenant",
        &event(
            2,
            TenantEventKind::Refused,
            Some(JOB),
            Some(RefusedCause::Disconnect),
        ),
        NOW * 1000,
    )
    .unwrap();
    assert_eq!(h.snapshot.leases[JOB].state, LeaseState::Released);
    assert_eq!(h.share("tenant", NOW * 1000), 0);
    h.claim(claims(JOB), NOW * 1000).unwrap();
    assert!(h.snapshot.zero_usage.is_empty());
}
#[test]
fn close_before_delivery_releases_without_event_and_expiry_settles_once() {
    let mut h = host();
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.close(id).unwrap();
    assert_eq!(h.share("tenant", NOW * 1000), 0);
    assert!(h.snapshot.zero_usage.is_empty());
    h.expire((NOW + 300) * 1000).unwrap();
    h.expire((NOW + 700) * 1000).unwrap();
    assert_eq!(h.snapshot.zero_usage.len(), 1);
    let report = &h.snapshot.zero_usage[JOB];
    assert_eq!(report.started_at_ms, report.ended_at_ms);
}
#[test]
fn delivered_expiry_needs_continuous_event_stream_and_all_refusal_causes_are_ordered() {
    for cause in [
        RefusedCause::Mismatch,
        RefusedCause::Rejected,
        RefusedCause::Expired,
    ] {
        let mut h = host();
        let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
        h.delivered(id).unwrap();
        h.event(
            "tenant",
            &event(1, TenantEventKind::Refused, Some(JOB), Some(cause)),
            NOW * 1000,
        )
        .unwrap();
        assert_eq!(h.snapshot.leases[JOB].state, LeaseState::Ended);
        assert_eq!(h.snapshot.zero_usage.len(), 1);
    }
    let mut c = claims(JOB);
    c.exp = NOW + 120;
    let mut h = host();
    let (id, _) = h.claim(c.clone(), NOW * 1000).unwrap();
    h.delivered(id).unwrap();
    h.expire((NOW + 180) * 1000).unwrap();
    assert!(h.snapshot.zero_usage.is_empty());
    h.events_connected("tenant", Some((NOW + 121) * 1000));
    h.expire((NOW + 200) * 1000).unwrap();
    assert!(h.snapshot.zero_usage.is_empty());
    let mut h = host();
    let (id, _) = h.claim(c, NOW * 1000).unwrap();
    h.delivered(id).unwrap();
    h.events_connected("tenant", Some(NOW * 1000));
    h.expire((NOW + 179) * 1000).unwrap();
    assert!(h.snapshot.zero_usage.is_empty());
    h.expire((NOW + 180) * 1000).unwrap();
    assert_eq!(h.snapshot.zero_usage.len(), 1);
}
#[test]
fn quota_stays_one_until_guest_ready_and_deadline_exclusion_survives_restart() {
    let directory = scratch();
    let mut h = disk_host(directory.path());
    let mut c = claims(JOB);
    c.vcpus = 2;
    c.mem_mib = 16_384;
    let (id, _) = h.claim(c, NOW * 1000).unwrap();
    assert_eq!(h.quota("tenant", NOW * 1000), 1);
    h.guest_ready("tenant", 7).unwrap();
    assert_eq!(h.quota("tenant", NOW * 1000), 2);
    h.delivered(id).unwrap();
    h.event(
        "tenant",
        &event(1, TenantEventKind::Admitted, Some(JOB), None),
        NOW * 1000,
    )
    .unwrap();
    h.event(
        "tenant",
        &event(2, TenantEventKind::Started, Some(JOB), None),
        NOW * 1000,
    )
    .unwrap();
    assert_eq!(h.quota("tenant", (NOW + 30) * 1000), 1);
    drop(h);
    let mut h = disk_host(directory.path());
    h.guest_ready("tenant", 7).unwrap();
    assert_eq!(h.quota("tenant", (NOW + 31) * 1000), 1);
    assert_eq!(h.snapshot.vms["tenant"].initialized_generation, Some(7));
}
#[test]
fn idle_requires_runner_idle_no_relay_reserved_or_prelude_and_exact_byte_floor() {
    let mut h = host();
    h.guest_ready("tenant", 1).unwrap();
    let required = 18432 * 1_048_576 + 2147483648;
    assert!(!h.suspend_due("tenant", 0, required));
    h.event("tenant", &event(1, TenantEventKind::Idle, None, None), 0)
        .unwrap();
    assert!(!h.suspend_due("tenant", 0, required));
    assert!(!h.suspend_due("tenant", 60_000, required - 1));
    assert!(h.suspend_due("tenant", 60_000, required));
    h.prelude("tenant", true);
    assert!(!h.suspend_due("tenant", 120_000, required));
    h.prelude("tenant", false);
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    assert!(!h.suspend_due("tenant", NOW * 1000, required));
    h.close(id).unwrap();
    assert!(!h.suspend_due("tenant", NOW * 1000, required));
    assert!(h.suspend_due("tenant", (NOW + 60) * 1000, required));
}
#[test]
fn fsync_snapshot_retains_nonce_binding_state_and_highest_sequence() {
    let directory = scratch();
    let mut h = disk_host(directory.path());
    h.consume_nonce("nonce".into(), NOW, NOW).unwrap();
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.delivered(id).unwrap();
    h.watermark(JOB, 41).unwrap();
    h.watermark(JOB, 9).unwrap();
    drop(h);
    let mut h = disk_host(directory.path());
    assert_eq!(h.snapshot.leases[JOB].highest_seq, 41);
    assert!(h.snapshot.leases[JOB].delivered);
    refused(h.consume_nonce("nonce".into(), NOW, NOW), Refusal::Auth);
}
#[test]
fn failed_journal_never_publishes_reservation_or_watermark() {
    struct Failed;
    impl Journal for Failed {
        fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::other("injected fsync failure"))
        }
    }
    let mut h = Host::new(tenants(), Snapshot::default(), Box::new(Failed));
    assert!(matches!(
        h.claim(claims(JOB), NOW * 1000),
        Err(Error::Journal(_))
    ));
    assert!(h.snapshot.leases.is_empty());
    let mut base = host();
    base.claim(claims(JOB), NOW * 1000).unwrap();
    let mut h = Host::new(tenants(), base.snapshot, Box::new(Failed));
    assert!(h.watermark(JOB, 99).is_err());
    assert_eq!(h.snapshot.leases[JOB].highest_seq, 0);
}

#[test]
fn admitted_unstarted_deadline_persists_zero_and_cancel_until_acknowledged() {
    let directory = scratch();
    let mut h = disk_host(directory.path());
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.delivered(id).unwrap();
    h.event(
        "tenant",
        &event(1, TenantEventKind::Admitted, Some(JOB), None),
        NOW * 1000,
    )
    .unwrap();
    assert!(h.expire((NOW + 359) * 1000).unwrap().is_empty());
    assert_eq!(
        h.expire((NOW + 360) * 1000).unwrap(),
        vec![("tenant".into(), JOB.into())]
    );
    drop(h);
    let mut h = disk_host(directory.path());
    assert_eq!(h.expire((NOW + 361) * 1000).unwrap().len(), 1);
    let report = &h.snapshot.zero_usage[JOB];
    assert_eq!(report.started_at_ms, report.ended_at_ms);
    h.cancel_delivered(JOB).unwrap();
    assert!(h.expire((NOW + 362) * 1000).unwrap().is_empty());
    assert_eq!(h.snapshot.zero_usage.len(), 1);
}

#[test]
fn abandoned_replacement_releases_on_old_refusal_and_terminal_does_not_cut_off_relay() {
    let mut h = host();
    let (old, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.delivered(old).unwrap();
    let mut newer = claims(JOB);
    newer.iat += 1;
    newer.exp += 1;
    let (new, _) = h.claim(newer.clone(), NOW * 1000).unwrap();
    h.close(new).unwrap();
    assert_eq!(h.share("tenant", NOW * 1000), 1);
    h.event(
        "tenant",
        &event(
            1,
            TenantEventKind::Refused,
            Some(JOB),
            Some(RefusedCause::Disconnect),
        ),
        NOW * 1000,
    )
    .unwrap();
    assert_eq!(h.share("tenant", NOW * 1000), 0);
    let (new, _) = h.claim(newer, NOW * 1000).unwrap();
    h.delivered(new).unwrap();
    h.event(
        "tenant",
        &event(2, TenantEventKind::Admitted, Some(JOB), None),
        NOW * 1000,
    )
    .unwrap();
    h.event(
        "tenant",
        &event(3, TenantEventKind::Started, Some(JOB), None),
        NOW * 1000,
    )
    .unwrap();
    h.event(
        "tenant",
        &event(4, TenantEventKind::Terminal, Some(JOB), None),
        NOW * 1000,
    )
    .unwrap();
    h.expire((NOW + 700) * 1000).unwrap();
    assert!(h.connection_live(new));
}

#[test]
fn absent_lease_access_is_typed_and_connection_cleanup_is_idempotent() {
    let mut h = host();
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.snapshot.leases.remove(JOB);
    refused(h.pending(JOB), Refusal::Auth);
    refused(h.delivered(id), Refusal::Auth);
    refused(h.watermark(JOB, 1), Refusal::Auth);
    h.close(id).unwrap();
    h.close(id).unwrap();
    assert!(!h.connection_live(id));
    h.claim(claims(JOB2), NOW * 1000).unwrap();
}

#[test]
fn connection_watch_preserves_close_and_durable_refusal_causes() {
    use motor_host::state::ConnectionState;
    let mut h = host();
    let (old, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    h.delivered(old).unwrap();
    let old_updates = h.watch_connection(old).unwrap();
    let mut c = claims(JOB);
    c.iat += 1;
    c.exp += 1;
    let (new, _) = h.claim(c, NOW * 1000).unwrap();
    assert_eq!(*old_updates.borrow(), ConnectionState::Closed);
    let updates = h.watch_connection(new).unwrap();
    assert_eq!(*updates.borrow(), ConnectionState::Waiting);
    h.expire((NOW + 362) * 1000).unwrap();
    assert_eq!(
        *updates.borrow(),
        ConnectionState::Refused(Refusal::Expired)
    );
    refused(h.pending(JOB), Refusal::Expired);
    h.close(new).unwrap();
    assert_eq!(
        *updates.borrow(),
        ConnectionState::Refused(Refusal::Expired)
    );
}

#[test]
fn connection_watch_tracks_lifecycle_but_not_unchanged_watermark_writes() {
    use motor_host::state::ConnectionState;
    let mut h = host();
    let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
    let mut updates = h.watch_connection(id).unwrap();
    updates.borrow_and_update();
    h.delivered(id).unwrap();
    assert_eq!(
        *updates.borrow_and_update(),
        ConnectionState::Live(LeaseState::Reserved)
    );
    h.watermark(JOB, 17).unwrap();
    assert!(!updates.has_changed().unwrap());
    for (seq, kind, state) in [
        (1, TenantEventKind::Admitted, LeaseState::Admitted),
        (2, TenantEventKind::Started, LeaseState::Started),
        (3, TenantEventKind::Terminal, LeaseState::Ended),
    ] {
        h.event("tenant", &event(seq, kind, Some(JOB), None), NOW * 1000)
            .unwrap();
        assert!(updates.has_changed().unwrap());
        assert_eq!(*updates.borrow_and_update(), ConnectionState::Live(state));
    }
    h.close(id).unwrap();
    assert_eq!(*updates.borrow(), ConnectionState::Closed);
}
