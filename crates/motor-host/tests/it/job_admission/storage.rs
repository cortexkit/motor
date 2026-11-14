use super::*;
mod images;
use motor_host::state::jobs::storage::{
    Base, ReplayCompletion, REPLAY_RECORD_BYTES, REPLAY_RESERVATION_BYTES, RETURN_BYTES,
};
use motor_host::{capacity::Capacity, helper::Control};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

const CAP: u64 = 1024 * 1024 * 1024;
const FLOOR: u64 = 4096;
const JOB_BYTES: u64 = REPLAY_RESERVATION_BYTES + motor_host::jobs::SPOOL_LIMIT;

fn fill_source_slot() -> crate::jobs::fill_source::Slot {
    use crate::jobs::fill_source::{Request, Slot};
    Slot {
        request: Request {
            account: job(JOB).sub,
            repository_root: "/home/repository".into(),
            integration_tip: "a".repeat(40),
            mem_mib: 100,
            observed_ms: AT,
        },
        generation: "source".into(),
        source: motor_protocol::FillSource {
            repository_root: "/home/repository".into(),
            integration_tip: "a".repeat(40),
            bundles: vec![],
            git_dependencies: vec![],
        },
        bytes: 1,
        last_fill_started_ms: None,
    }
}

#[test]
fn checkout_forks_free_pending_export_and_stored_source_growth_promises() {
    let scratch = scratch();
    let mut h = durable_host(scratch.path(), 10_000);
    let storage = Storage::new(u64::MAX);
    enable(&mut h, &storage);
    h.claim(job(JOB), AT).unwrap();
    let slot = fill_source_slot();
    let key = crate::jobs::fill_source::key(&slot.request.account, &slot.request.repository_root);
    let directory = crate::jobs::fill_source::directory(scratch.path(), &slot.generation);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("0"), b"x").unwrap();
    h.snapshot
        .per_job
        .export_tips
        .insert(JOB.into(), slot.request.clone());
    h.snapshot.per_job.fill_sources.insert(key, slot);
    storage
        .free
        .store(FLOOR + CAP + JOB_BYTES, Ordering::SeqCst);
    let mut next = h.snapshot.clone();
    assert!(h.reserve_storage(&mut next, "", AT).is_err());
    let motor_host::config::HostSettings::PerJob(settings) = &mut h.settings else {
        unreachable!()
    };
    settings.fork_checkouts = true;
    h.clean_fill_sources().unwrap();
    assert!(h.snapshot.per_job.export_tips.is_empty());
    assert!(h.snapshot.per_job.fill_sources.is_empty());
    assert!(!directory.exists());
    let mut next = h.snapshot.clone();
    assert!(h.reserve_storage(&mut next, "", AT).is_ok());
}

#[test]
fn source_slot_growth_and_atomic_replacement_are_both_reserved_before_a_fill() {
    // Leave room for the 24 GiB fill floor so only storage controls admission.
    let mut h = memory_host(64 * 1024);
    let storage = Storage::new(FLOOR + 3 * CAP - 2);
    enable(&mut h, &storage);
    let slot = fill_source_slot();
    h.snapshot
        .per_job
        .export_tips
        .insert("export".into(), slot.request.clone());
    h.snapshot
        .per_job
        .fill_sources
        .insert("source".into(), slot);
    assert!(h
        .reserve_fill(&job(JOB).sub, "repo", 100, AT)
        .unwrap()
        .is_none());
    assert_eq!(h.snapshot.per_job.fill_sources.len(), 1);
    storage.free.store(FLOOR + 3 * CAP - 1, Ordering::SeqCst);
    assert!(h
        .reserve_fill(&job(JOB).sub, "repo", 100, AT)
        .unwrap()
        .is_some());
}

#[test]
fn a_new_fill_reservation_cannot_evict_the_source_it_will_need() {
    // Leave room for the 24 GiB fill floor so only storage controls admission.
    let mut h = memory_host(64 * 1024);
    let storage = Storage::new(FLOOR + 2 * CAP - 2);
    enable(&mut h, &storage);
    let slot = fill_source_slot();
    let repository = crate::jobs::fill_source::repository_key(&slot.request.repository_root);
    h.snapshot
        .per_job
        .fill_sources
        .insert("source".into(), slot);
    assert!(h
        .reserve_fill(&job(JOB).sub, &repository, 100, AT)
        .unwrap()
        .is_none());
    assert_eq!(h.snapshot.per_job.fill_sources.len(), 1);
}

