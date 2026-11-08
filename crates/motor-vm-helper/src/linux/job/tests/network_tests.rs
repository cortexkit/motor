use super::*;
use crate::linux::job::network::{allocate, check_capacity, Access};
use crate::linux::tenant_network::{self, Tap};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(in crate::linux::job) struct State {
    tenants: Vec<Tap>,
    bytes: BTreeMap<u8, u64>,
    events: Vec<String>,
    record_path: Option<PathBuf>,
    fail_remove: bool,
    fail_install: bool,
}

#[derive(Clone, Default)]
pub(in crate::linux::job) struct Fake(Arc<Mutex<State>>);

impl Access for Fake {
    fn tenants(&self) -> Result<Vec<Tap>> {
        Ok(self.0.lock().unwrap().tenants.clone())
    }
    fn install(&mut self, taps: &[Tap]) -> Result<()> {
        tenant_network::validate_taps(taps)?;
        self.0.lock().unwrap().events.push("install".into());
        if self.0.lock().unwrap().fail_install {
            return Err(Error("network policy failed".into()));
        }
        Ok(())
    }
    fn ensure(&mut self, tap: &Tap) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .events
            .push(format!("ensure:{}", tap.tap_index));
        Ok(())
    }
    fn egress_bytes(&mut self, tap: &Tap) -> Result<u64> {
        let mut state = self.0.lock().unwrap();
        state.events.push(format!("read:{}", tap.tap_index));
        state
            .bytes
            .get(&tap.tap_index)
            .copied()
            .ok_or_else(|| Error("counter unavailable".into()))
    }
    fn remove(&mut self, tap: &Tap) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if let Some(path) = &state.record_path {
            let record: Record = serde_json::from_slice(&std::fs::read(path)?).unwrap();
            assert_eq!(record.tap_index, Some(tap.tap_index));
            assert!(record.oom_kill.is_some());
            assert!(record.egress_bytes.is_some());
        }
        state.events.push(format!("remove:{}", tap.tap_index));
        if state.fail_remove {
            return Err(Error("network removal interrupted".into()));
        }
        state.bytes.remove(&tap.tap_index);
        Ok(())
    }
}

