use crate::diagnostics::{record_unreachable, UnreachableStep};
use crate::{agent::VaultAgent, Config};
use cortexkit_claustrum_client::{CredentialErrorClass, CredentialResolver};
use motor_protocol::*;
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::{mpsc, oneshot, watch, Mutex},
    task::JoinHandle,
};

pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

#[derive(Debug)]
pub(crate) struct HostedHelloRefusal(pub(crate) String);
impl std::fmt::Display for HostedHelloRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("host refused tenant runner initialization")
    }
}
impl std::error::Error for HostedHelloRefusal {}
type ConnectFuture<'a> = Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + 'a>>;

pub trait Connector: Send + Sync {
    fn refusal(&self) -> Option<&'static str> {
        None
    }
    fn windows_refusal(&self) -> Option<&'static str> {
        self.refusal()
    }
    fn connect(&self, id: Uuid) -> ConnectFuture<'_>;
    fn connect_to<'a>(
        &'a self,
        id: Uuid,
        _instance: &'a str,
        state_root: Option<&'a str>,
    ) -> ConnectFuture<'a> {
        if state_root.is_some() {
            return Box::pin(async {
                Err(io::Error::other(
                    "connector does not support instance routing",
                ))
            });
        }
        self.connect(id)
    }
    fn connect_windows<'a>(
        &'a self,
        id: Uuid,
        runner: &'a crate::WindowsRunner,
    ) -> ConnectFuture<'a> {
        self.connect_to(id, &runner.instance, Some(&runner.state_root))
    }
}

pub(crate) struct InstanceConnector {
    pub connector: Arc<dyn Connector>,
    pub target: crate::config::RunnerTarget,
}
impl Connector for InstanceConnector {
    fn refusal(&self) -> Option<&'static str> {
        if self.target.windows.is_some() {
            self.connector.windows_refusal()
        } else {
            self.connector.refusal()
        }
    }
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        if let Some(runner) = &self.target.windows {
            self.connector.connect_windows(id, runner)
        } else {
            self.connector
                .connect_to(id, &self.target.instance, self.target.state_root.as_deref())
        }
    }
}

