//! ck-motor's real hosted channel, through motor-host's real tenant relay, to
//! the real tenant runner, with a client that falls behind a job's output.
//!
//! When a job ends, the tenant runner writes the rest of its output and the
//! terminal into its job stream and closes that stream. A client that is still
//! behind leaves those frames queued between the runner and itself, while its
//! own transport heartbeats keep arriving once a second. motor-host forwards
//! each heartbeat to the closed guest socket. That write fails with a broken
//! pipe, and motor-host used to end the whole relay on it, discarding the
//! queued output and the terminal. The client then had to reattach, and a
//! client that was still slow lost the stream again the same way, up to its
//! reattach limit.

use super::tls;
use crate::transport::{Channel, Incoming};
use ed25519_dalek::SigningKey;
use motor_host::server::{serve, unix_ms, Runtime};
use motor_host::state::{Host, Snapshot, Tenant};
use motor_lease::{Claims, Confirmation, Jwks, JwksKey, LeaseResponse, Placement, RequesterJwk};
use motor_protocol::*;
use sha2::Digest;
use std::{
    collections::BTreeMap,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Output lines the job prints, and the bytes in each (with its newline).
const LINES: usize = 40;
const LINE_BYTES: usize = 3001;

/// The VM side of motor-host's helper: each job relay is a socket pair whose
/// guest end is served by the real tenant runner, as the tenant's vsock
/// listener would serve it.
struct TenantVm {
    runner: motor_runner::Runner,
    streams: Mutex<Vec<tokio::task::JoinHandle<std::io::Result<()>>>>,
}
impl motor_host::helper::Control for TenantVm {
    fn adopt(&self, _: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn wake(&self, _: &str, _: Option<u64>) -> anyhow::Result<u64> {
        Ok(1)
    }
    fn quota(&self, _: &str, _: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn relay(&self, _: &str, port: u32) -> anyhow::Result<std::os::unix::net::UnixStream> {
        // Lifecycle events (port 53) are recorded by the test itself.
        anyhow::ensure!(port == 52, "no lifecycle stream in this test");
        let (host, guest) = std::os::unix::net::UnixStream::pair()?;
        guest.set_nonblocking(true)?;
        // motor-host calls this on its blocking pool, inside its runtime.
        let guest = tokio::net::UnixStream::from_std(guest)?;
        self.streams
            .lock()
            .unwrap()
            .push(tokio::spawn(motor_runner::serve_tenant_connection(
                self.runner.clone(),
                guest,
            )));
        Ok(host)
    }
    fn admin(&self, _: &str, _: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"ok": true}))
    }
    fn suspend(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn available_bytes(&self) -> anyhow::Result<u64> {
        Ok(u64::MAX)
    }
}

#[derive(Default)]
struct MemoryJournal;
impl motor_host::journal::Journal for MemoryJournal {
    fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
        Ok(())
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A one-commit repository and a bundle of it, as ck-motor uploads it.
fn repository(scratch: &Path) -> (String, String, Vec<u8>) {
    let source = scratch.join("local/main");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q"]);
    std::fs::write(source.join("source"), "first\n").unwrap();
    git(&source, &["add", "."]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "snapshot",
        ],
    );
    let reference = format!("refs/ck-motor/{}/snapshot", Uuid::now_v7());
    git(&source, &["update-ref", &reference, "HEAD"]);
    let bundle = scratch.join("snapshot.bundle");
    git(
        &source,
        &["bundle", "create", bundle.to_str().unwrap(), &reference],
    );
    (
        git(&source, &["rev-parse", "HEAD"]),
        git(&source, &["rev-parse", "HEAD^{tree}"]),
        std::fs::read(bundle).unwrap(),
    )
}

/// A loopback TCP connection with small socket buffers, standing in for a
/// client whose link or reader is slower than the job's output: a client that
/// stops reading leaves the job's frames queued at the host and the runner,
/// not read ahead into kernel buffers. Returns (client, host side).
async fn narrow_tcp_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
    let listening = tokio::net::TcpSocket::new_v4().unwrap();
    listening.set_recv_buffer_size(4096).unwrap();
    listening.set_send_buffer_size(4096).unwrap();
    listening.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = listening.listen(1).unwrap();
    let address = listener.local_addr().unwrap();
    let connecting = tokio::net::TcpSocket::new_v4().unwrap();
    connecting.set_recv_buffer_size(4096).unwrap();
    let (client, accepted) = tokio::join!(connecting.connect(address), listener.accept());
    (client.unwrap(), accepted.unwrap().0)
}