#[test]
fn a_replacement_export_reservation_keeps_the_older_slot_if_storage_is_insufficient() {
    let mut h = memory_host(10_000);
    let storage = Storage::new(u64::MAX);
    enable(&mut h, &storage);
    h.claim(job(JOB), AT).unwrap();
    let slot = fill_source_slot();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/exec-remote-v1/frames/run.json");
    let motor_protocol::Frame::Run(mut run) =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    else {
        unreachable!()
    };
    run.repository_root = slot.request.repository_root.clone();
    run.integration_tip = "b".repeat(40);
    h.snapshot.per_job.fill_sources.insert(
        crate::jobs::fill_source::key(&slot.request.account, &slot.request.repository_root),
        slot,
    );
    storage
        .free
        .store(FLOOR + 2 * CAP + JOB_BYTES, Ordering::SeqCst);
    h.note_fill_source(JOB, &run, AT).unwrap();
    assert!(h.snapshot.per_job.export_tips.is_empty());
    assert_eq!(h.snapshot.per_job.fill_sources.len(), 1);
}

#[test]
fn pressure_evicts_an_idle_repository_source_and_its_payload_before_refusing_a_job() {
    let scratch = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
    let mut h = durable_host(scratch.path(), 10_000);
    let storage = Storage::new(FLOOR + CAP + JOB_BYTES);
    enable(&mut h, &storage);
    let slot = fill_source_slot();
    let path =
        crate::jobs::fill_source::directory(&h.journal_directory().unwrap(), &slot.generation);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("0"), b"x").unwrap();
    h.snapshot
        .per_job
        .fill_sources
        .insert("source".into(), slot);
    assert!(h.claim(job(JOB), AT).is_ok());
    assert!(h.snapshot.per_job.fill_sources.is_empty());
    assert!(!path.exists());
    assert!(storage.evicted.lock().unwrap().is_empty());
}

#[test]
fn fill_storage_uses_the_same_growth_promises_as_run_claims() {
    // Leave room for the 24 GiB fill floor so only storage controls admission.
    let mut h = memory_host(64 * 1024);
    let storage = Storage::new(FLOOR + CAP - 1);
    enable(&mut h, &storage);
    assert!(h
        .reserve_fill(&job(JOB).sub, "repo", 100, AT)
        .unwrap()
        .is_none());
    assert!(h.snapshot.per_job.fills.is_empty());
    storage.free.store(FLOOR + CAP, Ordering::SeqCst);
    let fill = h
        .reserve_fill(&job(JOB).sub, "repo", 100, AT)
        .unwrap()
        .unwrap();
    full(h.claim(job(JOB), AT));
    assert!(h.snapshot.per_job.fills.contains_key(&fill.fill.fill_id));
    h.fill_finished(&fill.fill.fill_id, false, AT).unwrap();
    assert!(h.snapshot.outbox.is_empty());
}

fn memory_host(available: u64) -> Host {
    let mut host = super::memory_host(available);
    if let motor_host::config::HostSettings::PerJob(s) = &mut host.settings {
        s.workspace_disk_cap_bytes = CAP;
        s.workspace_growth_reservation_bytes = CAP;
    }
    host
}
fn durable_host(path: &std::path::Path, available: u64) -> Host {
    let mut host = super::durable_host(path, available);
    if let motor_host::config::HostSettings::PerJob(s) = &mut host.settings {
        s.workspace_disk_cap_bytes = CAP;
        s.workspace_growth_reservation_bytes = CAP;
    }
    host
}

