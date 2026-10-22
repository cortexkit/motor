//! ck-motor's real Boat connect path against the real in-sandbox endpoint
//! (`motor-host --sandbox`), with job tokens shaped like the ones CLOUD's
//! broker issues for Boat. Only the SSH hop is replaced by a loopback socket:
//! the endpoint and `tls::authenticate` are the production code.

use super::{placement::Lease, resource_mismatch, tls, Error};
use ed25519_dalek::SigningKey;
use motor_host::sandbox::{self, Config};
use motor_host::server::{read_frame, serve, write_frame, Runtime};
use motor_lease::{Claims, Jwks, JwksKey, LeaseRequest, Purpose, RequesterJwk};
use serde_json::{json, Value};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;

/// A sandbox id in the format Boat assigns (`bx_` and eight characters); the
/// endpoint's identity file and every token's audience name it.
const SANDBOX: &str = "bx_648e8t62";
/// The broker's signing key id in production.
const KID: &str = "r1";

fn cloud_vector() -> Value {
    serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/boat-lease.json"
    ))
    .unwrap()
}

struct Endpoint {
    dir: std::path::PathBuf,
    runtime: Arc<Runtime>,
    jobs: tokio::net::UnixListener,
    broker: SigningKey,
}
impl Endpoint {
    /// The sandbox endpoint as the image configures it: vcpu_ceiling 8, the
    /// identity file holding the sandbox id, and the broker's JWKS loaded.
    fn new() -> Self {
        let unique = motor_protocol::Uuid::now_v7().simple().to_string();
        // The random tail keeps the socket paths short and unshared.
        let dir = cortexkit_test_support::scratch_root().join(format!("sbx-{}", &unique[20..]));
        std::fs::create_dir_all(&dir).unwrap();
        let id_file = dir.join("id");
        std::fs::write(&id_file, SANDBOX).unwrap();
        std::fs::set_permissions(&id_file, std::fs::Permissions::from_mode(0o444)).unwrap();
        // Production requires a root-owned file; a test cannot create one.
        let owner = std::fs::metadata(&id_file).unwrap().uid();
        let config: Config = serde_json::from_value(json!({
            "journal_dir": dir.join("state"),
            "job_socket": dir.join("j"),
            "event_socket": dir.join("e"),
            "admin_socket": dir.join("a"),
            "vcpu_ceiling": 8,
            "id_file": id_file,
        }))
        .unwrap();
        let jobs = tokio::net::UnixListener::bind(&config.job_socket).unwrap();
        let runtime = sandbox::runtime_with_identity_owner(config, owner).unwrap();
        let broker = SigningKey::from_bytes(&[41; 32]);
        runtime
            .keys
            .write()
            .unwrap()
            .refresh(Ok(Jwks {
                keys: vec![JwksKey::from_key(KID, &broker.verifying_key())],
            }))
            .unwrap();
        Self {
            dir,
            runtime,
            jobs,
            broker,
        }
    }

