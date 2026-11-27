use super::*;
use crate::config::RunnerTarget;
use crate::tests::placement::{FakeVault, SocketScratch};
use std::{fs, os::unix::fs::PermissionsExt};

const SSH: &str = r#"#!/bin/sh
set -eu
root=${0%/*}
master=0
log=''
while [ "$#" -gt 0 ]; do
    arg=$1
    shift
    case "$arg" in
        -N) master=1 ;;
        -E) log=$1; shift ;;
        ControlPath=*) control=${arg#ControlPath=} ;;
    esac
done
if [ "$master" = 1 ]; then
    if [ -n "$log" ] && [ -f "$root/diagnostic" ]; then
        cat "$root/diagnostic" >> "$log"
    fi
    printf '%s\n' "$$" >> "$root/masters"
    : > "$control"
    exec sleep 300
fi
job=${arg##* }
if [ -f "$root/echo-$job" ]; then
    while IFS= read -r line; do printf '%s\n' "$line"; done
elif [ -f "$root/fail-sessions" ]; then
    IFS= read -r line
    exit 255
elif [ -f "$root/stall-$job" ] && [ "$(wc -l < "$root/masters")" -eq 1 ]; then
    : > "$root/stalled-$job"
    exec sleep 300
else
    cat "$root/reply"
    exec sleep 300
fi
"#;

struct FakeSsh {
    connector: Arc<SshConnector>,
    scratch: SocketScratch,
    runner: crate::MacosRunner,
}

impl FakeSsh {
    async fn new() -> Self {
        let scratch = SocketScratch::new("mx");
        let vault = FakeVault::new();
        let mut connector =
            SshConnector::initialize(Config::default(), scratch.path(), vault.resolver()).await;
        assert_eq!(connector.macos_refusal(), None);
        let ssh = scratch.join("ssh");
        fs::write(&ssh, SSH).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        connector.ssh = ssh.into_os_string();
        let reply = Frame::Hello {
            return_paths: false,
            return_chunked_max_bytes: None,
            return_chunked: false,
            sibling_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: true,
            gate_jobs: false,
            platforms: vec!["fake-macos".into()],
        };
        let mut bytes = Vec::new();
        write_active(&mut bytes, &reply).await.unwrap();
        fs::write(scratch.join("reply"), bytes).unwrap();
        Self {
            connector: Arc::new(connector),
            scratch,
            runner: crate::MacosRunner {
                host: "fake-macos.invalid".into(),
                port: 22,
                user: "motor".into(),
                host_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl".into(),
                instance: "macos".into(),
                state_root: "/state".into(),
            },
        }
    }

    fn route(&self) -> InstanceConnector {
        InstanceConnector {
            connector: self.connector.clone(),
            target: RunnerTarget {
                instance: self.runner.instance.clone(),
                state_root: Some(self.runner.state_root.clone()),
                windows: None,
                macos: Some(self.runner.clone()),
            },
        }
    }

    async fn open(&self, id: Uuid) -> Channel {
        let channel = Channel::open_with_cause(&self.route(), id).await.unwrap();
        assert_eq!(channel.platforms, ["fake-macos"]);
        channel
    }

    async fn pid(&self) -> Option<u32> {
        self.connector
            .macos_master
            .lock()
            .await
            .as_ref()?
            .child
            .id()
    }

    fn control(&self) -> PathBuf {
        self.connector
            .agent()
            .unwrap()
            .control_path
            .with_file_name(MACOS_CONTROL_NAME)
    }

    async fn wait_for_retirement(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if self.connector.macos_master.lock().await.is_none() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the old master must be killed and reaped without waiting for a retry");
        assert!(
            !self.control().exists(),
            "retirement must remove the stale socket"
        );
    }

    async fn begin_stuck_hello(
        &self,
        id: Uuid,
    ) -> JoinHandle<Result<Channel, (RefusalReason, String)>> {
        fs::write(self.scratch.join(format!("stall-{id}")), "").unwrap();
        let route = self.route();
        let stuck = tokio::spawn(async move { Channel::open_with_cause(&route, id).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.scratch.join(format!("stalled-{id}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake SSH session did not stall");
        stuck
    }
}

async fn finish_timeout(stuck: JoinHandle<Result<Channel, (RefusalReason, String)>>) {
    // Let the OS child enter its stall before advancing the in-process clock.
    tokio::time::pause();
    tokio::time::advance(HANDSHAKE_TIMEOUT).await;
    let Err((reason, cause)) = stuck.await.unwrap() else {
        panic!("stuck ssh unexpectedly replied to Hello");
    };
    tokio::time::resume();
    assert_eq!(reason, RefusalReason::Unreachable);
    assert!(cause.contains("still waiting in hello_recv"), "{cause}");
}

#[tokio::test]
async fn exited_master_is_replaced_before_the_next_session() {
    let ssh = FakeSsh::new().await;
    drop(ssh.open(Uuid::now_v7()).await);
    let old = ssh.pid().await.unwrap();
    ssh.connector
        .macos_master
        .lock()
        .await
        .as_mut()
        .unwrap()
        .child
        .kill()
        .await
        .unwrap();

    drop(ssh.open(Uuid::now_v7()).await);
    assert_ne!(ssh.pid().await.unwrap(), old);
    ssh.connector.shutdown().await;
}

#[tokio::test]
async fn stuck_master_hello_timeout_reaps_it_before_retry() {
    let ssh = FakeSsh::new().await;
    let id = Uuid::now_v7();
    let stuck = ssh.begin_stuck_hello(id).await;
    let old = ssh
        .pid()
        .await
        .expect("the stuck master must still be alive");
    finish_timeout(stuck).await;
    ssh.wait_for_retirement().await;

    drop(ssh.open(id).await);
    assert_ne!(ssh.pid().await.unwrap(), old);
    ssh.connector.shutdown().await;
}

#[tokio::test]
async fn another_session_making_progress_vetoes_hello_timeout_replacement() {
    let ssh = FakeSsh::new().await;
    let echo_job = Uuid::now_v7();
    fs::write(ssh.scratch.join(format!("echo-{echo_job}")), "").unwrap();
    let mut peer = ssh.route().connect(echo_job).await.unwrap();
    let old = ssh.pid().await.unwrap();
    let stuck = ssh.begin_stuck_hello(Uuid::now_v7()).await;

    // Deliver progress after the stuck session opened, not just an old reply
    // from before the suspect period. The same peer must still work afterward.
    async fn echo(peer: &mut dyn Duplex, text: &[u8; 13]) {
        peer.write_all(text).await.unwrap();
        let mut reply = [0; 13];
        tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply, text);
    }
    echo(peer.as_mut(), b"still active\n").await;
    finish_timeout(stuck).await;
    drop(ssh.open(Uuid::now_v7()).await);
    assert_eq!(ssh.pid().await, Some(old));
    echo(peer.as_mut(), b"still usable\n").await;
    ssh.connector.shutdown().await;
}

#[tokio::test]
async fn overlapping_ssh_failures_retire_the_master_but_one_does_not() {
    let ssh = FakeSsh::new().await;
    fs::write(ssh.scratch.join("fail-sessions"), "").unwrap();
    let failed = |pipe: Box<dyn Duplex>| async move {
        let mut pipe = pipe;
        pipe.write_all(b"exit\n").await.unwrap();
        let mut tail = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), pipe.read_to_end(&mut tail))
            .await
            .unwrap()
            .unwrap();
        assert!(tail.is_empty());
        pipe
    };

    let first = failed(ssh.route().connect(Uuid::now_v7()).await.unwrap()).await;
    let old = ssh.pid().await.unwrap();
    let second = ssh.route().connect(Uuid::now_v7()).await.unwrap();
    assert_eq!(ssh.pid().await, Some(old));
    let third = ssh.route().connect(Uuid::now_v7()).await.unwrap();
    let remaining = tokio::join!(failed(second), failed(third));
    ssh.wait_for_retirement().await;
    drop((first, remaining));
    ssh.connector.shutdown().await;
}

#[tokio::test]
async fn master_exit_logs_status_and_its_sanitized_file_tail_once() {
    let ssh = FakeSsh::new().await;
    let log = crate::logging::Captured::default();
    let _guard = crate::logging::scoped(log.clone());
    fs::write(
        ssh.scratch.join("diagnostic"),
        format!(
            "old noise {}\nnetwork closed\x1b\nlast message\n",
            "x".repeat(70 * 1024)
        ),
    )
    .unwrap();
    drop(ssh.open(Uuid::now_v7()).await);
    ssh.connector
        .macos_master
        .lock()
        .await
        .as_mut()
        .unwrap()
        .child
        .kill()
        .await
        .unwrap();
    ssh.wait_for_retirement().await;

    let text = log.text();
    let lines: Vec<_> = text
        .lines()
        .filter(|line| line.contains("macOS SSH master exited"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(lines[0].contains("status=signal: 9"), "{}", lines[0]);
    let (_, tail) = lines[0].split_once(" ssh_stderr_tail=").unwrap();
    let tail: Vec<String> = serde_json::from_str(tail).unwrap();
    assert!(
        tail.ends_with(&["network closed".into(), "last message".into()]),
        "{tail:?}"
    );
    assert!(!lines[0].contains("old noise"));
    let previous = fs::read(ssh.scratch.join("macos-ssh-master.previous.log")).unwrap();
    assert_eq!(previous.len(), 64 * 1024);
    assert!(previous.ends_with(b"network closed\x1b\nlast message\n"));
    ssh.connector.shutdown().await;
}
