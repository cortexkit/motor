use super::*;
use ed25519_dalek::SigningKey;
use motor_lease::{
    base64url,
    fake::{BrokerRequest, FakeBroker, FakeConfig},
    sign_token, Claims, Placement, RequesterJwk, ISSUER,
};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

const NOW: u64 = 1_700_000_000;
const ACCOUNT: &str = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW)
}

fn claims() -> Claims {
    let jti = uuid::Uuid::from_u128(
        (u128::from(NOW * 1000) << 80) | (0x7000u128 << 64) | 0x8000_0000_0000_0001,
    )
    .to_string();
    serde_json::from_value(json!({
        "iss":ISSUER, "aud":"test-host", "jti":jti, "sub":ACCOUNT,
        "purpose":"run", "iat":NOW, "exp":NOW+300, "vcpus":2, "mem_mib":16384,
        "platform":"linux-x64", "net":false, "placement":{"host":"test-host","vm":null},
        "max_s":30, "cnf":{"jkt":RequesterJwk::from_key(&SigningKey::from_bytes(&[3;32]).verifying_key()).thumbprint().unwrap()},
        "workspace_key_hash":base64url(&[5;32]), "cache_write":true
    })).unwrap()
}

fn broker(seed: u8, kid: &str) -> FakeBroker {
    FakeBroker::new(
        "helper-tests",
        FakeConfig {
            worker_bearer: "test".into(),
            subject: ACCOUNT.into(),
            placement: Placement {
                host: "test-host".into(),
                vm: None,
            },
            endpoint: "https://unused.invalid".into(),
            endpoint_spki_sha256: base64url(&[0; 32]),
            kid: kid.into(),
            signing_key: SigningKey::from_bytes(&[seed; 32]),
            announced_keys: vec![],
        },
    )
    .unwrap()
}

struct Server {
    address: std::net::SocketAddr,
    broker: Arc<Mutex<FakeBroker>>,
    requests: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let broker = Arc::new(Mutex::new(broker(1, "current")));
        let requests = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (b, r, f, s) = (broker.clone(), requests.clone(), fail.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if s.load(Ordering::SeqCst) {
                    break;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line, "GET /.well-known/jwks.json HTTP/1.1\r\n");
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                r.fetch_add(1, Ordering::SeqCst);
                let reply = b.lock().unwrap().handle(
                    BrokerRequest {
                        method: "GET",
                        path: "/.well-known/jwks.json",
                        bearer: None,
                        content_type: None,
                        host_headers: None,
                        body: b"",
                    },
                    NOW * 1000,
                );
                let status = if f.load(Ordering::SeqCst) {
                    503
                } else {
                    reply.status
                };
                let body = serde_json::to_vec(&reply.body).unwrap();
                write!(stream,"HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            address,
            broker,
            requests,
            fail,
            stop,
            thread: Some(thread),
        }
    }

