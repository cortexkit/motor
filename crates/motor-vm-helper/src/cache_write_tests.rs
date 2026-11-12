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
pub(crate) const ACCOUNT: &str = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";

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
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    arrived: std::sync::mpsc::Receiver<()>,
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
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (arrived_tx, arrived) = std::sync::mpsc::channel();
        let fetch_gate = gate.clone();
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
                arrived_tx.send(()).unwrap();
                let mut held = fetch_gate.0.lock().unwrap();
                while *held {
                    held = fetch_gate.1.wait(held).unwrap();
                }
                drop(held);
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
            gate,
            arrived,
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
        *self.gate.0.lock().unwrap() = false;
        self.gate.1.notify_all();
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn token(c: &Claims) -> String {
    sign_token(c, "current", &SigningKey::from_bytes(&[1; 32])).unwrap()
}

#[cfg(target_os = "linux")]
pub(crate) fn with_fresh_token(jti: &str, account: &str, run: impl FnOnce(&Config, &str)) {
    let server = Server::new();
    let mut c = claims();
    c.jti = jti.into();
    c.sub = account.into();
    c.iat = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    c.exp = c.iat + 300;
    run(&server.config(), &token(&c));
}

#[test]
fn invalid_credentials_never_record_a_grant() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
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
        assert!(grants.records.lock().unwrap().grants.is_empty());
    }
    server.arrived.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn valid_admission_is_immutable_but_expires_before_start_window_ends() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
    let c = claims();
    grants.admit_with_clock(&token(&c), now).unwrap();
    let expected = Grant {
        exp: c.exp,
        sub: c.sub.clone(),
        jti: c.jti.clone(),
        workspace_key_hash: c.workspace_key_hash.clone().unwrap(),
    };
    assert_eq!(grants.get_at(&c.jti, now()), Some(expected.clone()));
    grants.admit_with_clock(&token(&c), now).unwrap();
    let mut changed = c.clone();
    changed.sub = "org:01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
    changed.workspace_key_hash = Some(base64url(&[8; 32]));
    assert!(grants.admit_with_clock(&token(&changed), now).is_err());
    assert_eq!(grants.records.lock().unwrap().grants.len(), 1);
    assert_eq!(
        grants.get_at(&c.jti, now() + Duration::from_secs(301)),
        None
    );
    assert_eq!(
        grants.get_at(&c.jti, now() + Duration::from_secs(24 * 60 * 60)),
        None
    );
    let stale = now() + Duration::from_secs(24 * 60 * 60 + 1);
    assert!(grants.get_at(&c.jti, stale).is_none());
    grants.prune_at(stale);
    assert!(grants.records.lock().unwrap().grants.is_empty());
}

#[test]
fn keys_supplied_by_host_cannot_authorize_and_requests_accept_only_raw_token() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
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
    assert!(grants.records.lock().unwrap().grants.is_empty());
}

#[test]
fn missing_host_identity_and_failed_key_fetch_fail_closed() {
    let server = Server::new();
    let mut config = server.config();
    config.host_id = None;
    let grants = Grants::new(&config);
    assert!(grants.admit_with_clock(&token(&claims()), now).is_err());
    assert!(grants.records.lock().unwrap().grants.is_empty());
    assert_eq!(server.requests.load(Ordering::SeqCst), 0);
    server.fail.store(true, Ordering::SeqCst);
    let grants = Grants::new(&server.config());
    assert!(grants.admit_with_clock(&token(&claims()), now).is_err());
    assert!(grants.records.lock().unwrap().grants.is_empty());
    // A known kid cannot bypass a failed refresh by retrying before the throttle.
    assert!(grants.admit_with_clock(&token(&claims()), now).is_err());
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn admission_rechecks_expiry_after_fetch_and_rejects_stale_start_identity() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
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
    assert!(grants.records.lock().unwrap().grants.is_empty());
    let mut stale = c.clone();
    stale.jti = uuid::Uuid::from_u128(
        (u128::from((NOW - 24 * 60 * 60 - 1) * 1000) << 80)
            | (0x7000u128 << 64)
            | 0x8000_0000_0000_0001,
    )
    .to_string();
    assert!(grants.admit_with_clock(&token(&stale), now).is_err());
    assert!(grants.records.lock().unwrap().grants.is_empty());
}

