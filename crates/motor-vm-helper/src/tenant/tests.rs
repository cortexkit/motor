use super::*;
use serde_json::json;

pub(crate) fn registry(name: &str) -> Registry {
    let root = PathBuf::from(TENANTS).join(name);
    Registry {
        capture_console: false,
        disk: root.join("disk.img"),
        generation: root.join("generation"),
        suspend: root.join("suspend"),
        image_version: "v1".into(),
        vcpu_ceiling: 2,
        guest_ram_mib: 18432,
        disk_quota_bytes: 21474836480,
        cpuset: "0-1".into(),
        tap_index: 1,
        idle_suspend_s: 60,
        host_floor_bytes: 2147483648,
        memory_headroom_mib: 512,
        snapshot_memory_high_mib: 16384,
        io_weight: 100,
    }
}

#[test]
fn tenant_paths_use_trusted_bind_view_and_refuse_old_source_spelling() {
    let mut r = registry("a");
    assert_eq!(
        r.disk,
        PathBuf::from("/var/lib/motor-vm/tenants/a/disk.img")
    );
    assert_eq!(
        r.image(),
        PathBuf::from("/var/lib/motor-vm/tenant-images/v1")
    );
    r.validate("a").unwrap();
    r.disk = PathBuf::from("/srv/build/vm/tenants/a/disk.img");
    r.generation = PathBuf::from("/srv/build/vm/tenants/a/generation");
    r.suspend = PathBuf::from("/srv/build/vm/tenants/a/suspend");
    assert!(r.validate("a").is_err());
}
struct Fake {
    calls: Vec<String>,
    running: bool,
    generation: u64,
    saved: Option<SnapshotKey>,
    fail: &'static str,
    space: u64,
    unclean: bool,
    marker: bool,
    present: bool,
    frozen: bool,
    partial_snapshot: bool,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            calls: vec![],
            running: false,
            generation: 4,
            saved: None,
            fail: "",
            space: u64::MAX,
            unclean: false,
            marker: false,
            present: false,
            frozen: false,
            partial_snapshot: false,
        }
    }
}
impl Fake {
    fn call(&mut self, name: &str) -> Result<()> {
        self.calls.push(name.into());
        if self.fail == name {
            Err(Error(format!("{name} failed")))
        } else {
            Ok(())
        }
    }
}
fn key(generation: u64) -> SnapshotKey {
    SnapshotKey {
        version: "v1".into(),
        kernel_sha256: "a".repeat(64),
        rootfs_sha256: "b".repeat(64),
        generation,
    }
}
impl Backend for Fake {
    fn unclean(&mut self, _: &Registry) -> Result<bool> {
        Ok(self.unclean)
    }
    fn mark_unclean(&mut self, _: &Registry, _: &str) -> Result<()> {
        self.call("unclean-fsync")?;
        self.unclean = true;
        Ok(())
    }
    fn recovery_complete(&mut self, _: &str, _: &Registry) -> Result<()> {
        self.call("guest-recovery-status")?;
        self.unclean = false;
        Ok(())
    }
    fn was_running(&mut self, _: &Registry) -> Result<bool> {
        Ok(self.marker)
    }
    fn clear_running(&mut self, _: &Registry) -> Result<()> {
        self.call("clear-running-marker")?;
        self.marker = false;
        Ok(())
    }
    fn running_tenants(&mut self) -> Vec<String> {
        if self.running {
            vec!["a".into()]
        } else {
            vec![]
        }
    }
    fn metrics(&mut self, _: &str, _: &Registry) -> Result<(u64, u64)> {
        self.call("metrics")?;
        Ok((1048576, 123))
    }
    fn registry(&mut self, _: &str) -> Result<Registry> {
        self.call("registry")?;
        Ok(registry("a"))
    }
    fn vm_present(&mut self, _: &str) -> Result<bool> {
        Ok(self.present)
    }
    fn remove(&mut self, tenant: &str, _: &Registry) -> Result<PathBuf> {
        self.call("remove")?;
        Ok(PathBuf::from(format!(
            "/var/lib/motor-vm/removed/{tenant}-1"
        )))
    }
    fn key(&mut self, _: &Registry) -> Result<SnapshotKey> {
        Ok(key(self.generation))
    }
    fn saved_key(&mut self, _: &Registry) -> Result<Option<SnapshotKey>> {
        Ok(self.saved.clone())
    }
    fn running(&mut self, _: &str) -> bool {
        self.running
    }
    fn load(&mut self, _: &str, _: &Registry, saved: bool) -> Result<()> {
        self.running = true;
        // The real backend writes the marker just before the VMM spawns.
        self.marker = true;
        self.call(if saved {
            "load-saved-tap"
        } else {
            "load-base-size-tap"
        })
    }
    fn drive(&mut self, _: &str, _: &Registry) -> Result<()> {
        self.call("drive")
    }
    fn consume(&mut self, _: &Registry) -> Result<()> {
        self.saved = None;
        self.call("unlink-fsync")
    }
    fn advance(&mut self, _: &Registry) -> Result<u64> {
        self.call("generation-fsync")?;
        self.generation += 1;
        Ok(self.generation)
    }
    fn resume(&mut self, _: &str) -> Result<()> {
        self.call("resume")
    }
    fn prepare(&mut self, _: &str) -> Result<()> {
        self.call("prepare")?;
        #[cfg(target_os = "linux")]
        if matches!(self.fail, "agent-refused" | "agent-timeout") {
            use std::{
                io::{Read, Write},
                os::unix::net::UnixStream,
                time::Duration,
            };
            let (client, mut server) = UnixStream::pair()?;
            let refusal = self.fail == "agent-refused";
            let thread = std::thread::spawn(move || {
                let mut header = [0; 4];
                server.read_exact(&mut header).unwrap();
                let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
                server.read_exact(&mut bytes).unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                    json!({"op":"prepare_snapshot","kind":"suspend"})
                );
                if refusal {
                    let reply = br#"{"ok":false,"error":"snapshot_busy"}"#;
                    server
                        .write_all(&(reply.len() as u32).to_be_bytes())
                        .unwrap();
                    server.write_all(reply).unwrap();
                } else {
                    // Keep the server end open without responding until the
                    // client's kernel read deadline expires and closes it.
                    let mut byte = [0];
                    assert_eq!(server.read(&mut byte).unwrap(), 0);
                }
            });
            let result = crate::linux::tenant::exchange_agent(
                client,
                &json!({"op":"prepare_snapshot","kind":"suspend"}),
                Duration::from_millis(10),
            );
            thread.join().unwrap();
            return result;
        }
        #[cfg(not(target_os = "linux"))]
        if matches!(self.fail, "agent-refused" | "agent-timeout") {
            return Err(Error(self.fail.into()));
        }
        self.frozen = true;
        Ok(())
    }
    fn thaw(&mut self, _: &str) -> Result<()> {
        self.call("thaw")?;
        self.frozen = false;
        Ok(())
    }
    fn snapshot(&mut self, _: &str, _: &Registry, k: &SnapshotKey) -> Result<()> {
        self.call("snapshot-create")?;
        if self.fail == "create-timeout" {
            assert!(
                self.frozen,
                "only a drained and frozen VM may be snapshotted"
            );
            self.partial_snapshot = true;
            return Err(Error(format!("{SNAPSHOT_CREATE_UNCERTAIN}: Firecracker PUT /snapshot/create step=read_status api_timeout elapsed_ms=480000")));
        }
        self.saved = Some(k.clone());
        Ok(())
    }
    fn stop(&mut self, _: &str) -> Result<()> {
        self.call("stop")?;
        self.running = false;
        self.partial_snapshot = false;
        self.frozen = false;
        Ok(())
    }
    fn available(&mut self, _: &Registry) -> Result<u64> {
        Ok(self.space)
    }
    fn cpu(&mut self, _: &str, value: &str) -> Result<()> {
        self.call(value)
    }
    fn egress(&mut self, _: &str, _: &Registry) -> Result<()> {
        self.call("egress")
    }
    fn relay(&mut self, _: &str, port: u32) -> Result<OwnedFd> {
        self.call(&format!("relay-{port}"))?;
        Ok(std::os::unix::net::UnixStream::pair()?.0.into())
    }
}

