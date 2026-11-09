use super::*;
use motor_host::state::jobs::{
    recovery::{Phase, Recovery},
    WorkspaceDisk,
};

// The broker's per-job amendment changes meanings, not JSON shape. The mirrored
// contract below has no new vectors, so literal bodies pin the host's interpretation.
const CONTRACT: &str = include_str!("../../../../motor-lease/contracts/cloud/runners-contract.md");
const TENANT: &str = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
const HASH: &str = "BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU";

fn bytes(h: &mut Host, now: u64) -> String {
    let memory = h.free_mem_mib(now);
    let heartbeat = h.heartbeat(now, memory, true);
    assert!(motor_host::wire::heartbeat_violation(&heartbeat).is_none());
    String::from_utf8(serde_json::to_vec(&heartbeat).unwrap()).unwrap()
}

fn disk(h: &mut Host, id: &str, used: u64) {
    h.snapshot.per_job.disks.insert(
        id.into(),
        WorkspaceDisk {
            storage: Some(motor_host::state::jobs::storage::Disk::new(1024, used)),
            sub: TENANT.into(),
            workspace_key_hash: HASH.into(),
            queue: vec![],
            attached_job: None,
            quarantined: false,
            recovery: None,
            recovery_result: None,
            deleted: false,
            have: vec![],
        },
    );
}

fn warm(id: &str, busy: bool) -> String {
    format!(
        r#"{{"tenant":"{TENANT}","workspace_key_hash":"{HASH}","vm":"{id}"{}}}"#,
        if busy { ",\"busy\":true" } else { "" }
    )
}

fn body(cpu: u64, mem: u64, queued: u64, warm: &str, running: &str) -> String {
    format!(
        r#"{{"free_vcpus":{cpu},"free_mem_mib":{mem},"queued":{queued},"warm":[{warm}],"running":[{running}],"healthy":true}}"#
    )
}

