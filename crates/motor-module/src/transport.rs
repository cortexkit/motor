use crate::diagnostics::{record_unreachable, UnreachableStep};
use crate::{agent::VaultAgent, Config};
use cortexkit_claustrum_client::{CredentialErrorClass, CredentialResolver};
use motor_protocol::*;
use std::{
    ffi::OsStr,
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
    fn macos_refusal(&self) -> Option<&'static str> {
        self.refusal()
    }
    /// The `retry_after_ms` hint for a job this connector refused with
    /// `code`: how long until the refusal may clear without an operator.
    /// `None` unless the refusal is transient.
    fn retry_after_ms(&self, _code: &str) -> Option<u64> {
        None
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
    fn connect_macos<'a>(&'a self, id: Uuid, runner: &'a crate::MacosRunner) -> ConnectFuture<'a> {
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
        } else if self.target.macos.is_some() {
            self.connector.macos_refusal()
        } else {
            self.connector.refusal()
        }
    }
    fn retry_after_ms(&self, code: &str) -> Option<u64> {
        self.connector.retry_after_ms(code)
    }
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        if let Some(runner) = &self.target.windows {
            self.connector.connect_windows(id, runner)
        } else if let Some(runner) = &self.target.macos {
            self.connector.connect_macos(id, runner)
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
    // The macOS endpoint's own master, separate from the build-server one so
    // it keeps the endpoint's pinned host key and settings. Also killed before
    // the agent is retired.
    macos_master: Mutex<Option<EndpointMaster>>,
    credentials: Arc<CredentialState>,
    retry: Option<JoinHandle<()>>,
    // The ssh program for explicit endpoints. Always "ssh" in production;
    // tests point it at a fake to make ssh fail in a chosen way.
    ssh: std::ffi::OsString,
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
            macos_master: Mutex::new(None),
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
            ssh: "ssh".into(),
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
        if let Some(mut master) = self.macos_master.lock().await.take() {
            let _ = master.child.kill().await;
            let _ = master.child.wait().await;
            let _ = std::fs::remove_file(&master.endpoint.control);
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
    /// Returns the control socket of a live SSH master to the macOS endpoint,
    /// starting one if there is none, so each job opens a session on an
    /// existing connection instead of paying a new TCP and SSH handshake. On a
    /// shared, saturated uplink that handshake can outlast ssh's
    /// ConnectTimeout, while a session on an open connection does not.
    async fn ensure_macos_master(
        &self,
        runner: &crate::MacosRunner,
        agent: &VaultAgent,
    ) -> io::Result<PathBuf> {
        let endpoint = MasterEndpoint {
            // Beside the agent socket in ck-motor's private 0700 runtime
            // directory. A one-letter name makes this path exactly as long as
            // the build-server master's, so it fits the macOS 104-byte Unix
            // socket limit, with OpenSSH's temporary suffix, wherever that
            // master's does.
            control: agent.control_path.with_file_name(MACOS_CONTROL_NAME),
            host: runner.host.clone(),
            port: runner.port,
            user: runner.user.clone(),
            host_key: runner.host_key.clone(),
        };
        // Serialize creation so concurrent jobs share one connection and one
        // vault signature.
        let mut owned = self.macos_master.lock().await;
        if let Some(reason) = self.macos_refusal() {
            return Err(io::Error::other(reason));
        }
        if let Some(master) = owned.as_mut() {
            let alive = master.child.try_wait()?.is_none();
            if alive && master.endpoint == endpoint && endpoint.control.exists() {
                return Ok(endpoint.control);
            }
            // Dead, configured for another endpoint, or lost its socket: never
            // reuse it. A killed master leaves its socket behind, and a stale
            // socket would look ready to the check below.
            let mut stale = owned.take().unwrap();
            let _ = stale.child.kill().await;
            let _ = stale.child.wait().await;
            let _ = std::fs::remove_file(&stale.endpoint.control);
            if !alive {
                let cause = stale.stderr.last_line(Duration::from_secs(1)).await;
                tracing::warn!(
                    ssh_error = cause.as_deref().unwrap_or("none"),
                    "macOS SSH master exited; starting a new one"
                );
            }
        }
        let _ = std::fs::remove_file(&endpoint.control);
        let pin = EndpointPin::for_endpoint("motor-macos", &runner.host_key)?;
        let mut command = macos_master_command(
            &self.ssh,
            runner,
            &agent.socket,
            &agent.identity,
            &pin.0,
            &endpoint.control,
        );
        let mut child = command.spawn()?;
        let stderr = SshStderr::collect(child.stderr.take().unwrap());
        let ready = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if !matches!(child.try_wait(), Ok(None)) {
                    return Err(None);
                }
                if endpoint.control.exists() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or(Err(Some("ssh master did not start within 15s")));
        if let Err(timeout) = ready {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = std::fs::remove_file(&endpoint.control);
            if let Some(reason) = self.macos_refusal() {
                return Err(io::Error::other(reason));
            }
            return Err(io::Error::other(
                match (stderr.last_line(Duration::from_secs(1)).await, timeout) {
                    (Some(line), _) => format!("ssh master exited: {line}"),
                    (None, Some(timeout)) => timeout.to_owned(),
                    (None, None) => "ssh master exited without an error message".to_owned(),
                },
            ));
        }
        let control = endpoint.control.clone();
        *owned = Some(EndpointMaster {
            endpoint,
            child,
            stderr,
            _pin: pin,
        });
        Ok(control)
    }
    /// The code health reports. Unlike `refusal`, which lets a job try again
    /// once a sign failure's backoff has passed, this keeps showing the sign
    /// failure until a sign succeeds.
    pub(crate) fn health_refusal(&self) -> Option<&'static str> {
        self.refusal_from(VaultAgent::health_refusal)
    }
    fn refusal_from(
        &self,
        agent_refusal: fn(&VaultAgent) -> Option<&'static str>,
    ) -> Option<&'static str> {
        if self.credentials.stopped.load(Ordering::Acquire) {
            return Some("runner_agent_stopped");
        }
        let current = self.credentials.current.lock().unwrap();
        current
            .failure
            .or_else(|| current.agent.as_deref().and_then(agent_refusal))
    }
    #[cfg(test)]
    pub(crate) fn agent(&self) -> Option<Arc<VaultAgent>> {
        self.credentials.current.lock().unwrap().agent.clone()
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

/// The file name of the macOS master's control socket in the agent's runtime
/// directory, which removes it with the directory.
pub(crate) const MACOS_CONTROL_NAME: &str = "d";
/// How much of an ssh child's stderr is kept for a failure message.
const STDERR_TAIL_BYTES: usize = 2048;
/// How long a failed connection waits for ssh to finish writing its error.
const STDERR_WAIT: Duration = Duration::from_secs(1);

/// The last bytes an ssh child wrote to stderr, published when ssh closes it.
/// Explicit endpoints use it so a refusal names ssh's own error, telling a
/// network timeout from an authentication or host-key failure.
#[derive(Clone)]
struct SshStderr(watch::Receiver<Option<Vec<u8>>>);
impl SshStderr {
    /// Drains `stderr` for the child's whole life, so a chatty remote command
    /// never blocks on a full pipe, keeping only the last bytes.
    fn collect(stderr: tokio::process::ChildStderr) -> Self {
        let (done, tail) = watch::channel(None);
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut kept = Vec::new();
            let mut chunk = [0; 1024];
            while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
                kept.extend_from_slice(&chunk[..read]);
                if kept.len() > STDERR_TAIL_BYTES {
                    kept.drain(..kept.len() - STDERR_TAIL_BYTES);
                }
            }
            let _ = done.send(Some(kept));
        });
        Self(tail)
    }
    /// ssh's last meaningful stderr line, once ssh has closed stderr. `None` if
    /// it wrote nothing useful or is still running after `wait`.
    async fn last_line(mut self, wait: Duration) -> Option<String> {
        let tail = tokio::time::timeout(wait, self.0.wait_for(Option::is_some))
            .await
            .ok()?
            .ok()?;
        last_meaningful_line(tail.as_deref()?)
    }
}

