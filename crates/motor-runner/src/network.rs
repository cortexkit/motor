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
    enable(config, false).err().map(|error| error.to_string())
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
                config
                    .network_cleanup_pending
                    .store(true, std::sync::atomic::Ordering::Release);
                config.job_network_unavailable = format!("network startup cleanup failed: {error}");
                tracing::warn!(%error, "job network cleanup failed; net requests will retry");
                return;
            }
        }
    }
    let result = enable(config, true);
    if let Err(error) = result {
        config.job_network_unavailable = error.to_string();
        tracing::warn!(%error, "job outbound network not ready; net requests will re-probe");
    }
}

fn enable(config: &Config, full_probe: bool) -> io::Result<()> {
    #[cfg(not(target_os = "linux"))]
    let _ = full_probe;
    if config.windows_instance() {
        return Err(io::Error::other("Windows job network is unsupported"));
    }
    if config.job_network.is_none() {
        return Err(io::Error::other("--job-network veth is not enabled"));
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
        if config
            .network_cleanup_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            linux::reconcile()?;
            config
                .network_cleanup_pending
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if full_probe {
            linux::probe()
        } else {
            linux::admission_probe()
        }
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::other("veth job network requires a Linux host"))
}

#[cfg(target_os = "linux")]
#[path = "network_linux.rs"]
pub(crate) mod linux;