    /// A booked Boat lease as CLOUD's broker returns it: the recorded vector's
    /// reply with this sandbox's placement, the given size, a current clock,
    /// and a token signed by this endpoint's trusted broker key.
    fn lease(&self, requester: &SigningKey, vcpus: u32, mem_mib: u64) -> (LeaseRequest, Lease) {
        let vector = cloud_vector();
        let id = motor_protocol::Uuid::now_v7().to_string();
        let mut body: LeaseRequest =
            serde_json::from_value(vector["booked"]["request"]["body"].clone()).unwrap();
        body.job_id = id.clone();
        body.vcpus = vcpus;
        body.timeout_s = 1800;
        body.requester_jwk = RequesterJwk::from_key(&requester.verifying_key());
        let mut reply = vector["booked"]["reply"]["body"].clone();
        let recorded = reply["job_token"]
            .as_str()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap();
        let mut claims: Claims = serde_json::from_slice(
            &base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, recorded)
                .unwrap(),
        )
        .unwrap();
        let now = super::now_s();
        claims.aud = format!("boat:{SANDBOX}");
        claims.placement = motor_lease::Placement {
            host: claims.aud.clone(),
            vm: Some(SANDBOX.into()),
        };
        claims.jti = id.clone();
        claims.iat = now;
        claims.exp = now + 300;
        claims.vcpus = vcpus;
        claims.mem_mib = mem_mib;
        claims.max_s = 1800;
        claims.cnf.jkt = body.requester_jwk.thumbprint().unwrap();
        reply["job_id"] = json!(id);
        reply["placement"] = serde_json::to_value(&claims.placement).unwrap();
        reply["job_token"] = json!(motor_lease::sign_token(&claims, KID, &self.broker).unwrap());
        reply["token_expires_at_ms"] = json!(claims.exp * 1000);
        reply["max_s"] = json!(claims.max_s);
        (body, serde_json::from_value(reply).unwrap())
    }

    /// Runs ck-motor's claim binding and lease authentication against one
    /// endpoint connection, with a stand-in runner behind the job socket.
    async fn connect(
        &self,
        requester: &SigningKey,
        lease: &Lease,
    ) -> Result<(), tls::ConnectFailure> {
        let id: motor_protocol::Uuid = lease.job_id.parse().unwrap();
        let claims = tls::claims(id, lease, requester, Purpose::Run)
            .map_err(|e| tls::ConnectFailure::at("claims", e))?;
        let listener = sandbox::bind(0).await.unwrap();
        let address = listener.local_addr().unwrap();
        let runtime = self.runtime.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve(runtime, socket).await
        });
        let pipe = tokio::net::TcpStream::connect(address).await.unwrap();
        let client = tls::authenticate(Box::new(pipe), lease, requester, &claims);
        let runner = async {
            let (mut runner, _) = self.jobs.accept().await.unwrap();
            let verified = read_frame(&mut runner).await.unwrap();
            assert_eq!(verified["type"], "verified_lease");
            assert_eq!(read_frame(&mut runner).await.unwrap()["type"], "hello");
            let hello = motor_protocol::Frame::Hello {
                protocol_version: motor_protocol::PROTOCOL_VERSION,
                started: true,
                default_branch_tip: true,
                gate_jobs: true,
                return_paths: false,
                return_chunked: false,
                platforms: Vec::new(),
            };
            write_frame(&mut runner, &serde_json::to_value(hello).unwrap())
                .await
                .unwrap();
            // Hold the runner's end open while the client finishes its hello.
            std::future::pending::<()>().await;
            drop(runner);
        };
        let result = tokio::select! {
            result = client => result,
            () = runner => unreachable!("the stand-in runner never finishes"),
        };
        let channel = match result {
            Ok(channel) => channel,
            Err(failure) => {
                server.await.unwrap().unwrap();
                return Err(failure);
            }
        };
        drop(channel);
        server.abort();
        Ok(())
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.runtime.host.lock().unwrap().guest_unready("sandbox");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Every spilled job failed in production with `hosted_auth`: the endpoint
/// refused the broker's token at the lease hello. CLOUD's contract makes a
/// Boat token's `mem_mib` the sandbox size's usable memory (13312 MiB for the
/// 8-vCPU size), which the endpoint compared with the fleet formula
/// (28672 MiB for 8 vCPUs). Both sizes production leased must now admit.
#[tokio::test]
async fn boat_tokens_as_cloud_issues_them_admit_through_the_sandbox_endpoint() {
    for vcpus in [8, 4] {
        let endpoint = Endpoint::new();
        let requester = SigningKey::from_bytes(&[42; 32]);
        let (body, lease) = endpoint.lease(&requester, vcpus, 13312);
        let claims = tls::claims(
            lease.job_id.parse().unwrap(),
            &lease,
            &requester,
            Purpose::Run,
        )
        .unwrap();
        assert_ne!(claims.mem_mib, motor_lease::memory_max_mib(vcpus));
        assert_eq!(resource_mismatch(&claims, &body, true), None);
        // ck-motor's connect result and the endpoint's own verdict are compared
        // together, so a refusal reports both the client's step and reason and
        // the endpoint's failing check.
        let endpoint_verdict = endpoint
            .runtime
            .authenticate(
                &lease.job_token,
                &RequesterJwk::from_key(&requester.verifying_key()),
                super::now_s(),
            )
            .map(|claims| claims.mem_mib)
            .map_err(|refusal| refusal.check);
        assert_eq!(
            (endpoint.connect(&requester, &lease).await, endpoint_verdict),
            (Ok(()), Ok(13312)),
            "{vcpus} vCPUs"
        );
    }
}

