use super::*;
use motor_host::state::{Admission, Error, LeaseState, Refusal};
use motor_host::wire::heartbeat_violation;

fn permit_host() -> Host {
    let mut h = host();
    let tenant = h.tenants.get_mut("vm").unwrap();
    tenant.vcpu_ceiling = 24;
    tenant.max_weight = Some(64);
    assert_eq!(tenant.budget(), 64);
    h
}

fn refused<T: std::fmt::Debug>(result: Result<T, Error>, reason: Refusal) {
    assert!(
        matches!(result, Err(Error::Refused(r)) if r == reason),
        "{result:?}"
    );
}

#[test]
fn permit_admission_accepts_exact_budget_but_not_an_oversized_claim() {
    let mut h = permit_host();
    let now = NOW * 1000;
    let mut oversized = claims(JOB);
    oversized.vcpus = 26;
    refused(h.claim(oversized, now), Refusal::HostFull);
    assert!(h.snapshot.leases.is_empty());

    for (job, weight, total) in [
        (JOB, 24, 24),
        (JOB2, 6, 30),
        (JOB3, 24, 54),
        ("00000000-0000-7000-8000-000000000004", 10, 64),
    ] {
        let mut c = claims(job);
        c.vcpus = weight;
        h.claim(c, now).unwrap();
        assert_eq!(h.share("tenant", now), total);
        assert_eq!(h.pending(job).unwrap(), Admission::Ready);
    }
    refused(
        h.claim(claims("00000000-0000-7000-8000-000000000005"), now),
        Refusal::HostFull,
    );
    assert_eq!(h.snapshot.leases.len(), 4);
    let mut replacement = h.snapshot.leases[JOB].claims.clone();
    replacement.iat += 1;
    replacement.exp += 1;
    h.claim(replacement, now).unwrap();
    assert_eq!(h.share("tenant", now), 64);
}

#[test]
fn permit_admission_fails_closed_on_counted_weight_overflow() {
    let mut h = permit_host();
    let now = NOW * 1000;
    h.claim(claims(JOB), now).unwrap();
    h.claim(claims(JOB2), now).unwrap();
    // A damaged or older journal must not wrap the weight back into capacity.
    h.snapshot.leases.get_mut(JOB).unwrap().claims.vcpus = u32::MAX;
    h.tenants.get_mut("vm").unwrap().max_weight = Some(u32::MAX);
    refused(h.claim(claims(JOB3), now), Refusal::HostFull);
    refused(h.pending(JOB2), Refusal::HostFull);
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 0);
}

#[test]
fn host_lease_cap_retains_every_non_ended_job_and_allows_reconnects() {
    let mut h = host();
    h.tenants.get_mut("vm").unwrap().max_weight = Some(64);
    let mut second = h.tenants["vm"].clone();
    second.tenant = "tenant-2".into();
    second.sub = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAW".into();
    h.tenants.insert("vm-2".into(), second);
    let now = NOW * 1000;
    let mut ids = Vec::new();
    for n in 1..=64 {
        let job = format!("00000000-0000-7000-8000-{n:012x}");
        let mut c = claims(&job);
        if n % 2 == 0 {
            c.sub = h.tenants["vm-2"].sub.clone();
            c.placement.vm = Some("vm-2".into());
        }
        let (id, _) = h.claim(c, now).unwrap();
        ids.push((id, job.clone()));
        // Released entries no longer consume weight, but the broker still
        // requires them in `running` until they become Ended.
        h.close(id).unwrap();
        let heartbeat = h.heartbeat(now, 123, true);
        assert_eq!(heartbeat.running.len(), n as usize);
        assert_eq!(heartbeat_violation(&heartbeat), None);
    }
    // A Started lease past its weight deadline also retains its wire entry.
    let lease = h.snapshot.leases.get_mut(JOB2).unwrap();
    lease.state = LeaseState::Started;
    lease.started_at_ms = Some(now - 30_000);
    refused(
        h.claim(claims("00000000-0000-7000-8000-000000000065"), now),
        Refusal::HostFull,
    );
    h.claim(claims(JOB), now).unwrap();
    let mut newer = claims(JOB);
    newer.iat += 1;
    newer.exp += 1;
    h.claim(newer, now).unwrap();
    let mut attach = claims(JOB2);
    attach.sub = h.tenants["vm-2"].sub.clone();
    attach.placement.vm = Some("vm-2".into());
    attach.purpose = Purpose::Attach;
    h.claim(attach, now).unwrap();
    let heartbeat = h.heartbeat(now, 123, true);
    assert_eq!(heartbeat_violation(&heartbeat), None);
    assert_eq!(heartbeat.running.len(), 64);
    assert_eq!(
        heartbeat
            .running
            .iter()
            .map(|r| r.job_id.as_str())
            .collect::<Vec<_>>(),
        ids.iter().map(|(_, job)| job.as_str()).collect::<Vec<_>>()
    );

    h.event(
        "tenant-2",
        &event(1, TenantEventKind::Terminal, Some(JOB2), None),
        now,
    )
    .unwrap();
    h.claim(claims("00000000-0000-7000-8000-000000000065"), now)
        .unwrap();
    assert_eq!(h.snapshot.leases.len(), 65);
    let heartbeat = h.heartbeat(now, 123, true);
    assert_eq!(heartbeat.running.len(), 64);
    assert!(!heartbeat.running.iter().any(|r| r.job_id == JOB2));
    assert_eq!(heartbeat_violation(&heartbeat), None);
}