#[test]
fn key_refresh_uses_ten_minute_cadence_and_throttled_unknown_kid_refetch() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
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
    grants
        .refresh("", now() + Duration::from_secs(1260))
        .unwrap();
    let mut c = claims();
    c.jti.replace_range(35..36, "3");
    c.iat = NOW + 1260;
    c.exp = c.iat + 300;
    let token = sign_token(&c, "next", &SigningKey::from_bytes(&[2; 32])).unwrap();
    grants
        .admit_with_clock(&token, || now() + Duration::from_secs(1260))
        .unwrap();
    assert!(grants.records.lock().unwrap().grants.contains_key(&c.jti));
}

#[cfg(target_os = "linux")]
#[test]
fn held_jwks_fetch_keeps_ensure_start_and_stop_job_responsive() {
    use crate::{
        job::{Operation, Workspace, WorkspaceRole},
        Helper, Kind, Lease, Machine, Peer, Reply, Response,
    };
    use std::os::{fd::OwnedFd, unix::net::UnixStream};

    // Exercise the exact grant lookups used by LinuxJobs under the real Helper
    // mutex. VM/disk IO is irrelevant to this lock-order test; the real-VM matrix
    // separately qualifies authorization and the sandbox boundary.
    struct LifecycleMachine {
        authority: Authority,
        entered: std::sync::mpsc::Sender<()>,
        stopped: Arc<AtomicBool>,
        running: BTreeSet<String>,
    }
    impl Machine for LifecycleMachine {
        fn cache_write_authority(&self) -> Option<Authority> {
            Some(self.authority.clone())
        }
        fn reconcile(&mut self) -> Result<()> {
            Ok(())
        }
        fn start(&mut self, _: usize, _: &Lease, _: Kind) -> Result<Vec<OwnedFd>> {
            unreachable!()
        }
        fn stop(&mut self, _: usize, _: &Lease, _: bool) -> Result<()> {
            unreachable!()
        }
        fn cleanup(&mut self, _: usize) -> Result<()> {
            Ok(())
        }
        fn alive(&mut self, _: usize) -> bool {
            true
        }
        fn job(&mut self, op: Operation, _: Peer) -> Result<Response> {
            match op {
                Operation::EnsureWorkspace {
                    cache_write_job_id, ..
                } => {
                    self.entered.send(()).unwrap();
                    let _grant = disk_grant(&self.authority, cache_write_job_id.as_deref());
                }
                Operation::Start(start) => {
                    self.entered.send(()).unwrap();
                    let disk = Workspace {
                        account: start.account.clone(),
                        workspace_disk_id: start.workspace_disk_id.clone(),
                        role: WorkspaceRole::Job,
                        workspace_key_hash: Some(base64url(&[5; 32])),
                    };
                    let _grant =
                        crate::linux::job::take_job_write_grant(&self.authority, &start, &disk);
                    self.running.insert(start.job_id);
                }
                Operation::Stop(id) => {
                    assert!(self.running.remove(&id), "StopJob must target a running VM");
                    self.stopped.store(true, Ordering::SeqCst);
                }
                _ => unreachable!(),
            }
            Ok(Response::new(Reply::Ok))
        }
    }
    fn connection(
        helper: &Arc<Mutex<Helper<LifecycleMachine>>>,
    ) -> (BufReader<UnixStream>, JoinHandle<()>) {
        let (client, stream) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let peer = crate::linux::server::peer(&stream).unwrap();
        let id = helper.lock().unwrap().connect(peer).unwrap();
        let helper = helper.clone();
        let worker = std::thread::spawn(move || {
            crate::linux::server::connection_loop(&helper, id, stream).unwrap();
            helper.lock().unwrap().disconnect(id);
        });
        (BufReader::new(client), worker)
    }
    fn send(client: &mut BufReader<UnixStream>, request: serde_json::Value) {
        writeln!(client.get_mut(), "{request}").unwrap();
    }
    fn receive(client: &mut BufReader<UnixStream>) -> std::io::Result<serde_json::Value> {
        let mut line = String::new();
        client.read_line(&mut line)?;
        Ok(serde_json::from_str(&line).unwrap())
    }
    let server = Server::new();
    let authority = Arc::new(Grants::new(&server.config()));
    let stopped = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let helper = Arc::new(Mutex::new(
        Helper::new(
            LifecycleMachine {
                authority: authority.clone(),
                entered: entered_tx,
                stopped: stopped.clone(),
                running: BTreeSet::new(),
            },
            1,
            vec![unsafe { libc::geteuid() }],
        )
        .unwrap(),
    ));
    // Connect before holding the fetch; connect itself takes the Helper mutex.
    let (mut admission, admission_thread) = connection(&helper);
    let (mut ensure, ensure_thread) = connection(&helper);
    let (mut start, start_thread) = connection(&helper);
    let (mut stop, stop_thread) = connection(&helper);
    let other = crate::job::test_id(102);
    send(
        &mut stop,
        json!({"op":"start_job","kind":"job","job_id":other,"account":ACCOUNT,"workspace_disk_id":"other-disk","image_generation_id":"gen-1","vcpus":2,"memory_mib":16384}),
    );
    assert_eq!(receive(&mut stop).unwrap(), json!({"result":"ok"}));
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut c = claims();
    c.jti = crate::job::test_id(101);
    c.iat = seconds(SystemTime::now()).unwrap();
    c.exp = c.iat + 300;
    *server.gate.0.lock().unwrap() = true;
    send(
        &mut admission,
        json!({"op":"admit_cache_write","token":token(&c)}),
    );
    server.arrived.recv_timeout(Duration::from_secs(2)).unwrap();
    send(
        &mut ensure,
        json!({"op":"ensure_workspace","account":ACCOUNT,"disk_id":"disk-1","size_bytes":4096,"cache_write_job_id":c.jti}),
    );
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    send(
        &mut start,
        json!({"op":"start_job","kind":"job","job_id":c.jti,"account":ACCOUNT,"workspace_disk_id":"disk-1","image_generation_id":"gen-1","vcpus":2,"memory_mib":16384}),
    );
    let start_entered = entered.recv_timeout(Duration::from_secs(2));
    send(&mut stop, json!({"op":"stop_job","job_id":other}));
    let stop_reply = receive(&mut stop);
    let stopped_while_fetch_held = stopped.load(Ordering::SeqCst);
    *server.gate.0.lock().unwrap() = false;
    server.gate.1.notify_all();
    assert_eq!(receive(&mut admission).unwrap(), json!({"result":"ok"}));
    assert_eq!(receive(&mut ensure).unwrap(), json!({"result":"ok"}));
    assert_eq!(receive(&mut start).unwrap(), json!({"result":"ok"}));
    if stop_reply.is_err() {
        // Drain the late reply before disconnecting, so a deadline failure is
        // reported as a lifecycle stall rather than a connection-reset panic.
        assert_eq!(receive(&mut stop).unwrap(), json!({"result":"ok"}));
    }
    assert!(helper.lock().unwrap().admission_work(u64::MAX).is_err());
    helper.lock().unwrap().shutdown().unwrap();
    assert!(helper.lock().unwrap().admission_work(1).is_err());
    drop((admission, ensure, start, stop));
    for worker in [admission_thread, ensure_thread, start_thread, stop_thread] {
        worker.join().unwrap();
    }
    assert!(
        start_entered.is_ok(),
        "Start blocked behind held JWKS fetch"
    );
    assert_eq!(
        stop_reply.unwrap(),
        json!({"result":"ok"}),
        "StopJob exceeded its two-second deadline"
    );
    assert!(stopped_while_fetch_held, "StopJob waited for broker");
}