/// A token outside the range both sides accept is refused at the lease hello.
/// The endpoint names the failing check and ck-motor names the step and the
/// host's reason, so a refusal is diagnosable from either log.
#[tokio::test]
async fn boat_memory_outside_the_shared_range_is_refused_and_named_on_both_sides() {
    for mem_mib in [8 * 1024 - 1, motor_lease::memory_max_mib(8) + 1] {
        let endpoint = Endpoint::new();
        let requester = SigningKey::from_bytes(&[42; 32]);
        let (body, lease) = endpoint.lease(&requester, 8, mem_mib);
        let claims = tls::claims(
            lease.job_id.parse().unwrap(),
            &lease,
            &requester,
            Purpose::Run,
        )
        .unwrap();
        // ck-motor would refuse this lease before connecting; this drives the
        // endpoint anyway to prove it refuses on its own.
        assert_eq!(
            resource_mismatch(&claims, &body, true).map(|(field, ..)| field),
            Some("mem_mib")
        );
        let refusal = endpoint
            .runtime
            .authenticate(
                &lease.job_token,
                &RequesterJwk::from_key(&requester.verifying_key()),
                super::now_s(),
            )
            .unwrap_err();
        assert_eq!(
            (refusal.check, refusal.jti.as_deref()),
            ("mem_mib", Some(lease.job_id.as_str()))
        );
        assert_eq!(
            endpoint.connect(&requester, &lease).await,
            Err(tls::ConnectFailure {
                error: Error::Auth,
                step: "lease_hello",
                host_reason: Some("auth".into()),
            })
        );
    }
}

/// With nothing listening behind the SSH forward (the endpoint not yet bound
/// after a resume), the forward closes the connection. ck-motor reports a
/// connect failure at the lease hello, never `hosted_auth`.
#[tokio::test]
async fn a_closed_forward_is_a_connect_failure_at_the_lease_hello() {
    let endpoint = Endpoint::new();
    let requester = SigningKey::from_bytes(&[42; 32]);
    let (_, lease) = endpoint.lease(&requester, 8, 13312);
    let claims = tls::claims(
        lease.job_id.parse().unwrap(),
        &lease,
        &requester,
        Purpose::Run,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let forward = tokio::spawn(async move { drop(listener.accept().await.unwrap()) });
    let pipe = tokio::net::TcpStream::connect(address).await.unwrap();
    forward.await.unwrap();
    assert_eq!(
        tls::authenticate(Box::new(pipe), &lease, &requester, &claims)
            .await
            .err(),
        Some(tls::ConnectFailure {
            error: Error::Connect,
            step: "lease_hello",
            host_reason: None,
        })
    );
}

/// A journal on a full disk, as the endpoint's journal was when the runner
/// filled the sandbox's disk.
struct FullDisk;
impl motor_host::journal::Journal for FullDisk {
    fn store(&mut self, _: &motor_host::state::Snapshot) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
    }
}

/// An endpoint that cannot record the lease answers with `storage_full`
/// instead of closing the connection. ck-motor then reports the host's
/// reason, where before it logged `hosted_connect_failed` with an empty
/// `host_reason`. Like a refusal from a full host, the endpoint never
/// started the job, so ck-motor may place it on another host.
#[tokio::test]
async fn a_full_endpoint_journal_is_a_named_storage_full_refusal() {
    let endpoint = Endpoint::new();
    endpoint
        .runtime
        .host
        .lock()
        .unwrap()
        .replace_journal(Box::new(FullDisk));
    let requester = SigningKey::from_bytes(&[42; 32]);
    let (_, lease) = endpoint.lease(&requester, 8, 13312);
    let claims = tls::claims(
        lease.job_id.parse().unwrap(),
        &lease,
        &requester,
        Purpose::Run,
    )
    .unwrap();
    let listener = sandbox::bind(0).await.unwrap();
    let address = listener.local_addr().unwrap();
    let runtime = endpoint.runtime.clone();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        serve(runtime, socket).await
    });
    let pipe = tokio::net::TcpStream::connect(address).await.unwrap();
    let failure = tls::authenticate(Box::new(pipe), &lease, &requester, &claims)
        .await
        .err();
    assert_eq!(
        failure,
        Some(tls::ConnectFailure {
            error: Error::StorageFull,
            step: "lease_ok",
            host_reason: Some("storage_full".into()),
        })
    );
    assert!(
        server.await.unwrap().is_err(),
        "the endpoint logs the close"
    );
    assert_eq!(Error::StorageFull.code(), "hosted_storage_full");
    assert_eq!(
        Error::StorageFull.reason(),
        motor_protocol::RefusalReason::RunnerFull
    );
}
