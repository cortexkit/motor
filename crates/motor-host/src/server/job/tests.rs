use super::*;
use crate::jobs::{hello_covers, Resources};
use crate::state::jobs::HostResources;
use crate::state::{Host, Snapshot};
use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::sync::Condvar;

mod heartbeat_tests;
mod recovery_tests;
mod startup_tests;
#[path = "upload_tests.rs"]
mod upload_tests;

const JOB: &str = "00000000-0000-7000-8000-000000000001";
const JOB2: &str = "00000000-0000-7000-8000-000000000002";
struct Journal;
impl crate::journal::Journal for Journal {
    fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
        Ok(())
    }
}

fn claims(job: &str) -> Claims {
    Claims {
        iss: motor_lease::ISSUER.into(),
        aud: "host".into(),
        jti: job.into(),
        sub: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        purpose: Purpose::Run,
        iat: unix_ms() / 1000,
        exp: unix_ms() / 1000 + 300,
        vcpus: 1,
        mem_mib: 100,
        mem_burst_mib: Some(50),
        platform: "linux-x64".into(),
        net: false,
        max_s: 30,
        workspace_key_hash: Some(motor_lease::base64url(&[5; 32])),
        placement: motor_lease::Placement {
            host: "host".into(),
            vm: None,
        },
        cnf: motor_lease::Confirmation {
            jkt: motor_lease::base64url(&[1; 32]),
        },
    }
}
fn hello() -> Value {
    json!({"type":"hello","protocol_version":1,"return_paths":true,"return_chunked":true,"sibling_paths":true})
}
fn run(job: &str) -> Value {
    let mut run: Value = serde_json::from_str(include_str!(
        "../../../../../test-vectors/exec-remote-v1/frames/run.json"
    ))
    .unwrap();
    run["job_id"] = json!(job);
    run["weight_hint"] = json!(1);
    run["timeout"] = json!(30);
    run["network"] = Value::Null;
    run
}

#[derive(Default)]
struct Fake {
    readiness: Mutex<Option<motor_vm_helper::images::Readiness>>,
    fill_export: Mutex<Option<UnixStream>>,
    capacity_available: AtomicBool,
    fail_ensure: AtomicBool,
    ensured_caps: Mutex<Vec<u64>>,
    calls: Mutex<Vec<String>>,
    inputs: Arc<Mutex<Vec<Vec<u8>>>>,
    gate: (Mutex<bool>, Condvar),
    hang: Mutex<&'static str>,
    entered: tokio::sync::Notify,
    hello: Mutex<Option<Value>>,
    peers: Mutex<Vec<UnixStream>>,
    lifecycle: Mutex<Option<UnixStream>>,
    exits: Mutex<Vec<motor_vm_helper::job::ExitReport>>,
    gone: Mutex<BTreeSet<String>>,
    fail_ack: AtomicBool,
    fail_poll: AtomicBool,
    exit_polls: std::sync::atomic::AtomicU64,
    disconnect_on_run: AtomicBool,
    acknowledgements: Mutex<Vec<motor_vm_helper::job::ExitReport>>,
    balloon_supported: AtomicBool,
    resident: Mutex<BTreeMap<String, u64>>,
    balloons: Mutex<Vec<(String, u64)>>,
    scripted: Mutex<std::collections::VecDeque<UnixStream>>,
    recovery_reply: Mutex<Option<Value>>,
    init_reply: Mutex<Option<Value>>,
    guest_identity: Mutex<Vec<Value>>,
    recovery_starts: Mutex<Vec<motor_vm_helper::job::Start>>,
}
impl Fake {
    fn wait(&self, stage: &str) {
        if *self.hang.lock().unwrap() == stage {
            self.entered.notify_one();
            let mut stopped = self.gate.0.lock().unwrap();
            while !*stopped {
                stopped = self.gate.1.wait(stopped).unwrap();
            }
        }
    }
}
impl Control for Fake {
    fn per_job_readiness(&self) -> Result<motor_vm_helper::images::Readiness> {
        self.calls.lock().unwrap().push("readiness".into());
        self.readiness
            .lock()
            .unwrap()
            .clone()
            .context("helper does not support per-job readiness")
    }
    fn inspect_job_images(&self, _: &str) -> Result<motor_vm_helper::images::Manifest> {
        let artifact = motor_vm_helper::images::Artifact {
            bytes: 4096,
            sha256: "a".repeat(64),
        };
        Ok(motor_vm_helper::images::Manifest {
            kernel: artifact.clone(),
            rootfs: artifact.clone(),
            toolchain: artifact,
            hello: hello(),
            guest_init: json!({"resolvers":["1.1.1.1"],"hostname":"job","machine_id":"0123456789abcdef0123456789abcdef"}),
        })
    }
    fn publish_job_images(&self, generation: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("publish:{generation}"));
        Ok(())
    }
    fn prepare_job_images(&self, generation: &str, _: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("prepare:{generation}"));
        Ok(())
    }
    fn remove_job_images(&self, generation: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("remove:{generation}"));
        Ok(())
    }
    fn host_available_mib(&self) -> anyhow::Result<u64> {
        Ok(1000)
    }
    fn online_cpus(&self) -> anyhow::Result<u32> {
        Ok(2)
    }
    fn capacity(&self) -> Result<crate::capacity::Capacity> {
        ensure!(
            self.capacity_available.load(Ordering::SeqCst),
            "host capacity file unavailable"
        );
        Ok(crate::capacity::Capacity {
            windows_vm_bytes: 0,
            host_reserve_bytes: 0,
            jobs_min_bytes: 1,
            storage_margin_bytes: 0,
            growth_allowance_bytes: 0,
            tenant_memory_reserve_bytes: 0,
        })
    }
    fn ensure_workspace(&self, _: &str, _: &str, cap: u64) -> Result<()> {
        self.ensured_caps.lock().unwrap().push(cap);
        ensure!(
            !self.fail_ensure.load(Ordering::SeqCst),
            "workspace size mismatch"
        );
        Ok(())
    }
    fn job_memory(&self, job: &str) -> Result<u64> {
        ensure!(
            self.balloon_supported.load(Ordering::Acquire),
            crate::helper::BalloonUnavailable
        );
        self.resident
            .lock()
            .unwrap()
            .get(job)
            .copied()
            .context("missing host RAM sample")
    }
    fn balloon_job(&self, job: &str, amount_mib: u64) -> Result<()> {
        ensure!(
            self.balloon_supported.load(Ordering::Acquire),
            crate::helper::BalloonUnavailable
        );
        self.balloons
            .lock()
            .unwrap()
            .push((job.to_owned(), amount_mib));
        Ok(())
    }
    fn job_exits(&self) -> Result<Vec<motor_vm_helper::job::ExitReport>> {
        self.exit_polls.fetch_add(1, Ordering::SeqCst);
        ensure!(
            !self.fail_poll.load(Ordering::Acquire),
            "injected exit poll failure"
        );
        Ok(self.exits.lock().unwrap().clone())
    }
    fn ack_job_exit(&self, report: &motor_vm_helper::job::ExitReport) -> Result<()> {
        ensure!(
            !self.fail_ack.load(Ordering::Acquire),
            "injected ack failure"
        );
        self.acknowledgements.lock().unwrap().push(report.clone());
        self.exits.lock().unwrap().retain(|r| r != report);
        Ok(())
    }
    fn start_job(&self, start: &motor_vm_helper::job::Start) -> Result<Value> {
        assert_eq!(start.account, "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!([200, 32768].contains(&start.memory_mib));
        self.calls
            .lock()
            .unwrap()
            .push(format!("start:{}", start.job_id));
        self.wait("boot");
        if start.kind == motor_vm_helper::job::VmKind::Recovery {
            self.recovery_starts.lock().unwrap().push(start.clone());
            return Ok(json!({"result":"job","running":true}));
        }
        Ok(json!({"result":"job","running":true,"tap_index":1}))
    }
    fn job_admin(&self, _: &str, request: &Value) -> Result<Value> {
        if request["op"] == "attach_tenant" {
            self.guest_identity.lock().unwrap().push(request.clone());
        }
        if request["op"] == "recover_workspace" {
            self.wait("recovery");
            return Ok(self
                .recovery_reply
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(json!({"ok":true,"recovery":"repaired"})));
        }
        assert_eq!(request["op"], "attach_tenant");
        assert!(request.get("recovery_required").is_none());
        self.wait("runner");
        Ok(self
            .init_reply
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(json!({"ok":true})))
    }
    fn adopt_job(&self, job: &str) -> Result<bool> {
        self.calls.lock().unwrap().push(format!("adopt:{job}"));
        Ok(!self.gone.lock().unwrap().contains(job))
    }
    fn delete_workspace(&self, _: &str, disk_id: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("delete:{disk_id}"));
        Ok(())
    }
    fn stop_job(&self, job: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("stop:{job}"));
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
        self.peers.lock().unwrap().clear();
        self.exits
            .lock()
            .unwrap()
            .push(motor_vm_helper::job::ExitReport {
                job_id: job.into(),
                oom_kill: 0,
                egress_bytes: None,
            });
        Ok(())
    }
    fn job_relay(&self, _: &str, port: u32) -> Result<UnixStream> {
        if port == 54 {
            return self
                .fill_export
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow::anyhow!("export unavailable"));
        }
        if port == 53 {
            return self
                .lifecycle
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow::anyhow!("no lifecycle fixture"));
        }
        ensure!(port == 52, "no lifecycle fixture");
        if let Some(stream) = self.scripted.lock().unwrap().pop_front() {
            return Ok(stream);
        }
        let (a, mut b) = UnixStream::pair()?;
        self.peers.lock().unwrap().push(b.try_clone()?);
        let reply = self.hello.lock().unwrap().clone().unwrap_or_else(hello);
        let inputs = self.inputs.clone();
        let disconnect_on_run = self.disconnect_on_run.load(Ordering::SeqCst);
        std::thread::spawn(move || {
            let read = |stream: &mut UnixStream| -> Result<(Vec<u8>, Value)> {
                use std::io::Read;
                let mut header = [0; 4];
                stream.read_exact(&mut header)?;
                let mut data = vec![0; u32::from_be_bytes(header) as usize];
                stream.read_exact(&mut data)?;
                let value = serde_json::from_slice(&data)?;
                Ok(([header.as_slice(), data.as_slice()].concat(), value))
            };
            let Ok((_, lease)) = read(&mut b) else { return };
            assert_eq!(lease["type"], "verified_lease");
            assert!([json!(50), json!(8192)].contains(&lease["mem_burst_mib"]));
            if lease["mem_burst_mib"] == 8192 {
                assert_eq!(lease["mem_mib"], 16384);
            }
            let Ok((bytes, _)) = read(&mut b) else { return };
            inputs.lock().unwrap().push(bytes);
            if write_json(&mut b, &reply).is_err() {
                return;
            }
            while let Ok((bytes, frame)) = read(&mut b) {
                inputs.lock().unwrap().push(bytes);
                match frame["type"].as_str() {
                    Some("have") => {
                        let _ = write_json(
                            &mut b,
                            &json!({"type":"have_reply","repositories":[],"git_dependencies":[]}),
                        );
                    }
                    Some("run") => {
                        if disconnect_on_run {
                            // A missing guest acknowledgment cannot undo Run delivery.
                            b.shutdown(std::net::Shutdown::Both).unwrap();
                            return;
                        }
                        let _ = write_json(
                            &mut b,
                            &json!({"type":"accepted","job_id":frame["job_id"],"queue_position":0,"env_not_forwarded":[]}),
                        );
                        let _ = write_json(
                            &mut b,
                            &json!({"type":"output","seq":1,"stream":"stdout","bytes":"eA=="}),
                        );
                    }
                    _ => {}
                }
            }
        });
        Ok(a)
    }
    fn adopt(&self, _: &str) -> Result<bool> {
        unreachable!()
    }
    fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
        unreachable!()
    }
    fn quota(&self, _: &str, _: u32) -> Result<()> {
        unreachable!()
    }
    fn relay(&self, _: &str, _: u32) -> Result<UnixStream> {
        unreachable!()
    }
    fn admin(&self, _: &str, _: &Value) -> Result<Value> {
        unreachable!()
    }
    fn suspend(&self, _: &str) -> Result<()> {
        unreachable!()
    }
    fn available_bytes(&self) -> Result<u64> {
        Ok(u64::MAX)
    }
}