#[test]
fn amended_contract_cold_idle_busy_quarantine_and_recovery_bytes() {
    assert!(CONTRACT.contains("#### Hosts in per-job mode"));
    assert!(CONTRACT.contains("`tenant_disk` is omitted"));
    let mut h = memory_host(1000);
    assert_eq!(bytes(&mut h, AT), body(4, 950, 0, "", ""));
    disk(&mut h, "disk-1", AT);
    assert_eq!(
        bytes(&mut h, AT),
        body(4, 950, 0, &warm("disk-1", false), "")
    );
    h.snapshot
        .per_job
        .disks
        .get_mut("disk-1")
        .unwrap()
        .quarantined = true;
    assert_eq!(
        bytes(&mut h, AT),
        body(4, 950, 0, &warm("disk-1", true), "")
    );
    h.snapshot.per_job.disks.get_mut("disk-1").unwrap().recovery = Some(Recovery {
        id: JOB.into(),
        memory_debit_mib: 120,
        guest_ram_mib: 134,
        started_at_ms: Some(AT),
        phase: Phase::Running,
    });
    assert_eq!(
        bytes(&mut h, AT),
        body(3, 830, 0, &warm("disk-1", true), "")
    );
    h.snapshot.per_job.disks.get_mut("disk-1").unwrap().recovery = None;
    h.snapshot
        .per_job
        .disks
        .get_mut("disk-1")
        .unwrap()
        .quarantined = false;
    let mut c = job(JOB);
    c.placement.vm = Some("disk-1".into());
    h.claim(c, AT).unwrap();
    h.begin_job_boot(JOB).unwrap().unwrap();
    let running = format!(r#"{{"job_id":"{JOB}","state":"reserved","elapsed_ms":0}}"#);
    assert_eq!(
        bytes(&mut h, AT),
        body(3, 830, 1, &warm("disk-1", true), &running)
    );
    // Power-on is not runner readiness; only authenticated hello releases the debit.
    h.job_runner_hello(JOB).unwrap();
    assert_eq!(
        bytes(&mut h, AT),
        body(3, 950, 1, &warm("disk-1", true), &running)
    );
    h.job_runner_hello(JOB).unwrap();
    assert_eq!(
        bytes(&mut h, AT),
        body(3, 950, 1, &warm("disk-1", true), &running)
    );
}

#[test]
fn amended_contract_stale_missing_capacity_storage_and_negative_memory_bytes() {
    let mut h = memory_host(49);
    assert_eq!(bytes(&mut h, AT), body(4, 0, 0, "", ""));
    sample(&mut h, 1000);
    assert_eq!(
        bytes(&mut h, AT + motor_host::state::MEMORY_FRESH_MS + 1),
        body(0, 0, 0, "", "")
    );
    assert_eq!(bytes(&mut h, AT - 1), body(0, 0, 0, "", ""));
    let mut h = configure(host(), 1000);
    h.enforce_storage();
    h.observe_storage(Err("no capacity setting".into()), Ok(u64::MAX));
    // No trusted host sample exists after a restart, even with retained disks.
    let mut restarted = Host::new(
        h.tenants.clone(),
        h.snapshot.clone(),
        Box::<MemoryJournal>::default(),
    );
    restarted.settings = h.settings.clone();
    restarted.enforce_storage();
    assert_eq!(bytes(&mut restarted, AT), body(0, 0, 0, "", ""));
    // Storage refusal zeros CPU independently of a still-fresh host memory sample.
    assert_eq!(bytes(&mut h, AT), body(0, 950, 0, "", ""));
}

#[test]
fn unknown_hint_reuses_idle_disk_and_busy_hint_or_unknown_hint_queues() {
    let mut h = memory_host(10_000);
    disk(&mut h, "disk-1", AT);
    for (id, hint) in [(JOB, "unknown"), (JOB2, "disk-1"), (JOB3, "unknown")] {
        let mut c = job(id);
        c.placement.vm = Some(hint.into());
        h.claim(c, AT).unwrap();
        assert_eq!(h.snapshot.per_job.executions[id].disk_id, "disk-1");
        if id == JOB {
            h.begin_job_boot(id).unwrap().unwrap();
        } else {
            assert!(h.begin_job_boot(id).unwrap().is_none());
        }
    }
    assert_eq!(h.snapshot.per_job.disks.len(), 1);
}

#[test]
fn warm_limit_uses_disk_recency_after_execution_retention_and_busy_priority() {
    let mut h = memory_host(1000);
    for i in 0..70 {
        let id = format!("idle-{i:02}");
        disk(&mut h, &id, AT + i);
    }
    // Busy entries beat newer idle ones. No retained execution is needed for recency.
    disk(&mut h, "quarantined", 0);
    h.snapshot
        .per_job
        .disks
        .get_mut("quarantined")
        .unwrap()
        .quarantined = true;
    disk(&mut h, "queued", 0);
    h.snapshot
        .per_job
        .disks
        .get_mut("queued")
        .unwrap()
        .queue
        .push(JOB2.into());
    disk(&mut h, "running", 0);
    h.snapshot
        .per_job
        .disks
        .get_mut("running")
        .unwrap()
        .attached_job = Some(JOB.into());
    disk(&mut h, "running-old", 0);
    h.snapshot
        .per_job
        .disks
        .get_mut("running-old")
        .unwrap()
        .attached_job = Some(JOB2.into());
    let mut expected = vec![
        warm("running", true),
        warm("running-old", true),
        warm("queued", true),
        warm("quarantined", true),
    ];
    for i in (10..70).rev() {
        expected.push(warm(&format!("idle-{i:02}"), false));
    }
    assert_eq!(expected.len(), 64);
    assert_eq!(bytes(&mut h, AT), body(4, 950, 0, &expected.join(","), ""));

    // Even more than 64 busy disks cannot put queued or recovery work before running work.
    for i in 0..70 {
        let d = h
            .snapshot
            .per_job
            .disks
            .get_mut(&format!("idle-{i:02}"))
            .unwrap();
        d.queue.push(JOB3.into());
    }
    let mut expected = vec![warm("running", true), warm("running-old", true)];
    for i in (8..70).rev() {
        expected.push(warm(&format!("idle-{i:02}"), true));
    }
    assert_eq!(bytes(&mut h, AT), body(4, 950, 0, &expected.join(","), ""));
}

#[test]
fn legacy_ledger_loads_without_dax_latch_and_false_is_omitted() {
    let legacy = r#"{"disks":{},"executions":{}}"#;
    let ledger: motor_host::state::jobs::Ledger = serde_json::from_str(legacy).unwrap();
    assert!(!ledger.dax_failed);
    assert!(ledger.is_empty());
    assert_eq!(serde_json::to_string(&ledger).unwrap(), legacy);
    let ledger: motor_host::state::jobs::Ledger =
        serde_json::from_str(r#"{"dax_failed":true,"disks":{},"executions":{}}"#).unwrap();
    assert!(!ledger.is_empty());
}
