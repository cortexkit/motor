use super::*;

const JOB: &str = "00000000-0000-4000-8000-000000000001";
const NEXT: &str = "00000000-0000-4000-8000-000000000002";
#[derive(Default)]
struct Fake {
    running: bool,
    dirty: bool,
    fail_cleanup: bool,
    fail_reconcile: bool,
    calls: Vec<&'static str>,
}
impl Machine for Fake {
    fn reconcile(&mut self) -> Result<()> {
        self.calls.push("reconcile");
        if self.fail_reconcile {
            return Err(Error("reconcile failed".into()));
        }
        self.dirty = false;
        Ok(())
    }
    fn start(&mut self, _: usize, _: &Lease, _: Kind) -> Result<Vec<OwnedFd>> {
        self.calls.push("start");
        self.running = true;
        Ok(Vec::new())
    }
    fn stop(&mut self, _: usize, _: &Lease, _: bool) -> Result<()> {
        self.calls.push("stop");
        self.running = false;
        Ok(())
    }
    fn cleanup(&mut self, _: usize) -> Result<()> {
        self.calls.push("cleanup");
        if self.fail_cleanup {
            return Err(Error("cleanup failed".into()));
        }
        self.running = false;
        self.dirty = false;
        Ok(())
    }
    fn alive(&mut self, _: usize) -> bool {
        self.running
    }
}
fn fixture() -> (Helper<Fake>, u64, u64) {
    let mut h = Helper::new(Fake::default(), 1, vec![1000]).unwrap();
    let a = h
        .connect(Peer {
            uid: 1000,
            gid: 1000,
        })
        .unwrap();
    let b = h
        .connect(Peer {
            uid: 1000,
            gid: 1000,
        })
        .unwrap();
    (h, a, b)
}
fn request(h: &mut Helper<Fake>, c: u64, s: serde_json::Value) -> Result<Response> {
    h.handle(c, serde_json::from_value(s).unwrap())
}
fn acquire(h: &mut Helper<Fake>, c: u64, instance: &str, job: &str) -> Option<usize> {
    match request(
        h,
        c,
        serde_json::json!({"op":"acquire","instance":instance,"job_id":job}),
    )
    .unwrap()
    .reply
    {
        Reply::Acquired { slot } => slot,
        _ => panic!("not acquired"),
    }
}
fn op(h: &mut Helper<Fake>, c: u64, operation: &str, job: &str) -> Result<Response> {
    let mut v = serde_json::json!({"op":operation,"slot":0,"instance":"a","job_id":job});
    if operation == "start" {
        v["kind"] = serde_json::json!("job");
    }
    if operation == "stop" {
        v["export"] = serde_json::json!(false);
    }
    request(h, c, v)
}
#[test]
fn unauthorised_peer_uid_is_refused_before_operations() {
    let (mut h, _, _) = fixture();
    assert!(h
        .connect(Peer {
            uid: 200000,
            gid: 1000
        })
        .is_err());
    assert!(request(&mut h, 99, serde_json::json!({"op":"list","instance":"a"})).is_err());
    assert_eq!(h.machine.calls, ["reconcile"]);
}
#[test]
fn unknown_operation_paths_argv_and_extra_fields_are_refused() {
    for json in [
        serde_json::json!({"op":"exec","instance":"a"}),
        serde_json::json!({"op":"acquire","instance":"a","job_id":JOB,"path":"/etc/shadow"}),
        serde_json::json!({"op":"start","slot":0,"instance":"a","job_id":JOB,"kind":"job","argv":["sh"]}),
    ] {
        assert!(serde_json::from_value::<Request>(json).is_err());
    }
}
#[test]
fn malformed_identifiers_and_out_of_range_slots_never_reach_machine() {
    let (mut h, a, _) = fixture();
    for instance in [
        "",
        "A",
        "a/b",
        "..",
        "é",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert!(request(
            &mut h,
            a,
            serde_json::json!({"op":"acquire","instance":instance,"job_id":JOB})
        )
        .is_err());
    }
    assert!(request(
        &mut h,
        a,
        serde_json::json!({"op":"acquire","instance":"a","job_id":"../x"})
    )
    .is_err());
    acquire(&mut h, a, "a", JOB);
    for operation in ["start", "stop", "release"] {
        let mut v = serde_json::json!({"op":operation,"slot":1,"instance":"a","job_id":JOB});
        if operation == "start" {
            v["kind"] = serde_json::json!("job");
        }
        if operation == "stop" {
            v["export"] = serde_json::json!(false);
        }
        assert!(request(&mut h, a, v).is_err());
    }
    assert_eq!(h.machine.calls, ["reconcile"]);
}
#[test]
fn all_mutating_operations_require_both_identity_and_connection() {
    let (mut h, a, b) = fixture();
    acquire(&mut h, a, "a", JOB);
    for operation in ["start", "stop", "release"] {
        assert!(op(&mut h, b, operation, JOB).is_err());
        assert!(op(&mut h, a, operation, NEXT).is_err());
        let mut v = serde_json::json!({"op":operation,"slot":0,"instance":"b","job_id":JOB});
        if operation == "start" {
            v["kind"] = serde_json::json!("job");
        }
        if operation == "stop" {
            v["export"] = serde_json::json!(false);
        }
        assert!(request(&mut h, a, v).is_err());
    }
    assert_eq!(h.machine.calls, ["reconcile"]);
}
#[test]
fn delayed_stop_cannot_kill_the_new_occupant() {
    let (mut h, a, _) = fixture();
    assert_eq!(acquire(&mut h, a, "a", JOB), Some(0));
    op(&mut h, a, "start", JOB).unwrap();
    op(&mut h, a, "stop", JOB).unwrap();
    op(&mut h, a, "release", JOB).unwrap();
    assert_eq!(acquire(&mut h, a, "a", NEXT), Some(0));
    op(&mut h, a, "start", NEXT).unwrap();
    let calls = h.machine.calls.clone();
    assert!(op(&mut h, a, "stop", JOB).is_err());
    assert!(h.machine.running);
    assert_eq!(h.machine.calls, calls);
}
#[test]
fn stop_retains_lease_through_scan_until_explicit_release() {
    let (mut h, a, b) = fixture();
    acquire(&mut h, a, "a", JOB);
    op(&mut h, a, "start", JOB).unwrap();
    assert!(op(&mut h, a, "start", JOB).is_err());
    assert!(op(&mut h, a, "release", JOB).is_err());
    op(&mut h, a, "stop", JOB).unwrap();
    assert_eq!(acquire(&mut h, b, "b", NEXT), None);
    request(
        &mut h,
        a,
        serde_json::json!({"op":"start","slot":0,"instance":"a","job_id":JOB,"kind":"scan"}),
    )
    .unwrap();
    op(&mut h, a, "stop", JOB).unwrap();
    assert_eq!(acquire(&mut h, b, "b", NEXT), None);
    op(&mut h, a, "release", JOB).unwrap();
    assert_eq!(acquire(&mut h, b, "b", NEXT), Some(0));
}
#[test]
fn closed_connection_is_dirty_until_cleanup_completes() {
    let (mut h, a, b) = fixture();
    acquire(&mut h, a, "a", JOB);
    op(&mut h, a, "start", JOB).unwrap();
    h.disconnect(a);
    h.machine.fail_cleanup = true;
    h.maintain();
    assert!(matches!(h.slots[0], Slot::Dirty(_)));
    assert_eq!(acquire(&mut h, b, "b", NEXT), None);
    h.machine.fail_cleanup = false;
    h.maintain();
    assert!(!h.machine.running);
    assert_eq!(acquire(&mut h, b, "b", NEXT), Some(0));
}
#[test]
fn child_death_cleans_the_slot_without_export() {
    let (mut h, a, b) = fixture();
    acquire(&mut h, a, "a", JOB);
    op(&mut h, a, "start", JOB).unwrap();
    h.machine.running = false;
    h.maintain();
    assert_eq!(h.machine.calls, ["reconcile", "start", "cleanup"]);
    assert_eq!(acquire(&mut h, b, "b", NEXT), Some(0));
}
#[test]
fn list_is_instance_scoped_and_reports_dirty_leases() {
    let (mut h, a, b) = fixture();
    acquire(&mut h, a, "a", JOB);
    h.disconnect(a);
    let json = serde_json::to_value(
        request(&mut h, b, serde_json::json!({"op":"list","instance":"a"}))
            .unwrap()
            .reply,
    )
    .unwrap();
    assert_eq!(json["slots"][0]["state"], "dirty");
    let json = serde_json::to_value(
        request(&mut h, b, serde_json::json!({"op":"list","instance":"b"}))
            .unwrap()
            .reply,
    )
    .unwrap();
    assert_eq!(json["slots"], serde_json::json!([]));
}
#[test]
fn reconcile_failure_prevents_helper_construction() {
    assert!(Helper::new(
        Fake {
            fail_reconcile: true,
            ..Fake::default()
        },
        1,
        vec![1000]
    )
    .is_err());
    let h = Helper::new(
        Fake {
            dirty: true,
            running: true,
            ..Fake::default()
        },
        1,
        vec![1000],
    )
    .unwrap();
    assert!(!h.machine.dirty);
    assert!(matches!(h.slots[0], Slot::Free));
}
#[test]
fn service_uses_delegation_leaf_and_sends_sigterm_only_to_the_helper() {
    // SIGTERM to the whole cgroup (KillMode=control-group) would reach each
    // tenant's Firecracker at the same moment as the helper and cut the
    // guest's power before the helper could stop it cleanly. With mixed,
    // systemd still SIGKILLs everything left once the helper exits, so the
    // unit leaves no process behind either way.
    let unit = include_str!("../systemd/motor-vm-helper.service");
    for setting in ["Delegate=yes", "DelegateSubgroup=helper", "KillMode=mixed"] {
        assert!(unit.lines().any(|l| l == setting), "{setting}");
    }
    assert!(!unit.lines().any(|l| l.starts_with("SendSIGKILL=")));
    assert!(!unit.lines().any(|l| l.starts_with("ExecStop=")));
    // One in-flight suspend (50 s prepare, 300 s snapshot, 10 s VMM exit)
    // plus one tenant's clean stop (45 s); the unit file explains each part.
    let timeout: u64 = unit
        .lines()
        .find_map(|l| l.strip_prefix("TimeoutStopSec="))
        .unwrap()
        .parse()
        .unwrap();
    assert!(timeout >= 405, "{timeout}");
}

