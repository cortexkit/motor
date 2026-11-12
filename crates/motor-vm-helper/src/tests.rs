use super::*;

#[test]
fn job_network_allocation_is_helper_selected_and_ack_egress_is_optional_and_strict() {
    let job = serde_json::json!({"op":"start_job", "job_id":"00000000-0000-7000-8000-000000000001", "account":"a", "workspace_disk_id":"d", "image_generation_id":"g", "vcpus":1, "memory_mib":1024});
    assert!(serde_json::from_value::<Request>(job.clone()).is_ok());
    let mut caller_selected = job;
    caller_selected["tap_index"] = 17.into();
    assert!(serde_json::from_value::<Request>(caller_selected).is_err());
    let mut ack = serde_json::json!({"op":"ack_job_exit", "job_id":"00000000-0000-7000-8000-000000000001", "oom_kill":0, "egress_bytes":1234});
    assert!(matches!(
        serde_json::from_value::<Request>(ack.clone()).unwrap(),
        Request::AckJobExit {
            egress_bytes: Some(1234),
            ..
        }
    ));
    ack["future_field"] = true.into();
    assert!(serde_json::from_value::<Request>(ack).is_err());
}

const JOB: &str = "00000000-0000-7000-8000-000000000001";
const NEXT: &str = "00000000-0000-7000-8000-000000000002";
#[derive(Default)]
struct Fake {
    job_vms_configured: bool,
    running: bool,
    job_running: bool,
    dirty: bool,
    fail_cleanup: bool,
    fail_reconcile: bool,
    calls: Vec<&'static str>,
}
impl Machine for Fake {
    fn job_vms_configured(&self) -> bool {
        self.job_vms_configured
    }
    fn workspace_disk_max_bytes(&self) -> Option<u64> {
        self.job_vms_configured.then_some(4096)
    }
    fn images(&mut self, op: images::Operation) -> Result<Response> {
        match op {
            images::Operation::Readiness {
                job_vms_configured,
                workspace_disk_max_bytes,
            } => {
                self.calls.push("per_job_readiness");
                Ok(Response::new(Reply::PerJobReadiness {
                    job_vms_configured,
                    workspace_disk_max_bytes,
                    published_generations: if job_vms_configured {
                        vec!["gen-1".into()]
                    } else {
                        vec![]
                    },
                }))
            }
            _ => Err(Error("unexpected image operation".into())),
        }
    }
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
    fn job(&mut self, op: job::Operation, _: Peer) -> Result<Response> {
        match op {
            job::Operation::Start(_) => self.job_running = true,
            job::Operation::Stop(_) => self.job_running = false,
            job::Operation::Exits => {
                self.calls.push("job_exits");
                return Ok(Response::new(Reply::JobExits { exits: vec![] }));
            }
            job::Operation::AckExit(_) => {
                self.calls.push("ack_job_exit");
                return Ok(Response::new(Reply::Ok));
            }
            _ => {}
        }
        Ok(Response::new(Reply::Job {
            known: true,
            running: self.job_running,
            vmm_pid: None,
            exit_report: None,
            tap_index: None,
        }))
    }
    fn maintain_jobs(&mut self) -> Result<()> {
        self.calls.push("maintain_jobs");
        Ok(())
    }
}

#[test]
fn job_vm_survives_connection_loss_and_is_adopted_on_another_connection() {
    let (mut helper, connection, other) = fixture();
    let start = serde_json::json!({"op":"start_job", "job_id":JOB, "account":"a",
        "workspace_disk_id":"workspace-1", "image_generation_id":"gen-1", "vcpus":7,
        "memory_mib":1536, "burst_mib":512});
    assert!(matches!(
        request(&mut helper, connection, start).unwrap().reply,
        Reply::Job { running: true, .. }
    ));
    helper.disconnect(connection);
    helper.maintain();
    assert!(matches!(
        request(
            &mut helper,
            other,
            serde_json::json!({"op":"adopt_job", "job_id":JOB})
        )
        .unwrap()
        .reply,
        Reply::Job { running: true, .. }
    ));
    assert_eq!(helper.machine.calls, ["reconcile", "maintain_jobs"]);
}

