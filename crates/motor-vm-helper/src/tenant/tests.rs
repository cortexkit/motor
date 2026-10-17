use super::*;
use serde_json::json;

pub(crate) fn registry(name: &str) -> Registry {
    let root = PathBuf::from(TENANTS).join(name);
    Registry {
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
struct Fake {
    calls: Vec<String>,
    running: bool,
    generation: u64,
    saved: Option<SnapshotKey>,
    fail: &'static str,
    space: u64,
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
    fn registry(&mut self, _: &str) -> Result<Registry> {
        self.call("registry")?;
        Ok(registry("a"))
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
        Ok(())
    }
    fn thaw(&mut self, _: &str) -> Result<()> {
        self.call("thaw")
    }
    fn snapshot(&mut self, _: &str, _: &Registry, k: &SnapshotKey) -> Result<()> {
        self.call("snapshot-create")?;
        self.saved = Some(k.clone());
        Ok(())
    }
    fn stop(&mut self, _: &str) -> Result<()> {
        self.call("stop")?;
        self.running = false;
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
        ["registry", "prepare", "snapshot-create", "stop"]
    );
    assert_eq!(c.0.saved, Some(key(4)));
    c.0.running = true;
    c.0.calls.clear();
    c.handle("a", Operation::Stop).unwrap();
    assert_eq!(c.0.calls, ["registry", "prepare", "stop"]);
}
#[test]
fn hosted_prepare_refusal_or_timeout_never_creates_snapshot() {
    for reason in ["agent-refused", "agent-timeout"] {
        let mut c = Controller(Fake {
            running: true,
            fail: reason,
            ..Fake::default()
        });
        assert!(c.handle("a", Operation::Suspend).is_err(), "{reason}");
        assert_eq!(c.0.calls, ["registry", "prepare", "thaw"]);
        assert!(c.0.running);
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

#[test]
fn hosted_reconnect_adopts_without_waking_or_stopping_the_tenant() {
    struct Machine(Controller<Fake>);
    impl crate::Machine for Machine {
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
    }
    let mut helper =
        crate::Helper::new(Machine(Controller(Fake::default())), 1, vec![1000]).unwrap();
    let peer = crate::Peer {
        uid: 1000,
        gid: 1000,
    };
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