#[test]
fn tenant_relay_failure_identifies_tenant_port_and_step() {
    let mut c = Controller(Fake {
        fail: "relay-52",
        ..Fake::default()
    });
    let error = c.handle("a", Operation::Relay(52)).err().unwrap();
    assert_eq!(
        error.to_string(),
        "open_relay tenant=a port=52 step=open_guest_connection: relay-52 failed"
    );
}

#[test]
fn tenant_metrics_are_read_only_bound_to_registry_and_reject_paths() {
    let mut c = Controller(Fake::default());
    assert!(matches!(
        c.handle("a", Operation::Metrics).unwrap().reply,
        Reply::TenantMetrics {
            memory_bytes: 1048576,
            egress_bytes: 123
        }
    ));
    assert_eq!(c.0.calls, ["registry", "metrics"]);
    c.0.calls.clear();
    assert!(c.handle("b", Operation::Metrics).is_err());
    assert_eq!(c.0.calls, ["registry"]);
    c.0.calls.clear();
    for name in ["", ".", "..", "../a", "a/b"] {
        assert!(c.handle(name, Operation::Metrics).is_err());
    }
    assert!(c.0.calls.is_empty());
    for body in [
        json!({"op":"tenant_metrics","tenant":"a","path":"/etc/passwd"}),
        json!({"op":"tenant_metrics","tenant":"a","handle":1}),
    ] {
        assert!(serde_json::from_value::<crate::Request>(body).is_err());
    }
}
#[test]
fn hosted_restore_consumes_snapshot_and_advances_before_resume() {
    let mut c = Controller(Fake {
        saved: Some(key(4)),
        ..Fake::default()
    });
    let response = c.handle("a", Operation::Restore).unwrap();
    assert!(matches!(
        response.reply,
        Reply::Tenant {
            restored: true,
            generation: 5,
            ..
        }
    ));
    assert_eq!(
        c.0.calls,
        [
            "registry",
            "egress",
            "load-saved-tap",
            "drive",
            "unlink-fsync",
            "generation-fsync",
            "resume",
            "thaw"
        ]
    );
    assert!(c.0.saved.is_none());
    c.0.running = false;
    c.0.calls.clear();
    c.handle("a", Operation::Ensure).unwrap();
    assert!(c.0.calls.contains(&"load-base-size-tap".into()));
    assert!(!c.0.calls.contains(&"load-saved-tap".into()));
}
#[test]
fn hosted_mismatched_keys_and_failed_load_take_fresh_start() {
    for mismatch in ["version", "kernel", "rootfs", "generation", "failed-load"] {
        let mut saved = key(4);
        match mismatch {
            "version" => saved.version = "v0".into(),
            "kernel" => saved.kernel_sha256 = "c".repeat(64),
            "rootfs" => saved.rootfs_sha256 = "c".repeat(64),
            "generation" => saved.generation = 3,
            _ => {}
        }
        let mut c = Controller(Fake {
            saved: Some(saved),
            fail: if mismatch == "failed-load" {
                "load-saved-tap"
            } else {
                ""
            },
            ..Fake::default()
        });
        let response = c.handle("a", Operation::Restore).unwrap();
        assert!(
            matches!(
                response.reply,
                Reply::Tenant {
                    restored: false,
                    generation: 5,
                    ..
                }
            ),
            "{mismatch}"
        );
        let calls = &c.0.calls;
        assert!(
            calls.iter().position(|s| s == "drive").unwrap()
                < calls.iter().position(|s| s == "resume").unwrap()
        );
        assert!(calls.contains(&"load-base-size-tap".into()));
    }
}
#[test]
fn hosted_suspend_keys_then_stops_and_stop_never_snapshots() {
    let mut c = Controller(Fake {
        running: true,
        ..Fake::default()
    });
    c.handle("a", Operation::Suspend).unwrap();
    assert_eq!(
        c.0.calls,
        [
            "registry",
            "prepare",
            "snapshot-create",
            "stop",
            "clear-running-marker"
        ]
    );
    assert_eq!(c.0.saved, Some(key(4)));
    c.0.running = true;
    c.0.calls.clear();
    c.handle("a", Operation::Stop).unwrap();
    assert_eq!(
        c.0.calls,
        ["registry", "prepare", "stop", "clear-running-marker"]
    );
}
#[test]
fn hosted_prepare_refusal_or_timeout_never_creates_snapshot() {
    for reason in ["agent-refused", "agent-timeout"] {
        let mut c = Controller(Fake {
            running: true,
            fail: reason,
            ..Fake::default()
        });
        let error = c.handle("a", Operation::Suspend).err().unwrap();
        assert!(error.0.starts_with("suspend_prepare_refused:"), "{error}");
        assert_eq!(c.0.calls, ["registry", "prepare", "thaw"]);
        assert!(c.0.running);
        assert!(!c.0.frozen && !c.0.partial_snapshot && c.0.saved.is_none());
    }
}
#[test]
fn hosted_failed_snapshot_thaws_and_keeps_vm_running() {
    let mut c = Controller(Fake {
        running: true,
        fail: "snapshot-create",
        ..Fake::default()
    });
    assert!(c.handle("a", Operation::Suspend).is_err());
    assert_eq!(
        c.0.calls,
        [
            "registry",
            "prepare",
            "snapshot-create",
            "unlink-fsync",
            "thaw"
        ]
    );
    assert!(c.0.running);
}

