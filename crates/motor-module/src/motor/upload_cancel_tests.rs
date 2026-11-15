use super::*;
use crate::hosted::test_http::Http;
use cortexkit_test_support::ScratchDir;
use ed25519_dalek::SigningKey;
use motor_host::server::{read_frame, serve, unix_ms, write_frame as write_json, Runtime};
use motor_host::state::Host;
use motor_lease::{
    Claims, Confirmation, Jwks, JwksKey, Placement, Prelude, Purpose, RequesterJwk, Version1,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct NoVm;
impl motor_host::helper::Control for NoVm {
    fn adopt(&self, _: &str) -> anyhow::Result<bool> {
        unreachable!("no VM may be used")
    }
    fn wake(&self, _: &str, _: Option<u64>) -> anyhow::Result<u64> {
        unreachable!("no VM may boot")
    }
    fn quota(&self, _: &str, _: u32) -> anyhow::Result<()> {
        unreachable!()
    }
    fn relay(&self, _: &str, _: u32) -> anyhow::Result<std::os::unix::net::UnixStream> {
        unreachable!("no runner exists")
    }
    fn admin(&self, _: &str, _: &Value) -> anyhow::Result<Value> {
        unreachable!()
    }
    fn suspend(&self, _: &str) -> anyhow::Result<()> {
        unreachable!()
    }
    fn available_bytes(&self) -> anyhow::Result<u64> {
        Ok(u64::MAX)
    }
    fn host_available_mib(&self) -> anyhow::Result<u64> {
        Ok(100_000)
    }
    fn job_vm_headroom_mib(&self) -> anyhow::Result<u64> {
        Ok(100_000)
    }
    fn online_cpus(&self) -> anyhow::Result<u32> {
        Ok(8)
    }
}

struct Resources(PathBuf);
impl motor_host::jobs::Resources for Resources {
    fn acquire(
        &self,
        execution: &motor_host::state::jobs::Execution,
    ) -> anyhow::Result<motor_host::jobs::Assignment> {
        Ok(motor_host::jobs::Assignment {
            image_generation_id: "gen-1".into(),
            hello: json!({"type":"hello","protocol_version":1,"return_paths":true,"sibling_paths":true}),
            spool: self.0.join(&execution.claims.jti),
            guest_init: json!({}),
        })
    }
    fn prepare(&self, _: &motor_host::state::jobs::Execution) -> anyhow::Result<()> {
        Ok(())
    }
    fn release(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

struct TrackedPipe {
    pipe: tokio::io::DuplexStream,
    active: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
}
impl Drop for TrackedPipe {
    fn drop(&mut self) {
        assert_eq!(self.active.fetch_sub(1, Ordering::SeqCst), 1);
    }
}
impl AsyncRead for TrackedPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_read(cx, bytes)
    }
}
impl AsyncWrite for TrackedPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.pipe).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Pin::new(&mut self.pipe).poll_shutdown(cx)
    }
}

struct HostConnector {
    runtime: Arc<Runtime>,
    claims: Claims,
    broker: SigningKey,
    requester: SigningKey,
    active: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    connects: AtomicUsize,
    tasks: Mutex<Vec<tokio::task::JoinHandle<anyhow::Result<()>>>>,
}
impl Connector for HostConnector {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn crate::transport::Duplex>>> + Send + '_>>
    {
        Box::pin(async move {
            assert_eq!(
                self.active.fetch_add(1, Ordering::SeqCst),
                0,
                "two host connections overlapped"
            );
            let attempt = self.connects.fetch_add(1, Ordering::SeqCst);
            let (client, server) = tokio::io::duplex(4096);
            self.tasks
                .lock()
                .unwrap()
                .push(tokio::spawn(serve(self.runtime.clone(), server)));
            let mut pipe = TrackedPipe {
                pipe: client,
                active: self.active.clone(),
                shutdowns: self.shutdowns.clone(),
            };
            let mut claims = self.claims.clone();
            if attempt > 0 {
                claims.purpose = Purpose::Attach;
            }
            // Exercise normal host token and proof-of-possession checks, even
            // though the transport is a bounded in-process socket.
            let token = motor_lease::sign_token(&claims, "test", &self.broker).unwrap();
            write_json(
                &mut pipe,
                &serde_json::to_value(Prelude::LeaseHello {
                    v: Version1,
                    token,
                    requester_jwk: RequesterJwk::from_key(&self.requester.verifying_key()),
                })
                .unwrap(),
            )
            .await
            .map_err(io::Error::other)?;
            let challenge: Prelude =
                serde_json::from_value(read_frame(&mut pipe).await.map_err(io::Error::other)?)
                    .unwrap();
            let Prelude::Challenge(challenge) = challenge else {
                panic!("host refused authenticated test job")
            };
            let sig =
                motor_lease::sign_challenge(&challenge, &claims, &self.requester, unix_ms() / 1000)
                    .unwrap();
            write_json(
                &mut pipe,
                &serde_json::to_value(Prelude::ChallengeResponse { sig }).unwrap(),
            )
            .await
            .map_err(io::Error::other)?;
            let response = read_frame(&mut pipe).await.map_err(io::Error::other)?;
            assert_eq!(response["type"], "lease_ok", "{response}");
            Ok(Box::new(pipe) as Box<dyn crate::transport::Duplex>)
        })
    }
}

