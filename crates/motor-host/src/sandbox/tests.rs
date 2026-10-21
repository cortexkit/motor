use super::*;
use crate::server::{read_frame, write_frame};
use ed25519_dalek::{Signer, SigningKey};
use motor_lease::fake::{BrokerRequest, FakeBroker, FakeConfig};
use motor_lease::{Challenge, Claims, LeaseResponse, Placement, RequesterJwk};
use serde_json::json;
use tokio::io::AsyncWriteExt;

const JOB: &str = "019e4567-89ab-7def-8123-456789abcdef";
const ID: &str = "bx_test";

struct Fixture {
    dir: tempfile::TempDir,
    runtime: Arc<Runtime>,
    broker: FakeBroker,
    signer: SigningKey,
    requester: SigningKey,
    jobs: tokio::net::UnixListener,
}
impl Fixture {
    fn new() -> Self {
        let root = cortexkit_test_support::scratch_root();
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let id_file = dir.path().join("id");
        std::fs::write(&id_file, ID).unwrap();
        std::fs::set_permissions(&id_file, std::fs::Permissions::from_mode(0o444)).unwrap();
        let config = Config {
            journal_dir: dir.path().join("state"),
            job_socket: dir.path().join("j"),
            event_socket: dir.path().join("e"),
            admin_socket: dir.path().join("a"),
            vcpu_ceiling: 8,
            id_file,
            port: 0,
            ready_marker: None,
            storage_health: None,
        };
        let jobs = tokio::net::UnixListener::bind(&config.job_socket).unwrap();
        let mut runtime = runtime(config).unwrap();
        // Portable fixtures use their own uid; the production constructor pins 0.
        Arc::get_mut(&mut runtime)
            .unwrap()
            .sandbox_identity
            .as_mut()
            .unwrap()
            .owner = unsafe { libc::geteuid() };
        let signer = SigningKey::from_bytes(&[7; 32]);
        let requester = SigningKey::from_bytes(&[8; 32]);
        let broker = FakeBroker::new(
            "boat-test",
            FakeConfig {
                worker_bearer: "worker".into(),
                subject: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                placement: Placement {
                    host: audience(ID),
                    vm: Some(ID.into()),
                },
                endpoint: "127.0.0.1:9443".into(),
                endpoint_spki_sha256: motor_lease::base64url(&[0; 32]),
                kid: "key".into(),
                signing_key: signer.clone(),
                announced_keys: vec![],
            },
        )
        .unwrap();
        runtime
            .keys
            .write()
            .unwrap()
            .refresh(Ok(broker.jwks()))
            .unwrap();
        Self {
            dir,
            runtime,
            broker,
            signer,
            requester,
            jobs,
        }
    }
    fn lease(&mut self) -> (String, Claims) {
        let body = serde_json::to_vec(&json!({
            "job_id":JOB,"vcpus":1,"platform":"linux-x64","net":false,
            "workspace_key_hash":motor_lease::base64url(&[1;32]),
            "repo_hash":motor_lease::base64url(&[2;32]),"placement_hint":null,"avoid_host":null,
            "timeout_s":30,"requester_jwk":RequesterJwk::from_key(&self.requester.verifying_key())
        }))
        .unwrap();
        let reply = self.broker.handle(
            BrokerRequest {
                method: "POST",
                path: "/v1/leases",
                content_type: Some("application/json"),
                bearer: Some("worker"),
                host_headers: None,
                body: &body,
            },
            unix_ms(),
        );
        assert_eq!(reply.status, 201, "{}", reply.body);
        let lease: LeaseResponse = serde_json::from_value(reply.body).unwrap();
        let claims = motor_lease::verify_token(
            &lease.job_token,
            &self.runtime.keys.read().unwrap(),
            &audience(ID),
            None,
            motor_lease::Purpose::Run,
            unix_ms() / 1000,
        )
        .unwrap();
        (lease.job_token, claims)
    }
    async fn connect(&self) -> (tokio::net::TcpStream, tokio::task::JoinHandle<Result<()>>) {
        let listener = bind(0).await.unwrap();
        assert_eq!(listener.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
        let address = listener.local_addr().unwrap();
        let runtime = self.runtime.clone();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            serve(runtime, socket).await
        });
        (tokio::net::TcpStream::connect(address).await.unwrap(), task)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.runtime.host.lock().unwrap().guest_unready("sandbox");
    }
}

