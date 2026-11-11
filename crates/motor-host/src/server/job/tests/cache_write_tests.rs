use super::*;

#[tokio::test]
async fn boot_forwards_only_eligible_grant_selector_and_the_persisted_deny() {
    for (claim_value, blocked) in [
        (None, false),
        (Some(false), false),
        (Some(true), false),
        (Some(true), true),
    ] {
        let (runtime, fake, _storage) = fixture();
        hold_boot(&runtime, &fake, "unused");
        let mut c = claims(JOB);
        c.cache_write = claim_value;
        let id = claim(&runtime, &c);
        runtime
            .host
            .lock()
            .unwrap()
            .set_remote_cache_write_block(blocked)
            .unwrap();
        let (mut client, task) = start(runtime.clone(), c, id).await;
        queued(&mut client, JOB).await;
        next_type(&mut client, "output").await;
        let expected = (claim_value == Some(true) && !blocked).then(|| JOB.to_owned());
        assert_eq!(*fake.workspace_grant_selectors.lock().unwrap(), [expected]);
        let starts = fake.job_starts.lock().unwrap().clone();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].job_id, JOB);
        assert_eq!(starts[0].kind, motor_vm_helper::job::VmKind::Job);
        assert_eq!(starts[0].cache_write_blocked, blocked);
        drop(client);
        task.await.unwrap().unwrap();
        runtime.stop_job(JOB, true, false).unwrap();
    }
}
use ed25519_dalek::{Signer, SigningKey};
use motor_lease::{Jwks, JwksKey, RequesterJwk};

fn signed_claims(runtime: &Runtime) -> (SigningKey, SigningKey, Claims) {
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
    c.cnf.jkt = RequesterJwk::from_key(&requester.verifying_key())
        .thumbprint()
        .unwrap();
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
    (broker, requester, c)
}

async fn challenge(
    runtime: Arc<Runtime>,
    fake: &Fake,
    c: &Claims,
    broker: &SigningKey,
    requester: &SigningKey,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<Result<()>>,
    String,
) {
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(super::super::super::serve(runtime, server));
    let token = motor_lease::sign_token(c, "key", broker).unwrap();
    write_frame(&mut client,&json!({"type":"lease_hello","v":1,"token":token,"requester_jwk":RequesterJwk::from_key(&requester.verifying_key())})).await.unwrap();
    let challenge: Challenge =
        serde_json::from_value(read_frame(&mut client).await.unwrap()).unwrap();
    assert!(fake.cache_write_tokens.lock().unwrap().is_empty());
    let sig = motor_lease::base64url(
        &requester
            .sign(&motor_lease::challenge_bytes(&challenge, c).unwrap())
            .to_bytes(),
    );
    write_frame(&mut client, &json!({"type":"challenge_response","sig":sig}))
        .await
        .unwrap();
    (client, task, token)
}

#[tokio::test]
async fn first_true_claim_sends_raw_token_before_lease_ok_even_if_helper_refuses() {
    for refuse in [false, true] {
        let (runtime, fake, _storage) = fixture();
        let (broker, requester, mut c) = signed_claims(&runtime);
        c.cache_write = Some(true);
        fake.fail_cache_write.store(refuse, Ordering::SeqCst);
        *fake.hang.lock().unwrap() = "cache-write";
        let entered = fake.entered.notified();
        let (mut client, task, token) =
            challenge(runtime.clone(), &fake, &c, &broker, &requester).await;
        tokio::time::timeout(Duration::from_secs(60), entered)
            .await
            .unwrap();
        // The helper has received the token but has not answered. The client
        // cannot yet have an acknowledgement, regardless of executor load.
        let pending = std::future::poll_fn(|cx| {
            use std::pin::Pin;
            let mut byte = [0];
            let mut buf = tokio::io::ReadBuf::new(&mut byte);
            match Pin::new(&mut client).poll_read(cx, &mut buf) {
                std::task::Poll::Pending => std::task::Poll::Ready(true),
                std::task::Poll::Ready(_) => std::task::Poll::Ready(false),
            }
        })
        .await;
        let tokens = fake.cache_write_tokens.lock().unwrap().clone();
        *fake.gate.0.lock().unwrap() = true;
        fake.gate.1.notify_all();
        assert!(
            pending,
            "lease_ok arrived before helper admission completed"
        );
        assert_eq!(tokens, [token]);
        assert_eq!(read_frame(&mut client).await.unwrap()["type"], "lease_ok");
        drop(client);
        let _ = task.await.unwrap();
    }
}

#[tokio::test]
async fn false_absent_or_blocked_first_claim_never_requests_a_helper_grant() {
    for (claim, blocked) in [(None, false), (Some(false), false), (Some(true), true)] {
        let (runtime, fake, _storage) = fixture();
        let (broker, requester, mut c) = signed_claims(&runtime);
        c.cache_write = claim;
        runtime
            .host
            .lock()
            .unwrap()
            .set_remote_cache_write_block(blocked)
            .unwrap();
        let (mut client, task, _) =
            challenge(runtime.clone(), &fake, &c, &broker, &requester).await;
        assert_eq!(read_frame(&mut client).await.unwrap()["type"], "lease_ok");
        assert!(fake.cache_write_tokens.lock().unwrap().is_empty());
        drop(client);
        let _ = task.await.unwrap();
    }
}

#[tokio::test]
async fn reconnect_and_attach_never_send_a_grant_even_with_a_later_true_claim() {
    for first in [None, Some(false), Some(true)] {
        for purpose in [Purpose::Run, Purpose::Attach] {
            let (runtime, fake, _storage) = fixture();
            let (broker, requester, mut c) = signed_claims(&runtime);
            c.cache_write = first;
            let id = claim(&runtime, &c);
            {
                let mut host = runtime.host.lock().unwrap();
                host.begin_job_boot(JOB).unwrap().unwrap();
                host.job_runner_hello(JOB).unwrap();
                host.delivered(id).unwrap();
                host.close(id).unwrap();
            }
            c.purpose = purpose;
            c.cache_write = Some(true);
            let (mut client, task, _) =
                challenge(runtime.clone(), &fake, &c, &broker, &requester).await;
            assert_eq!(read_frame(&mut client).await.unwrap()["type"], "lease_ok");
            assert!(fake.cache_write_tokens.lock().unwrap().is_empty());
            assert_eq!(
                runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
                    .claims
                    .cache_write,
                first
            );
            drop(client);
            let _ = task.await.unwrap();
        }
    }
}