#[test]
fn heartbeat_advertises_free_permits_while_stopped_running_and_waking() {
    let mut h = permit_host();
    let now = NOW * 1000;
    let mut first = claims(JOB);
    first.vcpus = 24;
    h.claim(first, now).unwrap();
    let mut second = claims(JOB2);
    second.vcpus = 6;
    h.claim(second, now).unwrap();
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 34);
    h.guest_ready("tenant", 1).unwrap();
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 34);
    h.guest_unready("tenant");
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 34);
    h.guest_ready("tenant", 2).unwrap();
    let mut third = claims(JOB3);
    third.vcpus = 24;
    h.claim(third, now).unwrap();
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 10);
}

#[test]
fn keyless_single_tenant_heartbeat_bytes_and_budget_are_unchanged() {
    let mut h = host();
    let now = NOW * 1000;
    let serialized = serde_json::to_value(&h.tenants["vm"]).unwrap();
    assert!(serialized.get("max_weight").is_none());
    let legacy: Tenant = serde_json::from_value(serialized).unwrap();
    assert_eq!(legacy.budget(), 2);
    assert_eq!(
        serde_json::to_vec(&h.heartbeat(now, 123, true)).unwrap(),
        br#"{"free_vcpus":2,"free_mem_mib":123,"queued":0,"warm":[],"running":[],"healthy":true}"#
    );
    h.claim(claims(JOB), now).unwrap();
    assert_eq!(serde_json::to_vec(&h.heartbeat(now, 123, true)).unwrap(),
        br#"{"free_vcpus":1,"free_mem_mib":123,"queued":1,"warm":[],"running":[{"job_id":"00000000-0000-7000-8000-000000000001","state":"reserved","elapsed_ms":0}],"healthy":true}"#);
    h.claim(claims(JOB2), now).unwrap();
    refused(h.claim(claims(JOB3), now), Refusal::HostFull);
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 0);
}

#[test]
fn host_heartbeat_uses_the_largest_tenant_capacity_not_the_sum() {
    let mut h = host();
    h.tenants.get_mut("vm").unwrap().vcpu_ceiling = 8;
    let mut second = h.tenants["vm"].clone();
    second.tenant = "tenant-2".into();
    h.tenants.insert("vm-2".into(), second);
    let now = NOW * 1000;
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 8);
    h.claim(claims(JOB), now).unwrap();
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 8);
    h.tenants.get_mut("vm-2").unwrap().max_weight = Some(64);
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 64);
    h.tenants.clear();
    assert_eq!(h.heartbeat(now, 123, true).free_vcpus, 0);
}

#[test]
fn largest_permit_budget_produces_a_valid_idle_heartbeat() {
    let mut h = permit_host();
    h.tenants.get_mut("vm").unwrap().max_weight = Some(u32::MAX);
    let heartbeat = h.heartbeat(NOW * 1000, 123, true);
    assert_eq!(heartbeat.free_vcpus, 4_294_967_295);
    assert_eq!(heartbeat_violation(&heartbeat), None);
}

#[test]
fn permit_quota_is_one_after_last_release_or_counted_claim_expiry() {
    let mut h = permit_host();
    let now = NOW * 1000;
    assert_eq!(h.quota("tenant", now), 1);
    h.guest_ready("tenant", 1).unwrap();
    let (id, _) = h.claim(claims(JOB), now).unwrap();
    assert_eq!(h.quota("tenant", now), 1);
    h.close(id).unwrap();
    assert_eq!(h.quota("tenant", now), 1);
    let mut seq = 0;
    for (job, weight) in [
        (JOB2, 24),
        (JOB3, 24),
        ("00000000-0000-7000-8000-000000000004", 16),
    ] {
        let mut c = claims(job);
        c.vcpus = weight;
        let (id, _) = h.claim(c, now).unwrap();
        h.delivered(id).unwrap();
        for kind in [TenantEventKind::Admitted, TenantEventKind::Started] {
            seq += 1;
            h.event("tenant", &event(seq, kind, Some(job), None), now)
                .unwrap();
        }
    }
    assert_eq!(h.share("tenant", now), 64);
    assert_eq!(h.quota("tenant", now), 24);
    assert_eq!(h.quota("tenant", now + 29_999), 24);
    assert_eq!(h.share("tenant", now + 30_000), 0);
    assert_eq!(h.quota("tenant", now + 30_000), 1);
    h.expire(now + 30_000).unwrap();
    assert_eq!(h.quota("tenant", now + 30_000), 1);
}