struct Storage {
    prepares: Mutex<Vec<(String, String)>>,
    fail_prepare: std::sync::atomic::AtomicBool,
    free: AtomicU64,
    exclusive: Mutex<BTreeMap<String, u64>>,
    evicted: Mutex<Vec<String>>,
    reclaimed: u64,
    busy: Mutex<std::collections::BTreeSet<String>>,
    fail_delete: std::sync::atomic::AtomicBool,
}
impl Storage {
    fn new(free: u64) -> Arc<Self> {
        Arc::new(Self {
            prepares: Mutex::new(Vec::new()),
            fail_prepare: false.into(),
            free: AtomicU64::new(free),
            exclusive: Mutex::new(BTreeMap::new()),
            evicted: Mutex::new(Vec::new()),
            reclaimed: CAP,
            busy: Default::default(),
            fail_delete: false.into(),
        })
    }
    fn remove(&self, disk: &str) -> anyhow::Result<()> {
        if self.fail_delete.load(Ordering::SeqCst) {
            anyhow::bail!("injected deletion failure");
        }
        if self.busy.lock().unwrap().contains(disk) {
            anyhow::bail!("seed is held");
        }
        self.evicted.lock().unwrap().push(disk.into());
        self.free.fetch_add(self.reclaimed, Ordering::SeqCst);
        Ok(())
    }
}
impl Control for Storage {
    fn prepare_job_images(&self, generation: &str, account: &str) -> anyhow::Result<()> {
        self.prepares
            .lock()
            .unwrap()
            .push((generation.into(), account.into()));
        anyhow::ensure!(
            !self.fail_prepare.load(Ordering::SeqCst),
            "injected image copy failure"
        );
        Ok(())
    }
    fn host_available_mib(&self) -> anyhow::Result<u64> {
        Ok(100_000)
    }
    fn online_cpus(&self) -> anyhow::Result<u32> {
        Ok(4)
    }
    fn workspace_usage(&self, _: &str, disk: &str) -> anyhow::Result<Option<u64>> {
        Ok(self.exclusive.lock().unwrap().get(disk).copied())
    }
    fn delete_workspace(&self, _: &str, disk: &str) -> anyhow::Result<()> {
        self.remove(disk)
    }
    fn evict_base(&self, _: &str, disk: &str) -> anyhow::Result<()> {
        self.remove(disk)
    }
    fn available_bytes(&self) -> anyhow::Result<u64> {
        Ok(self.free.load(Ordering::SeqCst))
    }
    fn capacity(&self) -> anyhow::Result<Capacity> {
        Ok(Capacity {
            windows_vm_bytes: 0,
            host_reserve_bytes: 0,
            jobs_min_bytes: 1,
            storage_margin_bytes: FLOOR,
            growth_allowance_bytes: 0,
            tenant_memory_reserve_bytes: 0,
        })
    }
    fn adopt(&self, _: &str) -> anyhow::Result<bool> {
        unreachable!()
    }
    fn wake(&self, _: &str, _: Option<u64>) -> anyhow::Result<u64> {
        unreachable!()
    }
    fn quota(&self, _: &str, _: u32) -> anyhow::Result<()> {
        unreachable!()
    }
    fn relay(&self, _: &str, _: u32) -> anyhow::Result<std::os::unix::net::UnixStream> {
        unreachable!()
    }
    fn admin(&self, _: &str, _: &Value) -> anyhow::Result<Value> {
        unreachable!()
    }
    fn suspend(&self, _: &str) -> anyhow::Result<()> {
        unreachable!()
    }
}
fn enable(h: &mut Host, storage: &Arc<Storage>) {
    if let motor_host::config::HostSettings::PerJob(s) = &mut h.settings {
        s.workspace_disk_cap_bytes = CAP;
        s.workspace_growth_reservation_bytes = CAP;
    }
    h.set_job_storage(storage.clone());
    h.enforce_storage();
    h.observe_storage(
        storage.capacity().map_err(|e| e.to_string()),
        storage.available_bytes().map_err(|e| e.to_string()),
    );
}
fn other(id: &str, hash: u8) -> Claims {
    let mut c = job(id);
    c.workspace_key_hash = Some(motor_lease::base64url(&[hash; 32]));
    c
}
fn disk(h: &Host, job: &str) -> String {
    h.snapshot.per_job.executions[job].disk_id.clone()
}