#[test]
fn job_exit_requests_require_authorised_connections_and_strict_messages() {
    let (mut helper, connection, _) = fixture();
    for message in [
        serde_json::json!({"op":"job_exits"}),
        serde_json::json!({"op":"ack_job_exit", "job_id":JOB, "oom_kill":3}),
    ] {
        assert!(request(&mut helper, 99, message.clone()).is_err());
        request(&mut helper, connection, message.clone()).unwrap();
        let mut extra = message;
        extra["path"] = serde_json::json!("/etc/shadow");
        assert!(serde_json::from_value::<Request>(extra).is_err());
    }
    assert!(request(
        &mut helper,
        connection,
        serde_json::json!({"op":"ack_job_exit", "job_id":"../other", "oom_kill":3})
    )
    .is_err());
    assert_eq!(
        helper.machine.calls,
        ["reconcile", "job_exits", "ack_job_exit"]
    );
}

#[test]
fn job_vm_invalid_identity_or_paths_never_reach_machine() {
    let (mut helper, connection, _) = fixture();
    let start = serde_json::json!({"op":"start_job", "job_id":JOB, "account":"a",
        "workspace_disk_id":"../a", "image_generation_id":"gen-1", "vcpus":7,
        "memory_mib":1536});
    assert!(request(&mut helper, connection, start.clone()).is_err());
    let mut extra = start;
    extra["workspace_disk_id"] = serde_json::json!("workspace-1");
    extra["rootfs"] = serde_json::json!("/etc/shadow");
    assert!(serde_json::from_value::<Request>(extra).is_err());
    assert!(!helper.machine.job_running);
}

#[test]
fn job_vm_settings_require_a_separate_uid_range_and_a_slice_ceiling() {
    let mut config: config::Config =
        serde_json::from_str(include_str!("../fixtures/helper.json")).unwrap();
    config.job_vms = Some(config::JobVms {
        workspace_disk_max_bytes: Some(1073741824),
        fixed_path_repositories: Default::default(),
        uid_base: 500000,
        uid_count: 128,
        memory_max_bytes: 1073741824,
    });
    config.validate_resources().unwrap();
    for (base, count, ceiling) in [
        (500000, 128, 0),
        (500000, 128, 1),
        (500000, 0, 1073741824),
        (400000, 128, 1073741824),
        (config.uid_base, 128, 1073741824),
        (config.job_account_range.base, 128, 1073741824),
        (u32::MAX, 2, 1073741824),
    ] {
        config.job_vms = Some(config::JobVms {
            workspace_disk_max_bytes: Some(1073741824),
            fixed_path_repositories: Default::default(),
            uid_base: base,
            uid_count: count,
            memory_max_bytes: ceiling,
        });
        assert!(config.validate_resources().is_err());
    }
}
#[cfg(target_os = "linux")]
fn fixed_path_boot_config(roots: &[&str]) -> config::Config {
    let mut config: config::Config =
        serde_json::from_str(include_str!("../fixtures/helper.json")).unwrap();
    config.job_vms = Some(config::JobVms {
        workspace_disk_max_bytes: Some(u64::MAX - 511),
        fixed_path_repositories: std::collections::BTreeMap::from([(
            "acct:01KXJJHK4V6DB42W1YAN2XD9QN".into(),
            roots.iter().map(|root| (*root).into()).collect(),
        )]),
        uid_base: 500000,
        uid_count: 128,
        memory_max_bytes: 1073741824,
    });
    config
}