struct Storage {
    root: tempfile::TempDir,
    held: Mutex<BTreeSet<String>>,
    releases: Mutex<BTreeMap<String, usize>>,
    fail_prepare: AtomicBool,
    pause_prepare: AtomicBool,
    prepare_gate: (Mutex<bool>, Condvar),
    prepare_entered: tokio::sync::Notify,
    pinned_hello: Mutex<Option<Value>>,
    released: tokio::sync::Notify,
    recovery_available: AtomicBool,
    /// When set, acquire refuses if the host lock is already held, as the
    /// production image manager would hang taking it.
    host: std::sync::OnceLock<Arc<Mutex<Host>>>,
}
impl Storage {
    fn new() -> Arc<Self> {
        let root = cortexkit_test_support::scratch_root();
        std::fs::create_dir_all(&root).unwrap();
        Arc::new(Self {
            root: tempfile::tempdir_in(root).unwrap(),
            held: Mutex::new(BTreeSet::new()),
            releases: Mutex::new(BTreeMap::new()),
            fail_prepare: AtomicBool::new(false),
            pause_prepare: AtomicBool::new(false),
            prepare_gate: (Mutex::new(false), Condvar::new()),
            prepare_entered: tokio::sync::Notify::new(),
            pinned_hello: Mutex::new(None),
            released: tokio::sync::Notify::new(),
            recovery_available: AtomicBool::new(false),
            host: std::sync::OnceLock::new(),
        })
    }
    fn spool(&self, job: &str) -> std::path::PathBuf {
        self.root.path().join(job)
    }
}
impl Resources for Storage {
    fn acquire_recovery(
        &self,
        _: &str,
        _: &crate::state::jobs::WorkspaceDisk,
        r: &crate::state::jobs::recovery::Recovery,
    ) -> Result<Assignment> {
        ensure!(
            self.recovery_available.load(Ordering::Acquire),
            "images unavailable"
        );
        self.held.lock().unwrap().insert(r.id.clone());
        Ok(Assignment {
            image_generation_id: "gen-1".into(),
            hello: hello(),
            spool: self.spool(&r.id),
            guest_init: json!({}),
        })
    }
    fn acquire(&self, e: &Execution) -> Result<Assignment> {
        if let Some(host) = self.host.get() {
            ensure!(
                host.try_lock().is_ok(),
                "job resources acquired while the host lock is held"
            );
        }
        self.held.lock().unwrap().insert(e.claims.jti.clone());
        Ok(Assignment {
            image_generation_id: "gen-1".into(),
            hello: self
                .pinned_hello
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(hello),
            spool: self.spool(&e.claims.jti),
            guest_init: json!({"resolvers":["1.1.1.1"],"hostname":"job","machine_id":"0123456789abcdef0123456789abcdef"}),
        })
    }
    fn prepare(&self, _: &Execution) -> Result<()> {
        if self.pause_prepare.load(Ordering::Acquire) {
            self.prepare_entered.notify_one();
            let mut ready = self.prepare_gate.0.lock().unwrap();
            while !*ready {
                ready = self.prepare_gate.1.wait(ready).unwrap();
            }
        }
        ensure!(!self.fail_prepare.load(Ordering::Acquire), "copy failed");
        Ok(())
    }
    fn release(&self, job: &str) -> Result<()> {
        if self.held.lock().unwrap().remove(job) {
            *self.releases.lock().unwrap().entry(job.into()).or_default() += 1;
            self.released.notify_one();
        }
        remove_spool(&self.spool(job))
    }
}
fn fixture() -> (Arc<Runtime>, Arc<Fake>, Arc<Storage>) {
    let storage = Storage::new();
    let (journal, snapshot) = crate::journal::FileJournal::open(storage.root.path()).unwrap();
    let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = crate::config::HostSettings::PerJob(crate::config::PerJobSettings {
        vcpu_ceiling: 2,
        permit_budget: 4,
        host_reserve_mib: 50,
        boot_reserve_mib: 20,
        per_vm_overhead_mib: 10,
        reclaim_timeout_ms: 100,
        hysteresis_mib: 10,
        workspace_disk_cap_bytes: 1024,
        boot_deadline_ms: 1000,
        recovery_timeout_ms: 10000,
        exit_evidence_bound_ms: 1000,
        teardown_timeout_ms: 1000,
    });
    host.record_host_resources(HostResources {
        at_ms: unix_ms(),
        available_mem_mib: 1000,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
    let fake = Arc::new(Fake::default());
    let runtime = Runtime::with_job_resources("host".into(), host, fake.clone(), storage.clone());
    runtime
        .configure_job_resolvers(vec!["1.1.1.1".into()])
        .unwrap();
    (runtime, fake, storage)
}
fn claim(runtime: &Arc<Runtime>, c: &Claims) -> u64 {
    let mut host = runtime.host.lock().unwrap();
    let id = host.claim(c.clone(), unix_ms()).unwrap().0;
    let execution = host.snapshot.per_job.executions[&c.jti].clone();
    drop(host);
    runtime.job_resources.acquire(&execution).unwrap();
    id
}
async fn start(
    runtime: Arc<Runtime>,
    c: Claims,
    id: u64,
) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let (client, mut server) = tokio::io::duplex(1024 * 1024);
    let task = tokio::spawn(async move { serve(runtime, &c, id, &mut server).await });
    (client, task)
}
async fn next_type(client: &mut tokio::io::DuplexStream, kind: &str) -> Value {
    loop {
        let frame = read_frame(client).await.unwrap();
        if frame["type"] == kind {
            return frame;
        }
        assert_eq!(frame["type"], "heartbeat", "{frame}");
    }
}
async fn queued(client: &mut tokio::io::DuplexStream, job: &str) {
    next_type(client, "lease_ok").await;
    write_frame(client, &hello()).await.unwrap();
    next_type(client, "hello").await;
    write_frame(client, &run(job)).await.unwrap();
    next_type(client, "accepted").await;
}

#[test]
fn hello_promises_cannot_outgrow_the_assigned_runner() {
    let promised = hello();
    assert!(hello_covers(&promised, &promised));
    let mut old = promised.clone();
    old.as_object_mut().unwrap().remove("return_chunked");
    assert!(!hello_covers(&old, &promised));
    let mut small = promised.clone();
    small["return_chunked_max_bytes"] = json!(motor_protocol::RETURN_CHUNKED_MAX_BYTES_CAP - 1);
    assert!(!hello_covers(&small, &promised));
    let mut malformed = promised.clone();
    malformed["return_chunked_max_bytes"] = json!("not a byte count");
    assert!(!hello_covers(&malformed, &promised));
}

#[tokio::test]
async fn no_boot_before_first_frame_and_busy_disk_has_queue_frames() {
    let (runtime, fake, storage) = fixture();
    let first = claims(JOB);
    claim(&runtime, &first);
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB)
        .unwrap()
        .unwrap();
    let second = claims(JOB2);
    let id = claim(&runtime, &second);
    let warm = runtime
        .host
        .lock()
        .unwrap()
        .heartbeat(unix_ms(), 0, true)
        .warm;
    assert_eq!(warm.len(), 1);
    assert!(warm[0].busy);
    assert_eq!(warm[0].tenant, second.sub);
    let (mut client, task) = start(runtime.clone(), second, id).await;
    next_type(&mut client, "lease_ok").await;
    assert!(fake.calls.lock().unwrap().is_empty());
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    write_frame(&mut client, &run(JOB2)).await.unwrap();
    assert_eq!(
        next_type(&mut client, "accepted").await["queue_position"],
        1
    );
    assert!(next_type(&mut client, "heartbeat").await["queue_wait_ms"].is_number());
    assert!(fake.calls.lock().unwrap().is_empty());
    runtime.stop_job(JOB, false, false).unwrap();
    next_type(&mut client, "output").await;
    assert_eq!(
        fake.calls.lock().unwrap().as_slice(),
        [format!("stop:{JOB}"), format!("start:{JOB2}")]
    );
    assert!(!storage.spool(JOB2).exists());
    {
        let mut host = runtime.host.lock().unwrap();
        assert_eq!(host.free_mem_mib(unix_ms()), 950);
        host.job_runner_hello(JOB2).unwrap();
        assert_eq!(host.free_mem_mib(unix_ms()), 950);
    }
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB2, true, false).unwrap();
    assert_eq!(storage.releases.lock().unwrap()[JOB2], 1);
}