#[test]
fn maximum_return_is_reserved_before_run_and_fixed_free_sample_is_debited() {
    assert_eq!(
        motor_host::state::jobs::storage::REPLAY_BYTES,
        motor_runner::REPLAY_BYTES
    );
    let mut h = memory_host(10_000);
    let storage = Storage::new(FLOOR + CAP + JOB_BYTES - RETURN_BYTES);
    enable(&mut h, &storage);
    full(h.claim(job(JOB), AT));
    assert!(h.snapshot.per_job.is_empty());
    storage
        .free
        .store(FLOOR + CAP + JOB_BYTES, Ordering::SeqCst);
    let (id, _) = h.claim(job(JOB), AT).unwrap();
    assert_eq!(
        h.snapshot.per_job.executions[JOB].replay_reservation_bytes,
        Some(REPLAY_RESERVATION_BYTES)
    );
    full(h.claim(job(JOB2), AT));
    h.close(id).unwrap();
    // No second disk or replay promise is charged for this execution.
    storage.free.store(0, Ordering::SeqCst);
    h.claim(job(JOB), AT).unwrap();
    assert!(storage.evicted.lock().unwrap().is_empty());
}

#[test]
fn same_disk_queue_reserves_growth_once_and_each_jobs_replay() {
    let mut h = memory_host(10_000);
    let storage = Storage::new(FLOOR + CAP + 2 * JOB_BYTES);
    enable(&mut h, &storage);
    h.claim(job(JOB), AT).unwrap();
    h.begin_job_boot(JOB).unwrap().unwrap();
    h.claim(job(JOB2), AT).unwrap();
    assert!(h.begin_job_boot(JOB2).unwrap().is_none());
    assert_eq!(h.snapshot.per_job.disks.len(), 1);
    full(h.claim(job(JOB3), AT));
}

#[test]
fn bounded_growth_admits_three_disks_then_refuses_the_fourth_for_storage() {
    use motor_host::claim_log::RefusalCause;

    const GROWTH: u64 = CAP / 8;
    const FOURTH: &str = "00000000-0000-7000-8000-000000000004";
    let mut h = memory_host(10_000);
    let storage = Storage::new(FLOOR + 3 * (GROWTH + JOB_BYTES));
    enable(&mut h, &storage);
    if let motor_host::config::HostSettings::PerJob(s) = &mut h.settings {
        s.workspace_growth_reservation_bytes = GROWTH;
    }
    for (index, id) in [JOB, JOB2, JOB3].into_iter().enumerate() {
        h.claim(other(id, index as u8), AT).unwrap();
        h.begin_job_boot(id).unwrap().unwrap();
        let reservation = h.snapshot.per_job.disks[&disk(&h, id)]
            .storage
            .as_ref()
            .unwrap();
        assert_eq!(reservation.growth_reservation_bytes, GROWTH);
        assert_eq!(reservation.cap_bytes, CAP);
    }
    assert_eq!(h.snapshot.per_job.disks.len(), 3);
    let logs = crate::test_logs::capture(|| full(h.claim(other(FOURTH, 3), AT)));
    let refusal = logs
        .iter()
        .find(|line| line.contains("message=run_claim "))
        .expect("fourth claim must record its refusal cause");
    assert!(refusal.contains("reason=\"host_full\""), "{refusal}");
    assert!(
        refusal.contains(&format!("cause=\"{}\"", RefusalCause::Storage.as_str())),
        "{refusal}"
    );
    assert_eq!(h.snapshot.per_job.disks.len(), 3);
    assert!(storage.evicted.lock().unwrap().is_empty());
}

#[test]
fn remaining_disk_growth_below_the_reservation_bound_is_not_over_reserved() {
    const REMAINING: u64 = 4096;
    let mut h = memory_host(10_000);
    h.claim(job(JOB), AT).unwrap();
    let first = disk(&h, JOB);
    let storage = Storage::new(FLOOR + 2 * JOB_BYTES + REMAINING);
    storage
        .exclusive
        .lock()
        .unwrap()
        .insert(first.clone(), CAP - REMAINING);
    enable(&mut h, &storage);
    if let motor_host::config::HostSettings::PerJob(s) = &mut h.settings {
        s.workspace_growth_reservation_bytes = CAP / 4;
    }
    h.claim(job(JOB2), AT).unwrap();
    let reservation = h.snapshot.per_job.disks[&first].storage.as_ref().unwrap();
    assert_eq!(reservation.growth_reservation_bytes, REMAINING);
    assert_eq!(reservation.cap_bytes, CAP);
    storage
        .free
        .store(FLOOR + 3 * JOB_BYTES + REMAINING - 1, Ordering::SeqCst);
    full(h.claim(job(JOB3), AT));
}