#[tokio::test]
async fn queued_full_spool_cancel_detach_uses_one_connection_and_replays_no_start() {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    let scratch = ScratchDir::new("queued-upload-cancel");
    let root = scratch.join("host");
    std::fs::create_dir_all(&root).unwrap();
    let (journal, snapshot) = motor_host::journal::FileJournal::open(&root).unwrap();
    let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = motor_host::config::HostSettings::PerJob(motor_host::config::PerJobSettings {
        fork_checkouts: true,
        vcpu_ceiling: 8,
        permit_budget: 8,
        host_reserve_mib: 50,
        boot_reserve_mib: 20,
        per_vm_overhead_mib: 10,
        unused_memory_hold_percent: 0,
        reclaim_timeout_ms: 100,
        hysteresis_mib: 10,
        workspace_disk_cap_bytes: 1024,
        workspace_growth_reservation_bytes: 1024,
        boot_deadline_ms: 60_000,
        recovery_timeout_ms: 10_000,
        exit_evidence_bound_ms: 1000,
        teardown_timeout_ms: 1000,
    });
    host.record_host_resources(motor_host::state::jobs::HostResources {
        at_ms: unix_ms(),
        job_vm_headroom_mib: 100_000,
        available_mem_mib: 100_000,
        vcpu_ceiling: 8,
        permit_budget: 8,
    });
    let runtime = Runtime::with_job_resources(
        "host".into(),
        host,
        Arc::new(NoVm),
        Arc::new(Resources(root.clone())),
    );
    runtime
        .configure_job_resolvers(vec!["1.1.1.1".into()])
        .unwrap();
    let broker_key = SigningKey::from_bytes(&[7; 32]);
    let requester = SigningKey::from_bytes(&[8; 32]);
    runtime
        .keys
        .write()
        .unwrap()
        .refresh(Ok(Jwks {
            keys: vec![JwksKey::from_key("test", &broker_key.verifying_key())],
        }))
        .unwrap();
    let id = Uuid::now_v7();
    let claims = Claims {
        cache_write: None,
        mem_burst_mib: None,
        iss: motor_lease::ISSUER.into(),
        aud: "host".into(),
        jti: id.to_string(),
        sub: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        purpose: Purpose::Run,
        iat: unix_ms() / 1000,
        exp: unix_ms() / 1000 + 300,
        vcpus: 1,
        mem_mib: motor_lease::memory_max_mib(1),
        platform: "linux-x64".into(),
        net: false,
        max_s: 30,
        workspace_key_hash: Some(motor_lease::base64url(&[5; 32])),
        placement: Placement {
            host: "host".into(),
            vm: None,
        },
        cnf: Confirmation {
            jkt: RequesterJwk::from_key(&requester.verifying_key())
                .thumbprint()
                .unwrap(),
        },
    };
    // An admitted holder owns the same checkout disk. No helper is ever asked
    // to boot the target, regardless of upload speed or available memory.
    let holder = Uuid::now_v7().to_string();
    {
        let mut host = runtime.host.lock().unwrap();
        let mut previous = claims.clone();
        previous.jti = holder.clone();
        host.claim(previous, unix_ms()).unwrap();
        host.begin_job_boot(&holder).unwrap().unwrap();
    }
    let lease = json!({
        "lease_id":id, "job_id":id, "placement":claims.placement,
        "endpoint":"127.0.0.1:1", "endpoint_spki_sha256":motor_lease::base64url(&[7; 32]),
        "job_token":motor_lease::sign_token(&claims, "test", &broker_key).unwrap(),
        "token_expires_at_ms":claims.exp * 1000, "max_s":30, "reserved_vcpu_ms":30_000,
    });
    let broker = Http::new(move |path, _| {
        let body = if path.ends_with("/finish") {
            json!({"job_id":id,"settled_vcpu_ms":0,"released_vcpu_ms":30_000})
        } else {
            lease.clone()
        };
        (200, body)
    })
    .await;
    let connector = Arc::new(HostConnector {
        runtime: runtime.clone(),
        claims,
        broker: broker_key,
        requester,
        active: Arc::new(AtomicUsize::new(0)),
        shutdowns: Arc::new(AtomicUsize::new(0)),
        connects: AtomicUsize::new(0),
        tasks: Mutex::new(Vec::new()),
    });
    let config = Config {
        runner_instance: "test".into(),
        broker_url: Some(broker.url.clone()),
        broker_token_source: crate::BrokerTokenSource::Fake,
        ..Config::default()
    };
    let mut motor = Motor::open(config.clone(), &scratch.join("state")).unwrap();
    motor.hosted_connector = Some(connector.clone());
    motor.storage.create(id, 180, None, None, false).unwrap();
    motor
        .storage
        .hosted_route(id, crate::HostedJobState::new(&config))
        .unwrap();
    let repo = super::queue_tests::fixture_repo(scratch.path());
    let path = repo.to_str().unwrap();
    let snapshots = Snapshots::build(
        &config,
        path,
        path,
        &[],
        scratch.path(),
        id,
        None,
        &motor.storage,
    )
    .unwrap();
    let length = motor_host::jobs::SPOOL_LIMIT + 65536;
    std::fs::create_dir_all(&snapshots.spool.0).unwrap();
    std::fs::File::create(snapshots.spool.0.join("bundle-0"))
        .unwrap()
        .set_len(length)
        .unwrap();
    let bundle: BundleManifest = serde_json::from_value(
        json!({"index":0,"repository":path,"length":length,"sha256":"0".repeat(64)}),
    )
    .unwrap();
    let run: Run = serde_json::from_value(json!({
        "job_id":id,"workspace_key":path,"repository_root":path,"cwd":path,"command":"true",
        "env":{},"weight_hint":1,"timeout":30,"queue_wait_limit_s":180,
        "snapshot_commit":"0".repeat(40),"tree_hash":"0".repeat(40),"integration_tip":"",
        "siblings":[],"bundles":[bundle],
    }))
    .unwrap();
    let mut channel = Channel::from_hosted(connector.connect(id).await.unwrap(), false)
        .await
        .unwrap();
    motor.storage.run_sent(id, true).unwrap();
    channel.send(Frame::Run(run)).await.unwrap();
    let Incoming::Frame(Frame::Accepted(accepted)) = channel.next().await.unwrap() else {
        panic!("job must be accepted before cancellation")
    };
    let (mut caller, caller_tx, _) = motor.stream(id);
    let owner = motor.reserve_hosted_result(id, 0);
    caller.wake = owner.wake.clone();
    motor.subscribe_hosted_result(id, owner.clone(), 0, true, caller_tx);
    let (tx, records) = mpsc::channel(32);
    let producer_motor = motor.clone();
    let wake = owner.wake.subscribe();
    let producer_tx = tx.clone();
    let producer = tokio::spawn(async move {
        emit(&producer_tx, StreamRecord::Accepted(accepted))
            .await
            .unwrap();
        let (_, channel) = producer_motor
            .upload_submission(id, channel, &snapshots, &[bundle], &wake)
            .await;
        if let Err(error) = producer_motor
            .follow_attempt(id, 0, channel, &producer_tx, wake, None, None)
            .await
        {
            let _ = producer_tx.send(Err(error)).await;
        }
    });
    motor.collect_hosted_result(id, owner, tx, records, producer);
    assert!(matches!(
        caller.records.recv().await.unwrap().unwrap(),
        StreamRecord::Accepted(_)
    ));
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if std::fs::metadata(root.join(id.to_string())).is_ok_and(|m| {
                m.len() + motor_protocol::MAX_FRAME_BYTES as u64 + 4 > motor_host::jobs::SPOOL_LIMIT
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the upload must fill the host spool before cancel");
    assert!(
        !runtime.host.lock().unwrap().snapshot.per_job.executions[&id.to_string()].boot_started
    );
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    drop(caller);
    let mut attached = motor.attach(AttachRequest::new(id, 0)).unwrap();
    let terminal = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(record) = attached.records.recv().await {
            if let StreamRecord::Terminal(terminal) = record.unwrap() { return terminal; }
        }
        panic!("cancelled result missing")
    }).await.unwrap_or_else(|_| panic!("cancel must interrupt a blocked upload promptly: {} billing={:?} connects={} active={} tasks={:?}", motor.hosted_result_snapshot_for_test(id), runtime.host.lock().unwrap().snapshot.billing.get(&id.to_string()), connector.connects.load(Ordering::SeqCst), connector.active.load(Ordering::SeqCst), connector.tasks.lock().unwrap().iter().map(|task| task.is_finished()).collect::<Vec<_>>()));
    assert_eq!(terminal.outcome, Outcome::Cancelled);
    assert_eq!(terminal.ran, Some(Ran::None));
    drop(attached);
    motor.stop_hosted_results().await;
    let tasks = std::mem::take(&mut *connector.tasks.lock().unwrap());
    for task in tasks {
        let _ = task.await.unwrap();
    }
    assert_eq!(connector.active.load(Ordering::SeqCst), 0);
    assert_eq!(
        connector.shutdowns.load(Ordering::SeqCst),
        0,
        "takeover must drop the transport without a graceful shutdown"
    );
    assert!(connector.connects.load(Ordering::SeqCst) >= 2);
    let mut host = runtime.host.lock().unwrap();
    assert!(host.begin_job_boot(&id.to_string()).unwrap().is_none());
    assert!(!host.snapshot.per_job.executions[&id.to_string()].boot_started);
    assert!(host.snapshot.per_job.executions[&id.to_string()].ended);
    assert!(!host.snapshot.leases[&id.to_string()].delivered);
}