#[test]
fn storage_mount_keeps_vm_reads_below_root_owned_ancestors() {
    let mount = include_str!("../systemd/var-lib-motor\\x2dvm.mount");
    for setting in [
        "What=/srv/build/vm",
        "Where=/var/lib/motor-vm",
        "Type=none",
        "Options=bind",
        "WantedBy=local-fs.target",
    ] {
        assert!(mount.lines().any(|line| line == setting), "{setting}");
    }
    let service = include_str!("../systemd/motor-vm-helper.service");
    assert!(service
        .lines()
        .any(|line| line == "RequiresMountsFor=/var/lib/motor-vm"));
    #[cfg(target_os = "linux")]
    assert_eq!(crate::linux::BUILD_ROOT, "/var/lib/motor-vm");
}

#[test]
fn storage_companion_stops_propagation_before_helper_start() {
    let mount = include_str!("../systemd/var-lib-motor\\x2dvm.mount");
    assert!(mount
        .lines()
        .any(|line| line == "Wants=motor-vm-storage-private.service"));
    let private = include_str!("../systemd/motor-vm-storage-private.service");
    for setting in [
        "After=var-lib-motor\\x2dvm.mount",
        "BindsTo=var-lib-motor\\x2dvm.mount",
        "ExecStart=/usr/bin/mount --make-rprivate /var/lib/motor-vm",
        "RemainAfterExit=yes",
    ] {
        assert!(private.lines().any(|line| line == setting), "{setting}");
    }
    let helper = include_str!("../systemd/motor-vm-helper.service");
    assert!(helper
        .lines()
        .any(|line| line == "Requires=motor-vm-storage-private.service"));
}

#[test]
fn config_refuses_uid_overlap_invalid_limits_and_paths() {
    let original: crate::config::Config =
        serde_json::from_str(include_str!("../fixtures/helper.json")).unwrap();
    original.validate().unwrap();
    let mut config = original.clone();
    config.job_account_range.base = 200001;
    assert!(config.validate().is_err());
    let mut config = original.clone();
    config.allowed_uids.push(200000);
    assert!(config.validate().is_err());
    let mut config = original.clone();
    config.uid_base = u32::MAX;
    assert!(config.validate().is_err());
    let mut config = original.clone();
    config.slots[0].cpus = "1/../../".into();
    assert!(config.validate().is_err());
    let mut config = original.clone();
    config.slots[0].cpu_max = "max 100000".into();
    assert!(config.validate().is_err());
    let mut config = original.clone();
    config.loader = "relative".into();
    assert!(config.validate().is_err());
}
