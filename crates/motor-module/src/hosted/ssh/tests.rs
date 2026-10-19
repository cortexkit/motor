use super::*;
use cortexkit_test_support::ScratchDir;
use tokio::net::TcpListener;

pub(crate) struct Sshd {
    _scratch: ScratchDir,
    pub endpoint: String,
    pub host_key: String,
    pub user: String,
    authorized: PathBuf,
    _child: Child,
}
impl Sshd {
    pub(crate) async fn new() -> Self {
        let scratch = ScratchDir::new("boat-sshd");
        let key = ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519)
            .unwrap();
        let host = scratch.join("host");
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            &host,
            key.to_openssh(ssh_key::LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o600)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let user = String::from_utf8(
            std::process::Command::new("id")
                .arg("-un")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let authorized = scratch.join("authorized_keys");
        fs::write(&authorized, "").unwrap();
        let config = scratch.join("sshd_config");
        fs::write(&config, format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPidFile {}\nLogLevel ERROR\nAllowUsers {user}\n", host.display(), authorized.display(), scratch.join("pid").display())).unwrap();
        let mut child = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    let mut message = String::new();
                    child
                        .stderr
                        .take()
                        .unwrap()
                        .read_to_string(&mut message)
                        .await
                        .unwrap();
                    panic!("sshd fixture exited {status}: {message}");
                }
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        Self {
            endpoint: format!("127.0.0.1:{port}"),
            host_key: key.public_key().to_openssh().unwrap(),
            user,
            authorized,
            _scratch: scratch,
            _child: child,
        }
    }
    pub(crate) fn authorize(&self, key: &SigningKey, port: u16) {
        self.authorize_public(
            &motor_lease::RequesterJwk::from_key(&key.verifying_key()),
            port,
        );
    }
    pub(crate) fn authorize_public(&self, jwk: &motor_lease::RequesterJwk, port: u16) {
        let key = jwk.verifying_key().unwrap();
        let public = ssh_key::PublicKey::from(ssh_key::public::Ed25519PublicKey(key.to_bytes()));
        fs::write(&self.authorized, format!("restrict,port-forwarding,command=\"/bin/false\",permitopen=\"127.0.0.1:{port}\" {}\n", public.to_openssh().unwrap())).unwrap();
    }
    pub(crate) fn lease(&self, port: u16) -> Lease {
        serde_json::from_value(serde_json::json!({
            "provider":"boat", "ssh":{"endpoint":self.endpoint,"host_key":self.host_key,"user":self.user}, "runner_port":port,
            "lease_id":"fixture", "job_id":motor_protocol::Uuid::now_v7(), "placement":{"host":"boat:test", "vm":"test"},
            "job_token":"unused", "token_expires_at_ms":u64::MAX, "max_s":30,"reserved_vcpu_ms":60000
        })).unwrap()
    }
}

#[test]
fn lease_identity_is_private_and_removed_on_drop_and_failure() {
    use std::os::unix::fs::PermissionsExt;
    let runtime = Runtime::new(&SigningKey::from_bytes(&[9; 32])).unwrap();
    let root = runtime.root.clone();
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(root.join("identity"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let identity = ssh_key::PrivateKey::read_openssh_file(&root.join("identity")).unwrap();
    assert_eq!(
        identity.public_key().key_data().ed25519().unwrap().as_ref(),
        SigningKey::from_bytes(&[9; 32]).verifying_key().as_bytes()
    );
    assert!(runtime.write("identity", b"overwrite").is_err());
    drop(runtime);
    assert!(!root.exists());
}

#[tokio::test]
async fn pinned_forward_echo_and_identity_cleanup_after_run_and_key_mismatch() {
    use tokio::io::AsyncWriteExt;
    let server = Sshd::new().await;
    let key = SigningKey::from_bytes(&[5; 32]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    server.authorize(&key, port);
    let lease = server.lease(port);
    let echo = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4];
        socket.read_exact(&mut bytes).await.unwrap();
        socket.write_all(&bytes).await.unwrap();
    });
    let runtime = Runtime::new(&key).unwrap();
    let root = runtime.root.clone();
    let mut pipe = connect_runtime(&lease, runtime).await.unwrap();
    pipe.write_all(b"echo").await.unwrap();
    let mut bytes = [0; 4];
    pipe.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"echo");
    echo.await.unwrap();
    drop(pipe);
    assert!(!root.exists());
    let wrong = Sshd::new().await;
    let mut lease = lease;
    lease.ssh.as_mut().unwrap().host_key = wrong.host_key.clone();
    let runtime = Runtime::new(&key).unwrap();
    let root = runtime.root.clone();
    assert_eq!(
        connect_runtime(&lease, runtime).await.err(),
        Some(Error::SshKey)
    );
    assert!(!root.exists());
}