#[tokio::test(start_paused = true)]
async fn hung_boot_and_runner_start_time_out_without_delivery_and_release_all() {
    for stage in ["boot", "runner"] {
        let (runtime, fake, storage) = fixture();
        *fake.hang.lock().unwrap() = stage;
        let c = claims(JOB);
        let id = claim(&runtime, &c);
        let (mut client, task) = start(runtime.clone(), c, id).await;
        queued(&mut client, JOB).await;
        fake.entered.notified().await;
        tokio::time::advance(Duration::from_millis(1001)).await;
        next_type(&mut client, "lease_refused").await;
        task.await.unwrap().unwrap();
        if stage == "boot" {
            assert!(storage.held.lock().unwrap().contains(JOB));
            assert!(!runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].ended);
            assert!(fake.inputs.lock().unwrap().is_empty());
            *fake.gate.0.lock().unwrap() = true;
            fake.gate.1.notify_all();
            storage.released.notified().await;
            assert!(fake.inputs.lock().unwrap().is_empty());
            assert_eq!(
                fake.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| call.starts_with("stop:"))
                    .count(),
                1
            );
        }
        let host = runtime.host.lock().unwrap();
        assert!(!host.snapshot.leases[JOB].delivered);
        assert!(host.snapshot.zero_usage.contains_key(JOB));
        assert!(host.snapshot.per_job.executions[JOB].ended);
        assert_eq!(
            host.snapshot.per_job.executions[JOB].replay_reservation_bytes,
            None
        );
        drop(host);
        assert!(storage.held.lock().unwrap().is_empty());
        assert!(!storage.spool(JOB).exists());
        assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
        let mut later = claims(JOB2);
        later.workspace_key_hash = Some(motor_lease::base64url(&[6; 32]));
        claim(&runtime, &later);
    }
}

