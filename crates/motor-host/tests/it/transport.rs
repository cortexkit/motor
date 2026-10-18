use super::*;
use anyhow::{bail, Result};
use ed25519_dalek::{Signer, SigningKey};
use motor_host::helper::{write_json, Control};
use motor_host::server::{read_frame, serve, unix_ms, write_frame, Runtime};
use motor_lease::{Challenge, Jwks, JwksKey, RequesterJwk};
use serde_json::{json, Value};
use std::os::unix::net::UnixStream;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    peers: Mutex<Vec<UnixStream>>,
}
impl Control for Fake {
    fn adopt(&self, _: &str) -> Result<bool> {
        Ok(false)
    }
    fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
        self.calls.lock().unwrap().push("wake".into());
        Ok(1)
    }
    fn quota(&self, _: &str, vcpus: u32) -> Result<()> {
        self.calls.lock().unwrap().push(format!("quota:{vcpus}"));
        Ok(())
    }
    fn relay(&self, _: &str, port: u32) -> Result<UnixStream> {
        self.calls.lock().unwrap().push(format!("relay:{port}"));
        if port != 52 {
            bail!("no lifecycle stream in this fixture");
        }
        let (a, b) = UnixStream::pair()?;
        self.peers.lock().unwrap().push(b);
        Ok(a)
    }
    fn admin(&self, _: &str, request: &Value) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("admin:{}", request["op"].as_str().unwrap()));
        Ok(json!({"ok":true}))
    }
    fn suspend(&self, _: &str) -> Result<()> {
        self.calls.lock().unwrap().push("suspend".into());
        Ok(())
    }
    fn available_bytes(&self) -> Result<u64> {
        Ok(u64::MAX)
    }
}
fn fixture() -> (Arc<Runtime>, Arc<Fake>, SigningKey, SigningKey, Claims) {
    let fake = Arc::new(Fake::default());
    let runtime = Runtime::new("host".into(), host(), fake.clone());
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
    c.iat = unix_ms() / 1000;
    c.exp = c.iat + 300;
    c.cnf.jkt = RequesterJwk::from_key(&requester.verifying_key())
        .thumbprint()
        .unwrap();
    (runtime, fake, broker, requester, c)
}
async fn hello(
    client: &mut (impl AsyncRead + AsyncWrite + Unpin),
    broker: &SigningKey,
    requester: &SigningKey,
    c: &Claims,
) -> Value {
    let token = motor_lease::sign_token(c, "key", broker).unwrap();
    write_frame(client,&json!({"type":"lease_hello","v":1,"token":token,"requester_jwk":RequesterJwk::from_key(&requester.verifying_key())})).await.unwrap();
    read_frame(client).await.unwrap()
}
async fn respond(
    client: &mut (impl AsyncRead + AsyncWrite + Unpin),
    requester: &SigningKey,
    c: &Claims,
    frame: Value,
) -> Value {
    let challenge: Challenge = serde_json::from_value(frame).unwrap();
    let sig = motor_lease::base64url(
        &requester
            .sign(&motor_lease::challenge_bytes(&challenge, c).unwrap())
            .to_bytes(),
    );
    write_frame(client, &json!({"type":"challenge_response","sig":sig}))
        .await
        .unwrap();
    read_frame(client).await.unwrap()
}
async fn peer(fake: &Fake) -> UnixStream {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(stream) = fake.peers.lock().unwrap().pop() {
                return stream;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn invalid_signature_never_wakes_or_attaches_and_refuses_before_lease_ok() {
    let (runtime, fake, broker, requester, c) = fixture();
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime, server));
    let frame = hello(&mut client, &broker, &requester, &c).await;
    assert_eq!(frame["type"], "challenge");
    let result = respond(&mut client, &SigningKey::from_bytes(&[9; 32]), &c, frame).await;
    assert_eq!(result, json!({"type":"lease_refused","reason":"auth"}));
    task.await.unwrap().unwrap();
    assert!(fake.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn unknown_vm_wrong_sub_and_wrong_requester_are_auth_before_challenge_with_no_usage() {
    for case in 0..3 {
        let (runtime, fake, broker, requester, mut c) = fixture();
        if case == 0 {
            c.placement.vm = "unknown".into();
        }
        if case == 1 {
            c.sub = "org:01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
        }
        if case == 2 {
            c.cnf.jkt = motor_lease::base64url(&[4; 32]);
        }
        let (mut client, server) = tokio::io::duplex(8192);
        let task = tokio::spawn(serve(runtime.clone(), server));
        assert_eq!(
            hello(&mut client, &broker, &requester, &c).await,
            json!({"type":"lease_refused","reason":"auth"})
        );
        task.await.unwrap().unwrap();
        assert!(fake.calls.lock().unwrap().is_empty());
        assert!(runtime.host.lock().unwrap().snapshot.leases.is_empty());
        assert!(runtime.host.lock().unwrap().snapshot.zero_usage.is_empty());
    }
}
#[tokio::test]
async fn unknown_prelude_type_and_version_close_without_reply() {
    for first in [
        json!({"type":"hello"}),
        json!({"type":"lease_hello","v":2,"token":"x","requester_jwk":{"kty":"OKP","crv":"Ed25519","x":"x"}}),
    ] {
        let (runtime, fake, _, _, _) = fixture();
        let (mut client, server) = tokio::io::duplex(8192);
        let task = tokio::spawn(serve(runtime, server));
        write_frame(&mut client, &first).await.unwrap();
        assert!(read_frame(&mut client).await.is_err());
        assert!(task.await.unwrap().is_err());
        assert!(fake.calls.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn close_after_lease_ok_before_delivery_releases_without_refused_event() {
    let (runtime, fake, broker, requester, c) = fixture();
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime.clone(), server));
    let challenge = hello(&mut client, &broker, &requester, &c).await;
    assert_eq!(
        respond(&mut client, &requester, &c, challenge).await["type"],
        "lease_ok"
    );
    drop(client);
    assert!(task.await.unwrap().is_err());
    let host = runtime.host.lock().unwrap();
    assert_eq!(host.share("tenant", unix_ms()), 0);
    assert!(host.snapshot.zero_usage.is_empty());
    assert!(fake.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn verified_lease_is_first_and_watermark_is_durable_before_output_is_visible() {
    let (runtime, fake, broker, requester, c) = fixture();
    let directory = scratch();
    *runtime.host.lock().unwrap() = disk_host(directory.path());
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime.clone(), server));
    let challenge = hello(&mut client, &broker, &requester, &c).await;
    assert_eq!(
        respond(&mut client, &requester, &c, challenge).await["type"],
        "lease_ok"
    );
    write_frame(&mut client, &json!({"type":"hello"}))
        .await
        .unwrap();
    let guest = peer(&fake).await;
    guest.set_nonblocking(true).unwrap();
    let mut guest = tokio::net::UnixStream::from_std(guest).unwrap();
    let verified = read_frame(&mut guest).await.unwrap();
    assert_eq!(verified["type"], "verified_lease");
    assert_eq!(verified["jti"], JOB);
    assert_eq!(
        read_frame(&mut guest).await.unwrap(),
        json!({"type":"hello"})
    );
    let (_, journal) = FileJournal::open(directory.path()).unwrap();
    assert!(journal.leases[JOB].delivered);
    write_frame(
        &mut guest,
        &json!({"type":"output","job_id":JOB,"seq":17,"data":"aGVsbG8="}),
    )
    .await
    .unwrap();
    let output = read_frame(&mut client).await.unwrap();
    assert_eq!(output["seq"], 17);
    let (_, journal) = FileJournal::open(directory.path()).unwrap();
    assert_eq!(journal.leases[JOB].highest_seq, 17);
    drop(client);
    task.await.unwrap().unwrap();
    runtime.host.lock().unwrap().guest_unready("tenant");
}
#[tokio::test]
async fn journal_failure_blocks_sequenced_client_frame() {
    struct FailWatermark;
    impl Journal for FailWatermark {
        fn store(&mut self, snapshot: &Snapshot) -> std::io::Result<()> {
            if snapshot.leases.values().any(|l| l.highest_seq > 0) {
                return Err(std::io::Error::other("fsync failed"));
            }
            Ok(())
        }
    }
    let (runtime, fake, broker, requester, c) = fixture();
    *runtime.host.lock().unwrap() =
        Host::new(tenants(), Snapshot::default(), Box::new(FailWatermark));
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime.clone(), server));
    let challenge = hello(&mut client, &broker, &requester, &c).await;
    respond(&mut client, &requester, &c, challenge).await;
    write_frame(&mut client, &json!({"type":"hello"}))
        .await
        .unwrap();
    let mut guest = peer(&fake).await;
    write_json(
        &mut guest,
        &json!({"type":"output","job_id":JOB,"seq":1,"data":""}),
    )
    .unwrap();
    assert!(read_frame(&mut client).await.is_err());
    assert!(task.await.unwrap().is_err());
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.leases[JOB].highest_seq,
        0
    );
    runtime.host.lock().unwrap().guest_unready("tenant");
}
#[test]
fn admin_only_relays_four_closed_control_operations() {
    let (runtime, fake, _, _, _) = fixture();
    for request in [
        json!({"op":"drain","deadline_s":30,"exit_after":false}),
        json!({"op":"resume"}),
        json!({"op":"cancel","job_id":JOB}),
        json!({"op":"status"}),
    ] {
        assert_eq!(
            runtime.admin("tenant", &request).unwrap(),
            json!({"ok":true})
        );
    }
    assert!(runtime
        .admin("tenant", &json!({"op":"attach_tenant"}))
        .is_err());
    assert!(runtime.admin("unknown", &json!({"op":"status"})).is_err());
    assert_eq!(fake.calls.lock().unwrap().len(), 4);
}
#[tokio::test]
async fn tls_listener_runs_the_pinned_requester_bound_prelude() {
    let (runtime, fake, broker, requester, c) = fixture();
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = tokio_rustls::TlsAcceptor::from(Arc::new(server_config))
            .accept(tcp)
            .await
            .unwrap();
        serve(runtime, tls).await
    });
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut client = tokio_rustls::TlsConnector::from(Arc::new(client_config))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tcp,
        )
        .await
        .unwrap();
    let challenge = hello(&mut client, &broker, &requester, &c).await;
    assert_eq!(
        respond(&mut client, &requester, &c, challenge).await["type"],
        "lease_ok"
    );
    drop(client);
    assert!(task.await.unwrap().is_err());
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn warm_admission_applies_share_before_relay_and_third_run_keeps_quota_unchanged() {
    let (runtime, fake, broker, requester, c) = fixture();
    let mut clients = Vec::new();
    let mut tasks = Vec::new();
    for job in [JOB, JOB2] {
        let mut c = c.clone();
        c.jti = job.into();
        let (mut client, server) = tokio::io::duplex(8192);
        tasks.push(tokio::spawn(serve(runtime.clone(), server)));
        let challenge = hello(&mut client, &broker, &requester, &c).await;
        assert_eq!(
            respond(&mut client, &requester, &c, challenge).await["type"],
            "lease_ok"
        );
        write_frame(&mut client, &json!({"type":"hello"}))
            .await
            .unwrap();
        let guest = peer(&fake).await;
        guest.set_nonblocking(true).unwrap();
        let mut guest = tokio::net::UnixStream::from_std(guest).unwrap();
        assert_eq!(
            read_frame(&mut guest).await.unwrap()["type"],
            "verified_lease"
        );
        clients.push((client, guest));
    }
    let quotas = || {
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("quota:"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(quotas(), ["quota:1", "quota:2"]);
    let mut third = c.clone();
    third.jti = JOB3.into();
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime.clone(), server));
    let challenge = hello(&mut client, &broker, &requester, &third).await;
    assert_eq!(
        respond(&mut client, &requester, &third, challenge).await,
        json!({"type":"lease_refused","reason":"host_full"})
    );
    task.await.unwrap().unwrap();
    assert_eq!(quotas(), ["quota:1", "quota:2"]);
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|s| *s == "relay:52")
            .count(),
        2
    );
    let guests: Vec<_> = clients
        .into_iter()
        .map(|(client, guest)| {
            drop(client);
            guest
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    drop(guests);
    runtime.host.lock().unwrap().guest_unready("tenant");
}

async fn waiting_session(
    vcpus: u32,
) -> (
    Arc<Runtime>,
    SigningKey,
    SigningKey,
    Claims,
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<Result<()>>,
) {
    use motor_host::state::ConnectionState;
    let (runtime, _, broker, requester, mut c) = fixture();
    c.vcpus = vcpus;
    c.mem_mib = motor_lease::memory_max_mib(vcpus);
    let mut old_updates = {
        let mut host = runtime.host.lock().unwrap();
        let (old, _) = host.claim(c.clone(), unix_ms()).unwrap();
        host.delivered(old).unwrap();
        host.watch_connection(old).unwrap()
    };
    c.iat += 1;
    c.exp += 1;
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime.clone(), server));
    let challenge: Challenge =
        serde_json::from_value(hello(&mut client, &broker, &requester, &c).await).unwrap();
    let sig = motor_lease::sign_challenge(&challenge, &c, &requester).unwrap();
    write_frame(
        &mut client,
        &json!({"type":"challenge_response", "sig":sig}),
    )
    .await
    .unwrap();
    while *old_updates.borrow_and_update() != ConnectionState::Closed {
        old_updates.changed().await.unwrap();
    }
    (runtime, broker, requester, c, client, task)
}