#[test]
fn hosted_create_timeout_reaps_writer_cleans_partial_snapshot_and_cold_boots_with_recovery() {
    let mut c = Controller(Fake {
        running: true,
        marker: true,
        fail: "create-timeout",
        ..Fake::default()
    });
    let error = c.handle("a", Operation::Suspend).err().unwrap();
    assert!(
        error.0.starts_with("suspend_create_failed_vm_stopped"),
        "{error}"
    );
    assert_eq!(
        c.0.calls,
        [
            "registry",
            "prepare",
            "snapshot-create",
            "unclean-fsync",
            "stop",
            "unlink-fsync"
        ]
    );
    assert!(!c.0.running && !c.0.partial_snapshot && !c.0.frozen);
    assert!(c.0.saved.is_none() && c.0.marker && c.0.unclean);
    c.0.fail = "";
    c.0.calls.clear();
    let reply = c.handle("a", Operation::Ensure).unwrap().reply;
    assert!(matches!(
        reply,
        Reply::Tenant {
            running: true,
            restored: false,
            recovery_required: true,
            ..
        }
    ));
    assert!(c.0.calls.contains(&"load-base-size-tap".into()));
    assert!(!c.0.calls.contains(&"load-saved-tap".into()));
    assert!(!c.0.frozen);
    c.handle("a", Operation::RecoveryComplete).unwrap();
    assert!(!c.0.unclean);
    c.handle("a", Operation::Relay(52)).unwrap();
}
#[test]
fn hosted_suspend_space_check_is_bytes_and_precedes_prepare() {
    let mut c = Controller(Fake {
        running: true,
        space: registry("a").snapshot_bytes().unwrap() - 1,
        ..Fake::default()
    });
    assert_eq!(
        c.handle("a", Operation::Suspend).err().unwrap().0,
        "suspend_skipped_space"
    );
    assert_eq!(c.0.calls, ["registry"]);
}
#[test]
fn hosted_names_are_rejected_before_registry_open() {
    let mut c = Controller(Fake::default());
    for name in [".", "..", "", "foo/bar", "\0"] {
        assert!(c.handle(name, Operation::Ensure).is_err());
    }
    assert!(c.0.calls.is_empty());
}
#[test]
fn hosted_cpu_bounds_and_relay_ports_are_closed() {
    let mut c = Controller(Fake::default());
    for count in [0, 3, u32::MAX] {
        assert!(c.handle("a", Operation::Cpu(count)).is_err());
    }
    assert!(!c.0.calls.iter().any(|s| s.contains("100000")));
    for (count, value) in [(1, "100000 100000"), (2, "200000 100000")] {
        c.handle("a", Operation::Cpu(count)).unwrap();
        assert_eq!(c.0.calls.last().unwrap(), value);
    }
    for port in [0, 51, 55, u32::MAX] {
        assert!(c.handle("a", Operation::Relay(port)).is_err());
    }
    for port in 52..=54 {
        assert_eq!(c.handle("a", Operation::Relay(port)).unwrap().fds.len(), 1);
    }
}
#[test]
fn hosted_tenant_wire_rejects_extra_arguments_without_changing_job_ops() {
    for op in [
        "ensure",
        "attach_disk",
        "suspend",
        "restore",
        "stop_tenant",
        "set_egress",
        "adopt",
        "set_cpu_max",
        "open_relay",
        "remove_tenant",
    ] {
        let mut value = json!({"op":op,"tenant":"a"});
        if op == "set_cpu_max" {
            value["vcpus"] = json!(1);
        }
        if op == "open_relay" {
            value["port"] = json!(52);
        }
        assert!(
            serde_json::from_value::<crate::Request>(value.clone()).is_ok(),
            "{op}"
        );
        value["path"] = json!("/tmp/not-allowed");
        assert!(
            serde_json::from_value::<crate::Request>(value).is_err(),
            "{op}"
        );
    }
    assert!(serde_json::from_value::<crate::Request>(
        json!({"op":"acquire","instance":"main","job_id":"00000000-0000-4000-8000-000000000001"})
    )
    .is_ok());
}
#[test]
fn hosted_caps_and_bases_are_per_guest_size() {
    let r = registry("a");
    assert_eq!(r.memory_max().unwrap(), 19864223744);
    assert_eq!(r.vcpu_ceiling + 64, 66);
    assert_eq!(r.base(), PathBuf::from(IMAGES).join("v1/base/2-18432"));
    let mut small = registry("b");
    small.vcpu_ceiling = 1;
    small.guest_ram_mib = 8192;
    small.tap_index = 2;
    assert_ne!(r.base(), small.base());
    assert_ne!(r.tap(), small.tap());
}