#[tokio::test]
async fn workspace_size_mismatch_is_quarantined_before_vm_start() {
    let (runtime, fake, storage) = fixture();
    fake.fail_ensure.store(true, Ordering::SeqCst);
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    next_type(&mut client, "lease_refused").await;
    task.await.unwrap().unwrap();
    let mut host = runtime.host.lock().unwrap();
    let execution = &host.snapshot.per_job.executions[JOB];
    let disk = &host.snapshot.per_job.disks[&execution.disk_id];
    assert!(execution.ended);
    assert_eq!(execution.replay_reservation_bytes, None);
    assert!(disk.quarantined);
    assert_eq!(
        *fake.ensured_caps.lock().unwrap(),
        [disk.storage.as_ref().unwrap().cap_bytes]
    );
    assert!(!host.snapshot.leases[JOB].delivered);
    assert!(matches!(
        host.claim(claims(JOB2), unix_ms()),
        Err(Error::Refused(Refusal::HostFull))
    ));
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(!storage.spool(JOB).exists());
}

#[tokio::test]
async fn capability_mismatch_never_delivers_run_and_guest_refusal_cannot_rewrite_it() {
    for event_first in [false, true] {
        let (runtime, fake, storage) = fixture();
        *fake.hello.lock().unwrap() = Some(json!({"type":"hello","protocol_version":1}));
        let c = claims(JOB);
        let id = claim(&runtime, &c);
        let event: motor_protocol::TenantEvent = serde_json::from_value(
            json!({"type":"event","event_seq":1,"event":"refused","jti":JOB,"cause":"disconnect"}),
        )
        .unwrap();
        if event_first {
            runtime
                .host
                .lock()
                .unwrap()
                .event(JOB, &event, 1000)
                .unwrap();
        }
        let (mut client, task) = start(runtime.clone(), c, id).await;
        next_type(&mut client, "lease_ok").await;
        write_frame(&mut client, &hello()).await.unwrap();
        next_type(&mut client, "hello").await;
        let terminal = next_type(&mut client, "terminal").await;
        assert_eq!(terminal["ran"], "none");
        assert_eq!(terminal["outcome"]["reason"], "request_rejected");
        task.await.unwrap().unwrap();
        let before = runtime.host.lock().unwrap().snapshot.billing[JOB].clone();
        runtime
            .host
            .lock()
            .unwrap()
            .event(JOB, &event, 1000)
            .unwrap();
        let host = runtime.host.lock().unwrap();
        assert!(!host.snapshot.leases[JOB].delivered);
        assert_eq!(host.snapshot.billing[JOB], before);
        assert_eq!(
            host.snapshot.billing[JOB].terminal.as_ref().unwrap(),
            &terminal
        );
        assert!(host.snapshot.zero_usage.contains_key(JOB));
        drop(host);
        assert_eq!(fake.inputs.lock().unwrap().len(), 1);
        assert!(!storage.spool(JOB).exists());
        assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
    }
}

#[tokio::test]
async fn copy_failure_and_queued_cancel_withdraw_release_every_reservation() {
    for control in ["copy", "cancel", "withdraw"] {
        let (runtime, fake, storage) = fixture();
        let c = claims(JOB);
        let id = claim(&runtime, &c);
        if control != "copy" {
            // Hold the disk without booting the queued job's VM.
            let disk = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
                .disk_id
                .clone();
            runtime
                .host
                .lock()
                .unwrap()
                .snapshot
                .per_job
                .disks
                .get_mut(&disk)
                .unwrap()
                .attached_job = Some("holder".into());
        } else {
            storage.fail_prepare.store(true, Ordering::Release);
        }
        let (mut client, task) = start(runtime.clone(), c, id).await;
        next_type(&mut client, "lease_ok").await;
        write_frame(&mut client, &hello()).await.unwrap();
        next_type(&mut client, "hello").await;
        if control == "copy" {
            next_type(&mut client, "lease_refused").await;
        } else {
            let mut request = run(JOB);
            request["queue_wait_limit_s"] = json!(0);
            write_frame(&mut client, &request).await.unwrap();
            next_type(&mut client, "accepted").await;
            write_frame(&mut client, &json!({"type":control,"job_id":JOB}))
                .await
                .unwrap();
            next_type(&mut client, &format!("{control}_reply")).await;
            let terminal = next_type(&mut client, "terminal").await;
            assert_eq!(terminal["ran"], "none");
            let _: motor_protocol::Frame = serde_json::from_value(terminal).unwrap();
        }
        task.await.unwrap().unwrap();
        assert!(!fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("start:")));
        assert!(storage.held.lock().unwrap().is_empty());
        assert!(!storage.spool(JOB).exists());
        assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
    }
}

#[tokio::test(start_paused = true)]
async fn owner_wait_clock_is_paused_and_capacity_wait_clock_uses_the_budget() {
    let mut clock = QueueClock::new();
    tokio::time::advance(Duration::from_secs(90)).await;
    assert_eq!(clock.observe(false), 0);
    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(clock.observe(true), 3000);
    tokio::time::advance(Duration::from_secs(90)).await;
    assert_eq!(clock.observe(false), 3000);
    let (runtime, _, storage) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let disk = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .disks
        .get_mut(&disk)
        .unwrap()
        .attached_job = Some("holder".into());
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    tokio::time::advance(Duration::from_secs(90)).await;
    assert_eq!(
        next_type(&mut client, "heartbeat").await["queue_wait_ms"],
        0
    );
    write_frame(&mut client, &json!({"type":"withdraw","job_id":JOB}))
        .await
        .unwrap();
    assert_eq!(
        next_type(&mut client, "withdraw_reply").await["outcome"],
        "started"
    );
    assert!(storage.held.lock().unwrap().contains(JOB));
    write_frame(&mut client, &json!({"type":"cancel","job_id":JOB}))
        .await
        .unwrap();
    next_type(&mut client, "cancel_reply").await;
    next_type(&mut client, "terminal").await;
    task.await.unwrap().unwrap();
}

#[test]
fn timeout_after_delivery_keeps_delivery_and_lost_usage_and_releases_once() {
    let (runtime, fake, storage) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot_at(JOB, 100)
        .unwrap()
        .unwrap();
    runtime.host.lock().unwrap().delivered(id).unwrap();
    runtime.tick_jobs(1101).unwrap();
    runtime.tick_jobs(1200).unwrap();
    let host = runtime.host.lock().unwrap();
    assert!(host.snapshot.leases[JOB].delivered);
    assert!(!host.snapshot.zero_usage.contains_key(JOB));
    let billing = &host.snapshot.billing[JOB];
    assert_eq!(
        billing.terminal.as_ref().unwrap()["outcome"]["type"],
        "outcome_unknown"
    );
    let usage = billing.usage.as_ref().unwrap();
    assert_eq!(usage.outcome, motor_lease::UsageOutcome::Lost);
    // Never observed running: nothing is billed, as in tenant mode.
    assert_eq!(usage.ended_at_ms, usage.started_at_ms);
    drop(host);
    assert_eq!(
        fake.calls.lock().unwrap().as_slice(),
        [format!("stop:{JOB}")]
    );
    assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
    let mut later = claims(JOB2);
    later.workspace_key_hash = Some(motor_lease::base64url(&[9; 32]));
    claim(&runtime, &later);
}