#[tokio::test]
async fn waiting_refusal_transition_wakes_admission_without_advancing_clock() {
    let (runtime, _, _, c, mut client, task) = waiting_session(1).await;
    tokio::time::pause();
    let before = tokio::time::Instant::now();
    runtime
        .host
        .lock()
        .unwrap()
        .event(
            "tenant",
            &event(
                1,
                TenantEventKind::Refused,
                Some(JOB),
                Some(RefusedCause::Disconnect),
            ),
            unix_ms(),
        )
        .unwrap();
    assert_eq!(read_frame(&mut client).await.unwrap()["type"], "lease_ok");
    assert_eq!(tokio::time::Instant::now(), before);
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.leases[JOB].claims.iat,
        c.iat
    );
    drop(client);
    assert!(task.await.unwrap().is_err());
}

async fn waiting_refusal(reason: motor_host::state::Refusal) {
    use motor_host::state::Refusal;
    let (runtime, _, _, c, mut client, task) = waiting_session(2).await;
    tokio::time::pause();
    let before = tokio::time::Instant::now();
    {
        let mut host = runtime.host.lock().unwrap();
        match reason {
            Refusal::Capacity => host.set_stop_pending("tenant", true).unwrap(),
            Refusal::HostFull => {
                host.tenants.get_mut("vm").unwrap().vcpu_ceiling = 1;
                host.event(
                    "tenant",
                    &event(
                        1,
                        TenantEventKind::Refused,
                        Some(JOB),
                        Some(RefusedCause::Disconnect),
                    ),
                    unix_ms(),
                )
                .unwrap();
            }
            Refusal::Expired => {
                host.expire(c.exp * 1000).unwrap();
            }
            Refusal::Auth => {
                host.event(
                    "tenant",
                    &event(1, TenantEventKind::Admitted, Some(JOB), None),
                    unix_ms(),
                )
                .unwrap();
            }
        }
    }
    assert_eq!(
        read_frame(&mut client).await.unwrap(),
        json!({"type":"lease_refused", "reason":reason.wire()})
    );
    assert_eq!(tokio::time::Instant::now(), before);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn waiting_capacity_refusal_preserves_retryable_reason_without_polling() {
    waiting_refusal(motor_host::state::Refusal::Capacity).await;
}
#[tokio::test]
async fn waiting_host_full_refusal_preserves_retryable_reason_without_polling() {
    waiting_refusal(motor_host::state::Refusal::HostFull).await;
}
#[tokio::test]
async fn waiting_expiry_refusal_preserves_expired_reason_without_polling() {
    waiting_refusal(motor_host::state::Refusal::Expired).await;
}
#[tokio::test]
async fn waiting_admitted_original_refuses_replacement_as_auth_without_polling() {
    waiting_refusal(motor_host::state::Refusal::Auth).await;
}

#[tokio::test]
async fn expired_removed_waiting_lease_does_not_poison_host_or_block_second_connection() {
    use motor_host::state::{Error, Refusal};
    let (runtime, broker, requester, c, mut client, task) = waiting_session(1).await;
    {
        let mut host = runtime.host.lock().unwrap();
        host.expire((c.exp + 60) * 1000).unwrap();
        host.snapshot.leases.remove(JOB);
        assert!(matches!(
            host.pending(JOB),
            Err(Error::Refused(Refusal::Auth))
        ));
    }
    assert_eq!(
        read_frame(&mut client).await.unwrap(),
        json!({"type":"lease_refused", "reason":"expired"})
    );
    task.await.unwrap().unwrap();
    assert!(!runtime.host.is_poisoned());
    let mut fresh = c;
    fresh.jti = JOB2.into();
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime.clone(), server));
    let challenge = hello(&mut client, &broker, &requester, &fresh).await;
    assert_eq!(
        respond(&mut client, &requester, &fresh, challenge).await["type"],
        "lease_ok"
    );
    drop(client);
    assert!(task.await.unwrap().is_err());
    assert!(!runtime.host.is_poisoned());
}

#[tokio::test]
async fn waiting_lease_uses_expiry_timer_without_host_tick() {
    let (_, _, _, c, mut client, task) = waiting_session(1).await;
    tokio::time::pause();
    let until_expiry = std::time::Duration::from_millis(c.exp * 1000 - unix_ms());
    tokio::time::advance(until_expiry + std::time::Duration::from_millis(1)).await;
    assert_eq!(
        read_frame(&mut client).await.unwrap(),
        json!({"type":"lease_refused", "reason":"expired"})
    );
    task.await.unwrap().unwrap();
}
