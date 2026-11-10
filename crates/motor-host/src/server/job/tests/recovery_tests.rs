use super::*;
use crate::state::jobs::recovery::Phase;

fn unclean(runtime: &Arc<Runtime>) -> String {
    let id = claim(runtime, &claims(JOB));
    let mut host = runtime.host.lock().unwrap();
    let e = host.begin_job_boot(JOB).unwrap().unwrap();
    host.delivered(id).unwrap();
    host.job_started(JOB, unix_ms()).unwrap();
    let event = serde_json::from_value(json!({"event_seq":1,"event":"mirror_refs","mirror_refs":{
        "imported":true,"refs":[{"repository":"/repo","reference":"refs/snapshots/held","commit":"held"}]}})).unwrap();
    host.event(JOB, &event, unix_ms()).unwrap();
    assert_eq!(host.snapshot.per_job.disks[&e.disk_id].have.len(), 1);
    host.job_stop_intent(JOB, true).unwrap();
    assert!(host.snapshot.per_job.disks[&e.disk_id].quarantined);
    host.job_stopped(JOB, true).unwrap();
    e.disk_id
}

fn restart(runtime: Arc<Runtime>, fake: Arc<Fake>, storage: Arc<Storage>) -> Arc<Runtime> {
    let settings = runtime.host.lock().unwrap().settings.clone();
    drop(runtime);
    let (journal, snapshot) = crate::journal::FileJournal::open(storage.root.path()).unwrap();
    let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = settings;
    host.record_host_resources(HostResources {
        at_ms: unix_ms(),
        job_vm_headroom_mib: u64::MAX,
        available_mem_mib: 1000,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
    Runtime::with_job_resources("host".into(), host, fake, storage)
}

async fn finish(runtime: &Runtime, disk: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            runtime.tick_recoveries(unix_ms()).unwrap();
            if !runtime.host.lock().unwrap().snapshot.per_job.disks[disk].quarantined {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn unclean_mark_refuses_claim_and_keeps_warm_busy_across_restart_and_capacity_wait() {
    let (runtime, fake, storage) = fixture();
    let disk = unclean(&runtime);
    let runtime = restart(runtime, fake, storage);
    let mut host = runtime.host.lock().unwrap();
    host.record_host_resources(HostResources {
        at_ms: unix_ms(),
        job_vm_headroom_mib: u64::MAX,
        available_mem_mib: 50,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
    host.reserve_recovery(&disk, unix_ms()).unwrap();
    assert!(host.snapshot.per_job.disks[&disk].recovery.is_none());
    assert!(matches!(
        host.claim(claims(JOB2), unix_ms()),
        Err(crate::state::Error::Refused(
            crate::state::Refusal::HostFull
        ))
    ));
    assert!(host.heartbeat(unix_ms(), 0, true).warm[0].busy);
    assert!(host.snapshot.per_job.disks[&disk].quarantined);
}

#[tokio::test]
async fn killed_started_job_recovers_without_lease_before_next_workspace_boot() {
    let (runtime, fake, storage) = fixture();
    let id = claim(&runtime, &claims(JOB));
    let disk = {
        let mut host = runtime.host.lock().unwrap();
        let e = host.begin_job_boot(JOB).unwrap().unwrap();
        host.delivered(id).unwrap();
        host.job_started(JOB, unix_ms()).unwrap();
        e.disk_id
    };
    claim(&runtime, &claims(JOB2));
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB2)
        .unwrap()
        .is_none());
    fake.exits
        .lock()
        .unwrap()
        .push(motor_vm_helper::job::ExitReport {
            job_id: JOB.into(),
            oom_kill: 0,
            egress_bytes: None,
        });
    runtime.collect_job_exits(unix_ms()).unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.per_job.disks[&disk].quarantined);
    storage.recovery_available.store(true, Ordering::Release);
    finish(&runtime, &disk).await;
    let host = runtime.host.lock().unwrap();
    assert_eq!(host.snapshot.leases.len(), 2);
    assert_eq!(
        host.snapshot.per_job.disks[&disk]
            .recovery_result
            .as_deref(),
        Some("repaired")
    );
    assert!(host.snapshot.per_job.disks[&disk].have.is_empty());
    drop(host);
    let start = fake.recovery_starts.lock().unwrap()[0].clone();
    assert_eq!(start.kind, motor_vm_helper::job::VmKind::Recovery);
    assert_eq!(start.vcpus, 1);
    assert_eq!(start.burst_mib, None);
    let request = serde_json::to_value(start).unwrap();
    for key in ["nonce", "keys", "lease", "caller", "tap_index"] {
        assert!(request.get(key).is_none());
    }
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB2)
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn recovery_timeout_counts_from_start_stops_deletes_and_next_job_has_fresh_disk() {
    let (runtime, fake, storage) = fixture();
    let disk = unclean(&runtime);
    storage.recovery_available.store(true, Ordering::Release);
    *fake.hang.lock().unwrap() = "recovery";
    runtime.tick_recoveries(unix_ms()).unwrap();
    fake.entered.notified().await;
    let at = runtime.host.lock().unwrap().snapshot.per_job.disks[&disk]
        .recovery
        .as_ref()
        .unwrap()
        .started_at_ms
        .unwrap();
    {
        let host = runtime.host.lock().unwrap();
        assert_eq!(
            host.heartbeat(at, host.job_free_mem_mib(at), true)
                .free_vcpus,
            3
        );
        assert_eq!(host.job_free_mem_mib(at), 830);
    }
    runtime.tick_recoveries(at + 9999).unwrap();
    assert!(!fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.starts_with("delete:")));
    runtime.tick_recoveries(at + 10000).unwrap();
    assert!(fake
        .calls
        .lock()
        .unwrap()
        .contains(&format!("delete:{disk}")));
    assert!(runtime.host.lock().unwrap().snapshot.per_job.disks[&disk].deleted);
    claim(&runtime, &claims(JOB2));
    assert_ne!(
        runtime.host.lock().unwrap().snapshot.per_job.executions[JOB2].disk_id,
        disk
    );
}

#[tokio::test]
async fn unknown_guest_operation_deletes_and_reformatted_reply_clears_have() {
    for reply in [
        json!({"ok":false,"error":"unknown operation"}),
        json!({"ok":true,"recovery":"reformatted"}),
    ] {
        let (runtime, fake, storage) = fixture();
        let disk = unclean(&runtime);
        storage.recovery_available.store(true, Ordering::Release);
        *fake.recovery_reply.lock().unwrap() = Some(reply.clone());
        finish(&runtime, &disk).await;
        let host = runtime.host.lock().unwrap();
        assert_eq!(
            host.snapshot.per_job.disks[&disk].deleted,
            reply["ok"] != true
        );
        assert!(host.snapshot.per_job.disks[&disk].have.is_empty());
        let reply = host.job_have(
            JOB,
            vec![motor_protocol::RepositoryCandidates {
                repository: "/repo".into(),
                commits: vec!["held".into()],
            }],
            vec![],
        );
        assert_eq!(
            serde_json::to_value(reply).unwrap()["repositories"][0]["commits"],
            json!([])
        );
    }
}

#[tokio::test]
async fn every_durable_recovery_phase_resumes_after_host_restart() {
    for phase in [
        Phase::Reserved,
        Phase::Running,
        Phase::Stopping,
        Phase::Deleting,
    ] {
        let (runtime, fake, storage) = fixture();
        let disk = unclean(&runtime);
        storage.recovery_available.store(true, Ordering::Release);
        {
            let mut host = runtime.host.lock().unwrap();
            host.reserve_recovery(&disk, unix_ms()).unwrap();
            let id = host.snapshot.per_job.disks[&disk]
                .recovery
                .as_ref()
                .unwrap()
                .id
                .clone();
            host.recovery_phase(&disk, &id, phase, unix_ms(), None)
                .unwrap();
        }
        let runtime = restart(runtime, fake.clone(), storage);
        assert!(runtime.host.lock().unwrap().snapshot.per_job.disks[&disk].quarantined);
        finish(&runtime, &disk).await;
        let host = runtime.host.lock().unwrap();
        assert_eq!(
            host.snapshot.per_job.disks[&disk].deleted,
            phase == Phase::Deleting
        );
        assert_eq!(
            fake.recovery_starts.lock().unwrap().len(),
            usize::from(phase == Phase::Reserved)
        );
    }
}