pub struct SshConnector {
    config: Config,
    // The foreground master is killed on drop, before the agent is retired.
    master: Mutex<Option<Child>>,
    credentials: Arc<CredentialState>,
    retry: Option<JoinHandle<()>>,
}
struct Credentials {
    agent: Option<Arc<VaultAgent>>,
    failure: Option<&'static str>,
    endpoint: Option<SshEndpoint>,
}
struct CredentialState {
    current: StdMutex<Credentials>,
    source: Option<(PathBuf, Arc<dyn CredentialResolver>)>,
    initializing: Mutex<()>,
    stopped: AtomicBool,
}
impl CredentialState {
    fn unresolved(&self) -> bool {
        matches!(
            self.current.lock().unwrap().failure,
            Some(
                "runner_credential_unavailable"
                    | "runner_agent_start_failed"
                    | "runner_credential_missing"
                    | "runner_credential_grant_refused"
            )
        )
    }
    async fn refresh(&self) {
        // Background and demand-driven attempts share a lock: never start two
        // agents or let an older credential failure overwrite a newer success.
        let _attempt = self.initializing.lock().await;
        if self.stopped.load(Ordering::Acquire) || !self.unresolved() {
            return;
        }
        let Some((root, resolver)) = &self.source else {
            return;
        };
        let failure = match resolver.public_key(RUNNER_CREDENTIAL_ID).await {
            Ok(key) => match VaultAgent::start(root, key, Arc::clone(resolver)).await {
                Ok(agent) => {
                    let endpoint = SshEndpoint::resolve().await;
                    let mut current = self.current.lock().unwrap();
                    current.agent = Some(Arc::new(agent));
                    match endpoint {
                        Ok(endpoint) => {
                            current.endpoint = Some(endpoint);
                            current.failure = None;
                        }
                        Err(_) => current.failure = Some("runner_ssh_config_invalid"),
                    }
                    return;
                }
                Err(_) => "runner_agent_start_failed",
            },
            Err(error) => match error.class {
                CredentialErrorClass::CredentialAbsent => "runner_credential_missing",
                CredentialErrorClass::Unauthorized | CredentialErrorClass::AuthRequired => {
                    "runner_credential_grant_refused"
                }
                _ => "runner_credential_unavailable",
            },
        };
        self.current.lock().unwrap().failure = Some(failure);
    }
}
struct SshEndpoint {
    hostname: String,
    user: String,
    port: String,
    #[cfg(test)]
    known_hosts: Option<std::path::PathBuf>,
}
impl SshEndpoint {
    async fn resolve() -> io::Result<Self> {
        // Read only non-secret connection settings for the existing host alias.
        // The actual SSH connection uses no user config or disk identities.
        let mut command = Command::new("ssh");
        command
            .args(["-G", "alfonso-runner"])
            .env_remove("SUBC_LAUNCH_NONCE")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(3), command.output())
            .await
            .map_err(io::Error::other)??;
        if !output.status.success() {
            return Err(io::Error::other("cannot resolve runner SSH alias"));
        }
        let text = std::str::from_utf8(&output.stdout).map_err(io::Error::other)?;
        let field = |name: &str| -> io::Result<String> {
            text.lines()
                .find_map(|line| line.strip_prefix(&format!("{name} ")))
                .map(str::to_owned)
                .ok_or_else(|| io::Error::other("missing SSH endpoint setting"))
        };
        Ok(Self {
            hostname: field("hostname")?,
            user: field("user")?,
            port: field("port")?,
            #[cfg(test)]
            known_hosts: None,
        })
    }
}
pub const RUNNER_CREDENTIAL_ID: &str = "signing:ck-motor-ssh:1";
pub const RUNNER_CREDENTIAL_CATEGORY: &str = "exec-remote-runner";
impl SshConnector {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            master: Mutex::new(None),
            credentials: Arc::new(CredentialState {
                current: StdMutex::new(Credentials {
                    agent: None,
                    failure: Some("runner_credential_unavailable"),
                    endpoint: None,
                }),
                source: None,
                initializing: Mutex::new(()),
                stopped: AtomicBool::new(false),
            }),
            retry: None,
        }
    }
    pub(crate) async fn initialize(
        config: Config,
        root: &Path,
        resolver: Arc<dyn CredentialResolver>,
    ) -> Self {
        // The override is deliberately limited to non-main test instances by
        // Config::validate. It may name a fixture identity instead of Claustrum.
        let mut connector = Self::new(config);
        if connector.config.ssh_config_override.is_some()
            && connector.config.runner_instance != "main"
        {
            connector.credentials.current.lock().unwrap().failure = None;
            return connector;
        }
        Arc::get_mut(&mut connector.credentials).unwrap().source =
            Some((root.to_path_buf(), resolver));
        connector.credentials.refresh().await;
        if connector.credentials.unresolved() {
            let credentials = Arc::downgrade(&connector.credentials);
            connector.retry = Some(tokio::spawn(async move {
                let mut delay = Duration::from_secs(1);
                loop {
                    tokio::time::sleep(delay).await;
                    let Some(credentials) = credentials.upgrade() else {
                        break;
                    };
                    if credentials.stopped.load(Ordering::Acquire) || !credentials.unresolved() {
                        break;
                    }
                    // Missing enrollment and refused grants remain failing until
                    // an operator repairs them, but must not require a restart.
                    credentials.refresh().await;
                    if !credentials.unresolved() {
                        break;
                    }
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }));
        }
        connector
    }
    pub(crate) async fn shutdown(&self) {
        // Fence new connects before waiting for a concurrent master creation.
        self.credentials.stopped.store(true, Ordering::Release);
        if let Some(retry) = &self.retry {
            retry.abort();
        }
        let _attempt = self.credentials.initializing.lock().await;
        // Never leave an authenticated connection alive after removing its signer.
        if let Some(mut master) = self.master.lock().await.take() {
            let _ = master.kill().await;
            let _ = master.wait().await;
        }
        let agent = self.credentials.current.lock().unwrap().agent.clone();
        if let Some(agent) = agent {
            agent.shutdown().await;
        }
    }
    fn ssh_command(&self) -> Command {
        let mut cmd = Command::new("ssh");
        if let Some(path) = &self.config.ssh_config_override {
            cmd.arg("-F").arg(path);
        }
        cmd.args([
            "-o",
            "ConnectTimeout=10",
            "-o",
            "BatchMode=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "IdentitiesOnly=yes",
        ]);
        let current = self.credentials.current.lock().unwrap();
        if let Some(agent) = &current.agent {
            // Ignore ambient user configuration identities. A public-only file
            // selects exactly the public identity offered by the vault agent.
            cmd.arg("-F")
                .arg("/dev/null")
                .arg("-o")
                .arg(format!("IdentityAgent={}", agent.socket.display()))
                .arg("-o")
                .arg("ControlMaster=auto")
                .arg("-o")
                .arg("ControlPersist=no")
                .arg("-o")
                .arg(format!("ControlPath={}", agent.control_path.display()))
                .arg("-i")
                .arg(&agent.identity);
            if let Some(endpoint) = &current.endpoint {
                cmd.arg("-o")
                    .arg(format!("HostName={}", endpoint.hostname))
                    .arg("-o")
                    .arg(format!("User={}", endpoint.user))
                    .arg("-p")
                    .arg(&endpoint.port);
                #[cfg(test)]
                if let Some(path) = &endpoint.known_hosts {
                    cmd.arg("-o")
                        .arg(format!("UserKnownHostsFile={}", path.display()));
                }
            }
        } else {
            cmd.args([
                "-o",
                "IdentityAgent=none",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
            ]);
        }
        cmd.env_remove("SSH_AUTH_SOCK")
            .env_remove("SSH_AGENT_PID")
            .env_remove("SUBC_LAUNCH_NONCE");
        cmd
    }
    pub fn command(&self, id: Uuid) -> Command {
        self.command_for(id, &self.config.runner_instance, None)
    }
    pub(crate) fn command_for(
        &self,
        id: Uuid,
        instance: &str,
        state_root: Option<&str>,
    ) -> Command {
        let mut cmd = self.ssh_command();
        let state_root = state_root
            .map(|root| format!(" --state-root {root}"))
            .unwrap_or_default();
        cmd.arg("alfonso-runner").arg(format!(
            "~/.cargo/bin/motor-runner connect --instance {instance}{state_root} --job {id}"
        ));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd
    }
    async fn ensure_master(&self) -> io::Result<()> {
        let agent = self.credentials.current.lock().unwrap().agent.clone();
        let Some(agent) = agent else {
            return Ok(());
        };
        let mut owned = self.master.lock().await;
        if let Some(reason) = self.refusal() {
            return Err(io::Error::other(reason));
        }
        if let Some(master) = owned.as_mut() {
            if master.try_wait()?.is_none() {
                return Ok(());
            }
            *owned = None;
            let _ = std::fs::remove_file(&agent.control_path);
        }
        // A foreground -M master is motor-owned, unlike a detached ControlPersist
        // process. Serialize its creation so concurrent jobs share one auth/sign.
        let mut command = Command::new("ssh");
        let _ = std::fs::remove_file(&agent.control_path);
        command.args(["-M", "-N"]);
        let settings = self.ssh_command();
        command.args(settings.as_std().get_args());
        command
            .arg("alfonso-runner")
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("SSH_AGENT_PID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut master = command.spawn()?;
        let ready = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if master.try_wait()?.is_some() {
                    return Err(io::Error::other(
                        self.refusal().unwrap_or("runner_master_failed"),
                    ));
                }
                if agent.control_path.exists() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("runner_master_timeout")));
        if let Err(error) = ready {
            let _ = master.kill().await;
            let _ = master.wait().await;
            return Err(error);
        }
        *owned = Some(master);
        Ok(())
    }
}

