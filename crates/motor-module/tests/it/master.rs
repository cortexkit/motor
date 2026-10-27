use super::*;
use crate::tests::placement::{FakeVault, SocketScratch};
use std::{fs, sync::atomic::Ordering};

/// A loopback sshd that trusts only `identity` and a fresh host key of its own.
struct LocalSshd {
    server: Child,
    port: u16,
    username: String,
    host: ssh_key::PrivateKey,
}

/// `extra` is appended to the sshd configuration.
async fn start_sshd(scratch: &Path, identity: &Path, extra: &str) -> LocalSshd {
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
    let authorized = scratch.join("authorized_keys");
    fs::copy(identity, &authorized).unwrap();
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
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPidFile {}\nLogLevel ERROR\nAllowUsers {username}\n{extra}",
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
    LocalSshd {
        server,
        port,
        username,
        host,
    }
}

#[tokio::test]
async fn two_ssh_runs_share_one_master_and_one_vault_sign() {
    let scratch = SocketScratch::new("mm");
    let vault = FakeVault::new();
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
    let LocalSshd {
        mut server,
        port,
        username,
        host,
    } = start_sshd(&scratch, &agent.identity, "").await;
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
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            reply.status.success(),
            "{}",
            String::from_utf8_lossy(&reply.stderr)
        );
        assert_eq!(reply.stdout, output.as_bytes());
        assert_eq!(
            connector.master.lock().await.as_ref().unwrap().id(),
            Some(pid)
        );
        assert_eq!(vault.signs.load(Ordering::SeqCst), 1);
    }
    // A Windows endpoint must not inherit the legacy endpoint or its master.
    connector
        .credentials
        .current
        .lock()
        .unwrap()
        .endpoint
        .as_mut()
        .unwrap()
        .hostname = "192.0.2.99".into();
    let mut windows = crate::WindowsRunner {
        name: "portable".into(),
        host: "127.0.0.1".into(),
        port,
        user: std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| {
                String::from_utf8(
                    std::process::Command::new("id")
                        .arg("-un")
                        .output()
                        .unwrap()
                        .stdout,
                )
                .unwrap()
                .trim()
                .into()
            }),
        host_key: host.public_key().to_openssh().unwrap(),
        ..Default::default()
    };
    let pin = WindowsPin::new(&windows).unwrap();
    let reply = windows_ssh_command(
        OsStr::new("ssh"),
        &windows,
        &agent.socket,
        &agent.identity,
        &pin.0,
    )
    .arg("printf windows-endpoint")
    .output()
    .await
    .unwrap();
    assert!(
        reply.status.success(),
        "Windows endpoint must not use legacy host"
    );
    assert_eq!(reply.stdout, b"windows-endpoint");
    assert_eq!(
        vault.signs.load(Ordering::SeqCst),
        2,
        "Windows auth must use the vault, not the legacy master"
    );
    let wrong_host =
        ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519).unwrap();
    windows.host_key = wrong_host.public_key().to_openssh().unwrap();
    let wrong_pin = WindowsPin::new(&windows).unwrap();
    let reply = windows_ssh_command(
        OsStr::new("ssh"),
        &windows,
        &agent.socket,
        &agent.identity,
        &wrong_pin.0,
    )
    .arg("printf must-not-run")
    .output()
    .await
    .unwrap();
    assert!(!reply.status.success(), "a wrong host pin must refuse");
    assert!(reply.stdout.is_empty());
    assert_eq!(
        vault.signs.load(Ordering::SeqCst),
        2,
        "reject the host before vault authentication"
    );
    connector.shutdown().await;
    assert!(connector.master.lock().await.is_none());
    assert!(!runtime.exists());
    assert_eq!(connector.refusal(), Some("runner_agent_stopped"));
    assert!(connector.connect(Uuid::now_v7()).await.is_err());
    assert_eq!(
        vault.signs.load(Ordering::SeqCst),
        2,
        "shutdown must fence new authentication"
    );
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}

/// One job session on the macOS endpoint, read to its end.
async fn macos_session(connector: &SshConnector, runner: &crate::MacosRunner) -> Vec<u8> {
    let mut pipe = tokio::time::timeout(
        Duration::from_secs(15),
        connector.connect_macos(Uuid::now_v7(), runner),
    )
    .await
    .unwrap()
    .unwrap();
    let mut output = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), pipe.read_to_end(&mut output))
        .await
        .unwrap()
        .unwrap();
    output
}

async fn macos_master_pid(connector: &SshConnector) -> Option<u32> {
    connector.macos_master.lock().await.as_ref()?.child.id()
}

