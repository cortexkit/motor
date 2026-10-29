use super::helper::descriptor_reply;
use super::transport::{fixture_with_control, hello, respond};
use super::*;
use motor_host::helper::{read_json, write_json, Control, Helper};
use motor_host::server::{serve, unix_ms, write_frame};
use motor_vm_helper::tenant::{Backend, Controller, Operation, Registry, SnapshotKey, StartResult};
use motor_vm_helper::{Error, Request, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

type Activation = motor_vm_helper::tenant::ResizeActivation;

struct Guest {
    running: bool,
    attached: bool,
    staged: bool,
    generation: u64,
    events: Vec<String>,
    fail_space: bool,
}
impl Default for Guest {
    fn default() -> Self {
        Self {
            running: false,
            attached: false,
            staged: true,
            generation: 4,
            events: vec![],
            fail_space: false,
        }
    }
}
impl Guest {
    fn request(&mut self, request: &Value) -> Value {
        match request["op"].as_str().unwrap() {
            "attach_tenant" => {
                assert!(self.running);
                self.events.push("attach".into());
                self.attached = true;
                json!({"ok":true})
            }
            "apply_resize" => {
                // The real agent requires /run/motor-tenant-generation, written
                // only after attach has mounted the disk and started the runner.
                if !self.attached {
                    self.events.push("resize-before-attach".into());
                    eprintln!("guest refused: resize requires tenant attachment");
                    return json!({"ok":false,"error":"resize requires tenant attachment"});
                }
                self.events.push("resize".into());
                json!({"ok":true})
            }
            _ => json!({"ok":false,"error":"no lifecycle stream in fake"}),
        }
    }
}

struct Machine {
    guest: Arc<Mutex<Guest>>,
    activation_entered: SyncSender<()>,
    activation_continue: Receiver<()>,
    delivered: SyncSender<Value>,
}
impl Backend for Machine {
    fn registry(&mut self, _: &str) -> Result<Registry> {
        Ok(serde_json::from_value(json!({
            "disk":"/var/lib/motor-vm/tenants/tenant/disk.img",
            "generation":"/var/lib/motor-vm/tenants/tenant/generation",
            "suspend":"/var/lib/motor-vm/tenants/tenant/suspend",
            "image_version":"v1", "vcpu_ceiling":2,"guest_ram_mib":18432,
            "disk_quota_bytes":21474836480u64,"cpuset":"0-1","tap_index":1,
            "idle_suspend_s":60,"host_floor_bytes":2147483648u64,"memory_headroom_mib":2048
        }))
        .unwrap())
    }
    fn key(&mut self, _: &Registry) -> Result<SnapshotKey> {
        Ok(SnapshotKey {
            version: "v1".into(),
            kernel_sha256: "a".repeat(64),
            rootfs_sha256: "b".repeat(64),
            generation: self.guest.lock().unwrap().generation,
        })
    }
    fn saved_key(&mut self, r: &Registry) -> Result<Option<SnapshotKey>> {
        Ok(Some(self.key(r)?))
    }
    fn running(&mut self, _: &str) -> bool {
        self.guest.lock().unwrap().running
    }
    fn load(&mut self, _: &str, _: &Registry, saved: bool) -> StartResult<()> {
        assert!(
            !saved,
            "pending intent must exclude even a matching saved snapshot"
        );
        let mut g = self.guest.lock().unwrap();
        g.events.push("cold-boot".into());
        g.running = true;
        g.attached = false;
        Ok(())
    }
    fn drive(&mut self, _: &str, _: &Registry) -> Result<()> {
        Ok(())
    }
    fn consume(&mut self, _: &Registry) -> Result<()> {
        Ok(())
    }
    fn advance(&mut self, _: &Registry) -> Result<u64> {
        let mut g = self.guest.lock().unwrap();
        g.generation += 1;
        Ok(g.generation)
    }
    fn resume(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
    fn staged_resize(&mut self, _: &Registry) -> Result<Option<Activation>> {
        Ok(self.guest.lock().unwrap().staged.then(|| {
            serde_json::from_slice(include_bytes!(
                "../../../motor-vm-helper/tests/vectors/runner-args-staged.json"
            ))
            .unwrap()
        }))
    }
    fn apply_resize(&mut self, _: &str, _: &Activation) -> Result<()> {
        let reply = self
            .guest
            .lock()
            .unwrap()
            .request(&json!({"op":"apply_resize"}));
        if reply["ok"] != true {
            return Err(Error(reply["error"].to_string()));
        }
        self.activation_entered.send(()).unwrap();
        self.activation_continue
            .recv_timeout(Duration::from_secs(10))
            .expect("test must release activation");
        Ok(())
    }
    fn sync_disk(&mut self, _: &str) -> Result<()> {
        self.guest.lock().unwrap().events.push("disk-fsync".into());
        Ok(())
    }
    fn post_grow_check(&mut self, _: &Registry) -> Result<()> {
        let mut g = self.guest.lock().unwrap();
        g.events.push("post-grow-space".into());
        if g.fail_space {
            Err(Error("insufficient post-grow space".into()))
        } else {
            Ok(())
        }
    }
    fn complete_resize(&mut self, _: &Registry, _: &Activation) -> Result<()> {
        let mut g = self.guest.lock().unwrap();
        g.events.push("complete".into());
        g.staged = false;
        Ok(())
    }
    fn prepare(&mut self, _: &str) -> Result<()> {
        let mut g = self.guest.lock().unwrap();
        if !g.attached {
            return Err(Error("tenant is not attached".into()));
        }
        g.events.push("clean-stop".into());
        Ok(())
    }
    fn thaw(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
    fn snapshot(&mut self, _: &str, _: &Registry, _: &SnapshotKey) -> Result<()> {
        panic!("snapshot while activation pending")
    }
    fn stop(&mut self, _: &str) -> Result<()> {
        let mut g = self.guest.lock().unwrap();
        g.running = false;
        g.attached = false;
        Ok(())
    }
    fn available(&mut self, _: &Registry) -> Result<u64> {
        Ok(u64::MAX)
    }
    fn cpu(&mut self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn egress(&mut self, _: &str, _: &Registry) -> Result<()> {
        Ok(())
    }
    fn relay(&mut self, _: &str, port: u32) -> Result<OwnedFd> {
        let (fd, mut peer) = UnixStream::pair().unwrap();
        let guest = self.guest.clone();
        let delivered = self.delivered.clone();
        std::thread::spawn(move || {
            let Ok(request) = read_json(&mut peer) else {
                return;
            };
            if port == 52 {
                let mut g = guest.lock().unwrap();
                assert!(
                    !g.staged,
                    "a lease reached the runner before activation completed"
                );
                g.events.push("lease".into());
                delivered.send(request).unwrap();
            } else {
                let reply = guest.lock().unwrap().request(&request);
                let _ = write_json(&mut peer, &reply);
            }
        });
        Ok(fd.into())
    }
}

struct TenantMachine(Controller<Machine>);
impl motor_vm_helper::Machine for TenantMachine {
    fn reconcile(&mut self) -> Result<()> {
        Ok(())
    }
    fn start(
        &mut self,
        _: usize,
        _: &motor_vm_helper::Lease,
        _: motor_vm_helper::Kind,
    ) -> Result<Vec<OwnedFd>> {
        unreachable!()
    }
    fn stop(&mut self, _: usize, _: &motor_vm_helper::Lease, _: bool) -> Result<()> {
        unreachable!()
    }
    fn cleanup(&mut self, _: usize) -> Result<()> {
        unreachable!()
    }
    fn alive(&mut self, _: usize) -> bool {
        unreachable!()
    }
    fn tenant(&mut self, tenant: &str, op: Operation) -> Result<motor_vm_helper::Response> {
        self.0.handle(tenant, op)
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    helper: Arc<Helper>,
    guest: Arc<Mutex<Guest>>,
    entered: Receiver<()>,
    proceed: SyncSender<()>,
    delivered: Receiver<Value>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Harness {
    fn new() -> Self {
        let directory = super::scratch();
        let socket = directory.path().join("helper.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let guest = Arc::new(Mutex::new(Guest::default()));
        let (entered_tx, entered) = mpsc::sync_channel(1);
        let (proceed, proceed_rx) = mpsc::sync_channel(1);
        let (delivered_tx, delivered) = mpsc::sync_channel(1);
        let controller = Controller(Machine {
            guest: guest.clone(),
            activation_entered: entered_tx,
            activation_continue: proceed_rx,
            delivered: delivered_tx,
        });
        let worker = std::thread::spawn(move || {
            let mut helper =
                motor_vm_helper::Helper::new(TenantMachine(controller), 1, vec![1000]).unwrap();
            for connection in listener.incoming() {
                let mut stream = connection.unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                if line.contains("test_shutdown") {
                    break;
                }
                let request: Request = serde_json::from_str(&line).unwrap();
                let connection = helper
                    .connect(motor_vm_helper::Peer {
                        uid: 1000,
                        gid: 1000,
                    })
                    .unwrap();
                let response = helper.handle(connection, request);
                helper.disconnect(connection);
                match response {
                    Ok(mut response) => {
                        if let Some(fd) = response.fds.pop() {
                            descriptor_reply(&stream, &UnixStream::from(fd));
                        } else {
                            let _ = writeln!(
                                stream,
                                "{}",
                                serde_json::to_value(response.reply).unwrap()
                            );
                        }
                    }
                    Err(error) => {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({"result":"error","message":error.to_string()})
                        );
                    }
                }
            }
        });
        let helper = Arc::new(Helper {
            socket,
            storage: directory.path().into(),
            capacity: directory.path().join("capacity.json"),
            guest_init: BTreeMap::from([("tenant".into(), json!({}))]),
        });
        Self {
            _directory: directory,
            helper,
            guest,
            entered,
            proceed,
            delivered,
            worker: Some(worker),
        }
    }
    fn wait_for_activation(&self) {
        self.entered
            .recv_timeout(Duration::from_secs(10))
            .expect("cold staged resize must attach and reach activation");
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.proceed.try_send(());
        if let Ok(mut stream) = UnixStream::connect(&self.helper.socket) {
            let _ = writeln!(stream, "test_shutdown");
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn storage_ready(runtime: &motor_host::server::Runtime) {
    runtime.host.lock().unwrap().observe_storage(
        Ok(motor_host::capacity::Capacity {
            windows_vm_bytes: 1,
            host_reserve_bytes: 1,
            jobs_min_bytes: 1,
            storage_margin_bytes: 1,
            growth_allowance_bytes: 1,
            tenant_memory_reserve_bytes: 2048 << 20,
        }),
        Ok(u64::MAX),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_staged_resize_attaches_before_activation_and_never_advertises_or_delivers_early() {
    let h = Harness::new();
    let (runtime, broker, requester, claims) = fixture_with_control(h.helper.clone());
    storage_ready(&runtime);
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime.clone(), server));
    let challenge = hello(&mut client, &broker, &requester, &claims).await;
    assert_eq!(
        respond(&mut client, &requester, &claims, challenge).await,
        json!({"type":"lease_ok"})
    );
    write_frame(&mut client, &json!({"type":"hello","protocol_version":1}))
        .await
        .unwrap();
    h.wait_for_activation();
    {
        let mut host = runtime.host.lock().unwrap();
        assert!(!host.is_ready("tenant"));
        assert_eq!(host.heartbeat(unix_ms(), 0, true).free_vcpus, 0);
        assert_eq!(host.free_mem_mib(unix_ms()), 0);
    }
    assert!(h.delivered.try_recv().is_err());
    assert!(h.guest.lock().unwrap().staged);
    h.proceed.send(()).unwrap();
    let lease = h.delivered.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(lease["type"], "verified_lease");
    assert!(runtime.host.lock().unwrap().is_ready("tenant"));
    assert_eq!(
        h.guest.lock().unwrap().events,
        [
            "cold-boot",
            "attach",
            "resize",
            "disk-fsync",
            "post-grow-space",
            "complete",
            "lease"
        ]
    );
    drop(client);
    let _ = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap();
}

#[test]
fn resize_crash_after_attach_cold_boots_and_redelivers_before_ready() {
    let h = Harness::new();
    // Execute the production startup prefix, then discard the host before its
    // next operation (activate), as if it died immediately after attachment.
    let old_host = Helper {
        socket: h.helper.socket.clone(),
        storage: h.helper.storage.clone(),
        capacity: h.helper.capacity.clone(),
        guest_init: h.helper.guest_init.clone(),
    };
    let mut stream = UnixStream::connect(&old_host.socket).unwrap();
    writeln!(
        stream,
        "{}",
        json!({"op":"prepare_start","tenant":"tenant"})
    )
    .unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap()["generation"],
        5
    );
    assert_eq!(
        old_host
            .admin("tenant", &json!({"op":"attach_tenant","generation":5}))
            .unwrap()["ok"],
        true
    );
    drop(old_host);
    {
        let g = h.guest.lock().unwrap();
        assert!(g.running && g.attached && g.staged);
        assert_eq!(g.events, ["cold-boot", "attach"]);
    }
    // Discard the old host's state, as on a process restart. Even though the
    // helper still sees a running VM and a matching snapshot, intent wins.
    let (runtime, _, _, _) = fixture_with_control(h.helper.clone());
    storage_ready(&runtime);
    let worker_runtime = runtime.clone();
    let worker = std::thread::spawn(move || worker_runtime.recover());
    h.wait_for_activation();
    assert!(!runtime.host.lock().unwrap().is_ready("tenant"));
    assert!(h.guest.lock().unwrap().staged);
    h.proceed.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(runtime.host.lock().unwrap().is_ready("tenant"));
    assert_eq!(
        runtime
            .host
            .lock()
            .unwrap()
            .heartbeat(unix_ms(), 0, true)
            .free_vcpus,
        2
    );
    assert!(!h.guest.lock().unwrap().staged);
    assert_eq!(
        h.guest.lock().unwrap().events,
        [
            "cold-boot",
            "attach",
            "clean-stop",
            "cold-boot",
            "attach",
            "resize",
            "disk-fsync",
            "post-grow-space",
            "complete"
        ]
    );
    // Adopted starts still call the idempotent activation op when no intent is left.
    assert_eq!(h.helper.wake("tenant", Some(6)).unwrap(), 6);
}

#[test]
fn resize_post_grow_refusal_cleanly_stops_and_retains_intent_for_retry() {
    let h = Harness::new();
    h.guest.lock().unwrap().fail_space = true;
    h.proceed.send(()).unwrap();
    let error = h.helper.wake("tenant", None).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<motor_host::helper::StartRefused>()
            .unwrap()
            .case,
        "post_grow_space"
    );
    h.wait_for_activation();
    {
        let g = h.guest.lock().unwrap();
        assert!(!g.running && g.staged);
        assert!(g
            .events
            .ends_with(&["post-grow-space".into(), "clean-stop".into()]));
    }
    h.guest.lock().unwrap().fail_space = false;
    h.proceed.send(()).unwrap();
    assert_eq!(h.helper.wake("tenant", None).unwrap(), 6);
    h.wait_for_activation();
    assert!(!h.guest.lock().unwrap().staged);
}