impl Drop for SshConnector {
    fn drop(&mut self) {
        self.credentials.stopped.store(true, Ordering::Release);
        if let Some(retry) = &self.retry {
            retry.abort();
        }
    }
}

struct SshPipe {
    read: tokio::process::ChildStdout,
    write: tokio::process::ChildStdin,
    _child: Child,
    _pin: Option<WindowsPin>,
}
impl AsyncRead for SshPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}
impl AsyncWrite for SshPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        Pin::new(&mut self.write).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}
impl Connector for SshConnector {
    fn windows_refusal(&self) -> Option<&'static str> {
        match self.refusal() {
            Some("runner_ssh_config_invalid") => self
                .credentials
                .current
                .lock()
                .unwrap()
                .agent
                .as_ref()
                .and_then(|agent| agent.refusal()),
            reason => reason,
        }
    }
    fn refusal(&self) -> Option<&'static str> {
        if self.credentials.stopped.load(Ordering::Acquire) {
            return Some("runner_agent_stopped");
        }
        let current = self.credentials.current.lock().unwrap();
        current
            .failure
            .or_else(|| current.agent.as_ref().and_then(|agent| agent.refusal()))
    }
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        self.connect_to(id, &self.config.runner_instance, None)
    }
    fn connect_to<'a>(
        &'a self,
        id: Uuid,
        instance: &'a str,
        state_root: Option<&'a str>,
    ) -> ConnectFuture<'a> {
        Box::pin(async move {
            if self.credentials.unresolved() {
                self.credentials.refresh().await;
            }
            if let Some(reason) = self.refusal() {
                return Err(io::Error::other(reason));
            }
            self.ensure_master().await?;
            if let Some(reason) = self.refusal() {
                return Err(io::Error::other(reason));
            }
            let mut child = self.command_for(id, instance, state_root).spawn()?;
            Ok(Box::new(SshPipe {
                read: child.stdout.take().unwrap(),
                write: child.stdin.take().unwrap(),
                _child: child,
                _pin: None,
            }) as Box<dyn Duplex>)
        })
    }
    fn connect_windows<'a>(
        &'a self,
        id: Uuid,
        runner: &'a crate::WindowsRunner,
    ) -> ConnectFuture<'a> {
        Box::pin(async move {
            if self.credentials.unresolved() {
                self.credentials.refresh().await;
            }
            if let Some(reason) = self.windows_refusal() {
                return Err(io::Error::other(reason));
            }
            let agent = self
                .credentials
                .current
                .lock()
                .unwrap()
                .agent
                .clone()
                .ok_or_else(|| io::Error::other("runner_credential_unavailable"))?;
            let pin = WindowsPin::new(runner)?;
            let mut command = windows_command(id, runner, &agent.socket, &agent.identity, &pin.0);
            let mut child = command.spawn()?;
            Ok(Box::new(SshPipe {
                read: child.stdout.take().unwrap(),
                write: child.stdin.take().unwrap(),
                _child: child,
                _pin: Some(pin),
            }) as Box<dyn Duplex>)
        })
    }
}

