#[path = "common.rs"]
mod common;
use common::*;
use ed25519_dalek::SigningKey;
use motor_lease::*;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    dir: PathBuf,
    address: String,
}
impl Server {
    fn start(options: &[&str]) -> Self {
        let dir = cortexkit_test_support::scratch_root().join(format!(
            "motor-lease-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let config_path = dir.join("config.json");
        std::fs::write(&config_path,serde_json::to_vec(&json!({
            "worker_bearer":BEARER,"subject":"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "placement":{"host":HOST,"vm":"test-vm"},"endpoint":"127.0.0.1:4443",
            "endpoint_spki_sha256":base64url(&[8;32]),"kid":"current","signing_seed":base64url(&[1;32]),
            "registered_hosts":[{"host":HOST,"public_key":RequesterJwk::from_key(&SigningKey::from_bytes(&[4;32]).verifying_key())}]
        })).unwrap()).unwrap();
        let mut args = vec![
            "--runner-instance".into(),
            "lease-http-test".into(),
            "--config".into(),
            config_path.to_str().unwrap().into(),
        ];
        args.extend(options.iter().map(|s| (*s).to_owned()));
        let (mut broker, address) = super::configure(&args).unwrap();
        let listener = TcpListener::bind(address).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                broker.settle_due(now_ms());
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Model Darwin's inherited nonblocking mode on every test platform.
                        stream.set_nonblocking(true).unwrap();
                        let _ = super::serve_connection(stream, &mut broker);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::park_timeout(Duration::from_millis(2))
                    }
                    Err(e) => panic!("fake listener: {e}"),
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
            dir,
            address,
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        worker: bool,
        host: Option<&HostHeaders>,
    ) -> (u16, Value) {
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
        .unwrap();
        if worker {
            write!(stream, "Authorization: Bearer {BEARER}\r\n").unwrap();
        }
        if let Some(h) = host {
            write!(
                stream,
                "CK-Host: {}\r\nCK-Timestamp: {}\r\nCK-Counter: {}\r\nCK-Signature: {}\r\n",
                h.host, h.timestamp, h.counter, h.signature
            )
            .unwrap();
        }
        write!(stream, "\r\n").unwrap();
        stream.write_all(body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap())
    }

    fn post(&self, path: &str, body: &impl serde::Serialize) -> (u16, Value) {
        self.request("POST", path, &serde_json::to_vec(body).unwrap(), true, None)
    }

    fn host_post(
        &self,
        route: &str,
        body: &impl serde::Serialize,
        counter: u64,
        timestamp: u64,
    ) -> (u16, Value) {
        let body = serde_json::to_vec(body).unwrap();
        let path = format!("/v1/hosts/{HOST}/{route}");
        let headers = HostHeaders::sign(
            HOST,
            "POST",
            &path,
            &body,
            timestamp,
            counter,
            &SigningKey::from_bytes(&[4; 32]),
        )
        .unwrap();
        self.request("POST", &path, &body, false, Some(&headers))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn http_nonblocking_connection_waits_for_headers_and_body() {
    let server = Server::start(&[]);
    let mut stream = TcpStream::connect(&server.address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let body = serde_json::to_vec(&lease_request()).unwrap();
    stream.write_all(b"POST /v1/leases HTTP/1.1\r\n").unwrap();
    let assert_pending = |stream: &mut TcpStream| {
        let error = stream.read(&mut [0; 1]).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
    };
    assert_pending(&mut stream);
    write!(
        stream,
        "Host: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer {BEARER}\r\n\r\n",
        body.len()
    )
    .unwrap();
    let (first, last) = body.split_at(body.len() / 2);
    stream.write_all(first).unwrap();
    assert_pending(&mut stream);
    stream.write_all(last).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert_eq!(headers.split_whitespace().nth(1), Some("201"));
    let lease: LeaseResponse = serde_json::from_str(body).unwrap();
    assert_eq!(lease.reserved_vcpu_ms, 20000);
}

#[test]
fn real_http_serves_jwks_lease_reattach_heartbeat_usage_and_errors() {
    let server = Server::start(&[]);
    let (status, jwks) = server.request("GET", "/.well-known/jwks.json", &[], false, None);
    assert_eq!(status, 200);
    let mut keys = KeySet::default();
    keys.refresh(Ok(serde_json::from_value(jwks).unwrap()))
        .unwrap();
    let (status, body) = server.post("/v1/leases", &lease_request());
    assert_eq!(status, 201);
    let lease: LeaseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(lease.reserved_vcpu_ms, 20000);
    let token = verify_token(
        &lease.job_token,
        &keys,
        HOST,
        Some("019e4567-89ab-7cde-8012-3456789abcde"),
        Purpose::Run,
        now_ms() / 1000,
    )
    .unwrap();
    assert_eq!(token.purpose, Purpose::Run);
    let (status, body) = server.post(
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
        &ReattachRequest {
            requester_jwk: lease_request().requester_jwk,
        },
    );
    assert_eq!(status, 200);
    let attach: LeaseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(
        verify_token(
            &attach.job_token,
            &keys,
            HOST,
            Some("019e4567-89ab-7cde-8012-3456789abcde"),
            Purpose::Attach,
            now_ms() / 1000
        )
        .unwrap()
        .purpose,
        Purpose::Attach
    );
    let hb = heartbeat(JobState::Started, 3000);
    assert_eq!(
        server.host_post("heartbeat", &hb, 1, now_ms()),
        (200, json!({"ignored":[],"closed_subjects":[]}))
    );
    assert_eq!(
        server.host_post("heartbeat", &hb, 1, now_ms()),
        (401, json!({"error":"host_auth_failed"}))
    );
    assert_eq!(
        server.host_post("heartbeat", &hb, 2, now_ms() - 61000),
        (401, json!({"error":"host_auth_failed"}))
    );
    let mut u = usage();
    u.started_at_ms = now_ms() - 3000;
    u.ended_at_ms = u.started_at_ms + 3000;
    let first = server.host_post("usage", &u, 3, now_ms());
    assert_eq!(
        first,
        (
            200,
            json!({"job_id":"019e4567-89ab-7cde-8012-3456789abcde","settled_vcpu_ms":6000,"released_vcpu_ms":14000})
        )
    );
    assert_eq!(server.host_post("usage", &u, 4, now_ms()), first);
    // A job settled by its host's usage report can still be reattached, so the
    // host can replay output the client lost; the lease comes back unchanged.
    let (status, body) = server.post(
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
        &ReattachRequest {
            requester_jwk: lease_request().requester_jwk,
        },
    );
    assert_eq!(status, 200, "{body}");
    let settled_attach: LeaseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(
        (settled_attach.lease_id, settled_attach.reserved_vcpu_ms),
        (lease.lease_id.clone(), lease.reserved_vcpu_ms)
    );
    verify_token(
        &settled_attach.job_token,
        &keys,
        HOST,
        Some("019e4567-89ab-7cde-8012-3456789abcde"),
        Purpose::Attach,
        now_ms() / 1000,
    )
    .unwrap();
    assert_eq!(
        server.post("/v1/leases", &lease_request()),
        (409, json!({"error":"job_finished"}))
    );
    assert_eq!(
        server.request("POST", "/v1/leases", b"{}", false, None),
        (401, json!({"error":"unauthorized"}))
    );
    assert_eq!(
        server.post("/v1/leases", &json!({})),
        (400, json!({"error":"bad_request"}))
    );
}

#[test]
fn fake_binary_refuses_main_before_loading_config_or_listening() {
    let error = match super::configure(&["--runner-instance".into(), "main".into()]) {
        Err(error) => error,
        Ok(_) => panic!("main instance must be refused before a config can be read"),
    };
    assert!(error.to_string().contains("forbidden for the main"));
}

#[test]
fn fake_binary_refusal_and_shortening_flags_work_over_http() {
    let refusal = Server::start(&["--refuse", "capacity"]);
    assert_eq!(
        refusal.post("/v1/leases", &lease_request()),
        (403, json!({"error":"capacity"}))
    );
    let shortened = Server::start(&["--max-s", "5"]);
    let mut request = lease_request();
    request.timeout_s = 100;
    let (status, body) = shortened.post("/v1/leases", &request);
    assert_eq!(status, 201);
    assert_eq!(body["max_s"], 5);
    assert_eq!(body["reserved_vcpu_ms"], 10000);
}

#[test]
fn fake_binary_max_vcpus_flag_answers_no_host_fits_over_http() {
    let server = Server::start(&["--max-vcpus", "8"]);
    let mut request = lease_request();
    request.vcpus = 16;
    assert_eq!(
        server.post("/v1/leases", &request),
        (422, json!({"error":"no_host_fits","max_vcpus":8}))
    );
    request.vcpus = 8;
    assert_eq!(server.post("/v1/leases", &request).0, 201);
}

/// Over real HTTP, the fake executable answers a lease body without
/// `allow_spill` 400 `bad_request`, as the broker does, and still books the
/// same lease once the member is present.
#[test]
fn fake_binary_refuses_a_lease_without_allow_spill_over_http() {
    let server = Server::start(&[]);
    let mut body = serde_json::to_value(lease_request()).unwrap();
    body.as_object_mut().unwrap().remove("allow_spill");
    assert_eq!(
        server.post("/v1/leases", &body),
        (400, json!({"error":"bad_request"}))
    );
    body["allow_spill"] = json!(false);
    assert_eq!(server.post("/v1/leases", &body).0, 201);
}

#[test]
fn http_release_route_is_idempotent_and_admission_cancels_pending_release() {
    let server = Server::start(&[]);
    let (status, body) = server.post("/v1/leases", &lease_request());
    assert_eq!(status, 201);
    let lease: LeaseResponse = serde_json::from_value(body).unwrap();
    let first = server.post(
        "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
        &ReleaseRequest {},
    );
    assert_eq!(
        first,
        (
            202,
            json!({"job_id":"019e4567-89ab-7cde-8012-3456789abcde", "release_at_ms":lease.token_expires_at_ms + 60000})
        )
    );
    assert_eq!(
        server.post(
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {}
        ),
        first
    );
    assert_eq!(
        server.post("/v1/leases", &lease_request()),
        (409, json!({"error":"job_released"}))
    );
    assert_eq!(
        server.post(
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/reattach",
            &ReattachRequest {
                requester_jwk: lease_request().requester_jwk
            }
        ),
        (409, json!({"error":"job_released"}))
    );
    let (_, jwks) = server.request("GET", "/.well-known/jwks.json", &[], false, None);
    let mut keys = KeySet::default();
    keys.refresh(Ok(serde_json::from_value(jwks).unwrap()))
        .unwrap();
    verify_token(
        &lease.job_token,
        &keys,
        HOST,
        Some("019e4567-89ab-7cde-8012-3456789abcde"),
        Purpose::Run,
        now_ms() / 1000,
    )
    .unwrap();
    assert_eq!(
        server
            .host_post("heartbeat", &heartbeat(JobState::Admitted, 0), 1, now_ms())
            .0,
        200
    );
    assert_eq!(
        server.post(
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {}
        ),
        (409, json!({"error":"job_admitted"}))
    );
    let mut u = usage();
    u.started_at_ms = now_ms() - 3000;
    u.ended_at_ms = u.started_at_ms + 3000;
    assert_eq!(
        server.host_post("usage", &u, 2, now_ms()),
        (
            200,
            json!({"job_id":"019e4567-89ab-7cde-8012-3456789abcde", "settled_vcpu_ms":6000, "released_vcpu_ms":14000})
        )
    );
    assert_eq!(
        server.post(
            "/v1/leases/019e4567-89ab-7cde-8012-3456789abcde/release",
            &ReleaseRequest {}
        ),
        (409, json!({"error":"job_finished"}))
    );
}