/// A helper machine with only tenant VMs, as on a hosted host.
struct TenantMachine(Controller<Fake>);
impl crate::Machine for TenantMachine {
    fn reconcile(&mut self) -> Result<()> {
        Ok(())
    }
    fn start(&mut self, _: usize, _: &crate::Lease, _: crate::Kind) -> Result<Vec<OwnedFd>> {
        unreachable!()
    }
    fn stop(&mut self, _: usize, _: &crate::Lease, _: bool) -> Result<()> {
        unreachable!()
    }
    fn cleanup(&mut self, _: usize) -> Result<()> {
        unreachable!()
    }
    fn alive(&mut self, _: usize) -> bool {
        unreachable!()
    }
    fn tenant(&mut self, tenant: &str, op: Operation) -> Result<Response> {
        self.0.handle(tenant, op)
    }
    fn shutdown(&mut self) -> Result<()> {
        match self.0.stop_all().as_slice() {
            [] => Ok(()),
            failures => Err(Error(format!("{failures:?}"))),
        }
    }
}
const PEER: crate::Peer = crate::Peer {
    uid: 1000,
    gid: 1000,
};

#[test]
fn hosted_reconnect_adopts_without_waking_or_stopping_the_tenant() {
    let mut helper =
        crate::Helper::new(TenantMachine(Controller(Fake::default())), 1, vec![1000]).unwrap();
    let peer = PEER;
    let first = helper.connect(peer).unwrap();
    helper
        .handle(first, crate::Request::Ensure { tenant: "a".into() })
        .unwrap();
    helper.disconnect(first);
    helper.maintain();
    helper.machine.0 .0.calls.clear();
    let next = helper.connect(peer).unwrap();
    let response = helper
        .handle(next, crate::Request::Adopt { tenant: "a".into() })
        .unwrap();
    assert!(matches!(
        response.reply,
        Reply::Tenant {
            running: true,
            generation: 5,
            ..
        }
    ));
    assert_eq!(helper.machine.0 .0.calls, ["registry"]);
}

