use super::*;
use crate::tests::placement::{FakeVault, SocketScratch};
use std::{fs, sync::atomic::Ordering};

#[tokio::test]
async fn two_ssh_runs_share_one_master_and_one_vault_sign() {
    let scratch = SocketScratch::new("mm");
    let vault = FakeVault::new();
    // Host-server fixture material stays outside motor's public-only runtime.
    let host =
        ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519).unwrap();
    let host_path = scratch.join("host");
    fs::write(
        &host_path,
        host.to_openssh(ssh_key::LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&host_path, fs::Permissions::from_mode(0o600)).unwrap();
    let connector =
        SshConnector::initialize(Config::default(), scratch.path(), vault.resolver()).await;
    assert_eq!(connector.refusal(), None);
    let agent = connector
        .credentials
        .current
        .lock()
        .unwrap()
        .agent
        .clone()
        .unwrap();
    let authorized = scratch.join("authorized_keys");
    fs::copy(&agent.identity, &authorized).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    // CI containers often run without USER set; ask the system for the login name.
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| {
            let out = std::process::Command::new("id")
                .arg("-un")
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        });
    let config = scratch.join("sshd_config");
    fs::write(&config, format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPidFile {}\nLogLevel ERROR\nAllowUsers {username}\n",
        host_path.display(), authorized.display(), scratch.join("pid").display()
    )).unwrap();
    let mut server = Command::new("/usr/sbin/sshd")
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
            if let Some(status) = server.try_wait().unwrap() {
                let mut diagnostic = String::new();
                server
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut diagnostic)
                    .await
                    .unwrap();
                panic!("local SSH server exited {status}: {diagnostic}");
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
    let control_path = agent.control_path.clone();
    let runtime = control_path.parent().unwrap().to_path_buf();
    // Trust only this fixture's public host key. -F /dev/null remains in use.
    let known_hosts = scratch.join("known_hosts");
    fs::write(
        &known_hosts,
        format!(
            "[127.0.0.1]:{port} {}\n",
            host.public_key().to_openssh().unwrap()
        ),
    )
    .unwrap();
    connector.credentials.current.lock().unwrap().endpoint = Some(SshEndpoint {
        hostname: "127.0.0.1".into(),
        user: username,
        port: port.to_string(),
        known_hosts: Some(known_hosts),
    });
    if let Err(error) = connector.ensure_master().await {
        server.kill().await.unwrap();
        let mut diagnostic = String::new();
        server
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut diagnostic)
            .await
            .unwrap();
        panic!("SSH master failed: {error}; local server: {diagnostic}");
    }
    let pid = connector
        .master
        .lock()
        .await
        .as_ref()
        .unwrap()
        .id()
        .unwrap();
    for output in ["first-run", "second-run"] {
        connector.ensure_master().await.unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            connector
                .ssh_command()
                .arg("alfonso-runner")
                .arg(format!("printf {output}"))
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(reply.status.success());
        assert_eq!(reply.stdout, output.as_bytes());
        assert_eq!(
            connector.master.lock().await.as_ref().unwrap().id(),
            Some(pid)
        );
        assert_eq!(vault.signs.load(Ordering::SeqCst), 1);
    }
    connector.shutdown().await;
    assert!(connector.master.lock().await.is_none());
    assert!(!runtime.exists());
    assert_eq!(connector.refusal(), Some("runner_agent_stopped"));
    assert!(connector.connect(Uuid::now_v7()).await.is_err());
    assert_eq!(
        vault.signs.load(Ordering::SeqCst),
        1,
        "shutdown must fence new authentication"
    );
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}