#[test]
fn restart_adopts_by_job_id_never_reboots_and_old_boot_timestamp_is_expired() {
    let (runtime, fake, storage) = fixture();
    let c = claims(JOB);
    claim(&runtime, &c);
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB)
        .unwrap()
        .unwrap();
    runtime.host.lock().unwrap().job_runner_hello(JOB).unwrap();
    std::fs::write(storage.spool(JOB), b"abandoned upload").unwrap();
    runtime.recover().unwrap();
    assert_eq!(
        fake.calls.lock().unwrap().as_slice(),
        [format!("adopt:{JOB}")]
    );
    assert!(!storage.spool(JOB).exists());
    {
        let mut host = runtime.host.lock().unwrap();
        let e = host.snapshot.per_job.executions.get_mut(JOB).unwrap();
        e.runner_hello = false;
        e.boot_started_at_ms = None;
    }
    runtime.recover().unwrap();
    assert_eq!(
        fake.calls.lock().unwrap().as_slice(),
        [
            format!("adopt:{JOB}"),
            format!("adopt:{JOB}"),
            format!("stop:{JOB}")
        ]
    );
    assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
}

#[tokio::test]
async fn spool_is_bounded_and_drop_cleans_it() {
    let storage = Storage::new();
    let path = storage.spool(JOB);
    let mut spool = Spool::create(&path).unwrap();
    spool.bytes = SPOOL_LIMIT;
    assert!(spool
        .append(&Raw {
            bytes: vec![1],
            value: Value::Null
        })
        .await
        .is_err());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    // The queued session pauses the client before a largest frame could
    // overflow, so a full-history upload waits instead of being refused.
    spool.bytes = SPOOL_LIMIT - FRAME_LIMIT as u64 - 4;
    assert!(spool.has_room());
    spool.bytes += 1;
    assert!(!spool.has_room());
    drop(spool);
    assert!(!path.exists());
}

#[tokio::test]
async fn authenticated_per_job_transport_uses_no_tenant_binding_and_forwards_burst() {
    use ed25519_dalek::{Signer, SigningKey};
    use motor_lease::{Jwks, JwksKey, RequesterJwk};
    let (runtime, fake, storage) = fixture();
    assert!(storage.host.set(runtime.host.clone()).is_ok());
    let broker = SigningKey::from_bytes(&[7; 32]);
    let requester = SigningKey::from_bytes(&[8; 32]);
    runtime
        .keys
        .write()
        .unwrap()
        .refresh(Ok(Jwks {
            keys: vec![JwksKey::from_key("key", &broker.verifying_key())],
        }))
        .unwrap();
    let mut c = claims(JOB);
    c.mem_mib = 16384;
    c.mem_burst_mib = Some(8192);
    runtime
        .host
        .lock()
        .unwrap()
        .record_host_resources(HostResources {
            at_ms: unix_ms(),
            available_mem_mib: 100_000,
            vcpu_ceiling: 2,
            permit_budget: 4,
        });
    c.cnf.jkt = RequesterJwk::from_key(&requester.verifying_key())
        .thumbprint()
        .unwrap();
    let (mut client, server) = tokio::io::duplex(1024 * 1024);
    let r = runtime.clone();
    let task = tokio::spawn(async move { super::super::serve(r, server).await });
    let token = motor_lease::sign_token(&c, "key", &broker).unwrap();
    runtime
        .authenticate(
            &token,
            &RequesterJwk::from_key(&requester.verifying_key()),
            unix_ms() / 1000,
        )
        .unwrap();
    write_frame(&mut client,&json!({"type":"lease_hello","v":1,"token":token,"requester_jwk":RequesterJwk::from_key(&requester.verifying_key())})).await.unwrap();
    let challenge: Challenge =
        serde_json::from_value(read_frame(&mut client).await.unwrap()).unwrap();
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(runtime.host.lock().unwrap().snapshot.per_job.is_empty());
    let sig = motor_lease::base64url(
        &requester
            .sign(&motor_lease::challenge_bytes(&challenge, &c).unwrap())
            .to_bytes(),
    );
    write_frame(&mut client, &json!({"type":"challenge_response","sig":sig}))
        .await
        .unwrap();
    queued(&mut client, JOB).await;
    next_type(&mut client, "output").await;
    assert!(runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
    assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
}

#[tokio::test]
async fn queued_spool_replays_raw_bytes_in_order_without_duplicate_acknowledgments() {
    let (runtime, fake, storage) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let disk = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .disks
        .get_mut(&disk)
        .unwrap()
        .attached_job = Some("holder".into());
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    let text = b"{ \"type\": \"hello\", \"protocol_version\":1,\"extension\":\"preserved\" }";
    let bytes = [(text.len() as u32).to_be_bytes().as_slice(), text].concat();
    client.write_all(&bytes).await.unwrap();
    next_type(&mut client, "hello").await;
    let have = json!({"type":"have","repositories":[{"repository":"/repository","commits":["1111111111111111111111111111111111111111"]}],"git_dependencies":["https://example.invalid/a"]});
    write_frame(&mut client, &have).await.unwrap();
    let held = next_type(&mut client, "have_reply").await;
    assert_eq!(held["repositories"][0]["commits"], json!([]));
    assert_eq!(held["git_dependencies"][0]["commits"], json!([]));
    write_frame(&mut client, &run(JOB)).await.unwrap();
    next_type(&mut client, "accepted").await;
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .disks
        .get_mut(&disk)
        .unwrap()
        .attached_job = None;
    next_type(&mut client, "output").await;
    let inputs = fake.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 3);
    assert_eq!(inputs[0], bytes);
    assert_eq!(
        serde_json::from_slice::<Value>(&inputs[1][4..]).unwrap(),
        have
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&inputs[2][4..]).unwrap(),
        run(JOB)
    );
    assert!(!storage.spool(JOB).exists());
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test]
#[ignore = "build the pinned client with scripts/mixed-version.py's build_revision, then set MOTOR_COMPAT_CLIENT"]
async fn pinned_old_client_queues_and_withdraws_without_booting_a_vm() {
    let helper = std::env::var_os("MOTOR_COMPAT_CLIENT").expect("pinned client required");
    let (runtime, fake, storage) = fixture();
    let repo = storage.root.path().join("repository");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    ] {
        let result = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result);
    }
    let config = storage.root.path().join("config.json");
    let request = storage.root.path().join("request.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({"runner_instance":"test","worktree_roots":[repo]})).unwrap(),
    )
    .unwrap();
    std::fs::write(&request,serde_json::to_vec(&json!({"workspace_key":repo,"repository_root":repo,"cwd":repo,"command":"true","env":{},"weight_hint":1,"timeout":30,"queue_wait_limit_s":0})).unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let child = tokio::process::Command::new(helper)
        .arg(address)
        .arg(config)
        .arg(storage.root.path().join("old-state"))
        .arg("run")
        .arg(request)
        .kill_on_drop(true)
        .output();
    let bridge = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let hello_frame = raw(&mut socket).await.unwrap();
        write_frame(&mut socket, &hello()).await.unwrap();
        let mut prelude = vec![hello_frame];
        let run_frame = loop {
            let frame = raw(&mut socket).await.unwrap();
            if frame.value["type"] == "run" {
                break frame;
            }
            if frame.value["type"] == "heartbeat" {
                prelude.push(frame);
                continue;
            }
            assert_eq!(frame.value["type"], "have");
            let mut repositories = frame.value["repositories"].clone();
            for r in repositories.as_array_mut().unwrap() {
                r["commits"] = json!([]);
            }
            write_frame(
                &mut socket,
                &json!({"type":"have_reply","repositories":repositories,"git_dependencies":[]}),
            )
            .await
            .unwrap();
            prelude.push(frame);
        };
        let job = run_frame.value["job_id"].as_str().unwrap().to_owned();
        let c = claims(&job);
        let id = claim(&runtime, &c);
        let disk = runtime.host.lock().unwrap().snapshot.per_job.executions[&job]
            .disk_id
            .clone();
        runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .per_job
            .disks
            .get_mut(&disk)
            .unwrap()
            .attached_job = Some("holder".into());
        let (mut host, task) = start(runtime.clone(), c, id).await;
        next_type(&mut host, "lease_ok").await;
        for frame in prelude {
            host.write_all(&frame.bytes).await.unwrap();
            if frame.value["type"] == "hello" {
                next_type(&mut host, "hello").await;
            } else if frame.value["type"] == "have" {
                next_type(&mut host, "have_reply").await;
            }
        }
        host.write_all(&run_frame.bytes).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut host).await;
        task.await.unwrap().unwrap();
        job
    };
    let (output, job) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(child, bridge)
    })
    .await
    .unwrap();
    let output = output.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let terminal = records.iter().find(|r| r["type"] == "terminal").unwrap();
    assert_eq!(terminal["outcome"]["reason"], "queue_wait_exceeded");
    assert_eq!(terminal["ran"], "none");
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(!storage.spool(&job).exists());
    assert_eq!(storage.releases.lock().unwrap()[&job], 1);
}