#[test]
fn only_proven_exclusive_extents_reduce_growth_and_probe_is_refreshed() {
    let mut h = memory_host(10_000);
    h.claim(job(JOB), AT).unwrap();
    let first = disk(&h, JOB);
    h.job_stopped(JOB, false).unwrap();
    h.job_replay_closed(JOB, ReplayCompletion::Terminal)
        .unwrap();
    let storage = Storage::new(FLOOR + JOB_BYTES + CAP / 2);
    enable(&mut h, &storage);
    // Missing measurements (including a shared clone with zero exclusive bytes)
    // reserve the full cap, not the file's st_blocks value.
    full(h.claim(job(JOB2), AT));
    storage
        .exclusive
        .lock()
        .unwrap()
        .insert(first.clone(), CAP / 2);
    h.claim(job(JOB2), AT).unwrap();
    assert_eq!(
        h.snapshot.per_job.disks[&first]
            .storage
            .as_ref()
            .unwrap()
            .growth_reservation_bytes,
        CAP / 2
    );
    storage.exclusive.lock().unwrap().insert(first.clone(), CAP);
    storage.free.store(FLOOR + 2 * JOB_BYTES, Ordering::SeqCst);
    h.claim(job(JOB3), AT).unwrap();
    assert_eq!(
        h.snapshot.per_job.disks[&first]
            .storage
            .as_ref()
            .unwrap()
            .growth_reservation_bytes,
        0
    );
}

/// Raising the configured cap must reach checkout disks made under the old
/// one, or a repository whose build outgrew the old cap keeps failing on the
/// same small disk. Lowering it must not shrink them: the guest filesystem
/// can't shrink, and the helper refuses a disk larger than the size asked for.
#[test]
fn a_raised_cap_grows_an_existing_disk_on_its_next_claim_and_never_shrinks_it() {
    let mut h = memory_host(10_000);
    h.claim(job(JOB), AT).unwrap();
    let first = disk(&h, JOB);
    h.job_stopped(JOB, false).unwrap();
    h.job_replay_closed(JOB, ReplayCompletion::Terminal)
        .unwrap();
    let cap = |h: &Host| {
        let storage = h.snapshot.per_job.disks[&first].storage.clone().unwrap();
        (storage.cap_bytes, storage.growth_reservation_bytes)
    };
    assert_eq!(cap(&h).0, CAP);
    let set_cap = |h: &mut Host, bytes| {
        if let motor_host::config::HostSettings::PerJob(s) = &mut h.settings {
            s.workspace_disk_cap_bytes = bytes;
            s.workspace_growth_reservation_bytes = bytes;
        }
    };
    set_cap(&mut h, 2 * CAP);
    h.claim(job(JOB2), AT).unwrap();
    assert_eq!(disk(&h, JOB2), first);
    assert_eq!(cap(&h), (2 * CAP, 2 * CAP));
    h.job_stopped(JOB2, false).unwrap();
    h.job_replay_closed(JOB2, ReplayCompletion::Terminal)
        .unwrap();
    set_cap(&mut h, CAP);
    h.claim(job(JOB3), AT).unwrap();
    assert_eq!(cap(&h).0, 2 * CAP);
}

#[test]
fn reservation_transaction_failure_and_restart_preserve_all_promises() {
    let root = scratch();
    let mut h = durable_host(root.path(), 10_000);
    let storage = Storage::new(FLOOR + CAP + JOB_BYTES);
    enable(&mut h, &storage);
    struct Fail;
    impl Journal for Fail {
        fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::other("fsync"))
        }
    }
    let before = h.snapshot.clone();
    h.replace_journal(Box::new(Fail));
    assert!(matches!(h.claim(job(JOB), AT), Err(Error::Journal(_))));
    assert_eq!(h.snapshot, before);
    drop(h);
    let mut h = durable_host(root.path(), 10_000);
    enable(&mut h, &storage);
    h.claim(job(JOB), AT).unwrap();
    let saved = h.snapshot.clone();
    drop(h);
    let mut h = durable_host(root.path(), 10_000);
    enable(&mut h, &storage);
    assert_eq!(h.snapshot, saved);
    full(h.claim(job(JOB2), AT));
}

fn idle(h: &mut Host, id: &str, hash: u8, at: u64) -> String {
    h.claim(other(id, hash), at).unwrap();
    h.job_stopped(id, false).unwrap();
    h.job_replay_closed(id, ReplayCompletion::Terminal).unwrap();
    disk(h, id)
}

