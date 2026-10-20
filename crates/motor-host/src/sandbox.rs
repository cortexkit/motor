//! Boat's in-sandbox admission mode. SSH supplies transport authentication and
//! confidentiality; the ordinary host prelude still authorizes every lease.

use crate::helper::{read_json, write_json, Control};
use crate::journal::FileJournal;
use crate::server::{serve, unix_ms, Runtime};
use crate::state::{Error, Host, Refusal};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;

// Interim Boat lease values, confirmed with CLOUD's draft. Keep the audience,
// placement, identity-file and forwarding-port conventions together until the
// provider contract is final. The image must not contain an identity file:
// the broker installs a new one for every sandbox created from a snapshot.
pub const ID_FILE: &str = "/etc/motor/tenant/id";
pub const RUNNER_PORT: u16 = 9443;
pub fn audience(sandbox_id: &str) -> String {
    format!("boat:{sandbox_id}")
}

pub(crate) struct IdentityFile {
    path: PathBuf,
    owner: u32,
}
impl IdentityFile {
    pub(crate) fn read(&self) -> Result<String> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == self.owner
                && metadata.permissions().mode() & 0o7777 == 0o444,
            "sandbox identity must be a root-owned 0444 regular file"
        );
        let mut bytes = Vec::new();
        file.by_ref().take(65).read_to_end(&mut bytes)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= 64 && bytes.iter().all(u8::is_ascii_graphic),
            "sandbox identity must be nonempty ASCII without whitespace"
        );
        let identity = String::from_utf8(bytes)?;
        ensure!(
            motor_lease::valid_name(&identity),
            "invalid sandbox identity"
        );
        Ok(identity)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub journal_dir: PathBuf,
    pub job_socket: PathBuf,
    pub event_socket: PathBuf,
    pub admin_socket: PathBuf,
    pub vcpu_ceiling: u32,
    #[serde(default = "default_identity")]
    pub id_file: PathBuf,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Where to publish the readiness marker. The broker books a sandbox only
    /// after this file exists and holds the sandbox id; absent means no marker.
    #[serde(default)]
    pub ready_marker: Option<PathBuf>,
}
fn default_identity() -> PathBuf {
    ID_FILE.into()
}
fn default_port() -> u16 {
    RUNNER_PORT
}

struct LocalRunner {
    jobs: PathBuf,
    events: PathBuf,
    admin: PathBuf,
}
impl Control for LocalRunner {
    fn adopt(&self, _: &str) -> Result<bool> {
        Ok(true)
    }
    fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
        // The broker owns sandbox lifecycle. No helper, VM boot, quota or
        // suspension operation is authorized from inside the sandbox.
        Ok(1)
    }
    fn quota(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    fn relay(&self, _: &str, port: u32) -> Result<UnixStream> {
        let path = match port {
            52 => &self.jobs,
            53 => &self.events,
            54 => &self.admin,
            _ => anyhow::bail!("invalid local runner channel"),
        };
        UnixStream::connect(path).map_err(|_| Error::Refused(Refusal::Capacity).into())
    }
    fn admin(&self, tenant: &str, request: &Value) -> Result<Value> {
        let mut socket = self.relay(tenant, 54)?;
        socket.set_read_timeout(Some(Duration::from_secs(30)))?;
        socket.set_write_timeout(Some(Duration::from_secs(30)))?;
        write_json(&mut socket, request)?;
        read_json(&mut socket)
    }
    fn suspend(&self, _: &str) -> Result<()> {
        anyhow::bail!("sandbox lifecycle belongs to the broker")
    }
    fn available_bytes(&self) -> Result<u64> {
        anyhow::bail!("sandbox lifecycle belongs to the broker")
    }
}

pub fn runtime(config: Config) -> Result<Arc<Runtime>> {
    runtime_with_identity_owner(config, 0)
}