#[tokio::test]
async fn live_attach_and_run_reconnect_do_not_launch_stop_or_reassign_a_disk() {
    let (runtime, fake, storage) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    {
        let mut host = runtime.host.lock().unwrap();
        host.begin_job_boot(JOB).unwrap().unwrap();
        host.job_runner_hello(JOB).unwrap();
        host.delivered(id).unwrap();
        host.close(id).unwrap();
    }
    for purpose in [Purpose::Attach, Purpose::Run] {
        let mut c = claims(JOB);
        c.purpose = purpose;
        let id = runtime
            .host
            .lock()
            .unwrap()
            .claim(c.clone(), unix_ms())
            .unwrap()
            .0;
        let (mut client, task) = start(runtime.clone(), c, id).await;
        next_type(&mut client, "lease_ok").await;
        write_frame(&mut client, &hello()).await.unwrap();
        next_type(&mut client, "hello").await;
        drop(client);
        task.await.unwrap().unwrap();
        runtime.host.lock().unwrap().close(id).unwrap();
    }
    assert!(fake.calls.lock().unwrap().is_empty());
    assert_eq!(runtime.host.lock().unwrap().snapshot.per_job.disks.len(), 1);
    assert!(storage.held.lock().unwrap().contains(JOB));
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test(start_paused = true)]
async fn fragmented_prelude_frame_survives_a_heartbeat_turn() {
    let (mut writer, mut stream) = tokio::io::duplex(4096);
    let value = json!({"type":"hello","protocol_version":1});
    let text = serde_json::to_vec(&value).unwrap();
    let bytes = [
        (text.len() as u32).to_be_bytes().as_slice(),
        text.as_slice(),
    ]
    .concat();
    let mut reader = Reader::default();
    writer.write_all(&bytes[..2]).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), reader.next(&mut stream))
            .await
            .is_err()
    );
    writer.write_all(&bytes[2..]).await.unwrap();
    let frame = reader.next(&mut stream).await.unwrap();
    assert_eq!(frame.bytes, bytes);
    assert_eq!(frame.value, value);
}

/// A queued upload larger than the spool is paused, not refused: a first job
/// in a new workspace sends the whole history while its VM can't boot yet.
#[tokio::test]
async fn queued_upload_over_cap_pauses_within_the_spool_and_disconnect_deletes_it() {
    let (runtime, fake, storage) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let disk = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .disks
        .get_mut(&disk)
        .unwrap()
        .attached_job = Some("holder".into());
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    let chunk = json!({"type":"bundle_chunk","index":0,"offset":0,"data":"a".repeat(700_000)});
    // 100 chunks (70 MB) exceed the 64 MiB spool. The host stops reading, so
    // the writes stall rather than fail.
    let written = tokio::time::timeout(Duration::from_secs(3), async {
        for _ in 0..100 {
            write_frame(&mut client, &chunk).await.unwrap();
        }
    })
    .await;
    assert!(written.is_err(), "an over-cap upload must pause");
    let spooled = std::fs::metadata(storage.spool(JOB)).unwrap().len();
    assert!(spooled > SPOOL_LIMIT - 2 * FRAME_LIMIT as u64, "{spooled}");
    assert!(spooled <= SPOOL_LIMIT, "{spooled}");
    // The paused session stays alive and keeps the caller informed.
    next_type(&mut client, "heartbeat").await;
    assert!(!task.is_finished());
    drop(client);
    let _ = task.await.unwrap();
    assert!(!runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(!storage.spool(JOB).exists());
    assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
}

#[test]
fn restart_cleans_never_booted_spools_and_releases_admission() {
    let (runtime, _, storage) = fixture();
    let c = claims(JOB);
    claim(&runtime, &c);
    std::fs::write(storage.spool(JOB), b"abandoned").unwrap();
    let saved = runtime.host.lock().unwrap().snapshot.clone();
    let settings = runtime.host.lock().unwrap().settings.clone();
    let mut host = Host::new(BTreeMap::new(), saved, Box::new(Journal));
    host.settings = settings;
    let restarted = Runtime::with_job_resources(
        "host".into(),
        host,
        Arc::new(Fake::default()),
        storage.clone(),
    );
    restarted.recover().unwrap();
    assert!(restarted.host.lock().unwrap().snapshot.per_job.executions[JOB].ended);
    assert!(!storage.spool(JOB).exists());
    assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
}

#[test]
fn guest_disconnect_before_or_after_capability_refusal_leaves_identical_records_and_is_acked() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    claim(&runtime, &c);
    let snapshot = runtime.host.lock().unwrap().snapshot.clone();
    let terminal = refusal(
        JOB,
        "request_rejected",
        "assigned image capabilities changed",
    );
    let event: motor_protocol::TenantEvent = serde_json::from_value(
        json!({"type":"event","event_seq":1,"event":"refused","jti":JOB,"cause":"disconnect"}),
    )
    .unwrap();
    let mut records = Vec::new();
    for before in [true, false] {
        let mut host = Host::new(BTreeMap::new(), snapshot.clone(), Box::new(Journal));
        if before {
            host.event(JOB, &event, 1000).unwrap();
        }
        host.job_refused(JOB, terminal.clone(), 1000).unwrap();
        if !before {
            host.event(JOB, &event, 1000).unwrap();
        }
        records.push(host.snapshot);
    }
    assert_eq!(records[0], records[1]);
    runtime
        .host
        .lock()
        .unwrap()
        .job_refused(JOB, terminal.clone(), 1000)
        .unwrap();
    let (a, mut b) = UnixStream::pair().unwrap();
    b.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    *fake.lifecycle.lock().unwrap() = Some(a);
    runtime.start_job_events(JOB.into());
    let mut frame = serde_json::to_value(event).unwrap();
    frame["type"] = json!("event");
    write_json(&mut b, &frame).unwrap();
    assert_eq!(
        read_json(&mut b).unwrap(),
        json!({"type":"ack","event_seq":1})
    );
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.billing[JOB]
            .terminal
            .as_ref(),
        Some(&terminal)
    );
    runtime.stop_job(JOB, true, false).unwrap();
}