/// The last nonblank line, cleaned for a single-line log field. ssh prints no
/// key material at its default log level. If the kept bytes contain any part
/// of a PEM block anyway, nothing is returned, since a base64 body line alone
/// would not look like key material.
fn last_meaningful_line(tail: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(tail);
    if text.contains("-----BEGIN") || text.contains("-----END") || text.contains("PRIVATE KEY") {
        return None;
    }
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(200)
                .collect()
        })
}

type ExitCause = Pin<Box<dyn Future<Output = Option<String>> + Send>>;

struct SshPipe {
    read: tokio::process::ChildStdout,
    write: tokio::process::ChildStdin,
    _child: Child,
    _pin: Option<EndpointPin>,
    /// stderr of an explicit-endpoint ssh; the build-server route discards it.
    stderr: Option<SshStderr>,
    /// The runner has sent at least one byte. Until then an EOF or a broken
    /// pipe is a failure before the runner answered, and the error carries
    /// ssh's last stderr line when there is one.
    answered: bool,
    // Reads and writes run in different tasks, so each waits on its own future
    // and keeps its own waker.
    read_exit: Option<ExitCause>,
    write_exit: Option<(ExitCause, io::Error)>,
}
impl SshPipe {
    fn new(mut child: Child, pin: Option<EndpointPin>, stderr: Option<SshStderr>) -> Self {
        Self {
            read: child.stdout.take().unwrap(),
            write: child.stdin.take().unwrap(),
            _child: child,
            _pin: pin,
            stderr,
            answered: false,
            read_exit: None,
            write_exit: None,
        }
    }
    /// For an explicit endpoint, spawns `command` with its stderr collected.
    fn spawn_endpoint(mut command: Command, pin: EndpointPin) -> io::Result<Self> {
        let mut child = command.stderr(Stdio::piped()).spawn()?;
        let stderr = SshStderr::collect(child.stderr.take().unwrap());
        Ok(Self::new(child, Some(pin), Some(stderr)))
    }
    fn exit_cause(&self) -> Option<ExitCause> {
        if self.answered {
            return None;
        }
        let stderr = self.stderr.clone()?;
        Some(Box::pin(stderr.last_line(STDERR_WAIT)))
    }
}
impl AsyncRead for SshPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = &mut *self;
        if this.read_exit.is_none() {
            let before = buf.filled().len();
            let wanted = buf.remaining() > 0;
            let result = std::task::ready!(Pin::new(&mut this.read).poll_read(cx, buf));
            let eof = result.is_ok() && wanted && buf.filled().len() == before;
            if buf.filled().len() > before {
                this.answered = true;
            }
            match eof.then(|| this.exit_cause()).flatten() {
                Some(cause) => this.read_exit = Some(cause),
                None => return std::task::Poll::Ready(result),
            }
        }
        let cause = std::task::ready!(this.read_exit.as_mut().unwrap().as_mut().poll(cx));
        this.read_exit = None;
        std::task::Poll::Ready(match cause {
            Some(cause) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("ssh exited: {cause}"),
            )),
            None => Ok(()),
        })
    }
}
impl AsyncWrite for SshPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let this = &mut *self;
        if this.write_exit.is_none() {
            match std::task::ready!(Pin::new(&mut this.write).poll_write(cx, buf)) {
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                    match this.exit_cause() {
                        Some(cause) => this.write_exit = Some((cause, error)),
                        None => return std::task::Poll::Ready(Err(error)),
                    }
                }
                result => return std::task::Poll::Ready(result),
            }
        }
        let (cause, _) = this.write_exit.as_mut().unwrap();
        let cause = std::task::ready!(cause.as_mut().poll(cx));
        let (_, error) = this.write_exit.take().unwrap();
        std::task::Poll::Ready(Err(match cause {
            Some(cause) => {
                io::Error::new(io::ErrorKind::BrokenPipe, format!("ssh exited: {cause}"))
            }
            None => error,
        }))
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