#[test]
fn verified_grant_retries_before_boot_but_cannot_be_rearmed_after_consumption() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
    let c = claims();
    grants.admit_with_clock(&token(&c), now).unwrap();
    let first = grants.get_at(&c.jti, now()).unwrap().clone();
    assert_eq!(grants.get_at(&c.jti, now()), Some(first.clone()));
    assert!(grants.consume_at(&c.jti, now()).is_some());
    assert!(grants.get_at(&c.jti, now()).is_none());
    grants.admit_with_clock(&token(&c), now).unwrap();
    assert!(grants.get_at(&c.jti, now()).is_none());
}

#[test]
fn verified_grant_binds_exact_job_subject_workspace_and_deny_flag() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
    let c = claims();
    grants.admit_with_clock(&token(&c), now).unwrap();
    let grant = grants.get_at(&c.jti, now()).unwrap();
    let start = crate::job::Start {
        kind: crate::job::VmKind::Job,
        job_id: c.jti.clone(),
        account: c.sub.clone(),
        workspace_disk_id: "disk-1".into(),
        image_generation_id: "gen-1".into(),
        vcpus: 2,
        memory_mib: 16384,
        burst_mib: None,
        cache_write_blocked: false,
    };
    let disk = crate::job::Workspace {
        account: c.sub.clone(),
        workspace_disk_id: start.workspace_disk_id.clone(),
        role: crate::job::WorkspaceRole::Job,
        workspace_key_hash: c.workspace_key_hash.clone(),
    };
    assert!(grant.permits(&start, &disk));
    for case in [
        "job",
        "workspace",
        "legacy",
        "account",
        "block",
        "fill",
        "recovery",
    ] {
        let mut start = start.clone();
        let mut disk = disk.clone();
        match case {
            "job" => start.job_id = crate::job::test_id(83),
            "workspace" => disk.workspace_key_hash = Some(base64url(&[9; 32])),
            "legacy" => disk.workspace_key_hash = None,
            "account" => {
                disk.account = "other-account".into();
                start.account = disk.account.clone();
            }
            "block" => start.cache_write_blocked = true,
            "fill" => {
                start.kind = crate::job::VmKind::Fill;
                disk.role = crate::job::WorkspaceRole::Fill;
            }
            "recovery" => start.kind = crate::job::VmKind::Recovery,
            _ => unreachable!(),
        }
        assert!(!grant.permits(&start, &disk), "{case}");
    }
}