async fn hello(client: &mut tokio::net::TcpStream, token: &str, requester: &SigningKey) -> Value {
    write_frame(client, &json!({"type":"lease_hello","v":1,"token":token,"requester_jwk":RequesterJwk::from_key(&requester.verifying_key())})).await.unwrap();
    read_frame(client).await.unwrap()
}
fn signature(requester: &SigningKey, claims: &Claims, frame: Value) -> String {
    let challenge: Challenge = serde_json::from_value(frame).unwrap();
    motor_lease::base64url(
        &requester
            .sign(&motor_lease::challenge_bytes(&challenge, claims).unwrap())
            .to_bytes(),
    )
}
async fn respond(client: &mut tokio::net::TcpStream, sig: &str) -> Value {
    write_frame(client, &json!({"type":"challenge_response","sig":sig}))
        .await
        .unwrap();
    read_frame(client).await.unwrap()
}
fn refused(reason: &str) -> Value {
    json!({"type":"lease_refused","reason":reason})
}

#[tokio::test]
async fn sandbox_wrong_audience_is_auth_before_challenge() {
    let mut f = Fixture::new();
    let (_, mut c) = f.lease();
    // Leave placement.host equal to this sandbox's identity. A refusal must
    // depend on the wrong aud claim, not on a second wrong placement claim.
    c.aud = "boat:another".into();
    let token = motor_lease::sign_token(&c, "key", &f.signer).unwrap();
    let (mut client, task) = f.connect().await;
    assert_eq!(
        hello(&mut client, &token, &f.requester).await,
        refused("auth")
    );
    task.await.unwrap().unwrap();
    assert!(f.runtime.host.lock().unwrap().snapshot.leases.is_empty());
}