/// The identity of a macOS master's connection: a master opened for other
/// settings, or under another agent, is replaced rather than reused.
#[derive(PartialEq)]
struct MasterEndpoint {
    control: PathBuf,
    host: String,
    port: u16,
    user: String,
    host_key: String,
}
struct EndpointMaster {
    endpoint: MasterEndpoint,
    child: Child,
    stderr: SshStderr,
    _pin: EndpointPin,
}
impl Connector for SshConnector {
    fn macos_refusal(&self) -> Option<&'static str> {
        self.windows_refusal()
    }
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
        self.refusal_from(VaultAgent::refusal)
    }
    /// Only the signing agent's transient failures carry a hint. Credential,
    /// agent and SSH configuration refusals need an operator.
    fn retry_after_ms(&self, code: &str) -> Option<u64> {
        let agent = self.credentials.current.lock().unwrap().agent.clone()?;
        agent.retry_after_ms(code)
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
            let child = self.command_for(id, instance, state_root).spawn()?;
            Ok(Box::new(SshPipe::new(child, None, None)) as Box<dyn Duplex>)
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
            // Windows keeps one connection per job: its LAN endpoint does not
            // share the macOS endpoint's uplink, and its sshd differs.
            let pin = WindowsPin::new(runner)?;
            let command = windows_command(
                &self.ssh,
                id,
                runner,
                &agent.socket,
                &agent.identity,
                &pin.0,
            );
            Ok(Box::new(SshPipe::spawn_endpoint(command, pin)?) as Box<dyn Duplex>)
        })
    }
    fn connect_macos<'a>(&'a self, id: Uuid, runner: &'a crate::MacosRunner) -> ConnectFuture<'a> {
        Box::pin(async move {
            if self.credentials.unresolved() {
                self.credentials.refresh().await;
            }
            if let Some(reason) = self.macos_refusal() {
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
            let control = self.ensure_macos_master(runner, &agent).await?;
            if let Some(reason) = self.macos_refusal() {
                return Err(io::Error::other(reason));
            }
            // The job's own pin still matters: if the master dies between the
            // check above and this session, ssh connects directly instead.
            let pin = EndpointPin::for_endpoint("motor-macos", &runner.host_key)?;
            let command = macos_command(
                &self.ssh,
                id,
                runner,
                &agent.socket,
                &agent.identity,
                &pin.0,
                &control,
            );
            Ok(Box::new(SshPipe::spawn_endpoint(command, pin)?) as Box<dyn Duplex>)
        })
    }
}