#[cfg(target_os = "linux")]
#[test]
fn fixed_path_boot_validation_refuses_three_long_roots_and_names_account() {
    let config = fixed_path_boot_config(&[
        "/Users/ufukaltinok/Work/Projects/CortexKit/motor",
        "/Users/ufukaltinok/Work/Projects/CortexKit/aft",
        "/Users/ufukaltinok/Work/Projects/CortexKit/broca",
    ]);
    for error in [
        config.validate_resources().unwrap_err(),
        config.validate().unwrap_err(),
    ] {
        assert!(error.0.contains("acct:01KXJJHK4V6DB42W1YAN2XD9QN"));
        assert!(error.0.contains("1638 bytes"), "{error}");
        assert!(error.0.contains("1536-byte command-line budget"), "{error}");
        assert!(error.0.contains("'*' or fewer roots"), "{error}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn fixed_path_boot_validation_measures_lists_larger_than_the_runner_argument_limit() {
    let root = format!("/{}", "x".repeat(17 * 1024));
    let error = fixed_path_boot_config(&[&root]).validate().unwrap_err();
    assert!(error.0.contains("acct:01KXJJHK4V6DB42W1YAN2XD9QN"));
    assert!(error.0.contains("job VM boot arguments are"), "{error}");
    assert!(error.0.contains("1536-byte command-line budget"), "{error}");
    assert!(error.0.contains("'*' or fewer roots"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn fixed_path_boot_validation_accepts_two_long_roots() {
    fixed_path_boot_config(&[
        "/Users/ufukaltinok/Work/Projects/CortexKit/motor",
        "/Users/ufukaltinok/Work/Projects/CortexKit/aft",
    ])
    .validate()
    .unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn fixed_path_boot_validation_accepts_whole_account_wildcard() {
    fixed_path_boot_config(&["*"]).validate().unwrap();
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

#[test]
fn per_job_readiness_is_authorized_and_uses_helper_configuration() {
    let (mut helper, connection, _) = fixture();
    let value = serde_json::json!({"op":"per_job_readiness"});
    assert!(request(&mut helper, u64::MAX, value.clone()).is_err());
    assert!(!helper.machine.calls.contains(&"per_job_readiness"));
    for configured in [false, true] {
        helper.machine.job_vms_configured = configured;
        let reply = request(&mut helper, connection, value.clone()).unwrap();
        let Reply::PerJobReadiness {
            job_vms_configured,
            workspace_disk_max_bytes,
            published_generations,
        } = reply.reply
        else {
            panic!("wrong readiness reply");
        };
        assert_eq!(job_vms_configured, configured);
        assert_eq!(workspace_disk_max_bytes, configured.then_some(4096));
        assert_eq!(
            published_generations,
            if configured { vec!["gen-1"] } else { vec![] }
        );
    }
    assert!(serde_json::from_value::<Request>(
        serde_json::json!({"op":"per_job_readiness","job_vms_configured":true})
    )
    .is_err());
    helper.shutdown().unwrap();
    assert!(request(&mut helper, connection, value).is_err());
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
    assert_eq!(
        h.machine.calls,
        ["reconcile", "start", "maintain_jobs", "cleanup"]
    );
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
    // One in-flight stop/suspend reply (600 s), one tenant's prepare (400 s),
    // and Firecracker exit/helper cleanup (20 s); the unit explains each part.
    let timeout: u64 = unit
        .lines()
        .find_map(|l| l.strip_prefix("TimeoutStopSec="))
        .unwrap()
        .parse()
        .unwrap();
    assert!(timeout >= 1020, "{timeout}");
}

#[test]
fn storage_mount_keeps_vm_reads_below_root_owned_ancestors() {
    let mount = include_str!("../systemd/motor-vm-storage.mount");
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
    let mount = include_str!("../systemd/motor-vm-storage.mount");
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
fn config_requires_safe_explicit_network_before_egress_not_cleanup() {
    let value: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/helper.json")).unwrap();
    for missing in ["uplink", "public_ipv4"] {
        let mut broken = value.clone();
        broken.as_object_mut().unwrap().remove(missing);
        let config: crate::config::Config = serde_json::from_value(broken).unwrap();
        config.validate_resources().unwrap();
        assert!(config.validate_network().is_err());
    }
    for uplink in [
        "",
        "lo",
        "any",
        "..",
        "-eno1",
        ".eno1",
        "_eno1",
        "eno1:0",
        "eno/1",
        "en o1",
        "eno\t1",
        "enö1",
        "mtap*",
        "eno1\";accept",
        "0123456789012345",
    ] {
        let mut broken = value.clone();
        broken["uplink"] = uplink.into();
        let config: crate::config::Config = serde_json::from_value(broken).unwrap();
        config.validate_resources().unwrap();
        assert!(config.validate().unwrap_err().0.contains("uplink"));
    }
    for ip in ["10.0.0.1", "127.0.0.1", "224.0.0.1", "192.0.2.1"] {
        let mut broken = value.clone();
        broken["public_ipv4"] = ip.into();
        let config: crate::config::Config = serde_json::from_value(broken).unwrap();
        assert!(config.validate().unwrap_err().0.contains("uplink"));
    }
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
    config.firecracker = "relative".into();
    assert!(config.validate().is_err());
}

#[test]
fn installed_slot_config_remains_accepted_but_optional() {
    let mut installed: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/helper.json")).unwrap();
    let config: config::Config = serde_json::from_value(installed.clone()).unwrap();
    config.validate().unwrap();
    let fields = [
        "loader",
        "job_snapshot",
        "scan_snapshot",
        "hugepages_per_slot",
        "agent_port",
        "proxy_port",
        "proxy_for_jobs",
    ];
    assert_eq!(config.deprecated_slot_fields(), fields);
    for field in fields {
        installed.as_object_mut().unwrap().remove(field);
    }
    let config: config::Config = serde_json::from_value(installed.clone()).unwrap();
    config.validate().unwrap();
    assert!(config.deprecated_slot_fields().is_empty());
    installed["unknown_setting"] = true.into();
    assert!(serde_json::from_value::<config::Config>(installed).is_err());
}