#[tokio::test]
async fn sandbox_expiry_key_platform_placement_and_runtime_refusals_are_exact() {
    for case in 0..8 {
        let mut f = Fixture::new();
        let (_, mut c) = f.lease();
        let mut signer = f.signer.clone();
        match case {
            0 => {
                c.iat -= 301;
                c.exp = c.iat + 300;
            }
            1 => signer = SigningKey::from_bytes(&[9; 32]),
            2 => c.cnf.jkt = motor_lease::base64url(&[4; 32]),
            3 => c.platform = "linux-arm64".into(),
            4 => c.placement.vm = Some("bx_other".into()),
            5 => c.max_s = 0,
            6 => c.mem_mib = 1,
            _ => {}
        }
        let mut token = motor_lease::sign_token(&c, "key", &signer).unwrap();
        if case == 7 {
            let header = motor_lease::base64url(br#"{"alg":"EdDSA","typ":"JWT","kid":"key"}"#);
            let body = token.split('.').nth(1).unwrap();
            let input = format!("{header}.{body}");
            token = format!(
                "{input}.{}",
                motor_lease::base64url(&signer.sign(input.as_bytes()).to_bytes())
            );
        }
        let (mut client, task) = f.connect().await;
        assert_eq!(
            hello(&mut client, &token, &f.requester).await,
            refused(if case == 0 { "expired" } else { "auth" }),
            "case {case}"
        );
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn sandbox_missing_changed_and_unsafe_identity_refuse_auth() {
    let mut f = Fixture::new();
    let (token, _) = f.lease();
    let id = f.dir.path().join("id");
    std::fs::remove_file(&id).unwrap();
    for content in [None, Some("bx_other"), Some(""), Some("bx_test\n")] {
        if let Some(content) = content {
            std::fs::write(&id, content).unwrap();
            std::fs::set_permissions(&id, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        let (mut client, task) = f.connect().await;
        assert_eq!(
            hello(&mut client, &token, &f.requester).await,
            refused("auth")
        );
        task.await.unwrap().unwrap();
        if id.exists() {
            std::fs::remove_file(&id).unwrap();
        }
    }
    std::fs::write(&id, ID).unwrap();
    std::fs::set_permissions(&id, std::fs::Permissions::from_mode(0o444)).unwrap();
    let (mut client, task) = f.connect().await;
    assert_eq!(
        hello(&mut client, &token, &f.requester).await["type"],
        "challenge"
    );
    drop(client);
    assert!(task.await.unwrap().is_err());
    std::fs::remove_file(&id).unwrap();
    std::fs::write(&id, "bx_other").unwrap();
    std::fs::set_permissions(&id, std::fs::Permissions::from_mode(0o444)).unwrap();
    let (mut client, task) = f.connect().await;
    assert_eq!(
        hello(&mut client, &token, &f.requester).await,
        refused("auth")
    );
    task.await.unwrap().unwrap();
    let (_, mut updated) = f.lease();
    updated.aud = audience("bx_other");
    updated.placement.host = updated.aud.clone();
    updated.placement.vm = Some("bx_other".into());
    let fresh = motor_lease::sign_token(&updated, "key", &f.signer).unwrap();
    let (mut client, task) = f.connect().await;
    assert_eq!(
        hello(&mut client, &fresh, &f.requester).await["type"],
        "challenge"
    );
    drop(client);
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn sandbox_replayed_challenge_and_wrong_requester_signature_refuse_auth() {
    let mut f = Fixture::new();
    let (token, c) = f.lease();
    let (mut client, task) = f.connect().await;
    let challenge = hello(&mut client, &token, &f.requester).await;
    let stale = signature(&f.requester, &c, challenge);
    drop(client);
    assert!(task.await.unwrap().is_err());
    for wrong_key in [false, true] {
        let (mut client, task) = f.connect().await;
        let challenge = hello(&mut client, &token, &f.requester).await;
        let sig = if wrong_key {
            signature(&SigningKey::from_bytes(&[9; 32]), &c, challenge)
        } else {
            stale.clone()
        };
        assert_eq!(respond(&mut client, &sig).await, refused("auth"));
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn sandbox_tls_clienthello_is_closed_without_admission() {
    let f = Fixture::new();
    let (mut client, task) = f.connect().await;
    client
        .write_all(&[0x16, 0x03, 0x01, 0x00, 0x20, 0x01, 0x00])
        .await
        .unwrap();
    assert!(read_frame(&mut client).await.is_err());
    assert!(task.await.unwrap().is_err());
    assert!(f.runtime.host.lock().unwrap().snapshot.leases.is_empty());
}

async fn admitted_connection(
    f: &Fixture,
    token: &str,
    c: &Claims,
    key: &SigningKey,
) -> (
    tokio::net::TcpStream,
    tokio::net::UnixStream,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (mut client, task) = f.connect().await;
    let challenge = hello(&mut client, token, key).await;
    assert_eq!(
        respond(&mut client, &signature(key, c, challenge)).await,
        json!({"type":"lease_ok"})
    );
    write_frame(&mut client, &json!({"type":"hello","protocol_version":1}))
        .await
        .unwrap();
    let (mut runner, _) = f.jobs.accept().await.unwrap();
    let verified = read_frame(&mut runner).await.unwrap();
    assert_eq!(verified["type"], "verified_lease");
    assert_eq!(verified["jti"], JOB);
    assert_eq!(
        verified["purpose"],
        serde_json::to_value(c.purpose).unwrap()
    );
    assert_eq!(verified["max_s"], c.max_s);
    assert_eq!(read_frame(&mut runner).await.unwrap()["type"], "hello");
    (client, runner, task)
}

#[tokio::test]
async fn sandbox_valid_admission_orders_verified_lease_and_reattach_binds_fresh_key() {
    let mut f = Fixture::new();
    let (token, c) = f.lease();
    let (client, runner, task) = admitted_connection(&f, &token, &c, &f.requester).await;
    drop(client);
    task.await.unwrap().unwrap();
    drop(runner);
    let requester = SigningKey::from_bytes(&[10; 32]);
    let body = serde_json::to_vec(
        &json!({"requester_jwk":RequesterJwk::from_key(&requester.verifying_key())}),
    )
    .unwrap();
    let reply = f.broker.handle(
        BrokerRequest {
            method: "POST",
            path: &format!("/v1/leases/{JOB}/reattach"),
            content_type: Some("application/json"),
            bearer: Some("worker"),
            host_headers: None,
            body: &body,
        },
        unix_ms(),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let lease: LeaseResponse = serde_json::from_value(reply.body).unwrap();
    let c = motor_lease::verify_token(
        &lease.job_token,
        &f.runtime.keys.read().unwrap(),
        &audience(ID),
        None,
        motor_lease::Purpose::Attach,
        unix_ms() / 1000,
    )
    .unwrap();
    let (mut client, mut runner, task) =
        admitted_connection(&f, &lease.job_token, &c, &requester).await;
    write_frame(
        &mut client,
        &json!({"type":"attach","job_id":JOB,"from_seq":0}),
    )
    .await
    .unwrap();
    assert_eq!(read_frame(&mut runner).await.unwrap()["type"], "attach");
    write_frame(
        &mut runner,
        &json!({"type":"terminal","job_id":JOB,"outcome":"cancelled"}),
    )
    .await
    .unwrap();
    assert_eq!(read_frame(&mut client).await.unwrap()["type"], "terminal");
    // The host now waits for the client to close after its last frame.
    drop(client);
    task.await.unwrap().unwrap();
    let host = f.runtime.host.lock().unwrap();
    assert!(host.snapshot.billing.is_empty());
    assert!(host.snapshot.outbox.is_empty());
    assert_eq!(host.snapshot.counter, 0);
}

#[tokio::test]
async fn sandbox_capacity_and_host_full_keep_host_refusal_types() {
    for reason in ["capacity", "host_full"] {
        let mut f = Fixture::new();
        let (token, c) = f.lease();
        if reason == "capacity" {
            f.runtime
                .host
                .lock()
                .unwrap()
                .set_stop_pending("sandbox", true)
                .unwrap();
        } else {
            f.runtime
                .host
                .lock()
                .unwrap()
                .tenants
                .get_mut("sandbox")
                .unwrap()
                .vcpu_ceiling = 0;
        }
        let (mut client, task) = f.connect().await;
        let challenge = hello(&mut client, &token, &f.requester).await;
        assert_eq!(
            respond(&mut client, &signature(&f.requester, &c, challenge)).await,
            refused(reason)
        );
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn sandbox_max_s_cancels_through_runner_after_requester_disconnects() {
    let mut f = Fixture::new();
    let admin = tokio::net::UnixListener::bind(f.dir.path().join("a")).unwrap();
    let (token, c) = f.lease();
    let (client, runner, task) = admitted_connection(&f, &token, &c, &f.requester).await;
    drop(client);
    task.await.unwrap().unwrap();
    drop(runner);
    let now = unix_ms();
    {
        let mut host = f.runtime.host.lock().unwrap();
        for (seq, event) in [
            (1, motor_protocol::TenantEventKind::Admitted),
            (2, motor_protocol::TenantEventKind::Started),
        ] {
            host.event(
                "sandbox",
                &motor_protocol::TenantEvent {
                    event_seq: seq,
                    event,
                    jti: Some(JOB.parse().unwrap()),
                    cause: None,
                    setup: None,
                },
                now,
            )
            .unwrap();
        }
    }
    f.runtime.tick(now + c.max_s * 1000 - 1).unwrap();
    assert!(f
        .runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .pending_cancels
        .is_empty());
    let runtime = f.runtime.clone();
    let tick = tokio::task::spawn_blocking(move || runtime.tick(now + c.max_s * 1000));
    let (mut socket, _) = admin.accept().await.unwrap();
    assert_eq!(
        read_frame(&mut socket).await.unwrap(),
        json!({"op":"cancel","job_id":JOB})
    );
    write_frame(&mut socket, &json!({"ok":true})).await.unwrap();
    tick.await.unwrap().unwrap();
    let host = f.runtime.host.lock().unwrap();
    assert!(host.snapshot.outbox.is_empty());
    assert!(host.snapshot.billing.is_empty());
}

#[tokio::test]
async fn sandbox_identity_reader_rejects_wrong_owner_writable_files_and_symlinks() {
    let f = Fixture::new();
    let path = f.dir.path().join("id");
    let owner = unsafe { libc::geteuid() };
    assert!(IdentityFile {
        path: path.clone(),
        owner: owner + 1
    }
    .read()
    .is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(IdentityFile {
        path: path.clone(),
        owner
    }
    .read()
    .is_err());
    let link = f.dir.path().join("link");
    std::os::unix::fs::symlink(path, &link).unwrap();
    assert!(IdentityFile { path: link, owner }.read().is_err());
}

/// Each refusal names the check that failed: the endpoint logs it, while the
/// requester still receives only the wire reason.
#[tokio::test]
async fn sandbox_refusals_name_the_failing_check() {
    let mut f = Fixture::new();
    let (_, mut c) = f.lease();
    let jwk = RequesterJwk::from_key(&f.requester.verifying_key());
    let now = unix_ms() / 1000;
    let check = |token: &str| {
        f.runtime
            .authenticate(token, &jwk, now)
            .map(|_| ())
            .map_err(|r| r.check)
    };
    // Boat tokens state the sandbox size's usable memory: 13312 MiB for the
    // 8-vCPU size, below the fleet formula's 28672 MiB.
    c.vcpus = 8;
    c.mem_mib = 13312;
    let boat = motor_lease::sign_token(&c, "key", &f.signer).unwrap();
    assert_eq!(check(&boat), Ok(()));
    c.mem_mib = motor_lease::memory_max_mib(8) + 1;
    let too_much = motor_lease::sign_token(&c, "key", &f.signer).unwrap();
    assert_eq!(check(&too_much), Err("mem_mib"));
    let unknown = motor_lease::sign_token(&c, "other", &f.signer).unwrap();
    assert_eq!(check(&unknown), Err("kid"));
    let other = RequesterJwk::from_key(&SigningKey::from_bytes(&[9; 32]).verifying_key());
    assert_eq!(
        f.runtime
            .authenticate(&boat, &other, now)
            .map(|_| ())
            .map_err(|r| r.check),
        Err("requester_key")
    );
    std::fs::remove_file(f.dir.path().join("id")).unwrap();
    assert_eq!(check(&boat), Err("identity"));
}

/// The readiness marker the broker polls holds exactly the sandbox id, only
/// while broker keys are loaded and the identity file is valid, and follows a
/// rewritten identity file instead of keeping a stale id.
// The fixture binds a tokio Unix listener, so it needs a runtime.
#[tokio::test]
async fn sandbox_ready_marker_tracks_keys_and_identity() {
    let f = Fixture::new();
    let marker = f.dir.path().join("endpoint-ready");
    let id = f.dir.path().join("id");
    let keys = std::mem::take(&mut *f.runtime.keys.write().unwrap());
    publish_readiness(&f.runtime, &marker, None).unwrap();
    assert!(!marker.exists(), "no marker before a JWKS is loaded");
    *f.runtime.keys.write().unwrap() = keys;
    publish_readiness(&f.runtime, &marker, None).unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), ID);
    let mode = std::fs::metadata(&marker).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o644);
    std::fs::remove_file(&id).unwrap();
    publish_readiness(&f.runtime, &marker, None).unwrap();
    assert!(!marker.exists(), "a missing identity withdraws readiness");
    std::fs::write(&id, "bx_other").unwrap();
    std::fs::set_permissions(&id, std::fs::Permissions::from_mode(0o644)).unwrap();
    publish_readiness(&f.runtime, &marker, None).unwrap();
    assert!(
        !marker.exists(),
        "an identity file with the wrong mode is not ready"
    );
    std::fs::set_permissions(&id, std::fs::Permissions::from_mode(0o444)).unwrap();
    publish_readiness(&f.runtime, &marker, None).unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "bx_other");
    assert!(!marker.with_extension("tmp").exists());
}

fn storage_gate(f: &Fixture) -> StorageHealthGate {
    let state = f.dir.path().join("storage-state");
    let users = f.dir.path().join("storage-users");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&users).unwrap();
    let config = StorageHealthConfig {
        state_file: f.dir.path().join("storage-health.json"),
        lock_file: f.dir.path().join("storage-health.lock"),
        identity_paths: [state, users],
    };
    std::fs::write(&config.lock_file, "").unwrap();
    std::fs::set_permissions(&config.lock_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let gate = StorageHealthGate::new(config, unsafe { libc::geteuid() }).unwrap();
    write_storage_health(&gate, "healthy", Some(gate.identity));
    gate
}

fn write_storage_health(gate: &StorageHealthGate, status: &str, identity: Option<[u64; 3]>) {
    std::fs::write(
        &gate.config.state_file,
        serde_json::to_vec(&serde_json::json!({"status": status, "identity": identity,
            "expires_monotonic_ns": monotonic_ns().unwrap() + 5_000_000_000_u64}))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(
        &gate.config.state_file,
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}

#[tokio::test]
async fn sandbox_storage_withdrawal_prevents_endpoint_republication() {
    let f = Fixture::new();
    let gate = storage_gate(&f);
    let marker = f.dir.path().join("endpoint-ready");
    publish_readiness(&f.runtime, &marker, Some(&gate)).unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), ID);
    {
        let _lock = gate.lock().unwrap();
        write_storage_health(&gate, "unhealthy", None);
        std::fs::remove_file(&marker).unwrap();
    }
    for _ in 0..3 {
        publish_readiness(&f.runtime, &marker, Some(&gate)).unwrap();
        assert!(
            !marker.exists(),
            "valid endpoint keys cannot override storage withdrawal"
        );
    }
}

#[tokio::test]
async fn sandbox_storage_missing_health_withdraws_instead_of_publishing() {
    let f = Fixture::new();
    let gate = storage_gate(&f);
    let marker = f.dir.path().join("endpoint-ready");
    std::fs::write(&marker, ID).unwrap();
    std::fs::remove_file(&gate.config.state_file).unwrap();
    assert!(publish_readiness(&f.runtime, &marker, Some(&gate)).is_err());
    assert!(!marker.exists(), "missing watcher state is not healthy");
}

#[tokio::test]
async fn sandbox_storage_identity_must_match_endpoint_startup() {
    let f = Fixture::new();
    let gate = storage_gate(&f);
    let marker = f.dir.path().join("endpoint-ready");
    let mut changed = gate.identity;
    changed[2] += 1;
    write_storage_health(&gate, "healthy", Some(changed));
    publish_readiness(&f.runtime, &marker, Some(&gate)).unwrap();
    assert!(
        !marker.exists(),
        "health for another workspace identity cannot admit this endpoint"
    );
}

#[tokio::test]
async fn sandbox_storage_expired_health_cannot_recreate_readiness() {
    let f = Fixture::new();
    let gate = storage_gate(&f);
    let marker = f.dir.path().join("endpoint-ready");
    publish_readiness(&f.runtime, &marker, Some(&gate)).unwrap();
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&gate.config.state_file).unwrap()).unwrap();
    state["expires_monotonic_ns"] = serde_json::json!(0);
    std::fs::write(&gate.config.state_file, serde_json::to_vec(&state).unwrap()).unwrap();
    publish_readiness(&f.runtime, &marker, Some(&gate)).unwrap();
    assert!(
        !marker.exists(),
        "stale health cannot outlive a dead storage watcher"
    );
}

#[tokio::test]
async fn sandbox_storage_withdrawal_serializes_with_concurrent_publication() {
    let f = Fixture::new();
    let gate = Arc::new(storage_gate(&f));
    let marker = f.dir.path().join("endpoint-ready");
    publish_readiness(&f.runtime, &marker, Some(&gate)).unwrap();
    let lock = gate.lock().unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer_gate = gate.clone();
    let writer_marker = marker.clone();
    let runtime = f.runtime.clone();
    let writer = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        done_tx
            .send(publish_readiness(
                &runtime,
                &writer_marker,
                Some(&writer_gate),
            ))
            .unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "publisher must wait for the storage lock"
    );
    write_storage_health(&gate, "unhealthy", None);
    std::fs::remove_file(&marker).unwrap();
    drop(lock);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    writer.join().unwrap();
    assert!(
        !marker.exists(),
        "a queued publisher must read health after acquiring the lock"
    );
}

#[tokio::test]
async fn sandbox_storage_unsafe_or_malformed_health_is_not_ready() {
    let f = Fixture::new();
    let gate = storage_gate(&f);
    let marker = f.dir.path().join("endpoint-ready");
    for malformed in [false, true] {
        std::fs::write(&marker, ID).unwrap();
        if malformed {
            std::fs::write(&gate.config.state_file, "not json").unwrap();
            std::fs::set_permissions(
                &gate.config.state_file,
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        } else {
            std::fs::set_permissions(
                &gate.config.state_file,
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        assert!(publish_readiness(&f.runtime, &marker, Some(&gate)).is_err());
        assert!(!marker.exists());
    }
    std::fs::remove_file(&gate.config.lock_file).unwrap();
    assert!(publish_readiness(&f.runtime, &marker, Some(&gate)).is_err());
    assert!(
        !marker.exists(),
        "the endpoint never creates a missing watcher lock"
    );
    assert!(!gate.config.lock_file.exists());
}