fn lifecycle(seq: u64, event: TenantEventKind, job: Uuid) -> TenantEvent {
    TenantEvent {
        event_seq: seq,
        event,
        jti: Some(job),
        cause: None,
        setup: None,
        mem_peak_mib: None,
    }
}

struct HostedRelay {
    runner: motor_runner::Runner,
    vm: Arc<TenantVm>,
    runtime: Arc<Runtime>,
    channel: Channel,
    served: tokio::task::JoinHandle<anyhow::Result<()>>,
    id: Uuid,
}

async fn hosted_relay(scratch: &Path) -> HostedRelay {
    let remote = scratch.join("remote");
    let mut config = motor_runner::Config::new(scratch.join("runner-state"), "e2e");
    config.tenant_vm = true;
    config.free_space_floor_bytes = 0;
    config.max_weight = 2;
    config.workspace_roots = vec![remote.join("worktrees")];
    config.owned_workspace_roots = vec![remote.clone()];
    // A base command that builds nothing, so the runner's cached build
    // directory stays empty and the test needs no reflink-capable fixture.
    config.base_commands = vec!["true".into()];
    let runner = motor_runner::Runner::open(config).unwrap();
    let vm = Arc::new(TenantVm {
        runner: runner.clone(),
        streams: Mutex::new(Vec::new()),
    });

    let sub = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let host = Host::new(
        BTreeMap::from([(
            "vm".into(),
            Tenant {
                tenant: "tenant".into(),
                sub: sub.into(),
                vcpu_ceiling: 2,
                max_weight: None,
                guest_ram_mib: 18432,
                idle_suspend_s: 600,
                host_floor_bytes: 0,
            },
        )]),
        Snapshot::default(),
        Box::new(MemoryJournal),
    );
    let runtime = Runtime::new("host".into(), host, vm.clone());
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

    let id = Uuid::now_v7();
    let now_s = unix_ms() / 1000;
    let platform = if cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x64"
    };
    let claims = Claims {
        // Registered hosts refuse run tokens without a workspace hash.
        workspace_key_hash: Some("w".repeat(43)),
        iss: motor_lease::ISSUER.into(),
        aud: "host".into(),
        jti: id.to_string(),
        sub: sub.into(),
        purpose: motor_lease::Purpose::Run,
        iat: now_s,
        exp: now_s + 300,
        vcpus: 1,
        mem_mib: 16_384,
        platform: platform.into(),
        net: false,
        placement: Placement {
            host: "host".into(),
            vm: Some("vm".into()),
        },
        max_s: 30,
        cnf: Confirmation {
            jkt: RequesterJwk::from_key(&requester.verifying_key())
                .thumbprint()
                .unwrap(),
        },
    };
    let lease = LeaseResponse {
        lease_id: "lease".into(),
        job_id: id.to_string(),
        placement: claims.placement.clone(),
        endpoint: "127.0.0.1:1".into(),
        endpoint_spki_sha256: String::new(),
        job_token: motor_lease::sign_token(&claims, "key", &broker).unwrap(),
        token_expires_at_ms: claims.exp * 1000,
        max_s: claims.max_s,
        reserved_vcpu_ms: 0,
    };

    let (pipe, server) = narrow_tcp_pair().await;
    let served = tokio::spawn(serve(runtime.clone(), server));
    let channel = tls::authenticate(Box::new(pipe), &lease, &requester, &claims)
        .await
        .unwrap_or_else(|failure| panic!("hosted connect failed: {failure:?}"));
    HostedRelay {
        runner,
        vm,
        runtime,
        channel,
        served,
        id,
    }
}