async fn wait_for_runner_hello(runtime: &Arc<Runtime>, job: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !runtime.host.lock().unwrap().snapshot.per_job.executions[job].runner_hello {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn free_disk_boots_on_hello_and_ready_vm_waits_for_validated_run() {
    let (runtime, fake, storage) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    assert!(fake.calls.lock().unwrap().is_empty());
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    wait_for_runner_hello(&runtime, JOB).await;
    assert_eq!(
        fake.calls.lock().unwrap().as_slice(),
        [format!("start:{JOB}")]
    );
    assert!(!runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    assert_eq!(fake.inputs.lock().unwrap().len(), 1);
    write_frame(
        &mut client,
        &json!({"type":"have","repositories":[],"git_dependencies":[]}),
    )
    .await
    .unwrap();
    next_type(&mut client, "have_reply").await;
    assert_eq!(fake.inputs.lock().unwrap().len(), 1);
    write_frame(&mut client, &run(JOB)).await.unwrap();
    next_type(&mut client, "accepted").await;
    next_type(&mut client, "output").await;
    assert!(runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    assert!(!storage.spool(JOB).exists());
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test]
async fn hosted_client_wire_exchange_accepts_pinned_runner_hello_and_runs_after_boot() {
    use motor_protocol::{write_frame_async, Frame, PROTOCOL_VERSION};
    let (runtime, fake, storage) = fixture();
    runtime
        .configure_job_resolvers(vec!["9.9.9.9".into()])
        .unwrap();
    let mut config =
        motor_runner::Config::new(storage.root.path().join("unused-runner-state"), "main");
    config.tenant_vm = true;
    config.isolate_jobs = true;
    config.return_chunked_max_bytes = Some(134217728);
    let pinned =
        serde_json::to_value(motor_runner::runner_hello(&config, true, true, true)).unwrap();
    *storage.pinned_hello.lock().unwrap() = Some(pinned.clone());
    *fake.hello.lock().unwrap() = Some(pinned);
    *fake.hang.lock().unwrap() = "boot";
    struct ReleaseGate(Arc<Fake>);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            *self.0.gate.0.lock().unwrap() = true;
            self.0.gate.1.notify_all();
        }
    }
    let _release = ReleaseGate(fake.clone());
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    // This is Channel::from_hosted's frame and codec, including its false
    // `default_branch_tip: false` offer. Decode the reply as Frame too: an untyped object can
    // look like capabilities to the host but makes the production client close.
    write_frame_async(
        &mut client,
        &Frame::Hello {
            return_paths: false,
            return_chunked_max_bytes: None,
            return_chunked: false,
            sibling_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: true,
            platforms: Vec::new(),
        },
    )
    .await
    .unwrap();
    loop {
        match serde_json::from_value::<Frame>(read_frame(&mut client).await.unwrap()).unwrap() {
            Frame::Hello {
                protocol_version: PROTOCOL_VERSION,
                return_paths: true,
                ..
            } => break,
            Frame::Heartbeat { .. } => continue,
            other => panic!("invalid hosted runner hello: {other:?}"),
        }
    }
    tokio::time::timeout(Duration::from_secs(1), fake.entered.notified())
        .await
        .unwrap();
    write_frame_async(
        &mut client,
        &serde_json::from_value::<Frame>(run(JOB)).unwrap(),
    )
    .await
    .unwrap();
    next_type(&mut client, "accepted").await;
    assert!(!runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    next_type(&mut client, "output").await;
    assert!(runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    let request = fake.guest_identity.lock().unwrap()[0].clone();
    assert_eq!(request["resolvers"], json!(["9.9.9.9"]));
    assert_eq!(request["hostname"], "motor");
    let machine_id = request["machine_id"].as_str().unwrap();
    assert_eq!(machine_id.len(), 32);
    assert!(machine_id
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert!(machine_id.bytes().any(|b| b != b'0'));
    assert_ne!(machine_id, "0123456789abcdef0123456789abcdef");
    drop(client);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn slow_first_account_copy_is_outside_boot_clock_and_not_adoptable() {
    use motor_protocol::{write_frame_async, Frame, PROTOCOL_VERSION};
    let (runtime, fake, storage) = fixture();
    let mut manifest = fake.inspect_job_images("gen-1").unwrap();
    manifest.toolchain.bytes = 34 * 1024 * 1024 * 1024;
    runtime.host.lock().unwrap().snapshot.per_job.images.insert(
        "gen-1".into(),
        crate::state::jobs::images::Generation {
            manifest,
            current: true,
            pending: false,
            artifacts_ready: true,
            deleting: false,
            removed: false,
            accounts: BTreeMap::new(),
            recovery_holders: BTreeMap::new(),
        },
    );
    storage.pause_prepare.store(true, Ordering::Release);
    struct ReleaseCopy(Arc<Storage>);
    impl Drop for ReleaseCopy {
        fn drop(&mut self) {
            *self.0.prepare_gate.0.lock().unwrap() = true;
            self.0.prepare_gate.1.notify_all();
        }
    }
    let _release = ReleaseCopy(storage.clone());
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame_async(
        &mut client,
        &Frame::Hello {
            return_paths: false,
            return_chunked_max_bytes: None,
            return_chunked: false,
            sibling_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: true,
            platforms: Vec::new(),
        },
    )
    .await
    .unwrap();
    let hello: Frame = serde_json::from_value(next_type(&mut client, "hello").await).unwrap();
    assert!(matches!(
        hello,
        Frame::Hello {
            protocol_version: PROTOCOL_VERSION,
            ..
        }
    ));
    tokio::time::timeout(Duration::from_secs(1), storage.prepare_entered.notified())
        .await
        .unwrap();
    assert!(!runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].boot_started);
    runtime.tick_jobs(unix_ms()).unwrap();
    assert!(!fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.starts_with("adopt:")));
    write_frame_async(
        &mut client,
        &serde_json::from_value::<Frame>(run(JOB)).unwrap(),
    )
    .await
    .unwrap();
    next_type(&mut client, "accepted").await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(65)).await;
    tokio::time::resume();
    *storage.prepare_gate.0.lock().unwrap() = true;
    storage.prepare_gate.1.notify_all();
    next_type(&mut client, "output").await;
    assert!(runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    drop(client);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn account_preparation_timeout_refuses_before_start_and_releases_after_copy_cleanup() {
    let (runtime, fake, storage) = fixture();
    storage.pause_prepare.store(true, Ordering::Release);
    struct ReleaseCopy(Arc<Storage>);
    impl Drop for ReleaseCopy {
        fn drop(&mut self) {
            *self.0.prepare_gate.0.lock().unwrap() = true;
            self.0.prepare_gate.1.notify_all();
        }
    }
    let _release = ReleaseCopy(storage.clone());
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    tokio::time::timeout(Duration::from_secs(2), queued(&mut client, JOB))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), storage.prepare_entered.notified())
        .await
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();
    tokio::time::timeout(
        Duration::from_secs(2),
        next_type(&mut client, "lease_refused"),
    )
    .await
    .unwrap();
    drop(client);
    task.await.unwrap().unwrap();
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(storage.held.lock().unwrap().contains(JOB));
    storage.fail_prepare.store(true, Ordering::Release);
    *storage.prepare_gate.0.lock().unwrap() = true;
    storage.prepare_gate.1.notify_all();
    tokio::time::timeout(Duration::from_secs(2), storage.released.notified())
        .await
        .unwrap();
    assert!(!storage.held.lock().unwrap().contains(JOB));
    let host = runtime.host.lock().unwrap();
    assert!(host.snapshot.per_job.executions[JOB].ended);
    assert!(host.snapshot.leases[JOB].started_at_ms.is_none());
    assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
}

#[tokio::test]
async fn tick_never_adopts_while_helper_start_or_guest_attach_is_in_flight() {
    for stage in ["boot", "runner"] {
        let (runtime, fake, _) = fixture();
        if let crate::config::HostSettings::PerJob(settings) =
            &mut runtime.host.lock().unwrap().settings
        {
            settings.boot_deadline_ms = 60_000;
        }
        *fake.hang.lock().unwrap() = stage;
        struct ReleaseGate(Arc<Fake>);
        impl Drop for ReleaseGate {
            fn drop(&mut self) {
                *self.0.gate.0.lock().unwrap() = true;
                self.0.gate.1.notify_all();
            }
        }
        let _release = ReleaseGate(fake.clone());
        let c = claims(JOB);
        let id = claim(&runtime, &c);
        let (mut client, task) = start(runtime.clone(), c, id).await;
        queued(&mut client, JOB).await;
        tokio::time::timeout(Duration::from_secs(1), fake.entered.notified())
            .await
            .unwrap();
        assert!(runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].boot_started);
        runtime.tick_jobs(unix_ms()).unwrap();
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("adopt:")),
            "adoption during {stage}"
        );
        *fake.gate.0.lock().unwrap() = true;
        fake.gate.1.notify_all();
        next_type(&mut client, "output").await;
        drop(client);
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn run_validated_before_vm_ready_remains_spooled_until_hello() {
    let (runtime, fake, storage) = fixture();
    *fake.hang.lock().unwrap() = "runner";
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    fake.entered.notified().await;
    write_frame(&mut client, &run(JOB)).await.unwrap();
    next_type(&mut client, "accepted").await;
    assert!(!runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    assert!(fake.inputs.lock().unwrap().is_empty());
    assert!(storage.spool(JOB).exists());
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    next_type(&mut client, "output").await;
    assert!(runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    assert!(!storage.spool(JOB).exists());
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test]
async fn disconnect_or_refusal_before_run_stops_an_already_ready_vm_and_releases_admission() {
    for disconnect in [true, false] {
        let (runtime, fake, storage) = fixture();
        let c = claims(JOB);
        let id = claim(&runtime, &c);
        let (mut client, task) = start(runtime.clone(), c, id).await;
        next_type(&mut client, "lease_ok").await;
        write_frame(&mut client, &hello()).await.unwrap();
        next_type(&mut client, "hello").await;
        wait_for_runner_hello(&runtime, JOB).await;
        if disconnect {
            drop(client);
            assert!(task.await.unwrap().is_err());
        } else {
            let mut invalid = run(JOB);
            invalid["weight_hint"] = json!(2);
            write_frame(&mut client, &invalid).await.unwrap();
            next_type(&mut client, "lease_refused").await;
            task.await.unwrap().unwrap();
        }
        let host = runtime.host.lock().unwrap();
        assert!(!host.snapshot.leases[JOB].delivered);
        assert!(host.snapshot.zero_usage.contains_key(JOB));
        assert!(host.snapshot.per_job.executions[JOB].ended);
        drop(host);
        assert_eq!(fake.inputs.lock().unwrap().len(), 1);
        assert_eq!(
            fake.calls.lock().unwrap().as_slice(),
            [format!("start:{JOB}"), format!("stop:{JOB}")]
        );
        assert!(!storage.spool(JOB).exists());
        assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
    }
}

mod balloon_tests;
mod replay_tests;
mod settlement_tests;

#[tokio::test]
async fn delivered_run_without_guest_acceptance_replays_unknown_after_disconnect() {
    let (runtime, fake, _) = fixture();
    fake.disconnect_on_run.store(true, Ordering::SeqCst);
    let mut c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c.clone(), id).await;
    queued(&mut client, JOB).await;
    let output = tokio::time::timeout(Duration::from_secs(10), next_type(&mut client, "output"))
        .await
        .expect("helper exit settles the delivered Run");
    assert_eq!(output["bytes"], "");
    assert_eq!(output["truncated_before_seq"], output["seq"]);
    let terminal = next_type(&mut client, "terminal").await;
    assert_eq!(terminal["outcome"]["type"], "outcome_unknown");
    assert_eq!(terminal["ran"], "remote");
    next_type(&mut client, "hosted_terminal").await;
    task.await.unwrap().unwrap();
    drop(client);
    {
        let mut host = runtime.host.lock().unwrap();
        assert!(host.snapshot.leases[JOB].delivered);
        assert!(host.snapshot.leases[JOB].started_at_ms.is_none());
        assert!(!host.snapshot.zero_usage.contains_key(JOB));
        host.close(id).unwrap();
    }

    c.purpose = Purpose::Attach;
    let id = runtime
        .host
        .lock()
        .unwrap()
        .claim(c.clone(), unix_ms())
        .unwrap()
        .0;
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    write_frame(
        &mut client,
        &json!({"type":"attach","job_id":JOB,"from_seq":0}),
    )
    .await
    .unwrap();
    next_type(&mut client, "output").await;
    assert_eq!(next_type(&mut client, "terminal").await, terminal);
    next_type(&mut client, "hosted_terminal").await;
    drop(client);
    task.await.unwrap().unwrap();
    runtime.host.lock().unwrap().close(id).unwrap();
}

/// The guest runner closes its channel after 60 s with no byte from the host.
/// Once the upload is over, only host heartbeats keep a long job's channel
/// open, whether or not a client is attached.
#[tokio::test]
async fn quiet_running_job_channel_gets_host_heartbeats() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    next_type(&mut client, "output").await;
    let quiet = fake.inputs.lock().unwrap().len();
    tokio::time::sleep(super::stream::GUEST_HEARTBEAT + Duration::from_secs(1)).await;
    let beats = fake.inputs.lock().unwrap()[quiet..]
        .iter()
        .filter(|bytes| {
            serde_json::from_slice::<Value>(&bytes[4..]).unwrap()["type"] == "heartbeat"
        })
        .count();
    assert!(beats >= 1, "no host heartbeat reached the guest");
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[test]
fn mirror_report_is_recorded_before_lifecycle_ack() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    claim(&runtime, &c);
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB)
        .unwrap()
        .unwrap();
    let (host, mut guest) = UnixStream::pair().unwrap();
    guest
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    *fake.lifecycle.lock().unwrap() = Some(host);
    runtime.start_job_events(JOB.into());
    write_json(&mut guest, &json!({"type":"event","event_seq":1,"event":"mirror_refs","mirror_refs":{
        "imported":true,"refs":[{"repository":"/repo","reference":"refs/snapshots/held","commit":"held"}]
    }})).unwrap();
    assert_eq!(
        read_json(&mut guest).unwrap(),
        json!({"type":"ack","event_seq":1})
    );
    let mut host = runtime.host.lock().unwrap();
    let disk = &host.snapshot.per_job.executions[JOB].disk_id;
    assert_eq!(host.snapshot.per_job.disks[disk].have[0].commit, "held");
    assert_eq!(host.snapshot.vms[JOB].event_seq, 1);
    host.job_stopped(JOB, false).unwrap();
}
