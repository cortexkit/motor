use super::*;
use ed25519_dalek::SigningKey;
use motor_lease::{base64url, sign_token, Claims, KeySet, Purpose, RequesterJwk, ISSUER};
use motor_vm_helper::{config::Config, job::CreateWorkspace, linux::job::qualification::Fixture};
use std::{
    io::{BufRead, BufReader},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const ACCOUNT: &str = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
const HOST: &str = "cache-vm-test";

struct Broker {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Broker {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let body = serde_json::to_vec(&jwks(1)).unwrap();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stopped.load(Ordering::SeqCst) {
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
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn jwks(seed: u8) -> motor_lease::Jwks {
    use motor_lease::fake::{FakeBroker, FakeConfig};
    FakeBroker::new(
        "cache-vm-test",
        FakeConfig {
            worker_bearer: "test".into(),
            subject: ACCOUNT.into(),
            placement: motor_lease::Placement {
                host: HOST.into(),
                vm: None,
            },
            endpoint: "https://unused.invalid".into(),
            endpoint_spki_sha256: base64url(&[0; 32]),
            kid: "current".into(),
            signing_key: SigningKey::from_bytes(&[seed; 32]),
            announced_keys: vec![],
        },
    )
    .unwrap()
    .jwks()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims(id: uuid::Uuid) -> Claims {
    serde_json::from_value(json!({
        "iss":ISSUER,"aud":HOST,"jti":id,"sub":ACCOUNT,"purpose":"run",
        "iat":now(),"exp":now()+300,"vcpus":2,"mem_mib":4096,
        "platform":"linux-x64","net":false,"placement":{"host":HOST,"vm":null},
        "max_s":60,"cnf":{"jkt":RequesterJwk::from_key(&SigningKey::from_bytes(&[3;32]).verifying_key()).thumbprint().unwrap()},
        "workspace_key_hash":base64url(&[5;32]),"cache_write":true
    })).unwrap()
}

fn token(claims: &Claims, seed: u8) -> String {
    sign_token(claims, "current", &SigningKey::from_bytes(&[seed; 32])).unwrap()
}

pub(super) struct Prepared {
    pub id: uuid::Uuid,
    fixture: Fixture,
    case: &'static str,
    writable: AtomicBool,
    _scratch: tempfile::TempDir,
}

impl Prepared {
    fn new(broker: &Broker, case: &'static str) -> Self {
        let scratch = tempfile::Builder::new()
            .prefix("job-cache-authority-")
            .tempdir_in(cortexkit_test_support::scratch_root())
            .unwrap();
        let mut config: Config = serde_json::from_str(include_str!(
            "../../../../motor-vm-helper/fixtures/helper.json"
        ))
        .unwrap();
        config.host_id = Some(HOST.into());
        config.jwks_url = Some(format!("http://{}/.well-known/jwks.json", broker.address));
        config.job_vms = Some(
            serde_json::from_value(json!({
                "uid_base":900000,"uid_count":1,"memory_max_bytes":4u64<<30,
                "workspace_disk_max_bytes":4u64<<30,
                "fixed_path_repositories":{ACCOUNT:["/home/motor-contract"]}
            }))
            .unwrap(),
        );
        let fixture = Fixture::new(&config, scratch.path()).unwrap();
        let id = uuid::Uuid::now_v7();
        let mut c = claims(id);
        let create = |selector| {
            fixture
                .create(CreateWorkspace {
                    account: ACCOUNT.into(),
                    workspace_disk_id: "contract".into(),
                    size_bytes: 4u64 << 30,
                    cache_write_job_id: selector,
                })
                .unwrap()
        };
        if case == "legacy" {
            create(None);
        }
        if case == "other-workspace" {
            let mut prior = claims(uuid::Uuid::now_v7());
            prior.workspace_key_hash = Some(base64url(&[6; 32]));
            fixture.admit(&token(&prior, 1)).unwrap();
            create(Some(prior.jti));
        }
        match case {
            "wrong-audience" => c.aud = "another-host".into(),
            "expired-admission" => {
                c.iat = now() - 600;
                c.exp = now() - 300;
            }
            "expired-queued" => c.exp = now() + 30,
            _ => {}
        }
        let seed = if matches!(case, "forged" | "host-only-keys") {
            2
        } else {
            1
        };
        let signed = token(&c, seed);
        if case == "host-only-keys" {
            let mut host_keys = KeySet::default();
            host_keys.refresh(Ok(jwks(2))).unwrap();
            assert!(motor_lease::verify_token(
                &signed,
                &host_keys,
                HOST,
                None,
                Purpose::Run,
                now()
            )
            .is_ok());
            // Host verification is deliberately not a key source for the helper.
        }
        let admitted = fixture.admit(&signed);
        let valid = !matches!(
            case,
            "forged" | "wrong-audience" | "expired-admission" | "host-only-keys"
        );
        assert_eq!(admitted.is_ok(), valid, "{case}: {admitted:?}");
        create(Some(c.jti.clone()));
        if case == "expired-queued" {
            let deadline = Instant::now() + Duration::from_secs(35);
            while now() <= c.exp {
                assert!(
                    Instant::now() < deadline,
                    "test clock did not reach token expiry"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let writable = matches!(case, "valid" | "replay");
        let set: motor_protocol::RemoteCacheRequest = serde_json::from_value(json!({
            "op":"set_remote_cache",
            "config":json!({"generation":"test","tenant":ACCOUNT,"endpoint":"https://127.0.0.1:9","region":"test","bucket":"compile","prefix":"tenant/acct-01ARZ3NDEKTSV4RRFFQ69G5FAV/","storage_class":"STANDARD","enabled":true}).to_string(),
            "read_key":json!({"generation":"test","access_key_id":"read-id","secret_access_key":"READ-SECRET"}).to_string(),
            "write_key": if case == "flag-without-key" { None } else { Some(json!({"generation":"test","access_key_id":"write-id","secret_access_key":"WRITE-SECRET"}).to_string()) }
        })).unwrap();
        fixture.install(ACCOUNT, &set).unwrap();
        let id = if case == "other-job" {
            uuid::Uuid::now_v7()
        } else {
            id
        };
        Self {
            id,
            fixture,
            case,
            writable: AtomicBool::new(writable),
            _scratch: scratch,
        }
    }

    pub fn boot(
        &self,
        start: &Start,
        root: &Path,
        uid: u32,
        tap: u8,
    ) -> (File, motor_vm_helper::linux::job::BootRequests) {
        let mut start = start.clone();
        start.cache_write_blocked = self.case == "blocked";
        let (disk, mut requests) = self
            .fixture
            .boot(&start, &File::open(root).unwrap(), uid, tap)
            .unwrap();
        for (path, body) in &mut requests {
            if *path == "/boot-source" {
                let args = body["boot_args"].as_str().unwrap();
                if self.case == "flag-without-key" {
                    body["boot_args"] = format!("{args} motor.cache_write=1").into();
                } else if self.case == "key-without-flag" {
                    body["boot_args"] = args.replace(" motor.cache_write=1", "").into();
                }
            }
        }
        (disk, requests)
    }

    pub fn probe(&self) -> String {
        let writable = self.writable.load(Ordering::SeqCst);
        format!(
            r#"import os, socket
assert os.environ['SCCACHE_S3_RW_MODE'] == '{mode}', dict(os.environ)
assert 'SCCACHE_RECACHE' not in os.environ
assert 'SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY' not in os.environ
s = socket.socket(socket.AF_UNIX)
s.settimeout(10)
s.connect('/motor-gateway/sccache.sock')
s.sendall(b'PUT /compile/probe HTTP/1.1\r\nHost: cache\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx')
head = s.makefile('rb').readline(4097)
assert head.split()[1] == b'{status}', head
s.close()
assert set(os.listdir('/motor-gateway')) <= {{'sccache.sock', 'ccache.sock'}}
if os.path.exists('/motor-gateway/ccache.sock'):
    c = socket.socket(socket.AF_UNIX)
    c.settimeout(10)
    c.connect('/motor-gateway/ccache.sock')
    c.sendall(b'PUT /probe HTTP/1.1\r\nHost: cache\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx')
    assert c.makefile('rb').readline(4097).split()[1] == b'403'
    c.close()
assert not os.access('/run/motor/cache/write-key', os.R_OK)
print('PASS job cache: {case} {mode} PUT {status}')
"#,
            mode = if writable { "READ_WRITE" } else { "READ_ONLY" },
            status = if writable { 503 } else { 403 },
            case = self.case
        )
    }
}

/// Boots real production guests, but uses the helper's root-only scratch seam
/// rather than live helper IPC/systemd Start. The socket's 503 proves a Job PUT
/// was admitted (the fixture's upstream is closed); a read binding returns 403.
#[test]
#[ignore = "requires root, KVM, private cgroup/netns, Firecracker/jailer and current Job image artifacts"]
fn isolated_job_cache_write_authority_and_guest_binding() {
    assert_eq!(unsafe { libc::geteuid() }, 0, "run as the root operator");
    let broker = Broker::new();
    for case in [
        "valid",
        "expired-queued",
        "forged",
        "wrong-audience",
        "expired-admission",
        "host-only-keys",
        "other-workspace",
        "other-job",
        "legacy",
        "blocked",
        "flag-without-key",
        "key-without-flag",
        "replay",
    ] {
        println!("real Job VM cache case: {case}");
        let prepared = std::rc::Rc::new(Prepared::new(&broker, case));
        isolated_vm(Probe::Cache(prepared.clone()));
        if case == "replay" {
            prepared.writable.store(false, Ordering::SeqCst);
            isolated_vm(Probe::Cache(prepared));
        }
    }
}