fn fixture() -> (PathBuf, LinuxJobs, Fake) {
    let base = scratch();
    let mut jobs = machine(&base);
    jobs.settings = Some(JobVms {
        uid_base: unsafe { libc::geteuid() },
        uid_count: 64,
        memory_max_bytes: 1048576,
    });
    let fake = Fake::default();
    jobs.network = Box::new(fake.clone());
    let mut record = record();
    record.tap_index = Some(17);
    jobs.save(&record).unwrap();
    jobs.adopt_record(record).unwrap();
    let group = jobs.group(&start().job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    std::fs::write(group.join("memory.events"), "oom_kill 3\n").unwrap();
    {
        let mut state = fake.0.lock().unwrap();
        state.bytes.extend([(1, 111), (2, 222), (17, 1234)]);
        state.record_path = Some(jobs.records_path.join(format!("{}.json", start().job_id)));
    }
    (base, jobs, fake)
}

fn persisted(jobs: &LinuxJobs) -> Record {
    serde_json::from_slice(
        &std::fs::read(jobs.records_path.join(format!("{}.json", start().job_id))).unwrap(),
    )
    .unwrap()
}

#[test]
fn network_policy_failure_prevents_tap_activation() {
    let (base, mut jobs, fake) = fixture();
    let tap = jobs.records[&start().job_id].tap().unwrap();
    fake.0.lock().unwrap().fail_install = true;
    assert!(jobs.prepare_network(Vec::new(), tap).is_err());
    assert_eq!(fake.0.lock().unwrap().events, ["install"]);
    fake.0.lock().unwrap().fail_install = false;
    jobs.prepare_network(Vec::new(), tap).unwrap();
    assert_eq!(
        fake.0.lock().unwrap().events,
        ["install", "install", "ensure:17"]
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn job_indices_are_disjoint_and_reserved_even_by_a_released_crash_record() {
    let tenants = [Tap {
        tap_index: 1,
        uid: 400001,
    }];
    let mut records = BTreeMap::new();
    let mut job = record();
    job.tap_index = Some(2);
    job.released = true;
    records.insert(job.start.job_id.clone(), job);
    assert_eq!(allocate(&tenants, &records).unwrap(), 3);
    records.values_mut().next().unwrap().tap_index = None;
    assert_eq!(allocate(&tenants, &records).unwrap(), 2);
    records.values_mut().next().unwrap().tap_index = Some(1);
    assert!(allocate(&tenants, &records)
        .unwrap_err()
        .0
        .contains("duplicate"));
    assert!(allocate(
        &(1..=254)
            .map(|tap_index| Tap { tap_index, uid: 1 })
            .collect::<Vec<_>>(),
        &BTreeMap::new()
    )
    .is_err());
}

#[test]
fn startup_capacity_counts_all_job_slots_and_registered_tenants() {
    // The configured VMM pool includes job permits and concurrent VMs that
    // prepare reusable workspace bases.
    check_capacity(56 + 8, 190).unwrap();
    let error = check_capacity(56 + 8, 191).unwrap_err().0;
    for number in ["64", "191", "254"] {
        assert!(error.contains(number), "{error}");
    }
    let base = scratch();
    let mut jobs = machine(&base);
    jobs.settings = Some(JobVms {
        uid_base: 500000,
        uid_count: 255,
        memory_max_bytes: 1048576,
    });
    assert!(jobs
        .reconcile()
        .unwrap_err()
        .0
        .contains("255 + registered tenants 0"));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn startup_removes_leftover_job_network_only_after_durable_final_counters() {
    let (base, jobs, fake) = fixture();
    let saved = persisted(&jobs);
    drop(jobs);
    let mut restarted = machine(&base);
    restarted.settings = Some(JobVms {
        uid_base: saved.uid,
        uid_count: 64,
        memory_max_bytes: 1048576,
    });
    restarted.network = Box::new(fake.clone());
    // Reconciliation below consumes the records after trusted-file loading;
    // the portable test supplies its saved record without forging root ownership.
    restarted.reconcile_records(vec![saved]).unwrap();
    let state = fake.0.lock().unwrap();
    assert_eq!(state.events, ["read:17", "remove:17"]);
    assert_eq!(state.bytes, BTreeMap::from([(1, 111), (2, 222)]));
    let record = persisted(&restarted);
    assert_eq!(record.tap_index, None);
    assert_eq!(
        record.pending_exit(),
        Some(ExitReport {
            job_id: start().job_id,
            oom_kill: 3,
            egress_bytes: Some(1234),
        })
    );
    assert!(!restarted.group(&start().job_id).exists());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn failed_removal_replays_frozen_egress_and_ack_matches_the_entire_report() {
    let (base, mut jobs, fake) = fixture();
    fake.0.lock().unwrap().fail_remove = true;
    assert!(jobs.collect_exits().is_err());
    let saved = persisted(&jobs);
    assert_eq!(saved.tap_index, Some(17));
    assert_eq!(saved.egress_bytes, Some(1234));
    let report = saved.pending_exit().unwrap();
    fake.0.lock().unwrap().bytes.insert(17, 9000);
    fake.0.lock().unwrap().fail_remove = false;
    drop(jobs);
    let mut restarted = machine(&base);
    restarted.network = Box::new(fake.clone());
    restarted.adopt_record(saved).unwrap();
    restarted.maintain().unwrap();
    assert_eq!(persisted(&restarted).pending_exit(), Some(report.clone()));
    assert_eq!(
        fake.0.lock().unwrap().events,
        ["read:17", "remove:17", "remove:17"]
    );
    for wrong in [None, Some(0), Some(9000)] {
        let mut mismatch = report.clone();
        mismatch.egress_bytes = wrong;
        assert!(restarted.acknowledge_exit(mismatch).is_err());
        assert!(!persisted(&restarted).exit_acknowledged);
    }
    restarted.acknowledge_exit(report).unwrap();
    assert!(persisted(&restarted).exit_acknowledged);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn missing_counter_cannot_persist_partial_evidence_or_remove_network() {
    let (base, mut jobs, fake) = fixture();
    fake.0.lock().unwrap().bytes.remove(&17);
    assert!(jobs.collect_exits().is_err());
    let saved = persisted(&jobs);
    assert_eq!(saved.oom_kill, None);
    assert_eq!(saved.egress_bytes, None);
    assert_eq!(saved.tap_index, Some(17));
    assert!(jobs.group(&start().job_id).exists());
    assert_eq!(fake.0.lock().unwrap().events, ["read:17"]);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn missing_cgroup_removes_the_allocation_without_inventing_egress() {
    let (base, mut jobs, fake) = fixture();
    std::fs::remove_dir_all(jobs.group(&start().job_id)).unwrap();
    fake.0.lock().unwrap().record_path = None;
    jobs.collect_exits().unwrap();
    let saved = persisted(&jobs);
    assert_eq!(saved.tap_index, None);
    assert_eq!(saved.oom_kill, None);
    assert_eq!(saved.egress_bytes, None);
    assert_eq!(fake.0.lock().unwrap().events, ["remove:17"]);
    assert!(saved.pending_exit().is_none());
    let reply = serde_json::to_value(jobs.status(&start().job_id).unwrap().reply).unwrap();
    assert!(reply.get("tap_index").is_none());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn live_vmm_network_is_never_sampled_or_removed() {
    let mut vmm = Vmm::new();
    let mut jobs = machine(&vmm.base);
    let fake = Fake::default();
    jobs.network = Box::new(fake.clone());
    let group = jobs.group(&start().job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    let mut record = record();
    record.tap_index = Some(17);
    jobs.adopt_record(record).unwrap();
    let status = serde_json::to_value(jobs.status(&start().job_id).unwrap().reply).unwrap();
    assert_eq!(status["tap_index"], 17);
    jobs.collect_exits().unwrap();
    assert!(fake.0.lock().unwrap().events.is_empty());
    assert!(vmm.child.try_wait().unwrap().is_none());
}

#[test]
fn legacy_record_is_tolerant_but_unmeasured_egress_is_not_zero() {
    let mut value = serde_json::to_value(record()).unwrap();
    value.as_object_mut().unwrap().remove("tap_index");
    value.as_object_mut().unwrap().remove("egress_bytes");
    value["future_field"] = json!(true);
    value["oom_kill"] = json!(0);
    let old: Record = serde_json::from_value(value).unwrap();
    assert_eq!(old.tap_index, None);
    assert_eq!(old.pending_exit().unwrap().egress_bytes, None);
    assert!(serde_json::to_value(old.pending_exit().unwrap())
        .unwrap()
        .get("egress_bytes")
        .is_none());
}
