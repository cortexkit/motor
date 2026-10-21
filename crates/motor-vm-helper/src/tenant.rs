//! Persistent tenant VMs have no connection-owned job lease. All paths and
//! limits come from the root-owned registry, never from an RPC argument.
use crate::{Error, Reply, Response, Result};
use serde::{Deserialize, Serialize};
use std::{os::fd::OwnedFd, path::PathBuf};

pub const REGISTRY: &str = "/etc/motor-vm-helper/tenants";
pub const TENANTS: &str = "/var/lib/motor-vm/tenants";
pub const IMAGES: &str = "/var/lib/motor-vm/tenant-images";

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 128
        || name.bytes().any(|b| b == b'/' || b == 0)
    {
        return Err(Error("invalid tenant name".into()));
    }
    Ok(())
}
pub fn validate_port(port: u32) -> Result<()> {
    if !matches!(port, 52..=54) {
        return Err(Error("tenant relay port must be 52, 53 or 54".into()));
    }
    Ok(())
}
#[derive(Debug, Clone, Copy)]
pub enum Operation {
    Ensure,
    AttachDisk,
    Suspend,
    Restore,
    Stop,
    Cpu(u32),
    Egress,
    Relay(u32),
    Adopt,
    Metrics,
    RecoveryComplete,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub disk: PathBuf,
    pub generation: PathBuf,
    pub suspend: PathBuf,
    pub image_version: String,
    pub vcpu_ceiling: u32,
    pub guest_ram_mib: u64,
    pub disk_quota_bytes: u64,
    pub cpuset: String,
    pub tap_index: u8,
    pub idle_suspend_s: u64,
    pub host_floor_bytes: u64,
    #[serde(default = "default_headroom")]
    pub memory_headroom_mib: u64,
    #[serde(default = "default_snapshot_high")]
    pub snapshot_memory_high_mib: u64,
    #[serde(default = "default_io_weight")]
    pub io_weight: u32,
    #[serde(default)]
    pub capture_console: bool,
}
fn default_headroom() -> u64 {
    512
}
fn default_snapshot_high() -> u64 {
    16384
}
fn default_io_weight() -> u32 {
    100
}
impl Registry {
    pub fn validate(&self, tenant: &str) -> Result<()> {
        validate_name(tenant)?;
        validate_name(&self.image_version)?;
        let home = PathBuf::from(TENANTS).join(tenant);
        if self.disk != home.join("disk.img")
            || self.generation != home.join("generation")
            || self.suspend != home.join("suspend")
            || self.tap_index == 0
            || self.tap_index == 255
            || !(1..=256).contains(&self.vcpu_ceiling)
            || self.guest_ram_mib == 0
            || self.disk_quota_bytes == 0
            || !(1..=10000).contains(&self.io_weight)
            || !crate::config::cpu_list(&self.cpuset)
            || self.memory_headroom_mib < 512
        {
            return Err(Error("invalid tenant registry".into()));
        }
        self.memory_max()?;
        self.snapshot_bytes()?;
        if self.snapshot_memory_high_mib == 0 {
            return Err(Error("snapshot memory.high must be positive".into()));
        }
        Ok(())
    }
    pub fn image(&self) -> PathBuf {
        PathBuf::from(IMAGES).join(&self.image_version)
    }
    pub fn base(&self) -> PathBuf {
        self.image()
            .join(format!("base/{}-{}", self.vcpu_ceiling, self.guest_ram_mib))
    }
    pub fn tap(&self) -> String {
        format!("mtap{}", self.tap_index)
    }
    pub fn memory_max(&self) -> Result<u64> {
        self.guest_ram_mib
            .checked_add(self.memory_headroom_mib)
            .and_then(|v| v.checked_mul(1048576))
            .ok_or_else(|| Error("tenant memory limit overflow".into()))
    }
    pub fn snapshot_bytes(&self) -> Result<u64> {
        self.guest_ram_mib
            .checked_mul(1048576)
            .and_then(|v| v.checked_add(self.host_floor_bytes))
            .ok_or_else(|| Error("tenant snapshot space overflow".into()))
    }
    pub fn cpu_max(&self, vcpus: u32) -> Result<String> {
        if vcpus == 0 || vcpus > self.vcpu_ceiling {
            return Err(Error("vcpus outside tenant ceiling".into()));
        }
        Ok(format!("{} 100000", u64::from(vcpus) * 100000))
    }
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SnapshotKey {
    pub version: String,
    pub kernel_sha256: String,
    pub rootfs_sha256: String,
    #[serde(default)]
    pub generation: u64,
}

/// Platform methods keep the destructive boundary small. In particular, load
/// leaves the guest paused; consume must durably unlink before resume is called.
pub trait Backend {
    fn unclean(&mut self, _registry: &Registry) -> Result<bool> {
        Ok(false)
    }
    fn mark_unclean(&mut self, _registry: &Registry, _reason: &str) -> Result<()> {
        Err(Error("unclean record unavailable".into()))
    }
    fn prepare_stop(&mut self, tenant: &str) -> Result<()> {
        self.prepare(tenant)
    }
    fn recovery_complete(&mut self, _tenant: &str, _registry: &Registry) -> Result<()> {
        Err(Error("recovery unavailable".into()))
    }
    /// Whether the durable running marker is present. `load` writes it before
    /// the VMM starts and only a clean stop or a completed suspend removes it,
    /// so a marker on a tenant that is not running means its last VM ended
    /// without the guest flushing its disk: the helper was killed, the host
    /// lost power, or the VMM died.
    fn was_running(&mut self, _registry: &Registry) -> Result<bool> {
        Ok(false)
    }
    fn clear_running(&mut self, _registry: &Registry) -> Result<()> {
        Ok(())
    }
    /// Tenants this helper process started and may still be running.
    fn running_tenants(&mut self) -> Vec<String> {
        Vec::new()
    }
    fn registry(&mut self, tenant: &str) -> Result<Registry>;
    fn key(&mut self, registry: &Registry) -> Result<SnapshotKey>;
    fn saved_key(&mut self, registry: &Registry) -> Result<Option<SnapshotKey>>;
    fn running(&mut self, tenant: &str) -> bool;
    fn was_restored(&self, _tenant: &str) -> bool {
        false
    }
    fn load(&mut self, tenant: &str, r: &Registry, saved: bool) -> Result<()>;
    fn drive(&mut self, tenant: &str, r: &Registry) -> Result<()>;
    fn consume(&mut self, r: &Registry) -> Result<()>;
    fn advance(&mut self, r: &Registry) -> Result<u64>;
    fn resume(&mut self, tenant: &str) -> Result<()>;
    fn prepare(&mut self, tenant: &str) -> Result<()>;
    fn thaw(&mut self, tenant: &str) -> Result<()>;
    fn snapshot(&mut self, tenant: &str, r: &Registry, key: &SnapshotKey) -> Result<()>;
    fn stop(&mut self, tenant: &str) -> Result<()>;
    fn available(&mut self, r: &Registry) -> Result<u64>;
    fn cpu(&mut self, tenant: &str, value: &str) -> Result<()>;
    fn egress(&mut self, tenant: &str, r: &Registry) -> Result<()>;
    fn relay(&mut self, tenant: &str, port: u32) -> Result<OwnedFd>;
    fn metrics(&mut self, _tenant: &str, _registry: &Registry) -> Result<(u64, u64)> {
        Err(Error("tenant metrics unavailable".into()))
    }
}

pub struct Controller<B: Backend>(pub B);
impl<B: Backend> Controller<B> {
    pub fn handle(&mut self, tenant: &str, op: Operation) -> Result<Response> {
        validate_name(tenant)?;
        if let Operation::Relay(port) = op {
            validate_port(port)?;
        }
        let r = self.0.registry(tenant)?;
        r.validate(tenant)?;
        self.record_lost_vm(tenant, &r)?;
        let mut restored = self.0.was_restored(tenant);
        match op {
            Operation::Ensure | Operation::Restore => {
                if self.0.unclean(&r)? && self.0.running(tenant) {
                    self.0.stop(tenant)?;
                }
                if !self.0.running(tenant) {
                    restored = self.wake(tenant, &r)?;
                }
            }
            Operation::AttachDisk => self.0.drive(tenant, &r)?,
            Operation::Cpu(vcpus) => self.0.cpu(tenant, &r.cpu_max(vcpus)?)?,
            Operation::Egress => self.0.egress(tenant, &r)?,
            Operation::Adopt => {}
            Operation::RecoveryComplete => self.0.recovery_complete(tenant, &r)?,
            Operation::Metrics => {
                let (memory_bytes, egress_bytes) = self.0.metrics(tenant, &r)?;
                return Ok(Response::new(Reply::TenantMetrics {
                    memory_bytes,
                    egress_bytes,
                }));
            }
            Operation::Relay(port) => {
                return Ok(Response {
                    reply: Reply::Relay { fd_count: 1 },
                    fds: vec![self.0.relay(tenant, port).map_err(|error| {
                        crate::Error(format!(
                            "open_relay tenant={tenant} port={port} step=open_guest_connection: {error}"
                        ))
                    })?],
                })
            }
            Operation::Stop => {
                if self.0.running(tenant) {
                    let prepared = self.0.prepare_stop(tenant);
                    let record = match &prepared {
                        Ok(()) => Ok(()),
                        Err(error) => self.0.mark_unclean(&r, &error.to_string()),
                    };
                    let prepared = prepared.is_ok();
                    if let Err(error) = self.0.stop(tenant) {
                        let _ = self.0.thaw(tenant);
                        return Err(error);
                    }
                    record?;
                    // Only a stop whose guest flush succeeded is clean. After a
                    // failed flush the marker stays as a second durable record.
                    if prepared {
                        self.0.clear_running(&r)?;
                    }
                }
            }
            Operation::Suspend => {
                if !self.0.running(tenant) {
                    return Err(Error("tenant is not running".into()));
                }
                if self.0.available(&r)? < r.snapshot_bytes()? {
                    return Err(Error("suspend_skipped_space".into()));
                }
                let key = self.0.key(&r)?;
                // A refused or timed-out prepare must never reach snapshot/create.
                if let Err(error) = self.0.prepare(tenant) {
                    let _ = self.0.thaw(tenant);
                    return Err(error);
                }
                if let Err(error) = self.0.snapshot(tenant, &r, &key) {
                    let cleanup = self.0.consume(&r);
                    self.0.thaw(tenant)?;
                    cleanup?;
                    return Err(error);
                }
                if let Err(error) = self.0.stop(tenant) {
                    let cleanup = self.0.consume(&r);
                    self.0.thaw(tenant)?;
                    cleanup?;
                    return Err(error);
                }
                // The guest froze its filesystems before the snapshot, so the
                // disk is consistent whether the snapshot or a fresh boot runs next.
                self.0.clear_running(&r)?;
            }
        }
        let generation = self.0.key(&r)?.generation;
        Ok(Response::new(Reply::Tenant {
            running: self.0.running(tenant),
            restored,
            generation,
            recovery_required: self.0.unclean(&r)?,
        }))
    }
    /// Turns a running marker left by a VM that is no longer running into the
    /// unclean-stop record, so the next start skips the suspend snapshot and
    /// the guest runs its recovery check. This covers a helper killed by
    /// SIGKILL, a host power loss and a VMM that died on its own: none of them
    /// ran the clean stop that removes the marker.
    fn record_lost_vm(&mut self, tenant: &str, r: &Registry) -> Result<()> {
        if !self.0.running(tenant) && self.0.was_running(r)? && !self.0.unclean(r)? {
            self.0.mark_unclean(r, "vm_stopped_without_clean_stop")?;
        }
        Ok(())
    }
    /// The service stop path: stops every running tenant the way
    /// `Operation::Stop` does (guest drain, `sync -f /tenant` and freeze,
    /// bounded at 15 s, then the VMM is killed). A tenant that cannot be
    /// stopped cleanly is recorded as unclean before this returns, so the
    /// caller may exit and let systemd kill whatever is left. Returns the
    /// tenants that did not stop cleanly, with the reason.
    pub fn stop_all(&mut self) -> Vec<(String, Error)> {
        let mut failures = Vec::new();
        for tenant in self.0.running_tenants() {
            let started = std::time::Instant::now();
            let failure = match self.handle(&tenant, Operation::Stop) {
                // A refused or timed-out guest flush still stops the VM. Stop
                // has then recorded the reason and left the running marker.
                Ok(_) => {
                    let flushed = self
                        .0
                        .registry(&tenant)
                        .and_then(|r| self.0.was_running(&r))
                        .is_ok_and(|marker| !marker);
                    (!flushed).then(|| Error("guest flush failed; recorded as unclean".into()))
                }
                Err(error) => {
                    // The VM may still be running and will be killed by
                    // systemd. The marker is already durable; this adds the
                    // reason when the registry is readable.
                    if let Ok(r) = self.0.registry(&tenant) {
                        let _ = self.0.mark_unclean(&r, &format!("service_stop: {error}"));
                    }
                    Some(error)
                }
            };
            // The journal keeps these, the only measurements of the stop.
            let ms = started.elapsed().as_millis();
            match failure {
                None => eprintln!("tenant {tenant}: clean stop took {ms} ms"),
                Some(error) => {
                    eprintln!("tenant {tenant}: unclean stop after {ms} ms: {error}");
                    failures.push((tenant, error));
                }
            }
        }
        failures
    }
    fn wake(&mut self, tenant: &str, r: &Registry) -> Result<bool> {
        let key = self.0.key(r)?;
        let saved = self
            .0
            .saved_key(r)
            .ok()
            .flatten()
            .is_some_and(|saved| saved == key)
            && !self.0.unclean(r)?;
        self.0.egress(tenant, r)?;
        if saved {
            let result = self
                .0
                .load(tenant, r, true)
                .and_then(|()| self.0.drive(tenant, r));
            if result.is_ok() {
                let durable = (|| {
                    self.0.consume(r)?;
                    self.0.advance(r)
                })();
                if let Err(error) = durable {
                    // A failed durability barrier must not leave an apparently
                    // running, paused guest available for adoption by a reconnect.
                    self.0.stop(tenant)?;
                    return Err(error);
                }
                if self
                    .0
                    .resume(tenant)
                    .and_then(|()| self.0.thaw(tenant))
                    .is_ok()
                {
                    return Ok(true);
                }
            }
            self.0.stop(tenant)?;
        }
        self.0.consume(r)?;
        let result = (|| {
            self.0.load(tenant, r, false)?;
            self.0.drive(tenant, r)?;
            self.0.advance(r)?;
            self.0.resume(tenant)
        })();
        if result.is_err() {
            self.0.stop(tenant)?;
        }
        result.map(|()| false)
    }
}

#[cfg(test)]
pub(crate) mod tests;