#[test]
fn failed_refresh_keeps_verified_keys_and_retries_with_bounded_backoff() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
    grants.admit_with_clock(&token(&claims()), now).unwrap();
    server.fail.store(true, Ordering::SeqCst);
    let mut cached = claims();
    cached.jti.replace_range(35..36, "4");
    cached.iat = NOW + 600;
    cached.exp = cached.iat + 300;
    grants
        .admit_with_clock(&token(&cached), || now() + Duration::from_secs(600))
        .unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    for (elapsed, requests) in [
        (604, 2),
        (605, 3),
        (614, 3),
        (615, 4),
        (634, 4),
        (635, 5),
        (674, 5),
        (675, 6),
        (734, 6),
        (735, 7),
    ] {
        grants
            .refresh("", now() + Duration::from_secs(elapsed))
            .unwrap();
        assert_eq!(
            server.requests.load(Ordering::SeqCst),
            requests,
            "at {elapsed}s"
        );
    }
    server.fail.store(false, Ordering::SeqCst);
    *server.broker.lock().unwrap() = broker(2, "next");
    let mut rotated = cached.clone();
    rotated.jti.replace_range(35..36, "5");
    rotated.iat = NOW + 795;
    rotated.exp = rotated.iat + 300;
    let rotated_token = sign_token(&rotated, "next", &SigningKey::from_bytes(&[2; 32])).unwrap();
    grants
        .admit_with_clock(&rotated_token, || now() + Duration::from_secs(795))
        .unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 8);
    assert!(grants
        .admit_with_clock(&rotated_token, || now() + Duration::from_secs(1095))
        .is_err());
    server.fail.store(true, Ordering::SeqCst);
    let mut boundary = rotated.clone();
    boundary.jti.replace_range(35..36, "6");
    boundary.iat = NOW + 795 + 3600;
    boundary.exp = boundary.iat + 300;
    let sign = |c: &Claims| sign_token(c, "next", &SigningKey::from_bytes(&[2; 32])).unwrap();
    let hour = now() + Duration::from_secs(795 + 3600);
    grants.admit_with_clock(&sign(&boundary), || hour).unwrap();
    let admitted_id = boundary.jti.clone();
    boundary.jti.replace_range(35..36, "7");
    let stale = hour + Duration::from_secs(1);
    assert!(grants.admit_with_clock(&sign(&boundary), || stale).is_err());
    assert!(!grants
        .records
        .lock()
        .unwrap()
        .grants
        .contains_key(&boundary.jti));
    assert!(
        grants.get_at(&admitted_id, stale).is_some(),
        "keyset age cannot revoke an unexpired admitted grant"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn expired_grant_consumption_and_staging_fall_back_to_read_only() {
    let server = Server::new();
    let grants = Grants::new(&server.config());
    let c = claims();
    grants.admit_with_clock(&token(&c), now).unwrap();
    let grant = grants.get_at(&c.jti, now()).unwrap();
    let expired = now() + Duration::from_secs(300);
    assert!(grants.consume_at(&c.jti, expired).is_none());
    let start = crate::job::Start {
        kind: crate::job::VmKind::Job,
        job_id: c.jti.clone(),
        account: c.sub.clone(),
        workspace_disk_id: "disk-1".into(),
        image_generation_id: "gen-1".into(),
        vcpus: 2,
        memory_mib: 16384,
        burst_mib: None,
        cache_write_blocked: false,
    };
    let disk = crate::job::Workspace {
        account: c.sub,
        workspace_disk_id: start.workspace_disk_id.clone(),
        role: crate::job::WorkspaceRole::Job,
        workspace_key_hash: c.workspace_key_hash,
    };
    assert!(crate::linux::job::job_write_for_stage(
        Some(&grant),
        &start,
        &disk,
        now()
    ));
    assert!(!crate::linux::job::job_write_for_stage(
        Some(&grant),
        &start,
        &disk,
        expired
    ));
    assert!(
        grants.get_at(&c.jti, now()).is_none(),
        "expired consumption is still one-use"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn contended_or_poisoned_grant_lookup_never_blocks_or_refuses_compute() {
    let authority = Arc::new(Grants::new(&Server::new().config()));
    let lock = authority.records.lock().unwrap();
    assert!(disk_grant(&authority, None).is_none());
    assert!(disk_grant(&authority, Some(&crate::job::test_id(1))).is_none());
    assert!(authority.consume(&crate::job::test_id(1)).is_none());
    drop(lock);
    let poison = authority.clone();
    assert!(std::thread::spawn(move || {
        let _guard = poison.records.lock().unwrap();
        panic!("poison records");
    })
    .join()
    .is_err());
    assert!(disk_grant(&authority, Some(&crate::job::test_id(1))).is_none());
    assert!(authority.consume(&crate::job::test_id(1)).is_none());
}

#[test]
fn concurrent_admissions_share_one_fetch_and_one_immutable_grant() {
    let server = Server::new();
    let authority = Arc::new(Grants::new(&server.config()));
    *server.gate.0.lock().unwrap() = true;
    let first = authority.clone();
    let first = std::thread::spawn(move || first.admit_with_clock(&token(&claims()), now));
    server.arrived.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = authority.clone();
    let second = std::thread::spawn(move || second.admit_with_clock(&token(&claims()), now));
    *server.gate.0.lock().unwrap() = false;
    server.gate.1.notify_all();
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(authority.records.lock().unwrap().grants.len(), 1);
    assert_eq!(
        authority.get_at(&claims().jti, now()).unwrap().sub,
        claims().sub
    );
}
