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
            "repo_hash":motor_lease::base64url(&[2;32]),"placement_hint":null,
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