    fn config(&self) -> Config {
        let mut config: Config =
            serde_json::from_str(include_str!("../fixtures/helper.json")).unwrap();
        config.host_id = Some("test-host".into());
        config.jwks_url = Some(format!("http://{}/.well-known/jwks.json", self.address));
        config.validate_resources().unwrap();
        config
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn token(c: &Claims) -> String {
    sign_token(c, "current", &SigningKey::from_bytes(&[1; 32])).unwrap()
}

#[test]
fn invalid_credentials_never_record_a_grant() {
    let server = Server::new();
    let mut grants = Grants::new(&server.config());
    let original = claims();
    let mut cases =
        vec![sign_token(&original, "current", &SigningKey::from_bytes(&[2; 32])).unwrap()];
    for change in ["aud", "iss", "exp", "iat", "attach", "false", "absent"] {
        let mut c = original.clone();
        match change {
            "aud" => c.aud = "other-host".into(),
            "iss" => c.iss = "https://other.invalid".into(),
            "exp" => {
                c.iat = NOW - 300;
                c.exp = NOW;
            }
            "iat" => {
                c.iat = NOW + 61;
                c.exp = NOW + 300;
            }
            "attach" => c.purpose = Purpose::Attach,
            "false" => c.cache_write = Some(false),
            "absent" => c.cache_write = None,
            _ => unreachable!(),
        }
        cases.push(token(&c));
    }
    for token in cases {
        assert!(grants.admit_with_clock(&token, now).is_err());
        assert!(grants.grants.is_empty());
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn valid_admission_is_immutable_and_survives_token_expiry_until_start_window_ends() {
    let server = Server::new();
    let mut grants = Grants::new(&server.config());
    let c = claims();
    grants.admit_with_clock(&token(&c), now).unwrap();
    let expected = Grant {
        sub: c.sub.clone(),
        jti: c.jti.clone(),
        workspace_key_hash: c.workspace_key_hash.clone().unwrap(),
    };
    assert_eq!(grants.get_at(&c.jti, now()), Some(&expected));
    assert!(grants.admit_with_clock(&token(&c), now).is_err());
    let mut changed = c.clone();
    changed.sub = "org:01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
    changed.workspace_key_hash = Some(base64url(&[8; 32]));
    assert!(grants.admit_with_clock(&token(&changed), now).is_err());
    assert_eq!(grants.grants.len(), 1);
    assert_eq!(
        grants.get_at(&c.jti, now() + Duration::from_secs(301)),
        Some(&expected)
    );
    assert_eq!(
        grants.get_at(&c.jti, now() + Duration::from_secs(24 * 60 * 60)),
        Some(&expected)
    );
    let stale = now() + Duration::from_secs(24 * 60 * 60 + 1);
    assert!(grants.get_at(&c.jti, stale).is_none());
    grants.prune_at(stale);
    assert!(grants.grants.is_empty());
}

#[test]
fn keys_supplied_by_host_cannot_authorize_and_requests_accept_only_raw_token() {
    let server = Server::new();
    let mut grants = Grants::new(&server.config());
    let forged = sign_token(&claims(), "host-only", &SigningKey::from_bytes(&[9; 32])).unwrap();
    let request = json!({"op":"admit_cache_write","token":forged});
    assert!(serde_json::from_value::<crate::Request>(request.clone()).is_ok());
    for (name, value) in [
        (
            "keys",
            json!(motor_lease::JwksKey::from_key(
                "host-only",
                &SigningKey::from_bytes(&[9; 32]).verifying_key()
            )),
        ),
        ("jwks_url", json!("http://host.invalid/keys")),
        ("host_id", json!("other-host")),
        ("now_s", json!(NOW - 60)),
    ] {
        let mut extra = request.clone();
        extra[name] = value;
        assert!(serde_json::from_value::<crate::Request>(extra).is_err());
    }
    assert!(grants.admit_with_clock(&forged, now).is_err());
    assert!(grants.grants.is_empty());
}

#[test]
fn missing_host_identity_and_failed_key_fetch_fail_closed() {
    let server = Server::new();
    let mut config = server.config();
    config.host_id = None;
    let mut grants = Grants::new(&config);
    assert!(grants.admit_with_clock(&token(&claims()), now).is_err());
    assert!(grants.grants.is_empty());
    assert_eq!(server.requests.load(Ordering::SeqCst), 0);
    server.fail.store(true, Ordering::SeqCst);
    let mut grants = Grants::new(&server.config());
    assert!(grants.admit_with_clock(&token(&claims()), now).is_err());
    assert!(grants.grants.is_empty());
    // A known kid cannot bypass a failed refresh by retrying before the throttle.
    assert!(grants.admit_with_clock(&token(&claims()), now).is_err());
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn admission_rechecks_expiry_after_fetch_and_rejects_stale_start_identity() {
    let server = Server::new();
    let mut grants = Grants::new(&server.config());
    let c = claims();
    let calls = AtomicUsize::new(0);
    assert!(grants
        .admit_with_clock(&token(&c), || {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                now()
            } else {
                now() + Duration::from_secs(300)
            }
        })
        .is_err());
    assert!(grants.grants.is_empty());
    let mut stale = c.clone();
    stale.jti = uuid::Uuid::from_u128(
        (u128::from((NOW - 24 * 60 * 60 - 1) * 1000) << 80)
            | (0x7000u128 << 64)
            | 0x8000_0000_0000_0001,
    )
    .to_string();
    assert!(grants.admit_with_clock(&token(&stale), now).is_err());
    assert!(grants.grants.is_empty());
}

#[test]
fn key_refresh_uses_ten_minute_cadence_and_throttled_unknown_kid_refetch() {
    let server = Server::new();
    let mut grants = Grants::new(&server.config());
    let c = claims();
    grants.admit_with_clock(&token(&c), now).unwrap();
    *server.broker.lock().unwrap() = broker(2, "next");
    let mut next = c.clone();
    next.jti.replace_range(35..36, "2");
    let next_token = sign_token(&next, "next", &SigningKey::from_bytes(&[2; 32])).unwrap();
    assert!(grants
        .admit_with_clock(&next_token, || now() + Duration::from_secs(59))
        .is_err());
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    grants
        .admit_with_clock(&next_token, || now() + Duration::from_secs(60))
        .unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    grants
        .refresh("", now() + Duration::from_secs(659))
        .unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    grants
        .refresh("", now() + Duration::from_secs(660))
        .unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 3);
    server.fail.store(true, Ordering::SeqCst);
    assert!(grants
        .refresh("", now() + Duration::from_secs(1260))
        .is_err());
    let mut c = claims();
    c.jti.replace_range(35..36, "3");
    c.iat = NOW + 1260;
    c.exp = c.iat + 300;
    let token = sign_token(&c, "next", &SigningKey::from_bytes(&[2; 32])).unwrap();
    assert!(grants
        .admit_with_clock(&token, || now() + Duration::from_secs(1260))
        .is_err());
    assert!(!grants.grants.contains_key(&c.jti));
}