struct WindowsPin(std::path::PathBuf);
impl WindowsPin {
    fn new(runner: &crate::WindowsRunner) -> io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path =
            std::env::temp_dir().join(format!("motor-windows-{}.known_hosts", Uuid::now_v7()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let pin = Self(path);
        writeln!(file, "motor-windows {}", runner.host_key)?;
        Ok(pin)
    }
}
impl Drop for WindowsPin {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn windows_command(
    id: Uuid,
    runner: &crate::WindowsRunner,
    socket: &Path,
    identity: &Path,
    known_hosts: &Path,
) -> Command {
    let mut command = windows_ssh_command(runner, socket, identity, known_hosts);
    command.arg(format!(
        "~/.cargo/bin/motor-runner connect --instance {} --state-root {} --job {id}",
        runner.instance, runner.state_root
    ));
    command
}

fn windows_ssh_command(
    runner: &crate::WindowsRunner,
    socket: &Path,
    identity: &Path,
    known_hosts: &Path,
) -> Command {
    let mut command = Command::new("ssh");
    // Each explicit endpoint has its own pin and no shared master. In particular,
    // the legacy build-server master must never carry an explicit endpoint's connection.
    command
        .args([
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "HostKeyAlias=motor-windows",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ConnectTimeout=3",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
        ])
        .arg("-o")
        .arg(format!("IdentityAgent={}", socket.display()))
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .arg("-i")
        .arg(identity)
        .arg("-p")
        .arg(runner.port.to_string())
        .arg("-l")
        .arg(&runner.user)
        .arg(&runner.host)
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("SSH_AGENT_PID")
        .env_remove("SUBC_LAUNCH_NONCE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

// Boxing decoded frames allocates for every message and makes each consumer
// unpack a wrapper before it can handle the protocol variant.
pub(crate) use motor_protocol::DecodedFrame as Incoming;
type WriteRequest = (Frame, oneshot::Sender<io::Result<()>>);

#[cfg(test)]
mod windows_command_tests {
    use super::*;

    #[test]
    fn explicit_windows_endpoint_offers_only_vault_identity_and_never_legacy_host() {
        let runner = crate::WindowsRunner {
            host: "192.0.2.41".into(),
            port: 2222,
            user: "worker".into(),
            instance: "windows".into(),
            state_root: "/srv/windows".into(),
            ..Default::default()
        };
        let command = windows_command(
            Uuid::nil(),
            &runner,
            Path::new("/vault/s"),
            Path::new("/vault/identity.pub"),
            Path::new("/private/pin"),
        );
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();
        for required in [
            "/dev/null",
            "BatchMode=yes",
            "ForwardAgent=no",
            "IdentitiesOnly=yes",
            "StrictHostKeyChecking=yes",
            "GlobalKnownHostsFile=/dev/null",
            "IdentityAgent=/vault/s",
            "UserKnownHostsFile=/private/pin",
            "ControlPath=none",
            "192.0.2.41",
            "worker",
            "2222",
        ] {
            assert!(args.contains(&required), "missing {required}: {args:?}");
        }
        assert_eq!(args.iter().filter(|a| **a == "-F").count(), 1);
        assert_eq!(args.iter().filter(|a| **a == "-i").count(), 1);
        assert!(args.windows(2).any(|a| a == ["-i", "/vault/identity.pub"]));
        assert!(!args.contains(&"alfonso-runner"));
        assert!(args
            .last()
            .unwrap()
            .contains("--instance windows --state-root /srv/windows"));
        for name in ["SSH_AUTH_SOCK", "SSH_AGENT_PID"] {
            assert!(command
                .as_std()
                .get_envs()
                .any(|(n, v)| n == name && v.is_none()));
        }
    }
}

pub(crate) struct Channel {
    pub return_paths: bool,
    pub gate_jobs: bool,
    pub platforms: Vec<String>,
    tx: mpsc::Sender<WriteRequest>,
    rx: mpsc::Receiver<io::Result<Incoming>>,
    heartbeat_enabled: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Channel {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Channel {
    /// Exchange hello once on a TLS connection already authenticated by its lease.
    /// Preserve explicit host refusals before starting the generic frame reader;
    /// retryable transport failures need a fresh lease, never SSH's retry loop.
    pub(crate) async fn from_hosted(mut pipe: Box<dyn Duplex>) -> io::Result<Self> {
        write_frame_async(
            &mut pipe,
            &Frame::Hello {
                return_paths: false,
                protocol_version: PROTOCOL_VERSION,
                started: true,
                default_branch_tip: false,
                gate_jobs: true,
                platforms: Vec::new(),
            },
        )
        .await?;
        pipe.flush().await?;
        loop {
            let length = pipe.read_u32().await? as usize;
            if length > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized hosted runner hello",
                ));
            }
            let mut bytes = vec![0; length];
            pipe.read_exact(&mut bytes).await?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if value["type"] == "lease_refused" {
                let reason = value["reason"]
                    .as_str()
                    .ok_or_else(|| io::Error::other("invalid hosted refusal"))?;
                return Err(io::Error::other(HostedHelloRefusal(reason.to_owned())));
            }
            let frame: Frame = serde_json::from_value(value).map_err(io::Error::other)?;
            match frame {
                Frame::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    platforms,
                    gate_jobs,
                    return_paths,
                    ..
                } => {
                    let mut channel = Self::new(pipe);
                    channel.platforms = platforms;
                    channel.gate_jobs = gate_jobs;
                    channel.return_paths = return_paths;
                    channel.enable_heartbeats();
                    return Ok(channel);
                }
                Frame::Hello { .. } => {
                    return Err(io::Error::other("hosted runner protocol version mismatch"))
                }
                Frame::Heartbeat { .. } => continue,
                _ => return Err(io::Error::other("invalid hosted runner hello")),
            }
        }
    }
    pub async fn open(connector: &dyn Connector, id: Uuid) -> Result<Self, RefusalReason> {
        Self::open_with_cause(connector, id)
            .await
            .map_err(|(reason, _)| reason)
    }
    pub async fn open_with_cause(
        connector: &dyn Connector,
        id: Uuid,
    ) -> Result<Self, (RefusalReason, String)> {
        enum AttemptFailure {
            Step(UnreachableStep, String),
            UnexpectedFrame,
            VersionMismatch(u32),
        }

        // This bound includes process creation and the complete hello exchange,
        // rather than restarting a timer for every heartbeat before hello.
        let unavailable = || {
            connector
                .refusal()
                .map(|code| RefusalReason::Unknown(code.into()))
                .unwrap_or(RefusalReason::Unreachable)
        };
        let refuse = |step, cause: &dyn std::fmt::Display| {
            let reason = unavailable();
            if matches!(reason, RefusalReason::Unreachable) {
                record_unreachable(id, step, cause);
            }
            (reason, format!("{}: {cause}", step.label()))
        };
        // The timeout counter alone cannot say whether SSH never delivered a
        // pipe or the runner never answered the hello; remember which part of
        // the handshake was still in flight so the logged cause names it.
        let waiting = std::sync::atomic::AtomicUsize::new(UnreachableStep::Connect as usize);
        let last_failure = StdMutex::new(None::<String>);
        let started = tokio::time::Instant::now();
        let advance = |step: UnreachableStep| {
            waiting.store(step as usize, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(job_or_workspace_id = %id, step = step.label(), elapsed_ms = started.elapsed().as_millis() as u64, "runner handshake advanced");
        };
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut attempt = 1;
            let mut backoff = Duration::from_millis(250);
            loop {
                waiting.store(
                    UnreachableStep::Connect as usize,
                    std::sync::atomic::Ordering::Relaxed,
                );
                let result = async {
                    let pipe = connector.connect(id).await.map_err(|error| {
                        AttemptFailure::Step(UnreachableStep::Connect, error.to_string())
                    })?;
                    advance(UnreachableStep::HelloSend);
                    let mut channel = Self::new(pipe);
                    channel
                        .send(Frame::Hello { return_paths: false, protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: true,
                        gate_jobs: true,
                        platforms: Vec::new(), })
                        .await
                        .map_err(|error| {
                            AttemptFailure::Step(UnreachableStep::HelloSend, error.to_string())
                        })?;
                    advance(UnreachableStep::HelloRecv);
                    match channel.next().await.map_err(|error| {
                        AttemptFailure::Step(UnreachableStep::HelloRecv, error.to_string())
                    })? {
                        Incoming::Frame(Frame::Hello { protocol_version, platforms, gate_jobs, return_paths, .. })
                            if protocol_version == PROTOCOL_VERSION =>
                        {
                            channel.platforms = platforms;
                            channel.gate_jobs = gate_jobs;
                            channel.return_paths = return_paths;
                            channel.enable_heartbeats();
                            tracing::info!(job_or_workspace_id = %id, elapsed_ms = started.elapsed().as_millis() as u64, "runner handshake complete");
                            Ok(channel)
                        }
                        Incoming::Frame(Frame::Hello { protocol_version, .. }) => {
                            Err(AttemptFailure::VersionMismatch(protocol_version))
                        }
                        _ => Err(AttemptFailure::UnexpectedFrame),
                    }
                }
                .await;

                match result {
                    Ok(channel) => return Ok(channel),
                    Err(AttemptFailure::VersionMismatch(protocol_version)) => {
                        return Err((
                            RefusalReason::RunnerVersionMismatch,
                            format!(
                                "runner protocol version {protocol_version}, expected {PROTOCOL_VERSION}"
                            ),
                        ));
                    }
                    Err(AttemptFailure::UnexpectedFrame) => {
                        return Err(refuse(
                            UnreachableStep::UnexpectedFrame,
                            &"runner sent a non-hello first frame",
                        ));
                    }
                    Err(AttemptFailure::Step(step, cause)) => {
                        if let Some(code) = connector.refusal() {
                            return Err((
                                RefusalReason::Unknown(code.into()),
                                format!("{}: {cause}", step.label()),
                            ));
                        }
                        *last_failure.lock().unwrap() = Some(format!("{}: {cause}", step.label()));
                        tracing::info!(
                            job_or_workspace_id = %id,
                            step = step.label(),
                            cause = %cause,
                            attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            "retrying runner handshake"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        backoff = (backoff * 2).min(Duration::from_secs(2));
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            let stage =
                UnreachableStep::ALL[waiting.load(std::sync::atomic::Ordering::Relaxed)].label();
            let mut cause = format!(
                "no runner hello within {}s; still waiting in {stage}",
                started.elapsed().as_secs()
            );
            if let Some(last_failure) = last_failure.lock().unwrap().as_ref() {
                cause.push_str(&format!("; last attempt failed at {last_failure}"));
            }
            Err(refuse(
                UnreachableStep::Timeout15s,
                &cause,
            ))
        })
    }
    fn new(pipe: Box<dyn Duplex>) -> Self {
        let (mut read, mut write) = tokio::io::split(pipe);
        let (tx, mut writes) = mpsc::channel::<WriteRequest>(2);
        let (incoming, rx) = mpsc::channel(16);
        let (broken_tx, mut read_broken) = tokio::sync::watch::channel(false);
        let (heartbeat_enabled, mut heartbeat_state) = watch::channel(false);
        let reading = tokio::spawn(async move {
            loop {
                let mut frame_started = false;
                let result = async {
                    let mut first = [0; 1];
                    // A peer can finish its reply and close input while a
                    // heartbeat write fails. Drain readable frames before
                    // treating an outbound failure as an empty response stream.
                    tokio::select! {
                        biased;
                        result = read_exact_active(&mut read, &mut first) => result?,
                        _ = read_broken.wait_for(|failed| *failed) => return Err(io::Error::other("channel write failed")),
                    }
                    frame_started = true;
                    // Return the boundary byte to the shared reader. Once a
                    // frame starts, an outbound failure must not truncate it.
                    let mut prefixed = first.as_slice().chain(ActiveReader::new(&mut read));
                    motor_protocol::read_record_async(&mut prefixed)
                        .await?
                        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame"))
                }
                .await;
                match result {
                    Ok(frame) => {
                        if incoming.send(Ok(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(error)
                        if error.kind() == io::ErrorKind::UnexpectedEof && !frame_started =>
                    {
                        tracing::debug!("runner closed stream at a frame boundary");
                        let _ = incoming.send(Err(error)).await;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "runner frame read/decode failed; reconnecting job stream");
                        // Preserve the cause for the job owner, which can name
                        // the job and lifecycle step in its diagnostic.
                        let _ = incoming.send(Err(error)).await;
                        break;
                    }
                }
            }
        });
        let writing = tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(1),
                Duration::from_secs(1),
            );
            loop {
                let (frame, ack) = tokio::select! {
                    request = writes.recv() => match request { Some((f, a)) => (f, Some(a)), None => break },
                    _ = heartbeat.tick(), if *heartbeat_state.borrow() => (Frame::Heartbeat { queue_wait_ms: None }, None),
                    changed = heartbeat_state.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let result = write_active(&mut write, &frame).await;
                let failed = result.is_err();
                if let Some(ack) = ack {
                    let _ = ack.send(result);
                }
                if failed {
                    let _ = broken_tx.send(true);
                    break;
                }
            }
        });
        Self {
            gate_jobs: false,
            return_paths: false,
            platforms: Vec::new(),
            tx,
            rx,
            heartbeat_enabled,
            tasks: vec![reading, writing],
        }
    }
    fn enable_heartbeats(&self) {
        let _ = self.heartbeat_enabled.send(true);
    }
    pub async fn send(&self, frame: Frame) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send((frame, tx))
            .await
            .map_err(|_| io::Error::other("channel detached"))?;
        rx.await.map_err(|_| io::Error::other("channel detached"))?
    }
    pub async fn next(&mut self) -> io::Result<Incoming> {
        loop {
            match self.next_job().await? {
                Incoming::Frame(Frame::Heartbeat { .. }) => continue,
                frame => return Ok(frame),
            }
        }
    }
    /// Job followers consume heartbeats too, since they carry queue lifecycle.
    pub async fn next_job(&mut self) -> io::Result<Incoming> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::other("channel detached"))?
    }
}
#[cfg(test)]
#[path = "../tests/it/master.rs"]
mod master_tests;

#[cfg(test)]
#[path = "transport_retry_tests.rs"]
mod transport_retry_tests;

#[cfg(test)]
mod hello_tests {
    use super::*;

