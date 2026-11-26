use super::*;
use ed25519_dalek::{Signer, SigningKey};
use motor_lease::{Jwks, JwksKey, RequesterJwk};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[tokio::test]
async fn finished_job_drains_a_late_heartbeat_and_closes_tls_cleanly() {
    let (runtime, fake, _storage) = fixture();
    *fake.terminal_after_run.lock().unwrap() = Some(json!({
        "type":"terminal", "job_id":JOB, "outcome":{"type":"exit","code":0},
        "wall_ms":1, "queue_wait_ms":0, "bundle_bytes":0, "ran":"remote",
        "tree_hash":null, "workspace_changes":null
    }));
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
    let requester_jwk = RequesterJwk::from_key(&requester.verifying_key());
    let mut claims = claims(JOB);
    claims.mem_mib = 16384;
    claims.mem_burst_mib = Some(8192);
    runtime
        .host
        .lock()
        .unwrap()
        .record_host_resources(HostResources {
            at_ms: unix_ms(),
            job_vm_headroom_mib: u64::MAX,
            available_mem_mib: 100_000,
            vcpu_ceiling: 2,
            permit_budget: 4,
        });
    claims.cnf.jkt = requester_jwk.thumbprint().unwrap();
    let token = motor_lease::sign_token(&claims, "key", &broker).unwrap();
    runtime
        .authenticate(&token, &requester_jwk, unix_ms() / 1000)
        .unwrap();

    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut tls = TlsAcceptor::from(Arc::new(config))
            .accept(socket)
            .await
            .unwrap();
        super::super::super::serve_with_slot(runtime, &mut tls, None)
            .await
            .unwrap();
        // If the handler returns without draining, wait for the heartbeat to
        // reach the kernel before dropping the socket. Peeking doesn't consume
        // it, so the reset doesn't depend on which task Linux schedules first.
        tls.get_ref().0.peek(&mut [0]).await.unwrap();
    });

    tokio::time::timeout(Duration::from_secs(10), async {
        let mut client = connector
            .connect(
                rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(address).await.unwrap(),
            )
            .await
            .unwrap();
        write_frame(
            &mut client,
            &json!({"type":"lease_hello","v":1,"token":token,"requester_jwk":requester_jwk}),
        )
        .await
        .unwrap();
        let challenge: Challenge =
            serde_json::from_value(read_frame(&mut client).await.unwrap()).unwrap();
        let sig = motor_lease::base64url(
            &requester
                .sign(&motor_lease::challenge_bytes(&challenge, &claims).unwrap())
                .to_bytes(),
        );
        write_frame(&mut client, &json!({"type":"challenge_response","sig":sig}))
            .await
            .unwrap();
        queued(&mut client, JOB).await;
        assert_eq!(next_type(&mut client, "output").await["seq"], 1);
        let terminal = next_type(&mut client, "terminal").await;
        assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
        assert_eq!(
            next_type(&mut client, "hosted_terminal").await["job_id"],
            JOB
        );
        write_frame(&mut client, &json!({"type":"heartbeat"}))
            .await
            .unwrap();
        assert_eq!(
            client.read(&mut [0]).await.expect("clean TLS close"),
            0,
            "nothing follows the settlement frame"
        );
        assert!(!task.is_finished(), "host has not drained the client yet");
        client.shutdown().await.unwrap();
        task.await.unwrap();
    })
    .await
    .expect("job and TLS shutdown completed");
}