#[tokio::test]
async fn macos_jobs_share_one_endpoint_master_and_replace_a_dead_one() {
    let scratch = SocketScratch::new("mo");
    let vault = FakeVault::new();
    let connector =
        SshConnector::initialize(Config::default(), scratch.path(), vault.resolver()).await;
    let agent = connector.agent().unwrap();
    // ForceCommand replaces the runner command, so the test never starts a
    // motor-runner that happens to be installed on this host. Where the test
    // account has no login shell, sshd still answers each session with the
    // shell's refusal, so the test compares replies rather than expecting
    // the ForceCommand output.
    let LocalSshd {
        mut server,
        port,
        username,
        host,
    } = start_sshd(
        &scratch,
        &agent.identity,
        "ForceCommand printf macos-session\n",
    )
    .await;
    let runner = crate::MacosRunner {
        host: "127.0.0.1".into(),
        port,
        user: username,
        host_key: host.public_key().to_openssh().unwrap(),
        instance: "macos".into(),
        state_root: "/srv/macos".into(),
    };
    let control = agent.control_path.with_file_name(MACOS_CONTROL_NAME);
    let runtime = control.parent().unwrap().to_path_buf();

    // Each new SSH connection authenticates through the vault once; a session
    // on an existing master's connection signs nothing. So the sign count is
    // the number of connections: two jobs, one connection.
    let reply = macos_session(&connector, &runner).await;
    assert!(!reply.is_empty());
    let first = macos_master_pid(&connector).await.unwrap();
    assert!(control.exists());
    assert_eq!(macos_session(&connector, &runner).await, reply);
    assert_eq!(macos_master_pid(&connector).await, Some(first));
    assert_eq!(vault.signs.load(Ordering::SeqCst), 1);

    // A killed master leaves its socket behind. The next job must start a new
    // master rather than trust that socket: a new master that finds the path
    // taken runs without multiplexing, and every later job would then pay
    // for its own connection.
    {
        let mut owned = connector.macos_master.lock().await;
        let master = owned.as_mut().unwrap();
        master.child.start_kill().unwrap();
        master.child.wait().await.unwrap();
    }
    assert!(control.exists(), "SIGKILL leaves a stale control socket");
    assert_eq!(macos_session(&connector, &runner).await, reply);
    let second = macos_master_pid(&connector).await.unwrap();
    assert_ne!(second, first);
    assert_eq!(macos_session(&connector, &runner).await, reply);
    assert_eq!(macos_master_pid(&connector).await, Some(second));
    assert_eq!(vault.signs.load(Ordering::SeqCst), 2);

    connector.shutdown().await;
    assert!(connector.macos_master.lock().await.is_none());
    assert!(!runtime.exists());
    server.kill().await.unwrap();
    server.wait().await.unwrap();
}

#[tokio::test]
async fn explicit_endpoint_refusals_name_the_ssh_error() {
    let scratch = SocketScratch::new("me");
    let vault = FakeVault::new();
    let mut connector =
        SshConnector::initialize(Config::default(), scratch.path(), vault.resolver()).await;
    // A fake ssh that fails the way a saturated uplink makes the real one
    // fail, after some noise, with a trailing blank line.
    let fake = scratch.join("ssh");
    fs::write(
        &fake,
        "#!/bin/sh\necho 'debug1: earlier noise' >&2\necho 'Connection timed out during banner exchange' >&2\necho >&2\nexit 255\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    connector.ssh = fake.into_os_string();
    let connector = Arc::new(connector);
    let host_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    let macos = crate::MacosRunner {
        host: "macos.example.invalid".into(),
        port: 22,
        user: "motor".into(),
        host_key: host_key.into(),
        instance: "macos".into(),
        state_root: "/srv/macos".into(),
    };
    let windows = crate::WindowsRunner {
        name: "windows".into(),
        host: "windows.example.invalid".into(),
        user: "worker".into(),
        host_key: host_key.into(),
        instance: "windows".into(),
        state_root: "/srv/windows".into(),
        ..Default::default()
    };
    let open = |target: crate::config::RunnerTarget| {
        let route = InstanceConnector {
            connector: connector.clone(),
            target,
        };
        async move { Channel::open_with_cause(&route, Uuid::now_v7()).await }
    };
    let (macos_result, windows_result) = tokio::join!(
        open(crate::config::RunnerTarget {
            instance: macos.instance.clone(),
            state_root: Some(macos.state_root.clone()),
            windows: None,
            macos: Some(macos.clone()),
        }),
        open(windows.target()),
    );
    // macOS fails while starting its master, Windows in its per-job session.
    for (result, expected) in [
        (
            macos_result,
            "ssh master exited: Connection timed out during banner exchange",
        ),
        (
            windows_result,
            "ssh exited: Connection timed out during banner exchange",
        ),
    ] {
        let Err((reason, detail)) = result else {
            panic!("a failing ssh unexpectedly completed the handshake");
        };
        eprintln!("refusal detail: {detail}");
        assert_eq!(reason, RefusalReason::Unreachable, "{detail}");
        assert!(detail.contains(expected), "{detail}");
        assert!(!detail.contains("earlier noise"), "{detail}");
    }
    connector.shutdown().await;
}