    struct OneConnection(StdMutex<Option<tokio::io::DuplexStream>>);

    impl Connector for OneConnection {
        fn connect(
            &self,
            _: Uuid,
        ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
            Box::pin(async {
                Ok(Box::new(self.0.lock().unwrap().take().unwrap()) as Box<dyn Duplex>)
            })
        }
    }

    #[tokio::test]
    async fn hosted_hello_does_not_advertise_background_fill_support() {
        let (client, mut peer) = tokio::io::duplex(4096);
        let server = async {
            let hello = read_frame_async(&mut peer).await.unwrap().unwrap();
            assert!(matches!(
                hello,
                Frame::Hello {
                    default_branch_tip: false,
                    ..
                }
            ));
            write_frame_async(
                &mut peer,
                &Frame::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    started: true,
                    default_branch_tip: true,
                    gate_jobs: true,
                    return_paths: false,
                    platforms: vec!["linux".into()],
                },
            )
            .await
            .unwrap();
        };
        let (channel, ()) = tokio::join!(Channel::from_hosted(Box::new(client)), server);
        assert!(channel.is_ok());
    }

    #[tokio::test]
    async fn hello_advertises_default_branch_tip_on_the_wire() {
        let (client, mut peer) = tokio::io::duplex(4096);
        let connector = OneConnection(StdMutex::new(Some(client)));
        let read_hello = async {
            let mut header = [0; 4];
            peer.read_exact(&mut header).await.unwrap();
            let mut payload = vec![0; u32::from_be_bytes(header) as usize];
            peer.read_exact(&mut payload).await.unwrap();
            assert!(matches!(
                serde_json::from_slice::<Frame>(&payload).unwrap(),
                Frame::Hello {
                    default_branch_tip: true,
                    gate_jobs: true,
                    ..
                }
            ));
            write_active(
                &mut peer,
                &Frame::Hello {
                    return_paths: false,
                    protocol_version: PROTOCOL_VERSION,
                    started: true,
                    default_branch_tip: false,
                    gate_jobs: false,
                    platforms: Vec::new(),
                },
            )
            .await
            .unwrap();
        };
        let (channel, ()) = tokio::join!(Channel::open(&connector, Uuid::now_v7()), read_hello);
        assert!(channel.is_ok());
    }