struct ReleaseQueuedJob {
    unlock: std::sync::mpsc::Sender<()>,
    release_file: std::path::PathBuf,
}
impl Drop for ReleaseQueuedJob {
    fn drop(&mut self) {
        let _ = self.unlock.send(());
        let _ = std::fs::write(&self.release_file, "release");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_hosted_job_queued_past_read_inactivity_keeps_heartbeats_and_delivers_terminal() {
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-queued-heartbeat");
    let HostedRelay {
        runner,
        runtime,
        mut channel,
        served,
        id,
        ..
    } = hosted_relay(scratch.path()).await;
    let (commit, tree, bundle) = repository(scratch.path());
    let remote = scratch.join("remote");
    let root = remote.join("source/main").to_string_lossy().into_owned();
    let workspace = remote.join("worktrees/a").to_string_lossy().into_owned();
    let run = |job_id: Uuid, command: String, timeout: u64| -> Run {
        serde_json::from_value(serde_json::json!({
            "job_id": job_id, "workspace_key": workspace, "repository_root": root,
            "cwd": workspace, "command": command, "env": {},
            "weight_hint": 1, "timeout": timeout, "queue_wait_limit_s": 300,
            "snapshot_commit": commit, "tree_hash": tree, "integration_tip": commit,
            "base_commit": commit, "siblings": [],
            "bundles": [{"index": 0, "repository": root, "length": bundle.len(),
                "sha256": format!("{:x}", sha2::Sha256::digest(&bundle))}],
        }))
        .unwrap()
    };
    let upload = Frame::BundleChunk {
        index: 0,
        offset: 0,
        data: BytePayload(bundle.clone()),
    };
    let first_id = Uuid::now_v7();
    let (mut first_pipe, first_guest) = tokio::io::duplex(1 << 20);
    let first_served = tokio::spawn(motor_runner::serve_tenant_connection(
        runner.clone(),
        first_guest,
    ));
    write_frame_async(
        &mut first_pipe,
        &Frame::VerifiedLease(VerifiedLease {
            jti: first_id,
            purpose: LeasePurpose::Run,
            sub: "acct:test".into(),
            vcpus: 1,
            mem_mib: 16384,
            platform: if cfg!(target_arch = "aarch64") {
                "linux-arm64"
            } else {
                "linux-x64"
            }
            .into(),
            net: false,
            max_s: 180,
            exp: unix_ms() / 1000 + 300,
        }),
    )
    .await
    .unwrap();
    let mut first = Channel::from_hosted(Box::new(first_pipe), false)
        .await
        .unwrap();
    let (unlock, unlock_rx) = std::sync::mpsc::channel();
    let release = ReleaseQueuedJob {
        unlock,
        release_file: scratch.join("release-first"),
    };
    // Occupying the same checkout keeps the second job genuinely queued,
    // rather than simulating a silent runner or a slow upload.
    let command = format!(
        "printf ready; while [ ! -f '{}' ]; do sleep 0.05; done",
        release.release_file.display()
    );
    first
        .send(Frame::Run(run(first_id, command, 180)))
        .await
        .unwrap();
    assert!(matches!(
        first.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    first.send(upload.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(first.next().await.unwrap(), Incoming::Frame(Frame::Output(record)) if record.bytes.0 == b"ready") {
                break;
            }
        }
    }).await.expect("first job did not occupy the checkout");

    channel
        .send(Frame::Run(run(id, "printf queued; exit 7".into(), 30)))
        .await
        .unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    runtime
        .host
        .lock()
        .unwrap()
        .event(
            "tenant",
            &lifecycle(1, TenantEventKind::Admitted, id),
            unix_ms(),
        )
        .unwrap();
    channel.send(upload).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while runner.job_status(id).unwrap().state != "queued" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("accepted job never entered the tenant queue");

    let locked_runtime = runtime.clone();
    let (entered, locked) = tokio::sync::oneshot::channel();
    let lock_holder = tokio::task::spawn_blocking(move || {
        let _host = locked_runtime.host.lock().unwrap();
        entered.send(()).unwrap();
        unlock_rx
            .recv_timeout(Duration::from_secs(90))
            .expect("host lock was not released");
    });
    locked.await.unwrap();
    // Channel's real read-inactivity timer is 60 seconds. Keep the Host lock
    // held beyond it: only guest heartbeats may keep this connection alive.
    let held_at = tokio::time::Instant::now();
    let until = held_at + Duration::from_secs(65);
    let mut last_heartbeat = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(until) => break,
            incoming = channel.next_job() => match incoming.expect("queued hosted stream hit read inactivity while the lease was active") {
                Incoming::Frame(Frame::Heartbeat { .. }) => last_heartbeat = Some(tokio::time::Instant::now()),
                Incoming::Frame(Frame::Started { .. } | Frame::Terminal(_)) => panic!("job left its queue before the checkout was released"),
                _ => {}
            }
        }
    }
    assert!(
        last_heartbeat.expect("no guest heartbeat reached the client")
            >= held_at + Duration::from_secs(60)
    );
    assert_eq!(runner.job_status(id).unwrap().state, "queued");
    drop(release);
    lock_holder.await.unwrap();