#[test]
fn hosted_stop_bounds_quiesce_records_unclean_before_hard_stop_and_never_snapshots() {
    for fail in ["", "prepare", "agent-refused", "agent-timeout"] {
        let mut c = Controller(Fake {
            running: true,
            fail,
            ..Fake::default()
        });
        c.handle("a", Operation::Stop).unwrap();
        assert!(!c.0.running);
        assert_eq!(c.0.unclean, !fail.is_empty());
        assert!(!c.0.calls.iter().any(|c| c == "snapshot-create"));
        if !fail.is_empty() {
            let flag = c.0.calls.iter().position(|c| c == "unclean-fsync").unwrap();
            let stop = c.0.calls.iter().position(|c| c == "stop").unwrap();
            assert!(flag < stop);
        }
    }
}

#[test]
fn hosted_unclean_disk_never_restores_snapshot_and_flag_waits_for_guest_recovery_success() {
    let mut c = Controller(Fake {
        unclean: true,
        saved: Some(key(4)),
        ..Fake::default()
    });
    assert!(matches!(
        c.handle("a", Operation::Ensure).unwrap().reply,
        Reply::Tenant {
            recovery_required: true,
            ..
        }
    ));
    assert!(c.0.calls.iter().any(|c| c == "load-base-size-tap"));
    assert!(!c.0.calls.iter().any(|c| c == "load-saved-tap"));
    c.0.fail = "guest-recovery-status";
    assert!(c.handle("a", Operation::RecoveryComplete).is_err());
    assert!(c.0.unclean);
    c.0.fail = "";
    c.handle("a", Operation::RecoveryComplete).unwrap();
    assert!(!c.0.unclean);
}