    #[tokio::test]
    async fn gate_capability_requires_a_positive_runner_hello() {
        for capable in [false, true] {
            let (client, mut peer) = tokio::io::duplex(4096);
            let connector = OneConnection(StdMutex::new(Some(client)));
            let server = async {
                assert!(matches!(
                    read_frame_async(&mut peer).await.unwrap(),
                    Some(Frame::Hello {
                        gate_jobs: true,
                        ..
                    })
                ));
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        return_paths: false,
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: capable,
                        platforms: Vec::new(),
                    },
                )
                .await
                .unwrap();
            };
            let (channel, ()) = tokio::join!(Channel::open(&connector, Uuid::now_v7()), server);
            assert_eq!(channel.unwrap().gate_jobs, capable);
        }
    }
}

#[cfg(test)]
mod disconnect_tests {
    use super::*;
    use std::task::{Context, Poll};
    struct FailedWriter;
    impl AsyncRead for FailedWriter {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }
    impl AsyncWrite for FailedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    struct FinalResponse {
        bytes: Vec<u8>,
        offset: usize,
        readable: bool,
        waker: Option<std::task::Waker>,
    }
    struct ResponseOnWriteFailure(Arc<StdMutex<FinalResponse>>);
    impl AsyncRead for ResponseOnWriteFailure {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let mut state = self.0.lock().unwrap();
            if !state.readable {
                state.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let end = (state.offset + buffer.remaining()).min(state.bytes.len());
            buffer.put_slice(&state.bytes[state.offset..end]);
            state.offset = end;
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for ResponseOnWriteFailure {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut state = self.0.lock().unwrap();
            state.readable = true;
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
            Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn readable_final_response_survives_concurrent_write_failure() {
        let expected = Frame::Prepared {
            transfer_id: Uuid::now_v7(),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &expected).unwrap();
        let mut channel = Channel::new(Box::new(ResponseOnWriteFailure(Arc::new(StdMutex::new(
            FinalResponse {
                bytes,
                offset: 0,
                readable: false,
                waker: None,
            },
        )))));
        assert!(channel.send(Frame::Status).await.is_err());
        let incoming = channel
            .next()
            .await
            .expect("a failed write must not discard a readable final response");
        assert!(matches!(incoming, Incoming::Frame(frame) if frame == expected));
        assert!(channel.next().await.is_err());
    }

    #[tokio::test]
    async fn write_failure_wakes_next_without_waiting_for_read_timeout() {
        let mut channel = Channel::new(Box::new(FailedWriter));
        assert!(channel.send(Frame::Status).await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(250), channel.next())
                .await
                .expect("write failure must wake the reader")
                .is_err()
        );
    }
}

// Bound silence between actual I/O advances, not the duration of a large frame.
struct ActiveReader<R> {
    read: R,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl<R> ActiveReader<R> {
    fn new(read: R) -> Self {
        Self {
            read,
            deadline: Box::pin(tokio::time::sleep(Duration::from_secs(60))),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ActiveReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.read).poll_read(cx, buf) {
            std::task::Poll::Ready(result) => {
                if result.is_ok() && buf.filled().len() > before {
                    self.deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + Duration::from_secs(60));
                }
                std::task::Poll::Ready(result)
            }
            std::task::Poll::Pending => {
                if self.deadline.as_mut().poll(cx).is_ready() {
                    std::task::Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "read inactivity",
                    )))
                } else {
                    std::task::Poll::Pending
                }
            }
        }
    }
}

