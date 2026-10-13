//! Per-job outbound networking. Host resources belong to systemd, not the daemon.
use crate::engine::Config;
use std::{io, str::FromStr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobNetwork {
    Veth,
}

impl FromStr for JobNetwork {
    type Err = io::Error;
    fn from_str(value: &str) -> io::Result<Self> {
        match value {
            "veth" => Ok(Self::Veth),
            _ => Err(io::Error::other("unknown job network; expected veth")),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Phase {
    Probe,
    Attach(motor_protocol::Uuid),
}
#[cfg(test)]
pub(crate) type Hook = std::sync::Arc<dyn Fn(Phase) -> io::Result<()> + Send + Sync>;

pub(crate) fn unsupported(config: &Config) -> Option<String> {
    if config.windows_instance() {
        Some("outbound network is unavailable on the Windows instance".into())
    } else if config.job_network.is_none() {
        Some(config.job_network_unavailable.clone())
    } else {
        None
    }
}

pub(crate) fn initialize(config: &mut Config) {
    #[cfg(target_os = "linux")]
    if config.isolate_jobs && !config.windows_instance() {
        #[cfg(test)]
        let real_host = config.job_network_hook.is_none();
        #[cfg(not(test))]
        let real_host = true;
        // Reclaim orphan links even if networking was switched off or the
        // firewall probe fails. Live units remain untouched during deployments.
        if real_host {
            if let Err(error) = linux::reconcile() {
                config.job_network = None;
                config.job_network_unavailable = format!("network startup cleanup failed: {error}");
                tracing::warn!(%error, "job network cleanup failed; outbound network disabled");
                return;
            }
        }
    }
    let result = enable(config);
    if let Err(error) = result {
        config.job_network = None;
        config.job_network_unavailable = error.to_string();
        tracing::warn!(%error, "job outbound network disabled");
    }
}

fn enable(config: &Config) -> io::Result<()> {
    if config.job_network.is_none() {
        return Err(io::Error::other("--job-network veth is not enabled"));
    }
    if config.windows_instance() {
        return Err(io::Error::other("Windows job network is unsupported"));
    }
    #[cfg(test)]
    if let Some(hook) = &config.job_network_hook {
        return hook(Phase::Probe);
    }
    if !config.isolate_jobs {
        return Err(io::Error::other("job network requires isolate_jobs=true"));
    }
    #[cfg(target_os = "linux")]
    {
        linux::probe()
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::other("veth job network requires a Linux host"))
}

#[cfg(target_os = "linux")]
#[path = "network_linux.rs"]
pub(crate) mod linux;
