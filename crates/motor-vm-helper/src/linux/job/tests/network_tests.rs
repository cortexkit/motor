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
    namespaces: BTreeSet<String>,
    fail_namespace_remove: bool,
    egress_command: Option<PathBuf>,
}

#[derive(Clone)]
pub(in crate::linux::job) struct Fake(Arc<Mutex<State>>, Arc<cortexkit_test_support::ScratchDir>);

impl Default for Fake {
    fn default() -> Self {
        Self(
            Arc::default(),
            Arc::new(cortexkit_test_support::ScratchDir::new("job-network-lock")),
        )
    }
}

impl Fake {
    pub(super) fn has_namespace(&self, id: &str) -> bool {
        self.0.lock().unwrap().namespaces.contains(id)
    }
}

impl Access for Fake {
    fn lock(&self) -> Result<tenant_network::lock::Guard> {
        tenant_network::lock::acquire_at(&self.1.join("network"), unsafe { libc::geteuid() })
    }
    fn try_lock(&self) -> Result<Option<tenant_network::lock::Guard>> {
        tenant_network::lock::try_acquire_at(&self.1.join("network"), unsafe { libc::geteuid() })
    }
    fn tenants(&self) -> Result<Vec<Tap>> {
        Ok(self.0.lock().unwrap().tenants.clone())
    }
    fn install(&self) -> Result<()> {
        tenant_network::validate_taps(&self.0.lock().unwrap().tenants)?;
        self.0.lock().unwrap().events.push("install".into());
        if self.0.lock().unwrap().fail_install {
            return Err(Error("network policy failed".into()));
        }
        Ok(())
    }
    fn create_namespace(&self, id: &str) -> Result<PathBuf> {
        self.0.lock().unwrap().namespaces.insert(id.into());
        namespace::path(id)
    }
    fn reconcile_namespaces(&self, retained: &BTreeSet<String>) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .namespaces
            .retain(|id| retained.contains(id));
        Ok(())
    }
    fn remove_namespace(&self, id: &str) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.fail_namespace_remove {
            return Err(Error("namespace removal interrupted".into()));
        }
        state.namespaces.remove(id);
        Ok(())
    }
    fn ensure(&self, _id: &str, tap: &Tap) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .events
            .push(format!("ensure:{}", tap.tap_index));
        Ok(())
    }
    fn egress_bytes(&self, tap: &Tap) -> Result<Option<u64>> {
        let mut state = self.0.lock().unwrap();
        state.events.push(format!("read:{}", tap.tap_index));
        if let Some(command) = &state.egress_command {
            return tenant_network::optional_egress_bytes_with_command(
                tap,
                &mut Command::new(command),
            );
        }
        Ok(state.bytes.get(&tap.tap_index).copied())
    }
    fn remove(&self, _id: &str, tap: &Tap, _lock: &tenant_network::lock::Guard) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if let Some(path) = &state.record_path {
            let record: Record = serde_json::from_slice(&std::fs::read(path)?).unwrap();
            assert_eq!(record.tap_index, Some(tap.tap_index));
            assert!(record.oom_kill.is_some());
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
        workspace_disk_max_bytes: Some(1073741824),
        fixed_path_repositories: Default::default(),
        uid_base: unsafe { libc::geteuid() },
        uid_count: 64,
        memory_max_bytes: 1048576,
    });
    let fake = Fake::default();
    jobs.network = Box::new(fake.clone());
    std::fs::create_dir_all(&jobs.records_path).unwrap();
    std::fs::set_permissions(&jobs.records_path, std::fs::Permissions::from_mode(0o700)).unwrap();
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
    let (base, jobs, fake) = fixture();
    let tap = jobs.records()[&start().job_id].tap().unwrap();
    fake.0.lock().unwrap().fail_install = true;
    assert!(jobs
        .prepare_network(&start().job_id, tap)
        .is_err());
    assert_eq!(fake.0.lock().unwrap().events, ["install"]);
    fake.0.lock().unwrap().fail_install = false;
    jobs.prepare_network(&start().job_id, tap)
        .unwrap();
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
        workspace_disk_max_bytes: Some(1073741824),
        fixed_path_repositories: Default::default(),
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
        workspace_disk_max_bytes: Some(1073741824),
        fixed_path_repositories: Default::default(),
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
    let (base, jobs, fake) = fixture();
    fake.0.lock().unwrap().fail_remove = true;
    jobs.collect_exits().unwrap();
    assert!(jobs
        .failures(&start().job_id)
        .contains(&CollectionFailure::Network));
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
    restarted.collect_exits().unwrap();
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
fn missing_counter_records_absent_egress_once_and_releases_network_without_retry() {
    let (base, jobs, fake) = fixture();
    fake.0.lock().unwrap().bytes.remove(&17);
    jobs.collect_exits().unwrap();
    let saved = persisted(&jobs);
    assert_eq!(saved.oom_kill, Some(3));
    assert_eq!(saved.egress_bytes, None);
    assert_eq!(saved.tap_index, None);
    assert!(!jobs.group(&start().job_id).exists());
    assert!(serde_json::to_value(saved.pending_exit().unwrap())
        .unwrap()
        .get("egress_bytes")
        .is_none());
    assert_eq!(
        jobs.failures(&start().job_id),
        BTreeSet::from([CollectionFailure::MissingEgress])
    );
    assert!(!jobs.log_collection_failure(
        &mut jobs.cell(&start().job_id).unwrap().0.lock(),
        CollectionFailure::MissingEgress,
        &"same missing counter"
    ));
    jobs.collect_exits().unwrap();
    assert_eq!(fake.0.lock().unwrap().events, ["read:17", "remove:17"]);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn timed_out_egress_publishes_unmeasured_report_once_and_replays_after_restart() {
    let (base, mut jobs, fake) = fixture();
    let stalled = tenant_network::counter_tests::StalledCounter::new(&base);
    fake.0.lock().unwrap().egress_command = Some(stalled.command.clone());
    let started = std::time::Instant::now();
    jobs.collect_exits().unwrap();
    assert!(started.elapsed() < Duration::from_millis(2500));
    let saved = persisted(&jobs);
    let report = saved.pending_exit().unwrap();
    assert_eq!(report.oom_kill, 3);
    assert_eq!(report.egress_bytes, None);
    assert_eq!(saved.egress_bytes, None);
    assert_eq!(saved.tap_index, None);
    assert!(serde_json::to_value(&report)
        .unwrap()
        .get("egress_bytes")
        .is_none());
    let poll = |jobs: &mut LinuxJobs| match jobs
        .handle(
            Operation::Exits,
            Peer {
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
            },
        )
        .unwrap()
        .reply
    {
        Reply::JobExits { exits } => exits,
        other => panic!("unexpected reply: {other:?}"),
    };
    assert_eq!(poll(&mut jobs), std::slice::from_ref(&report));
    assert_eq!(poll(&mut jobs), std::slice::from_ref(&report));
    assert_eq!(
        jobs.hooks.lock().unwrap().collection_logs,
        [format!(
            "job {} exit evidence MissingEgress: counter missing; egress unmeasured",
            start().job_id
        )]
    );
    drop(jobs);
    let mut restarted = machine(&base);
    restarted.network = Box::new(fake.clone());
    restarted.adopt_record(persisted(&restarted)).unwrap();
    assert_eq!(poll(&mut restarted), [report]);
    assert_eq!(persisted(&restarted).egress_bytes, None);
    assert!(restarted.hooks.lock().unwrap().collection_logs.is_empty());
    assert_eq!(fake.0.lock().unwrap().events, ["read:17", "remove:17"]);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn network_release_holds_lock_until_tap_clear_is_durable() {
    use std::os::fd::AsRawFd;
    let (base, jobs, fake) = fixture();
    let path = jobs.records_path.join(format!("{}.json", start().job_id));
    let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let check_flag = checked.clone();
    jobs.hooks.lock().unwrap().tap_release_check = Some(Box::new(move || {
        let saved: Record = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.tap_index, None);
        let other = File::open(fake.1.join("network/lock")).unwrap();
        assert_ne!(
            unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "network lock dropped before the durable TAP release"
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        check_flag.store(true, Ordering::SeqCst);
    }));
    jobs.collect_exits().unwrap();
    assert!(checked.load(Ordering::SeqCst));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn missing_cgroup_removes_the_allocation_without_inventing_egress() {
    let (base, jobs, fake) = fixture();
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
fn namespace_removal_failure_keeps_index_fenced_and_replays_saved_evidence() {
    let (base, jobs, fake) = fixture();
    let id = start().job_id;
    fake.create_namespace(&id).unwrap();
    fake.0.lock().unwrap().fail_namespace_remove = true;
    jobs.collect_exits().unwrap();
    let saved = persisted(&jobs);
    assert_eq!(saved.tap_index, Some(17));
    assert_eq!(saved.egress_bytes, Some(1234));
    assert!(jobs.group(&id).exists());
    assert!(fake.0.lock().unwrap().namespaces.contains(&id));
    assert!(jobs.failures(&id).contains(&CollectionFailure::Network));
    fake.0.lock().unwrap().fail_namespace_remove = false;
    jobs.collect_exits().unwrap();
    assert_eq!(persisted(&jobs).tap_index, None);
    assert_eq!(persisted(&jobs).egress_bytes, Some(1234));
    assert!(!fake.0.lock().unwrap().namespaces.contains(&id));
    assert_eq!(
        fake.0.lock().unwrap().events,
        ["read:17", "remove:17", "remove:17"]
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn recovery_namespace_is_removed_without_a_tap_or_invented_egress() {
    let (base, jobs, fake) = fixture();
    let id = start().job_id;
    let mut saved = persisted(&jobs);
    saved.start.kind = job::VmKind::Recovery;
    saved.tap_index = None;
    jobs.save(&saved).unwrap();
    jobs.insert_record(saved);
    fake.create_namespace(&id).unwrap();
    jobs.collect_exits().unwrap();
    assert!(!fake.0.lock().unwrap().namespaces.contains(&id));
    assert!(fake.0.lock().unwrap().events.is_empty());
    assert_eq!(persisted(&jobs).egress_bytes, None);
    std::fs::remove_dir_all(base).unwrap();
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

#[test]
fn acknowledgement_and_tap_clear_serialize_whole_record_writes() {
    use std::sync::mpsc;
    let (base, jobs, _) = fixture();
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    jobs.pause_exit_at(ExitStep::Recorded, move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let jobs = Arc::new(jobs);
    let collecting = jobs.clone();
    let collector = std::thread::spawn(move || collecting.collect_exits().unwrap());
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    let (begun, begins) = mpsc::channel();
    jobs.pause_exit_at(ExitStep::Ack, move || begun.send(()).unwrap());
    let (done, completion) = mpsc::channel();
    let acknowledging = jobs.clone();
    let ack = std::thread::spawn(move || {
        let result = acknowledging.acknowledge_exit(ExitReport {
            job_id: start().job_id,
            oom_kill: 3,
            egress_bytes: Some(1234),
        });
        done.send(result.map(|response| matches!(response.reply, Reply::Ok)))
            .unwrap();
    });
    begins.recv_timeout(Duration::from_secs(2)).unwrap();
    let early = completion.recv_timeout(Duration::from_millis(100));
    release.send(()).unwrap();
    collector.join().unwrap();
    ack.join().unwrap();
    assert!(
        matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
        "ack bypassed the collector's writer lock"
    );
    assert!(completion
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap());
    let saved = persisted(&jobs);
    assert!(saved.exit_acknowledged);
    assert_eq!(saved.tap_index, None);
    assert!(jobs.records()[&start().job_id].exit_acknowledged);
    assert_eq!(jobs.records()[&start().job_id].tap_index, None);
    drop(jobs);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn collector_never_waits_for_the_network_lock_while_holding_the_job_writer() {
    use std::sync::mpsc;
    let (base, jobs, fake) = fixture();
    let id = start().job_id;
    let jobs = Arc::new(jobs);
    // Another job's teardown or a tenant render holds the machine-wide flock.
    let held = jobs.network.lock().unwrap();
    let collecting = jobs.clone();
    let (collected, collection) = mpsc::channel();
    let collector = std::thread::spawn(move || {
        collecting.collect_exits().unwrap();
        collected.send(()).unwrap();
    });
    let finished = collection.recv_timeout(Duration::from_secs(2));
    // The host acknowledges the published report while the flock is busy.
    let acknowledging = jobs.clone();
    let (acked, acknowledgement) = mpsc::channel();
    let ack = std::thread::spawn(move || {
        acked
            .send(acknowledging.acknowledge_exit(ExitReport {
                job_id: start().job_id,
                oom_kill: 3,
                egress_bytes: Some(1234),
            }))
            .unwrap();
    });
    let ack_result = acknowledgement.recv_timeout(Duration::from_secs(2));
    drop(held);
    collector.join().unwrap();
    ack.join().unwrap();
    assert!(
        finished.is_ok(),
        "collector waited for the network flock holding J"
    );
    ack_result.expect("ack waited behind network teardown").unwrap();
    let saved = persisted(&jobs);
    assert!(saved.exit_acknowledged);
    assert_eq!(saved.tap_index, Some(17), "TAP released without the flock");
    let view = jobs.view(&id).unwrap();
    assert_eq!(view.state, super::super::registry::State::Exited);
    assert!(view.cleanup_pending);
    assert!(jobs.group(&id).exists());
    assert_eq!(fake.0.lock().unwrap().events, ["read:17"]);
    // The next tick finishes the cleanup without losing the acknowledgement.
    jobs.collect_exits().unwrap();
    let saved = persisted(&jobs);
    assert_eq!(saved.tap_index, None);
    assert!(saved.exit_acknowledged);
    assert!(!jobs.view(&id).unwrap().cleanup_pending);
    assert!(!jobs.group(&id).exists());
    assert_eq!(fake.0.lock().unwrap().events, ["read:17", "remove:17"]);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn prune_never_unlinks_a_record_while_a_render_scans_the_directory() {
    use std::sync::mpsc;
    let (base, jobs, _) = fixture();
    let mut tombstone = record();
    tombstone.start.job_id = job::test_id(2);
    tombstone.released = true;
    tombstone.oom_kill = Some(3);
    tombstone.exit_acknowledged = true;
    jobs.save(&tombstone).unwrap();
    let path = jobs
        .records_path
        .join(format!("{}.json", tombstone.start.job_id));
    File::open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
        .unwrap();
    jobs.adopt_record(tombstone.clone()).unwrap();
    let jobs = Arc::new(jobs);
    // A render holds the flock and has listed the directory; it opens each
    // listed record next, as network::read_records does.
    let held = jobs.network.lock().unwrap();
    let listed: Vec<_> = std::fs::read_dir(&jobs.records_path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(listed.len(), 2);
    let pruning = jobs.clone();
    let (pruned, prune) = mpsc::channel();
    let pruner = std::thread::spawn(move || {
        pruned
            .send(pruning.prune_records(SystemTime::now()))
            .unwrap();
    });
    let early = prune.recv_timeout(Duration::from_millis(500));
    let opened: Vec<_> = listed.iter().map(std::fs::read).collect();
    drop(held);
    pruner.join().unwrap();
    assert!(early.is_err(), "prune unlinked a record during the scan");
    assert!(opened.iter().all(|read| read.is_ok()), "{opened:?}");
    prune.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    assert!(!path.exists());
    assert!(jobs.view(&tombstone.start.job_id).is_none());
    std::fs::remove_dir_all(base).unwrap();
}