/// [`runtime`], but trusting an identity file owned by `identity_owner`
/// instead of root. Only for tests in other crates, which cannot create a
/// root-owned file; the `--sandbox` binary always calls [`runtime`].
#[doc(hidden)]
pub fn runtime_with_identity_owner(config: Config, identity_owner: u32) -> Result<Arc<Runtime>> {
    ensure!(config.vcpu_ceiling > 0, "vcpu_ceiling must be positive");
    let (journal, snapshot) = FileJournal::open(&config.journal_dir)?;
    let mut host = Host::sandbox(config.vcpu_ceiling, snapshot, Box::new(journal));
    host.restart_ms = unix_ms();
    host.prior_alive_ms = host.restart_ms;
    Ok(Runtime::with_identity(
        String::new(),
        host,
        Arc::new(LocalRunner {
            jobs: config.job_socket,
            events: config.event_socket,
            admin: config.admin_socket,
        }),
        Some(IdentityFile {
            path: config.id_file,
            owner: identity_owner,
        }),
    ))
}

/// The bind address is not configurable: only the SSH forward may reach it.
pub async fn bind(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await
}

/// How often a missing JWKS is fetched again while no key set is loaded.
const KEY_RETRY_S: u64 = 5;

/// Make the readiness marker agree with the endpoint's current state: the
/// sandbox id when broker keys are loaded and the identity file is valid,
/// otherwise no file. Called only once the loopback listener is bound. The
/// marker is replaced by rename, so a reader never sees a partial id.
pub(crate) fn publish_readiness(runtime: &Runtime, marker: &Path) -> std::io::Result<()> {
    let wanted = if runtime.keys.read().expect("keys lock").is_empty() {
        None
    } else {
        runtime
            .sandbox_identity
            .as_ref()
            .and_then(|identity| identity.read().ok())
    };
    let current = std::fs::read_to_string(marker).ok();
    if wanted == current {
        return Ok(());
    }
    match wanted {
        Some(id) => {
            let temporary = marker.with_extension("tmp");
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)?;
            // The creation mode is filtered by the umask; readers outside root
            // (the broker's exec user) need 0644 exactly.
            file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
            std::io::Write::write_all(&mut file, id.as_bytes())?;
            drop(file);
            std::fs::rename(&temporary, marker)?;
            tracing::info!("endpoint_ready_marker_written");
        }
        None => {
            std::fs::remove_file(marker)?;
            tracing::info!("endpoint_ready_marker_removed");
        }
    }
    Ok(())
}

pub async fn run(config: Config) -> Result<()> {
    let port = config.port;
    let marker = config.ready_marker.clone();
    let runtime = runtime(config)?;
    runtime.fetch_keys.store(true, Ordering::Relaxed);
    let startup = runtime.clone();
    tokio::task::spawn_blocking(move || {
        startup.refresh_keys(0);
        startup.recover()
    })
    .await??;
    let refresher = runtime.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(600));
        refresher.refresh_keys(0);
    });
    let ticker = runtime.clone();
    std::thread::spawn(move || loop {
        if ticker.tick(unix_ms()).is_err() {
            tracing::error!("sandbox_tick_failed");
        }
        std::thread::sleep(Duration::from_millis(100));
    });
    let listener = bind(port).await.context("sandbox loopback listener")?;
    if let Some(marker) = marker {
        let ready = runtime.clone();
        std::thread::spawn(move || loop {
            // A startup fetch can fail (network not up yet); retry until a
            // key set is held instead of waiting for the 10-minute refresh.
            if ready.keys.read().expect("keys lock").is_empty() {
                ready.refresh_keys(KEY_RETRY_S);
            }
            if publish_readiness(&ready, &marker).is_err() {
                tracing::warn!("endpoint_ready_marker_failed");
            }
            std::thread::sleep(Duration::from_secs(1));
        });
    }
    loop {
        let (socket, _) = listener.accept().await?;
        let runtime = runtime.clone();
        tokio::spawn(async move {
            // This is deliberately plaintext framing, not a TLS listener.
            if let Err(error) = serve(runtime, socket).await {
                let report = crate::server::describe_close(&error);
                tracing::warn!(
                    jti = report.jti,
                    side = report.side,
                    step = report.step,
                    cause = %report.cause,
                    "sandbox_connection_closed"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests;