#[test]
fn eviction_is_host_lru_idle_workspaces_before_bases_and_never_tenants() {
    let mut h = memory_host(10_000);
    h.tenants = tenants();
    for tenant in h.tenants.values_mut() {
        tenant.host_floor_bytes = 0;
        tenant.idle_suspend_s = 0;
    }
    let first = idle(&mut h, JOB, 1, AT);
    let second = idle(&mut h, JOB2, 2, AT + 1);
    h.snapshot.per_job.bases.insert(
        "base-old".into(),
        Base {
            sub: job(JOB).sub,
            repository_key: None,
            have: Vec::new(),
            last_used_ms: 1,
            evicting: false,
        },
    );
    // Tenant files are deliberately outside the per-job catalog. Their used
    // blocks are already absent from the trusted free-space measurement.
    let tenants = serde_json::to_value(&h.tenants).unwrap();
    let storage = Storage::new(FLOOR + JOB_BYTES);
    enable(&mut h, &storage);
    h.claim(other(JOB3, 3), AT + 2).unwrap();
    assert_eq!(*storage.evicted.lock().unwrap(), [first]);
    assert!(!h.snapshot.per_job.disks[&second].deleted);
    assert!(h.snapshot.per_job.bases.contains_key("base-old"));
    h.job_stopped(JOB3, false).unwrap();
    storage
        .free
        .store(FLOOR + JOB_BYTES + REPLAY_RECORD_BYTES, Ordering::SeqCst);
    h.claim(other("00000000-0000-7000-8000-000000000004", 4), AT + 3)
        .unwrap();
    assert_eq!(storage.evicted.lock().unwrap()[1], second);
    assert_eq!(serde_json::to_value(&h.tenants).unwrap(), tenants);
}

#[test]
fn held_base_does_not_hide_another_evictable_base_or_lose_its_deletion_fence() {
    let mut h = memory_host(10_000);
    for (id, at) in [("old-held", 1), ("new-idle", 2)] {
        h.snapshot.per_job.bases.insert(
            id.into(),
            Base {
                sub: job(JOB).sub.clone(),
                repository_key: None,
                have: Vec::new(),
                last_used_ms: at,
                evicting: false,
            },
        );
    }
    let storage = Storage::new(FLOOR + JOB_BYTES + REPLAY_RECORD_BYTES);
    storage.busy.lock().unwrap().insert("old-held".into());
    enable(&mut h, &storage);
    h.claim(job(JOB), AT).unwrap();
    assert_eq!(*storage.evicted.lock().unwrap(), ["new-idle"]);
    assert!(h.snapshot.per_job.bases["old-held"].evicting);
    assert!(!h.snapshot.per_job.bases.contains_key("new-idle"));
}

#[test]
fn allocated_replay_is_already_used_space_not_a_second_growth_charge() {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let root = scratch();
    let mut h = durable_host(root.path(), 10_000);
    let storage = Storage::new(FLOOR + CAP + 2 * JOB_BYTES - 8192);
    enable(&mut h, &storage);
    h.claim(job(JOB), AT).unwrap();
    full(h.claim(job(JOB2), AT));
    let replay = root.path().join("replay").join(JOB);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&replay)
        .unwrap();
    for name in ["index.json", "segment-0000000000000000"] {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(replay.join(name))
            .unwrap();
        file.write_all(&[b' '; 4096]).unwrap();
        file.sync_all().unwrap();
    }
    h.claim(job(JOB2), AT).unwrap();
}

#[test]
fn busy_admitted_and_quarantined_disks_are_never_evicted() {
    let mut h = memory_host(10_000);
    h.claim(other(JOB, 1), AT).unwrap();
    h.begin_job_boot(JOB).unwrap().unwrap();
    h.claim(other(JOB2, 2), AT).unwrap();
    let quarantined = idle(&mut h, JOB3, 3, AT);
    h.snapshot
        .per_job
        .disks
        .get_mut(&quarantined)
        .unwrap()
        .quarantined = true;
    h.snapshot.per_job.bases.insert(
        "base".into(),
        Base {
            sub: job(JOB).sub,
            repository_key: None,
            have: Vec::new(),
            last_used_ms: 0,
            evicting: false,
        },
    );
    let storage = Storage::new(1);
    enable(&mut h, &storage);
    full(h.claim(other("00000000-0000-7000-8000-000000000004", 4), AT));
    assert_eq!(*storage.evicted.lock().unwrap(), ["base"]);
    assert!(h.snapshot.per_job.disks.values().all(|d| !d.deleted));
}

