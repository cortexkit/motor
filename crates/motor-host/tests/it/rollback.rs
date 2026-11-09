use super::*;
use anyhow::{bail, Result};
use motor_host::{helper::Control, server::Runtime, store::SqliteJournal};
use std::{
    collections::BTreeSet,
    os::unix::net::UnixStream,
    sync::atomic::{AtomicBool, Ordering},
};

type CacheSet = (&'static str, Option<&'static str>);

#[derive(Default)]
struct RollbackControl {
    calls: Mutex<Vec<String>>,
    discarded: Mutex<BTreeSet<String>>,
    current: Mutex<BTreeMap<String, CacheSet>>,
    installed: Mutex<BTreeMap<String, CacheSet>>,
    fail_install: AtomicBool,
}

impl Control for RollbackControl {
    fn discard_tenant_snapshot(&self, tenant: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("discard:{tenant}"));
        self.discarded.lock().unwrap().insert(tenant.into());
        Ok(())
    }
    fn install_tenant_cache(&self, tenant: &str, account: &str) -> Result<()> {
        assert_eq!(account, tenant_accounts()[tenant]);
        self.calls.lock().unwrap().push(format!("install:{tenant}"));
        if tenant == "tenant-b" && self.fail_install.load(Ordering::Acquire) {
            bail!("tenant_cache_install_failed");
        }
        let set = self.current.lock().unwrap()[tenant];
        self.installed.lock().unwrap().insert(tenant.into(), set);
        Ok(())
    }
    fn stop_job(&self, job: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("stop:{job}"));
        Ok(())
    }
    fn delete_workspace(&self, account: &str, disk: &str) -> Result<()> {
        assert_eq!(account, claims(JOB).sub);
        self.calls.lock().unwrap().push(format!("delete:{disk}"));
        Ok(())
    }
    fn adopt(&self, _: &str) -> Result<bool> {
        Ok(false)
    }
    fn wake(&self, tenant: &str, _: Option<u64>) -> Result<u64> {
        assert!(self.discarded.lock().unwrap().contains(tenant));
        self.calls.lock().unwrap().push(format!("wake:{tenant}"));
        Ok(2)
    }
    fn quota(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    fn relay(&self, _: &str, _: u32) -> Result<UnixStream> {
        bail!("unused")
    }
    fn admin(&self, _: &str, _: &serde_json::Value) -> Result<serde_json::Value> {
        bail!("unused")
    }
    fn suspend(&self, _: &str) -> Result<()> {
        bail!("unused")
    }
    fn available_bytes(&self) -> Result<u64> {
        Ok(u64::MAX)
    }
    fn host_available_mib(&self) -> Result<u64> {
        Ok(100000)
    }
    fn online_cpus(&self) -> Result<u32> {
        Ok(2)
    }
}

fn tenant_accounts() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("tenant", "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        ("tenant-b", "org:01ARZ3NDEKTSV4RRFFQ69G5FAV"),
    ])
}

fn runtime(path: &std::path::Path, control: Arc<RollbackControl>) -> Arc<Runtime> {
    let mut bindings = tenants();
    let mut second = bindings["vm"].clone();
    second.tenant = "tenant-b".into();
    second.sub = tenant_accounts()["tenant-b"].into();
    bindings.insert("vm-b".into(), second);
    let (journal, snapshot) = SqliteJournal::open(path).unwrap();
    Runtime::new(
        "host".into(),
        Host::new(bindings, snapshot, Box::new(journal)),
        control,
    )
}

fn fixture() -> (tempfile::TempDir, Arc<Runtime>, Arc<RollbackControl>) {
    let dir = scratch();
    let control = Arc::new(RollbackControl::default());
    {
        let (mut journal, mut next) = SqliteJournal::open(dir.path()).unwrap();
        next.per_job.tenant_rollback_pending = true;
        next.draining = true;
        next.per_job.disks.insert(
            "ws-old".into(),
            motor_host::state::jobs::WorkspaceDisk {
                storage: None,
                sub: claims(JOB).sub,
                workspace_key_hash: "hash".into(),
                queue: vec![],
                attached_job: None,
                quarantined: false,
                recovery: None,
                recovery_result: None,
                deleted: false,
                have: vec![],
            },
        );
        journal.store(&next).unwrap();
    }
    let runtime = runtime(dir.path(), control.clone());
    control.current.lock().unwrap().extend([
        ("tenant".into(), ("read-rotated", Some("write-rotated"))),
        ("tenant-b".into(), ("org-read-rotated", None)),
    ]);
    control.installed.lock().unwrap().extend([
        (
            "tenant".into(),
            ("read-superseded", Some("write-superseded")),
        ),
        (
            "tenant-b".into(),
            ("org-read-superseded", Some("org-write-superseded")),
        ),
    ]);
    (dir, runtime, control)
}

