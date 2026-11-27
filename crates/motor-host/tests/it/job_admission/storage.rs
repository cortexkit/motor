use super::*;
mod images;
use motor_host::server::{unix_ms, Runtime};
use motor_host::state::jobs::storage::{
    ReplayCompletion, REPLAY_RECORD_BYTES, REPLAY_RESERVATION_BYTES, RETURN_BYTES,
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
            fail_delete: false.into(),
        })
    }
    fn remove(&self, disk: &str) -> anyhow::Result<()> {
        if self.fail_delete.load(Ordering::SeqCst) {
            anyhow::bail!("injected deletion failure");
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

fn reserve(runtime: &Runtime, owner: &str, bytes: u64, ttl_s: u64) -> Value {
    runtime
        .host_admin(
            &json!({"op": "storage_reserve", "owner": owner, "bytes": bytes, "ttl_s": ttl_s}),
        )
        .unwrap()
}

fn reservations(runtime: &Runtime) -> Value {
    runtime.host_admin(&json!({"op": "host_status"})).unwrap()["storage_reservations"].clone()
}

fn claim_at(runtime: &Runtime, at_ms: u64) -> Result<(u64, Admission), Error> {
    let mut h = runtime.host.lock().unwrap();
    h.record_host_resources(HostResources {
        at_ms,
        available_mem_mib: 10_000,
        job_vm_headroom_mib: u64::MAX,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
    let mut claims = job(JOB);
    claims.iat = at_ms / 1000;
    claims.exp = claims.iat + 300;
    h.claim(claims, at_ms)
}

#[test]
fn operator_reservation_evicts_the_oldest_idle_checkout() {
    let mut h = memory_host(10_000);
    let oldest = idle(&mut h, JOB, 1, AT);
    let newest = idle(&mut h, JOB2, 2, AT + 1);
    let storage = Storage::new(FLOOR + CAP - 1);
    enable(&mut h, &storage);
    let runtime = Runtime::new("host".into(), h, storage.clone());

    let before = unix_ms();
    assert_eq!(
        reserve(&runtime, "image.1_build-test", CAP, 600),
        json!({"ok":true,"free_bytes":FLOOR + 2 * CAP - 1,"threshold_bytes":FLOOR,"reserved_bytes":CAP}),
    );
    let after = unix_ms();
    let listed = reservations(&runtime);
    let expires = listed[0]["expires_at_ms"].as_u64().unwrap();
    assert!((before + 600_000..=after + 600_000).contains(&expires));
    assert_eq!(
        listed,
        json!([{"owner":"image.1_build-test","bytes":CAP,"expires_at_ms":expires}])
    );
    assert_eq!(*storage.evicted.lock().unwrap(), [oldest]);
    assert!(!runtime.host.lock().unwrap().snapshot.per_job.disks[&newest].deleted);
}

#[test]
fn operator_reservation_is_refused_without_evicting_held_or_queued_checkouts() {
    let mut h = memory_host(10_000);
    h.claim(job(JOB), AT).unwrap();
    h.begin_job_boot(JOB).unwrap().unwrap();
    h.claim(other(JOB2, 2), AT).unwrap();
    let before = h.snapshot.clone();
    let storage = Storage::new(FLOOR + 2 * (CAP + JOB_BYTES) + CAP);
    enable(&mut h, &storage);
    let runtime = Runtime::new("host".into(), h, storage.clone());

    assert_eq!(
        reserve(&runtime, "image", CAP + 1, 600),
        json!({"ok":false,"error":"storage_reserve_short","short_bytes":1}),
    );
    assert_eq!(reservations(&runtime), json!([]));
    assert_eq!(runtime.host.lock().unwrap().snapshot, before);
    assert!(storage.evicted.lock().unwrap().is_empty());
}

#[test]
fn live_operator_reservation_refuses_job_admission_until_released() {
    let mut h = memory_host(10_000);
    let storage = Storage::new(FLOOR + CAP + JOB_BYTES);
    enable(&mut h, &storage);
    let runtime = Runtime::new("host".into(), h, storage);

    assert_eq!(reserve(&runtime, "image", CAP, 600)["ok"], true);
    let at_ms = unix_ms();
    full(claim_at(&runtime, at_ms));
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .executions
        .is_empty());
    for _ in 0..2 {
        let reply = runtime
            .host_admin(&json!({"op":"storage_release","owner":"image"}))
            .unwrap();
        assert_eq!(reply, json!({"ok":true}));
    }
    assert_eq!(reservations(&runtime), json!([]));
    claim_at(&runtime, at_ms).unwrap();
}

#[test]
fn operator_reservation_expires_at_its_deadline() {
    let mut h = memory_host(10_000);
    let storage = Storage::new(FLOOR + CAP + JOB_BYTES);
    enable(&mut h, &storage);
    let runtime = Runtime::new("host".into(), h, storage);

    assert_eq!(reserve(&runtime, "image", CAP, 600)["ok"], true);
    let expires = reservations(&runtime)[0]["expires_at_ms"].as_u64().unwrap();
    full(claim_at(&runtime, expires - 1));
    claim_at(&runtime, expires).unwrap();
    assert_eq!(reservations(&runtime), json!([]));
}

#[test]
fn operator_reservation_replaces_one_owner_and_preserves_it_on_refusal() {
    let mut h = memory_host(10_000);
    let storage = Storage::new(FLOOR + CAP);
    enable(&mut h, &storage);
    let runtime = Runtime::new("host".into(), h, storage);

    assert_eq!(reserve(&runtime, "other", CAP / 4, 600)["ok"], true);
    assert_eq!(reserve(&runtime, "image", CAP / 2, 600)["ok"], true);
    let prior_expiry = reservations(&runtime)[0]["expires_at_ms"].as_u64().unwrap();
    assert_eq!(
        reserve(&runtime, "image", 3 * CAP / 4, 1200),
        json!({"ok":true,"free_bytes":FLOOR + CAP,"threshold_bytes":FLOOR,"reserved_bytes":CAP}),
    );
    let listed = reservations(&runtime);
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["owner"], "image");
    assert_eq!(listed[0]["bytes"], 3 * CAP / 4);
    assert!(listed[0]["expires_at_ms"].as_u64().unwrap() >= prior_expiry + 600_000);
    assert_eq!(listed[1]["owner"], "other");
    assert_eq!(listed[1]["bytes"], CAP / 4);
    assert_eq!(
        reserve(&runtime, "image", CAP, 600),
        json!({"ok":false,"error":"storage_reserve_short","short_bytes":CAP / 4}),
    );
    assert_eq!(reservations(&runtime), listed);
}

#[test]
fn operator_reservation_admin_rejects_invalid_requests() {
    let mut h = memory_host(10_000);
    let storage = Storage::new(FLOOR + CAP);
    enable(&mut h, &storage);
    let runtime = Runtime::new("host".into(), h, storage);

    for owner in ["", "build/one", "buildé", &"x".repeat(65)] {
        assert!(runtime
            .host_admin(&json!({"op":"storage_reserve","owner":owner,"bytes":CAP,"ttl_s":1}))
            .is_err());
        assert!(runtime
            .host_admin(&json!({"op":"storage_release","owner":owner}))
            .is_err());
    }
    for request in [
        json!({"op":"storage_reserve","owner":"image","bytes":CAP,"ttl_s":0}),
        json!({"op":"storage_reserve","owner":"image","bytes":CAP,"ttl_s":21601}),
        json!({"op":"storage_reserve","owner":"image","bytes":-1,"ttl_s":1}),
        json!({"op":"storage_reserve","owner":"image","bytes":CAP,"ttl_s":1,"extra":true}),
        json!({"op":"storage_release","owner":"image","bytes":CAP}),
    ] {
        assert!(runtime.host_admin(&request).is_err(), "{request}");
    }
    assert_eq!(reservations(&runtime), json!([]));
}

#[test]
fn eviction_is_host_lru_idle_workspaces_and_never_tenants() {
    let mut h = memory_host(10_000);
    h.tenants = tenants();
    for tenant in h.tenants.values_mut() {
        tenant.host_floor_bytes = 0;
        tenant.idle_suspend_s = 0;
    }
    let first = idle(&mut h, JOB, 1, AT);
    let second = idle(&mut h, JOB2, 2, AT + 1);
    // Tenant files are deliberately outside the per-job catalog. Their used
    // blocks are already absent from the trusted free-space measurement.
    let tenants = serde_json::to_value(&h.tenants).unwrap();
    let storage = Storage::new(FLOOR + JOB_BYTES);
    enable(&mut h, &storage);
    h.claim(other(JOB3, 3), AT + 2).unwrap();
    assert_eq!(*storage.evicted.lock().unwrap(), [first]);
    assert!(!h.snapshot.per_job.disks[&second].deleted);
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
    let storage = Storage::new(1);
    enable(&mut h, &storage);
    full(h.claim(other("00000000-0000-7000-8000-000000000004", 4), AT));
    assert!(storage.evicted.lock().unwrap().is_empty());
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