async fn read_exact_active<R: AsyncRead + Unpin>(
    read: &mut R,
    mut bytes: &mut [u8],
) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = tokio::time::timeout(Duration::from_secs(60), read.read(bytes))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "read inactivity"))??;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated frame",
            ));
        }
        bytes = &mut bytes[n..];
    }
    Ok(())
}
async fn write_active<W: AsyncWrite + Unpin>(write: &mut W, frame: &Frame) -> io::Result<()> {
    let mut bytes = Vec::new();
    motor_protocol::write_frame(&mut bytes, frame)?;
    let mut offset = 0;
    while offset < bytes.len() {
        let n = tokio::time::timeout(Duration::from_secs(60), write.write(&bytes[offset..]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write inactivity"))??;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "frame write made no progress",
            ));
        }
        offset += n;
    }
    tokio::time::timeout(Duration::from_secs(60), write.flush())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "flush inactivity"))?
}

#[cfg(test)]
mod transfer_io_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn slow_reads_count_each_byte_not_total_frame_duration() {
        let (mut read, mut write) = tokio::io::duplex(1);
        let sender = tokio::spawn(async move {
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_secs(30)).await;
                write.write_all(&[42]).await.unwrap();
            }
        });
        let mut bytes = [0; 20];
        let start = tokio::time::Instant::now();
        read_exact_active(&mut read, &mut bytes).await.unwrap();
        assert_eq!(bytes, [42; 20]);
        assert_eq!(start.elapsed(), Duration::from_secs(600));
        sender.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn slow_writes_count_each_byte_not_total_frame_duration() {
        let (mut write, mut read) = tokio::io::duplex(1);
        let mut expected = Vec::new();
        write_frame(
            &mut expected,
            &Frame::Heartbeat {
                queue_wait_ms: None,
            },
        )
        .unwrap();
        let receiver = tokio::spawn(async move {
            let mut bytes = Vec::new();
            while bytes.len() < expected.len() {
                tokio::time::sleep(Duration::from_secs(30)).await;
                bytes.push(read.read_u8().await.unwrap());
            }
            assert_eq!(bytes, expected);
        });
        let start = tokio::time::Instant::now();
        write_active(
            &mut write,
            &Frame::Heartbeat {
                queue_wait_ms: None,
            },
        )
        .await
        .unwrap();
        assert!(start.elapsed() > Duration::from_secs(300));
        receiver.await.unwrap();
    }
}