/// Runs `stop_on_signal` on its own thread, as the service does, and returns
/// the thread with its id. The signal is sent to that thread only, and only it
/// has SIGTERM blocked, so no other test thread can take the default action.
fn signal_waiter(
    helper: &std::sync::Arc<std::sync::Mutex<crate::Helper<TenantMachine>>>,
) -> (std::thread::JoinHandle<Result<i32>>, usize) {
    let helper = helper.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let signals = crate::signals::StopSignals::block()?;
        sender
            .send(unsafe { libc::pthread_self() } as usize)
            .unwrap();
        crate::signals::stop_on_signal(&helper, &signals)
    });
    (waiter, receiver.recv().unwrap())
}

#[test]
fn service_stop_signal_runs_the_clean_tenant_stop_before_the_helper_exits() {
    let helper = std::sync::Arc::new(std::sync::Mutex::new(
        crate::Helper::new(TenantMachine(Controller(Fake::default())), 1, vec![1000]).unwrap(),
    ));
    let connection = helper.lock().unwrap().connect(PEER).unwrap();
    helper
        .lock()
        .unwrap()
        .handle(connection, crate::Request::Ensure { tenant: "a".into() })
        .unwrap();
    helper.lock().unwrap().machine.0 .0.calls.clear();
    let (waiter, thread) = signal_waiter(&helper);
    assert!(helper.lock().unwrap().machine.0 .0.running);
    assert_eq!(
        unsafe { libc::pthread_kill(thread as libc::pthread_t, libc::SIGTERM) },
        0
    );
    assert_eq!(waiter.join().unwrap().unwrap(), libc::SIGTERM);

    let mut helper = helper.lock().unwrap();
    let fake = &helper.machine.0 .0;
    assert_eq!(
        fake.calls,
        [
            "registry",
            "prepare",
            "stop",
            "clear-running-marker",
            "registry"
        ]
    );
    assert!(!fake.running);
    assert!(!fake.unclean);
    assert!(!fake.marker);
    // Nothing can start the VM again between the clean stop and the exit.
    let refused = helper
        .handle(connection, crate::Request::Ensure { tenant: "a".into() })
        .err()
        .unwrap();
    assert_eq!(refused.0, "helper is stopping");
    assert!(!helper.machine.0 .0.running);
}

#[test]
fn service_stop_signal_records_a_tenant_whose_guest_flush_fails_as_unclean() {
    let helper = std::sync::Arc::new(std::sync::Mutex::new(
        crate::Helper::new(TenantMachine(Controller(Fake::default())), 1, vec![1000]).unwrap(),
    ));
    let connection = helper.lock().unwrap().connect(PEER).unwrap();
    helper
        .lock()
        .unwrap()
        .handle(connection, crate::Request::Ensure { tenant: "a".into() })
        .unwrap();
    {
        let fake = &mut helper.lock().unwrap().machine.0 .0;
        fake.calls.clear();
        fake.fail = "agent-timeout";
    }
    let (waiter, thread) = signal_waiter(&helper);
    unsafe { libc::pthread_kill(thread as libc::pthread_t, libc::SIGTERM) };
    let error = waiter.join().unwrap().unwrap_err();
    assert!(error.0.contains("guest flush failed"), "{error}");

    let fake = &helper.lock().unwrap().machine.0 .0;
    assert!(!fake.running);
    assert!(fake.unclean);
    assert!(fake.marker);
    let flag = fake
        .calls
        .iter()
        .position(|c| c == "unclean-fsync")
        .unwrap();
    let stop = fake.calls.iter().position(|c| c == "stop").unwrap();
    assert!(flag < stop, "{:?}", fake.calls);
}