struct EndpointPin(std::path::PathBuf);
type WindowsPin = EndpointPin;
impl EndpointPin {
    fn new(runner: &crate::WindowsRunner) -> io::Result<Self> {
        Self::for_endpoint("motor-windows", &runner.host_key)
    }
    fn for_endpoint(alias: &str, host_key: &str) -> io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = std::env::temp_dir().join(format!("{alias}-{}.known_hosts", Uuid::now_v7()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let pin = Self(path);
        writeln!(file, "{alias} {host_key}")?;
        Ok(pin)
    }
}
impl Drop for EndpointPin {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn windows_command(
    ssh: &OsStr,
    id: Uuid,
    runner: &crate::WindowsRunner,
    socket: &Path,
    identity: &Path,
    known_hosts: &Path,
) -> Command {
    let mut command = windows_ssh_command(ssh, runner, socket, identity, known_hosts);
    command.arg(format!(
        "~/.cargo/bin/motor-runner connect --instance {} --state-root {} --job {id}",
        runner.instance, runner.state_root
    ));
    command
}

fn windows_ssh_command(
    ssh: &OsStr,
    runner: &crate::WindowsRunner,
    socket: &Path,
    identity: &Path,
    known_hosts: &Path,
) -> Command {
    endpoint_ssh_command(
        ExplicitEndpoint {
            ssh,
            host: &runner.host,
            port: runner.port,
            user: &runner.user,
            alias: "motor-windows",
            timeout: 3,
            mux: Mux::None,
        },
        socket,
        identity,
        known_hosts,
    )
}

/// A job's session on the macOS endpoint, carried by the master at `control`.
fn macos_command(
    ssh: &OsStr,
    id: Uuid,
    runner: &crate::MacosRunner,
    socket: &Path,
    identity: &Path,
    known_hosts: &Path,
    control: &Path,
) -> Command {
    let mut command = macos_ssh_command(
        ssh,
        runner,
        socket,
        identity,
        known_hosts,
        Mux::Client(control),
    );
    command.arg(format!(
        "~/.cargo/bin/motor-runner connect --instance {} --state-root {} --job {id}",
        runner.instance, runner.state_root
    ));
    command
}

/// The macOS endpoint's master: authenticates once, runs no command, and
/// serves job sessions on its control socket until it is killed.
fn macos_master_command(
    ssh: &OsStr,
    runner: &crate::MacosRunner,
    socket: &Path,
    identity: &Path,
    known_hosts: &Path,
    control: &Path,
) -> Command {
    let mut command = macos_ssh_command(
        ssh,
        runner,
        socket,
        identity,
        known_hosts,
        Mux::Master(control),
    );
    command.stdin(Stdio::null()).stdout(Stdio::null());
    command
}

fn macos_ssh_command(
    ssh: &OsStr,
    runner: &crate::MacosRunner,
    socket: &Path,
    identity: &Path,
    known_hosts: &Path,
    mux: Mux<'_>,
) -> Command {
    endpoint_ssh_command(
        ExplicitEndpoint {
            ssh,
            host: &runner.host,
            port: runner.port,
            user: &runner.user,
            alias: "motor-macos",
            timeout: 10,
            mux,
        },
        socket,
        identity,
        known_hosts,
    )
}

/// How an explicit endpoint's ssh shares its connection.
enum Mux<'a> {
    /// One connection per job.
    None,
    /// A session on the master listening at this control socket. If the socket
    /// is gone, ssh falls back to its own connection with the same settings.
    Client(&'a Path),
    /// The foreground master that creates this control socket.
    Master(&'a Path),
}

struct ExplicitEndpoint<'a> {
    ssh: &'a OsStr,
    host: &'a str,
    port: u16,
    user: &'a str,
    alias: &'a str,
    timeout: u64,
    mux: Mux<'a>,
}

fn endpoint_ssh_command(
    endpoint: ExplicitEndpoint<'_>,
    socket: &Path,
    identity: &Path,
    known_hosts: &Path,
) -> Command {
    let mut command = Command::new(endpoint.ssh);
    // Each explicit endpoint has its own pin, and any master it uses is its
    // own. In particular, the legacy build-server master must never carry an
    // explicit endpoint's connection.
    command.args([
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
        "PasswordAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
    ]);
    match endpoint.mux {
        Mux::None => {
            command.args(["-o", "ControlMaster=no", "-o", "ControlPath=none"]);
        }
        Mux::Client(control) => {
            command
                .args(["-o", "ControlMaster=no", "-o"])
                .arg(format!("ControlPath={}", control.display()));
        }
        Mux::Master(control) => {
            // Keepalives make a master whose connection silently died exit,
            // so the next job replaces it instead of hanging on it. They are
            // sent only after 15s without any data from the server.
            command
                .args([
                    "-N",
                    "-o",
                    "ControlMaster=yes",
                    "-o",
                    "ControlPersist=no",
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=4",
                    "-o",
                ])
                .arg(format!("ControlPath={}", control.display()));
        }
    }
    command
        .arg("-o")
        .arg(format!("HostKeyAlias={}", endpoint.alias))
        .arg("-o")
        .arg(format!("ConnectTimeout={}", endpoint.timeout))
        .arg("-o")
        .arg(format!("IdentityAgent={}", socket.display()))
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .arg("-i")
        .arg(identity)
        .arg("-p")
        .arg(endpoint.port.to_string())
        .arg("-l")
        .arg(endpoint.user)
        .arg(endpoint.host)
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("SSH_AGENT_PID")
        .env_remove("SUBC_LAUNCH_NONCE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // ssh's own error is what tells a network timeout from an
        // authentication or host-key failure; see `SshStderr`.
        .stderr(Stdio::piped())
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
            OsStr::new("ssh"),
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

#[cfg(test)]
mod macos_command_tests {
    use super::*;

    #[test]
    fn macos_endpoint_uses_its_own_pin_vault_identity_and_ten_second_timeout() {
        let runner = crate::MacosRunner {
            host: "macos.example.invalid".into(),
            port: 25578,
            user: "motor".into(),
            instance: "macos".into(),
            state_root: "/Users/motor/state".into(),
            host_key: "ssh-ed25519 test-pin".into(),
        };
        let pin = EndpointPin::for_endpoint("motor-macos", &runner.host_key).unwrap();
        assert_eq!(
            std::fs::read_to_string(&pin.0).unwrap(),
            "motor-macos ssh-ed25519 test-pin\n"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&pin.0).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let path = pin.0.clone();
        let command = macos_command(
            OsStr::new("ssh"),
            Uuid::nil(),
            &runner,
            Path::new("/vault/s"),
            Path::new("/vault/identity.pub"),
            &pin.0,
            Path::new("/runtime/d"),
        );
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();
        for required in [
            "BatchMode=yes",
            "ForwardAgent=no",
            "IdentitiesOnly=yes",
            "StrictHostKeyChecking=yes",
            "GlobalKnownHostsFile=/dev/null",
            "HostKeyAlias=motor-macos",
            "ControlMaster=no",
            "ControlPath=/runtime/d",
            "ConnectTimeout=10",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "IdentityAgent=/vault/s",
            "motor",
            "25578",
            "macos.example.invalid",
        ] {
            assert!(args.contains(&required), "missing {required}: {args:?}");
        }
        assert_eq!(args.iter().filter(|a| **a == "-F").count(), 1);
        assert!(args.windows(2).any(|a| a == ["-F", "/dev/null"]));
        assert_eq!(args.iter().filter(|a| **a == "-i").count(), 1);
        assert!(args.windows(2).any(|a| a == ["-i", "/vault/identity.pub"]));
        assert!(args.contains(&format!("UserKnownHostsFile={}", path.display()).as_str()));
        assert!(!args.contains(&"alfonso-runner"));
        assert!(args
            .last()
            .unwrap()
            .contains("--instance macos --state-root /Users/motor/state"));
        for name in ["SSH_AUTH_SOCK", "SSH_AGENT_PID", "SUBC_LAUNCH_NONCE"] {
            assert!(command
                .as_std()
                .get_envs()
                .any(|(n, v)| n == name && v.is_none()));
        }
        // The master carries the same pin, identity and timeout, owns the
        // job sessions' control socket, and runs no remote command.
        let master = macos_master_command(
            OsStr::new("ssh"),
            &runner,
            Path::new("/vault/s"),
            Path::new("/vault/identity.pub"),
            &pin.0,
            Path::new("/runtime/d"),
        );
        let master_args: Vec<_> = master
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();
        for required in [
            "-N",
            "ControlMaster=yes",
            "ControlPersist=no",
            "ControlPath=/runtime/d",
            "HostKeyAlias=motor-macos",
            "ConnectTimeout=10",
            "IdentityAgent=/vault/s",
            "StrictHostKeyChecking=yes",
        ] {
            assert!(
                master_args.contains(&required),
                "missing {required}: {master_args:?}"
            );
        }
        assert!(master_args.contains(&format!("UserKnownHostsFile={}", path.display()).as_str()));
        assert!(!master_args.contains(&"ControlMaster=no"));
        assert_eq!(master_args.last(), Some(&"macos.example.invalid"));
        drop(pin);
        assert!(!path.exists());
    }
}

pub(crate) struct Channel {
    pub return_paths: bool,
    /// The runner's Hello advertised chunked returned files.
    pub return_chunked: bool,
    pub return_chunked_max_bytes: u64,
    pub gate_jobs: bool,
    pub sibling_paths: bool,
    pub platforms: Vec<String>,
    /// This client's Hello advertised default-branch tips, so a Run on this
    /// channel carries the resolved integration tip and the runner may fill
    /// its cache from it. Boat never gets them: it only reads the cache.
    pub fill_tips: bool,
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
    /// `fill_tips` is true for a registered host's tenant runner, which fills
    /// its own cache from default-branch tips, and false for Boat.
    pub(crate) async fn from_hosted(
        mut pipe: Box<dyn Duplex>,
        fill_tips: bool,
    ) -> io::Result<Self> {
        write_frame_async(
            &mut pipe,
            &Frame::Hello {
                return_paths: false,
                return_chunked_max_bytes: None,
                return_chunked: false,
                sibling_paths: false,
                protocol_version: PROTOCOL_VERSION,
                started: true,
                default_branch_tip: fill_tips,
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
                    return_chunked,
                    return_chunked_max_bytes,
                    sibling_paths,
                    ..
                } => {
                    let mut channel = Self::new(pipe);
                    channel.platforms = platforms;
                    channel.gate_jobs = gate_jobs;
                    channel.return_paths = return_paths;
                    channel.return_chunked = return_chunked;
                    channel.return_chunked_max_bytes = return_chunked_max_bytes
                        .unwrap_or(motor_protocol::RETURN_CHUNKED_MAX_BYTES_CAP);
                    channel.sibling_paths = sibling_paths;
                    channel.fill_tips = fill_tips;
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
                        .send(Frame::Hello { return_paths: false, return_chunked_max_bytes: None, return_chunked: false, sibling_paths: false, protocol_version: PROTOCOL_VERSION,
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
                        Incoming::Frame(Frame::Hello { protocol_version, platforms, gate_jobs, return_paths, return_chunked, return_chunked_max_bytes, sibling_paths, .. })
                            if protocol_version == PROTOCOL_VERSION =>
                        {
                            channel.platforms = platforms;
                            channel.gate_jobs = gate_jobs;
                            channel.return_paths = return_paths;
                            channel.return_chunked = return_chunked;
                            channel.return_chunked_max_bytes = return_chunked_max_bytes.unwrap_or(motor_protocol::RETURN_CHUNKED_MAX_BYTES_CAP);
                            channel.sibling_paths = sibling_paths;
                            channel.fill_tips = true;
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
            return_chunked: false,
            return_chunked_max_bytes: motor_protocol::RETURN_CHUNKED_MAX_BYTES_CAP,
            sibling_paths: false,
            platforms: Vec::new(),
            fill_tips: false,
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

    /// Boat only reads the cache, so its Hello offers no default-branch tips;
    /// a registered host's tenant runner fills its own cache, so its Hello
    /// offers them.
    #[tokio::test]
    async fn hosted_hello_advertises_fill_support_only_to_registered_host_tenants() {
        for fill_tips in [false, true] {
            let (client, mut peer) = tokio::io::duplex(4096);
            let server = async {
                let hello = read_frame_async(&mut peer).await.unwrap().unwrap();
                assert!(
                    matches!(
                        hello,
                        Frame::Hello { default_branch_tip, .. } if default_branch_tip == fill_tips
                    ),
                    "{fill_tips}"
                );
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: true,
                        gate_jobs: true,
                        return_paths: false,
                        return_chunked_max_bytes: None,
                        return_chunked: false,
                        sibling_paths: true,
                        platforms: vec!["linux".into()],
                    },
                )
                .await
                .unwrap();
            };
            let (channel, ()) =
                tokio::join!(Channel::from_hosted(Box::new(client), fill_tips), server);
            let channel = channel.unwrap();
            assert!(channel.sibling_paths);
            assert_eq!(channel.fill_tips, fill_tips);
        }
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
                    return_chunked_max_bytes: None,
                    return_chunked: false,
                    sibling_paths: false,
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
                        return_chunked_max_bytes: None,
                        return_chunked: false,
                        sibling_paths: capable,
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
            let channel = channel.unwrap();
            assert_eq!(channel.gate_jobs, capable);
            assert_eq!(channel.sibling_paths, capable);
        }
    }
}

#[cfg(test)]
mod disconnect_tests {
    use super::*;

    #[tokio::test]
    async fn boundary_eof_is_not_reported_as_a_truncated_frame() {
        let (client, server) = tokio::io::duplex(64);
        drop(server);
        let mut channel = Channel::new(Box::new(client));
        let error = channel.next().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "EOF at frame boundary");
    }

    #[tokio::test]
    async fn partial_body_eof_still_reports_truncation() {
        let (client, mut server) = tokio::io::duplex(64);
        server.write_all(&[0, 0, 0, 12, b'{']).await.unwrap();
        drop(server);
        let mut channel = Channel::new(Box::new(client));
        let error = channel.next().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "truncated JSON frame");
    }
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
                "EOF at frame boundary",
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