#[test]
fn interrupted_eviction_is_fenced_durably_and_retried_after_restart() {
    let root = scratch();
    let mut h = durable_host(root.path(), 10_000);
    let old = idle(&mut h, JOB, 1, AT);
    let storage = Storage::new(FLOOR + JOB_BYTES);
    storage.fail_delete.store(true, Ordering::SeqCst);
    enable(&mut h, &storage);
    full(h.claim(other(JOB2, 2), AT));
    assert!(h.snapshot.per_job.disks[&old].deleted);
    assert!(
        h.snapshot.per_job.disks[&old]
            .storage
            .as_ref()
            .unwrap()
            .evicting
    );
    assert!(!h.snapshot.per_job.executions.contains_key(JOB2));
    drop(h);
    let mut h = durable_host(root.path(), 10_000);
    enable(&mut h, &storage);
    storage.fail_delete.store(false, Ordering::SeqCst);
    h.claim(other(JOB2, 2), AT).unwrap();
    assert_eq!(*storage.evicted.lock().unwrap(), [old]);
}

#[test]
fn replay_and_growth_release_only_after_stop_on_every_termination_path() {
    for ending in [
        "queued",
        "boot_failed",
        "delivered_boot_failed",
        "refused",
        "clean",
        "unclean",
    ] {
        let mut h = memory_host(10_000);
        let storage = Storage::new(FLOOR + CAP + JOB_BYTES + REPLAY_RECORD_BYTES);
        enable(&mut h, &storage);
        let (id, _) = h.claim(job(JOB), AT).unwrap();
        if ending != "queued" {
            h.begin_job_boot(JOB).unwrap().unwrap();
        }
        if ending == "delivered_boot_failed" {
            h.delivered(id).unwrap();
        }
        if ending.ends_with("boot_failed") {
            h.job_boot_failed(JOB, AT + 10).unwrap();
        }
        if ending == "refused" {
            h.job_refused(JOB, json!({"type":"terminal"}), AT + 10)
                .unwrap();
        }
        assert_eq!(
            h.snapshot.per_job.executions[JOB].replay_reservation_bytes,
            Some(REPLAY_RESERVATION_BYTES),
            "{ending}"
        );
        h.job_stopped(JOB, ending == "unclean").unwrap();
        assert_eq!(
            h.snapshot.per_job.executions[JOB].replay_reservation_bytes, None,
            "{ending}"
        );
        let first = disk(&h, JOB);
        assert_eq!(
            h.snapshot.per_job.disks[&first]
                .storage
                .as_ref()
                .unwrap()
                .growth_reservation_bytes,
            0,
            "{ending}"
        );
        h.job_stopped(JOB, false).unwrap();
        if ending != "unclean" {
            h.claim(job(JOB2), AT + 10).unwrap();
        } else {
            full(h.claim(job(JOB2), AT + 10));
        }
    }
}

#[test]
fn record_deletion_releases_an_uncommitted_final_replay_hold() {
    let mut h = memory_host(10_000);
    h.claim(job(JOB), AT).unwrap();
    h.job_boot_failed(JOB, AT + 10).unwrap();
    h.job_stopped(JOB, false).unwrap();
    assert_eq!(
        h.snapshot.per_job.executions[JOB].final_replay_reservation_bytes,
        Some(REPLAY_RECORD_BYTES)
    );
    h.snapshot.billing.get_mut(JOB).unwrap().enqueued = true;
    h.snapshot.outbox.clear();
    h.snapshot
        .per_job
        .executions
        .get_mut(JOB)
        .unwrap()
        .usage_accepted_at_ms = Some(AT + 10);
    assert_eq!(
        h.prune_retention(AT + motor_host::billing::REPLAY_RETENTION_MS + 1000)
            .unwrap(),
        1
    );
    assert_eq!(
        h.snapshot.per_job.executions[JOB].final_replay_reservation_bytes,
        None
    );
}