#[test]
fn rotate_then_rollback_installs_all_current_sets_before_undrain() {
    let (_dir, runtime, control) = fixture();
    assert!(runtime
        .host_admin(&serde_json::json!({"op":"undrain_host"}))
        .is_err());
    runtime.recover().unwrap();
    assert_eq!(
        *control.installed.lock().unwrap(),
        *control.current.lock().unwrap()
    );
    let host = runtime.host.lock().unwrap();
    assert!(host.snapshot.per_job.disks["ws-old"].deleted);
    assert!(host.snapshot.draining);
    assert!(!host.snapshot.per_job.tenant_rollback_pending);
    drop(host);
    assert_eq!(
        *control.calls.lock().unwrap(),
        [
            "delete:ws-old",
            "discard:tenant",
            "wake:tenant",
            "install:tenant",
            "discard:tenant-b",
            "wake:tenant-b",
            "install:tenant-b"
        ]
    );
    runtime
        .host_admin(&serde_json::json!({"op":"undrain_host"}))
        .unwrap();
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .claim(claims(JOB), NOW * 1000)
        .is_ok());
}

#[test]
fn failed_rollback_install_and_restart_keep_placement_fenced() {
    let (dir, first, control) = fixture();
    control.fail_install.store(true, Ordering::Release);
    assert!(first.recover().is_err());
    {
        let mut host = first.host.lock().unwrap();
        assert!(host.snapshot.draining);
        assert!(host.snapshot.per_job.tenant_rollback_pending);
        assert_eq!(
            host.claim(claims(JOB), NOW * 1000).unwrap_err().refusal(),
            motor_host::state::Refusal::HostFull
        );
    }
    drop(first);
    let restarted = runtime(dir.path(), control.clone());
    assert!(restarted
        .host_admin(&serde_json::json!({"op":"undrain_host"}))
        .is_err());
    assert!(restarted.host.lock().unwrap().snapshot.draining);
    control.fail_install.store(false, Ordering::Release);
    restarted.recover().unwrap();
    assert_eq!(
        *control.installed.lock().unwrap(),
        *control.current.lock().unwrap()
    );
    assert_eq!(
        control
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| *call == "delete:ws-old")
            .count(),
        1
    );
    assert!(restarted.host.lock().unwrap().snapshot.draining);
    restarted
        .host_admin(&serde_json::json!({"op":"undrain_host"}))
        .unwrap();
}

#[test]
fn tenant_rollback_marker_is_additive_and_legacy_disks_trigger_the_fence() {
    let mut ledger = serde_json::to_value(motor_host::state::jobs::Ledger::default()).unwrap();
    assert!(ledger.get("tenant_rollback_pending").is_none());
    assert!(
        !serde_json::from_value::<motor_host::state::jobs::Ledger>(ledger.clone())
            .unwrap()
            .tenant_rollback_pending
    );
    ledger["tenant_rollback_pending"] = serde_json::json!(true);
    assert!(
        serde_json::from_value::<motor_host::state::jobs::Ledger>(ledger)
            .unwrap()
            .tenant_rollback_pending
    );

    let (dir, original, control) = fixture();
    drop(original);
    {
        let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
        snapshot.per_job.tenant_rollback_pending = false;
        snapshot.draining = false;
        journal.store(&snapshot).unwrap();
    }
    let legacy = runtime(dir.path(), control.clone());
    control.fail_install.store(true, Ordering::Release);
    assert!(legacy.recover().is_err());
    assert!(legacy.host.lock().unwrap().snapshot.draining);
    assert!(
        legacy
            .host
            .lock()
            .unwrap()
            .snapshot
            .per_job
            .tenant_rollback_pending
    );
}