    tokio::time::timeout(Duration::from_secs(30), async {
        while runner.job_status(id).unwrap().terminal.is_none() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("queued job did not finish after the checkout was released");
    {
        let mut host = runtime.host.lock().unwrap();
        host.event(
            "tenant",
            &lifecycle(2, TenantEventKind::Started, id),
            unix_ms(),
        )
        .unwrap();
        host.sample(
            "tenant",
            motor_host::billing::Metrics {
                memory_bytes: 1 << 20,
                egress_bytes: 0,
            },
        )
        .unwrap();
        host.event(
            "tenant",
            &lifecycle(3, TenantEventKind::Terminal, id),
            unix_ms(),
        )
        .unwrap();
        host.billing_tick(unix_ms()).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut terminal = false;
        loop {
            match channel.next().await.unwrap() {
                Incoming::Frame(Frame::Terminal(record)) => {
                    assert_eq!(record.outcome, Outcome::Exit { code: 7 });
                    terminal = true;
                }
                Incoming::Frame(Frame::HostedTerminal(_)) => {
                    assert!(terminal);
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the attached stream did not deliver both terminal records");
    drop(channel);
    tokio::time::timeout(Duration::from_secs(30), served)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(first);
    tokio::time::timeout(Duration::from_secs(30), first_served)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_hosted_client_receives_every_frame_after_the_tenant_runner_closes_its_job_stream() {
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-relay-close");
    let HostedRelay {
        runner,
        vm,
        runtime,
        mut channel,
        served,
        id,
    } = hosted_relay(scratch.path()).await;
    let remote = scratch.join("remote");

    let (commit, tree, bundle) = repository(scratch.path());
    let root = remote.join("source/main").to_string_lossy().into_owned();
    let workspace = remote.join("worktrees/a").to_string_lossy().into_owned();
    // Separate writes, so each line becomes its own output frame.
    let command = format!(
        "for i in $(seq 1 {LINES}); do printf '%0{width}d\\n' \"$i\"; sleep 0.02; done",
        width = LINE_BYTES - 1
    );
    let run: Run = serde_json::from_value(serde_json::json!({
        "job_id": id, "workspace_key": workspace, "repository_root": root,
        "cwd": workspace, "command": command, "env": {},
        "weight_hint": 1, "timeout": 30, "queue_wait_limit_s": 300,
        "snapshot_commit": commit, "tree_hash": tree, "integration_tip": commit,
        "base_commit": commit, "siblings": [],
        "bundles": [{"index": 0, "repository": root, "length": bundle.len(),
            "sha256": format!("{:x}", sha2::Sha256::digest(&bundle))}],
    }))
    .unwrap();
    channel.send(Frame::Run(run)).await.unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    // The lifecycle events the runner's event channel would deliver.
    let started_ms = unix_ms();
    {
        let mut h = runtime.host.lock().unwrap();
        h.event(
            "tenant",
            &lifecycle(1, TenantEventKind::Admitted, id),
            started_ms,
        )
        .unwrap();
        h.event(
            "tenant",
            &lifecycle(2, TenantEventKind::Started, id),
            started_ms,
        )
        .unwrap();
        h.sample(
            "tenant",
            motor_host::billing::Metrics {
                memory_bytes: 1 << 20,
                egress_bytes: 0,
            },
        )
        .unwrap();
    }
    channel
        .send(Frame::BundleChunk {
            index: 0,
            offset: 0,
            data: BytePayload(bundle),
        })
        .await
        .unwrap();

    let mut output = Vec::new();
    let next = |frame: std::io::Result<Incoming>, output: &mut Vec<u8>| match frame {
        Ok(Incoming::Frame(Frame::Output(record))) => {
            output.extend(record.bytes.0);
            None
        }
        Ok(Incoming::Frame(frame @ (Frame::Terminal(_) | Frame::HostedTerminal(_)))) => Some(frame),
        Ok(_) => None,
        Err(error) => panic!(
            "hosted stream closed after {} of {} output bytes, before the terminal: {error}",
            output.len(),
            LINES * LINE_BYTES
        ),
    };
    // Read until the job's output starts, then fall behind: stop reading.
    tokio::time::timeout(Duration::from_secs(60), async {
        while output.is_empty() {
            assert!(next(channel.next_job().await, &mut output).is_none());
        }
    })
    .await
    .expect("no output within 60 seconds");

    // Hold the client back until the job has ended and the runner has
    // written the rest of its stream and closed it.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let ended = runner.job_status(id).unwrap().terminal.is_some();
            if ended && vm.streams.lock().unwrap().iter().all(|s| s.is_finished()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the runner did not finish and close its job stream within 60 seconds");
    {
        let mut h = runtime.host.lock().unwrap();
        h.event(
            "tenant",
            &lifecycle(3, TenantEventKind::Terminal, id),
            unix_ms(),
        )
        .unwrap();
        // motor-host's lifecycle reader runs a billing tick after every
        // event, and only that tick settles the job (publishes its
        // hosted_terminal) when the runner's terminal reached the relay before
        // this Terminal event. That happens when the runner's output arrives in
        // few enough frames that all of them fit in the client's receive queue
        // and the socket buffers; motor-host then reads the terminal while
        // the client is still held back. Without the tick the relay would wait
        // for a hosted_terminal that never comes.
        h.billing_tick(unix_ms()).unwrap();
    }
    // The client still sends a heartbeat every second while it is behind;
    // the first one after the close used to end the relay.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        !served.is_finished(),
        "motor-host ended the relay while the client was behind"
    );

    let (terminal, hosted) = tokio::time::timeout(Duration::from_secs(60), async {
        let terminal = loop {
            if let Some(frame) = next(channel.next_job().await, &mut output) {
                break frame;
            }
        };
        let hosted = loop {
            if let Some(frame) = next(channel.next_job().await, &mut output) {
                break frame;
            }
        };
        (terminal, hosted)
    })
    .await
    .expect("no terminal within 60 seconds");
    assert_eq!(
        output.len(),
        LINES * LINE_BYTES,
        "every output byte arrives"
    );
    let Frame::Terminal(terminal) = terminal else {
        panic!("expected the runner's terminal, got {terminal:?}");
    };
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 }, "{terminal:?}");
    assert!(
        matches!(hosted, Frame::HostedTerminal(_)),
        "expected hosted_terminal, got {hosted:?}"
    );
    drop(channel);
    tokio::time::timeout(Duration::from_secs(30), served)
        .await
        .expect("motor-host did not finish after the client closed")
        .unwrap()
        .unwrap();
}
