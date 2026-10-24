//! How build tools inside a job or fill sandbox reach the gateway.
//!
//! Jobs and fills run with a private network namespace, so no TCP port on the
//! host can reach them. Instead the runner creates, per unit, one Unix socket
//! per active tool in the unit's private runtime directory and binds that
//! directory read-only into the unit's sandbox at [`SANDBOX_SOCKETS`]. A job's
//! sockets are read sockets owned by that job, so every read connection is
//! attributed to exactly one job; a fill's sockets are write sockets, each
//! with its own PUT ledger. No socket is ever bound into another unit's
//! sandbox, so a job has no path at all to a fill's write endpoint.
//!
//! Tools that need an `http://` URL reach their socket through a forwarder
//! (built from `cache-forwarder.rs.in`) that the unit's fixed shell starts
//! before the unit's command. It listens on `127.0.0.1:<port>` inside the
//! unit's own network namespace, holds no key, and dies with the unit's
//! control group. A unit that kills its forwarder or takes its port only
//! loses its own cache.
use super::gateway::{Adapter, Gateway, Listener, Tool};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Where a unit's sockets appear inside its sandbox.
pub(crate) const SANDBOX_SOCKETS: &str = "/motor-gateway";
/// The forwarder's file name in the sandbox's read-only tool directory.
pub(crate) const FORWARDER_NAME: &str = "cache-forwarder";
/// The forwarder's path inside the sandbox.
const SANDBOX_FORWARDER: &str = "/motor-tools/cache-forwarder";

/// The wire format each tool speaks to its socket.
fn adapter(tool: Tool) -> Adapter {
    match tool {
        Tool::Sccache => Adapter::S3,
        Tool::Ccache => Adapter::Http,
    }
}

/// The socket file name of a tool, in the unit's socket directory.
fn socket_name(tool: Tool) -> String {
    format!("{}.sock", tool.id())
}

/// What the runner gives a sandbox so its active tools reach the gateway.
pub(crate) struct Transport {
    gateway: Gateway,
    runtime: tokio::runtime::Handle,
    tenant: String,
    tools: Vec<(Tool, u16)>,
}

/// Whose sockets a unit gets.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Role<'a> {
    /// A job's read sockets; the job id ties them to one admission owner.
    Job(&'a str),
    /// A trusted fill's write sockets.
    Fill,
}

impl Transport {
    /// `tools` names each tool active in the unit with the loopback port its
    /// forwarder listens on. Tools and ports must be distinct, and port 0 is
    /// refused because the tool's URL must be known before the unit starts.
    /// `tenant` is the cache tenant of the active generation; the gateway never
    /// signs a request from these sockets for another tenant.
    pub(crate) fn new(
        gateway: Gateway,
        runtime: tokio::runtime::Handle,
        tenant: &str,
        tools: &[(Tool, u16)],
    ) -> io::Result<Self> {
        for (index, (tool, port)) in tools.iter().enumerate() {
            if *port == 0 {
                return Err(io::Error::other(format!(
                    "forwarder port for {} must not be 0",
                    tool.id()
                )));
            }
            if tools[..index].iter().any(|(other, _)| other == tool) {
                return Err(io::Error::other(format!(
                    "{} is listed twice in the cache transport",
                    tool.id()
                )));
            }
            if tools[..index].iter().any(|(_, other)| other == port) {
                return Err(io::Error::other(format!(
                    "forwarder port {port} is used by more than one tool"
                )));
            }
        }
        Ok(Self {
            gateway,
            runtime,
            tenant: tenant.to_string(),
            tools: tools.to_vec(),
        })
    }

    /// Creates `directory` (mode 0755, which must not exist yet) and one
    /// listening socket per tool in it, each mode 0600. The caller grants the
    /// unit's account access to [`Sockets::paths`] and binds `directory` at
    /// [`SANDBOX_SOCKETS`] in that unit's sandbox only. Each socket is served
    /// until the returned value is dropped.
    pub(crate) fn open(&self, directory: &Path, role: Role<'_>) -> io::Result<Sockets> {
        std::fs::DirBuilder::new().mode(0o755).create(directory)?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755))?;
        // Binding a listener spawns its accept task on the runner's runtime;
        // sandbox preparation runs outside it.
        let _runtime = self.runtime.enter();
        let mut sockets = Sockets {
            listeners: Vec::new(),
            forwarders: Vec::new(),
        };
        for &(tool, port) in &self.tools {
            let binding = match role {
                Role::Job(job) => self
                    .gateway
                    .read_binding(job, &self.tenant, tool, adapter(tool)),
                Role::Fill => self
                    .gateway
                    .write_binding(&self.tenant, tool, adapter(tool)),
            };
            let path = directory.join(socket_name(tool));
            let listener = self.gateway.listen(&path, binding)?;
            // Only the owner may connect until the caller adds the unit's
            // account.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            sockets.listeners.push(listener);
            sockets.forwarders.push(Forwarder { tool, port });
        }
        Ok(sockets)
    }
}

/// One unit's sockets, served until dropped.
pub(crate) struct Sockets {
    listeners: Vec<Listener>,
    forwarders: Vec<Forwarder>,
}

impl Sockets {
    /// The socket files on the host, one per tool.
    pub(crate) fn paths(&self) -> Vec<PathBuf> {
        self.listeners
            .iter()
            .map(|listener| listener.path().to_path_buf())
            .collect()
    }
    /// The listeners, one per tool, in the order the tools were given.
    pub(crate) fn listeners(&self) -> &[Listener] {
        &self.listeners
    }
    /// The forwarders the unit's shell starts, one per tool.
    pub(crate) fn forwarders(&self) -> &[Forwarder] {
        &self.forwarders
    }
    /// The write ledgers of a fill's sockets, one per tool the fill runs.
    /// A read socket has none, so this is empty for a job.
    pub(crate) fn ledgers(&self) -> Vec<std::sync::Arc<super::gateway::WriteLedger>> {
        self.listeners
            .iter()
            .filter_map(|listener| listener.binding().ledger().cloned())
            .collect()
    }
}

/// A forwarder the unit's fixed shell starts before the unit's command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Forwarder {
    tool: Tool,
    port: u16,
}

impl Forwarder {
    #[cfg(test)]
    pub(crate) fn new(tool: Tool, port: u16) -> Self {
        Self { tool, port }
    }
    pub(crate) fn tool(&self) -> Tool {
        self.tool
    }
    pub(crate) fn port(&self) -> u16 {
        self.port
    }
    /// The URL the tool is configured with inside the unit.
    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    /// The tool's socket path inside the sandbox.
    pub(crate) fn socket(&self) -> String {
        format!("{SANDBOX_SOCKETS}/{}", socket_name(self.tool))
    }
    /// The shell source that starts this forwarder. Its output is discarded
    /// and its exit status ignored: a forwarder that cannot start leaves the
    /// unit to run uncached with its own exit status.
    pub(crate) fn start(&self) -> String {
        format!(
            "{SANDBOX_FORWARDER} 127.0.0.1:{} {} </dev/null >/dev/null 2>&1; ",
            self.port,
            self.socket()
        )
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod tests;