/// What survives a SIGKILL or a power loss: the tenant home on disk. The VMs
/// and the helper's memory of them do not.
fn after_kill(previous: &Fake) -> Fake {
    Fake {
        generation: previous.generation,
        saved: Some(key(previous.generation)),
        unclean: previous.unclean,
        marker: previous.marker,
        ..Fake::default()
    }
}

#[test]
fn killed_helper_leaves_the_running_marker_so_the_next_start_runs_recovery() {
    let mut first = Controller(Fake::default());
    first.handle("a", Operation::Ensure).unwrap();
    assert!(first.0.marker);
    // Killed with no clean stop. A suspend snapshot that matches the image
    // must still not be restored over a disk the guest never flushed.
    let mut next = Controller(after_kill(&first.0));
    let reply = next.handle("a", Operation::Ensure).unwrap().reply;
    assert!(matches!(
        reply,
        Reply::Tenant {
            running: true,
            restored: false,
            recovery_required: true,
            ..
        }
    ));
    let calls = &next.0.calls;
    let flag = calls.iter().position(|c| c == "unclean-fsync").unwrap();
    let load = calls
        .iter()
        .position(|c| c == "load-base-size-tap")
        .unwrap();
    assert!(flag < load, "{calls:?}");
    assert!(!calls.iter().any(|c| c == "load-saved-tap"));

    // A clean stop removes the marker, so the next start needs no recovery.
    let mut clean = Controller(Fake::default());
    clean.handle("a", Operation::Ensure).unwrap();
    clean.handle("a", Operation::Stop).unwrap();
    assert!(!clean.0.marker);
    let mut next = Controller(after_kill(&clean.0));
    let reply = next.handle("a", Operation::Ensure).unwrap().reply;
    assert!(matches!(
        reply,
        Reply::Tenant {
            restored: true,
            recovery_required: false,
            ..
        }
    ));
    assert!(!next.0.calls.iter().any(|c| c == "unclean-fsync"));
}

#[test]
fn a_vm_that_died_on_its_own_is_recorded_unclean_before_it_is_started_again() {
    let mut c = Controller(Fake::default());
    c.handle("a", Operation::Ensure).unwrap();
    c.0.running = false;
    c.0.calls.clear();
    let reply = c.handle("a", Operation::Ensure).unwrap().reply;
    assert!(matches!(
        reply,
        Reply::Tenant {
            recovery_required: true,
            ..
        }
    ));
    assert_eq!(c.0.calls[..2], ["registry", "unclean-fsync"]);
}

#[test]
fn removal_refuses_an_unknown_tenant_and_a_running_vm_before_touching_anything() {
    // An unknown name: no registry entry, and no interrupted removal to finish.
    let mut c = Controller(Fake {
        fail: "registry",
        ..Fake::default()
    });
    assert!(c.handle("a", Operation::Remove).is_err());
    assert_eq!(c.0.calls, ["registry"]);
    // A registry entry that belongs to another tenant name.
    let mut c = Controller(Fake::default());
    assert!(c.handle("b", Operation::Remove).is_err());
    assert_eq!(c.0.calls, ["registry"]);
    for (running, present) in [(true, false), (false, true)] {
        let mut c = Controller(Fake {
            running,
            present,
            ..Fake::default()
        });
        let error = c.handle("a", Operation::Remove).err().unwrap();
        assert_eq!(
            error.to_string(),
            "tenant VM is running; stop it before removing the tenant"
        );
        assert_eq!(
            c.0.calls,
            ["registry"],
            "running={running} present={present}"
        );
    }
    let mut c = Controller(Fake::default());
    assert!(matches!(
        c.handle("a", Operation::Remove).unwrap().reply,
        Reply::Removed { ref quarantine } if quarantine == "/var/lib/motor-vm/removed/a-1"
    ));
    assert_eq!(c.0.calls, ["registry", "remove"]);
    // The wire names a tenant and nothing else.
    assert!(serde_json::from_value::<crate::Request>(
        json!({"op":"remove_tenant","tenant":"a","quarantine":"/tmp"})
    )
    .is_err());
}
