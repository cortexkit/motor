//! Optional process-owned gateway. Its executor has no strong reference back to
//! the runner, so closing a runner also stops its watcher and listeners.
use super::gateway::{Gateway, Limits, Tool};
use super::provisioning::{Layout, Status, Unavailable, Watch};
use super::transport::{Forwarder, Transport};
use std::io;
use std::sync::{Arc, Mutex};

pub(crate) struct Runtime {
    pub(crate) watch: Watch,
    pub(crate) gateway: Gateway,
    pub(crate) handle: tokio::runtime::Handle,
    tools: Vec<(Tool, u16)>,
    pub(crate) fault: Arc<Mutex<Option<String>>>,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.get_mut().unwrap().take() {
            let _ = shutdown.send(());
        }
    }
}

impl Runtime {
    pub(crate) fn start(config: &crate::Config) -> Result<Arc<Self>, String> {
        let layout = Layout::for_config(config);
        layout
            .prepare()
            .map_err(|e| failure("provisioning directory", &e))?;
        let watch = Watch::new(layout);
        let gateway = Gateway::new(Limits::default(), Arc::new(watch.clone()))
            .map_err(|e| failure("gateway initialization", &e))?;
        let role = super::provisioning::HostRole::of(config);
        let mut tools = Vec::new();
        for tool in [Tool::Sccache, Tool::Ccache] {
            if tool == Tool::Ccache && !super::ccache::installed(role) {
                continue;
            }
            if std::process::Command::new(tool.id())
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
            {
                let port = config
                    .remote_cache_forwarder_ports
                    .get(tool.id())
                    .copied()
                    .ok_or_else(|| format!("missing {} forwarder port", tool.id()))?;
                tools.push((tool, port));
            }
        }
        if tools.is_empty() {
            return Err("no supported cache tools installed".into());
        }
        Self::with_gateway(watch, gateway, tools)
    }

    pub(crate) fn with_gateway(
        watch: Watch,
        gateway: Gateway,
        tools: Vec<(Tool, u16)>,
    ) -> Result<Arc<Self>, String> {
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("remote-cache".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(executor) => {
                        let _ = send.send(Ok(executor.handle().clone()));
                        executor.block_on(async {
                            let _ = stopped.await;
                        });
                    }
                    Err(error) => {
                        let _ = send.send(Err(failure("gateway executor", &error)));
                    }
                }
            })
            .map_err(|e| failure("gateway executor thread", &e))?;
        let handle = receive
            .recv()
            .map_err(|_| "gateway executor stopped".to_string())??;
        Transport::new(gateway.clone(), handle.clone(), "validation", &tools)
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            watch,
            gateway,
            handle,
            tools,
            fault: Arc::new(Mutex::new(None)),
            shutdown: Mutex::new(Some(shutdown)),
        }))
    }

    pub(crate) fn forwarders(&self) -> Vec<Forwarder> {
        self.tools
            .iter()
            .map(|&(tool, port)| Forwarder::new(tool, port))
            .collect()
    }

    pub(crate) fn transport(&self, tenant: &str, tools: &[Tool]) -> io::Result<Transport> {
        let tools: Vec<_> = self
            .tools
            .iter()
            .copied()
            .filter(|(tool, _)| tools.contains(tool))
            .collect();
        Transport::new(self.gateway.clone(), self.handle.clone(), tenant, &tools)
            .map(|transport| transport.reporting(self.fault.clone()))
    }
}

/// OS error text may contain paths; report the stage and error kind instead.
pub(crate) fn failure(stage: &str, error: &io::Error) -> String {
    format!(
        "{stage}: {:?} (errno {:?})",
        error.kind(),
        error.raw_os_error()
    )
}

/// Provisioning diagnostics must not copy config values or key material into
/// health. In particular, serde errors can quote invalid configuration values.
pub(crate) fn reason(status: &Status) -> Option<String> {
    match status {
        Status::Available => None,
        Status::Unavailable(Unavailable::PrefixMismatch { .. }) => Some("prefix mismatch".into()),
        Status::Unavailable(Unavailable::GenerationReused { .. }) => {
            Some("generation reused".into())
        }
        Status::Unavailable(Unavailable::NoCompleteGeneration { detail }) => {
            let cause = if detail.contains("generations differ") {
                "mixed generations"
            } else if detail.contains("No such file") {
                "missing provisioning file"
            } else if detail.contains("Permission denied") {
                "provisioning permission denied"
            } else if detail.contains("mode ") {
                "provisioning file mode"
            } else if detail.contains("owned by") {
                "provisioning ownership"
            } else {
                "invalid provisioning files"
            };
            Some(cause.into())
        }
    }
}
