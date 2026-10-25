//! Persistent tenant VMs have no connection-owned job lease. All paths and
//! limits come from the root-owned registry, never from an RPC argument.
use crate::{Error, Reply, Response, Result};
use serde::{Deserialize, Serialize};
use std::{os::fd::OwnedFd, path::PathBuf};
pub(crate) mod activation;
pub use activation::ResizeActivation;

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
    PrepareStart,
    Activate,
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
    Remove,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub disk: PathBuf,
    pub generation: PathBuf,
    pub suspend: PathBuf,
    pub image_version: String,
    pub vcpu_ceiling: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_weight"
    )]
    pub max_weight: Option<u32>,
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
fn optional_weight<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u32>, D::Error> {
    u32::deserialize(deserializer).map(Some)
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
    pub fn budget(&self) -> u32 {
        self.max_weight.unwrap_or(self.vcpu_ceiling)
    }
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
            || self.budget() < self.vcpu_ceiling
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
    /// Start-only policy checks must not prevent stopping or inspecting a VM.
    fn check_start(&mut self, _tenant: &str) -> std::result::Result<(), StartRefusal> {
        Ok(())
    }
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
    /// A failed create request may still be writing. Only a completed stop may
    /// clear its quarantine; child liveness alone cannot authorize adoption.
    fn snapshot_uncertain(&self, _tenant: &str) -> bool {
        false
    }
    fn was_restored(&self, _tenant: &str) -> bool {
        false
    }
    fn load(&mut self, tenant: &str, r: &Registry, saved: bool) -> StartResult<()>;
    fn drive(&mut self, tenant: &str, r: &Registry) -> Result<()>;
    fn consume(&mut self, r: &Registry) -> Result<()>;
    fn advance(&mut self, r: &Registry) -> Result<u64>;
    fn resume(&mut self, tenant: &str) -> Result<()>;
    fn staged_resize(&mut self, _r: &Registry) -> Result<Option<ResizeActivation>> {
        Ok(None)
    }
    fn apply_resize(&mut self, _tenant: &str, _activation: &ResizeActivation) -> Result<()> {
        Err(Error("resize activation unavailable".into()))
    }
    fn sync_disk(&mut self, _tenant: &str) -> Result<()> {
        Err(Error("tenant disk sync unavailable".into()))
    }
    fn post_grow_check(&mut self, _r: &Registry) -> Result<()> {
        Err(Error("post-grow space check unavailable".into()))
    }
    fn complete_resize(&mut self, _r: &Registry, _activation: &ResizeActivation) -> Result<()> {
        Err(Error("resize completion unavailable".into()))
    }
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
    /// Whether anything of a VM is left for this tenant even when this helper
    /// process did not start it: its cgroup or its jail.
    fn vm_present(&mut self, _tenant: &str) -> Result<bool> {
        Ok(false)
    }
    /// Quarantines the stopped tenant's data and removes its records and
    /// network setup; returns the quarantine folder.
    fn remove(&mut self, _tenant: &str, _registry: &Registry) -> Result<PathBuf> {
        Err(Error("tenant removal unavailable".into()))
    }
    /// Completes a removal whose registry entry is already gone but whose
    /// data was not yet moved; `None` when there is none.
    fn resume_removal(&mut self, _tenant: &str) -> Result<Option<PathBuf>> {
        Ok(None)
    }
}

pub struct Controller<B: Backend>(pub B);

// Backend errors use this prefix only when snapshot/create may still be writing.
pub(crate) const SNAPSHOT_CREATE_UNCERTAIN: &str = "suspend_create_uncertain";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartRefusalCase {
    CapacityJson,
    Fit,
    LiveCap,
    DropIn,
    ResizeJournal,
    Oom,
    Activation,
    PostGrowSpace,
}
#[derive(Debug)]
pub struct StartRefusal {
    pub case: StartRefusalCase,
    pub message: String,
}
pub(crate) fn start_refused(case: StartRefusalCase, error: impl std::fmt::Display) -> StartRefusal {
    StartRefusal {
        case,
        message: error.to_string(),
    }
}
#[derive(Debug)]
pub enum StartError {
    Refused(StartRefusal),
    Other(Error),
}
pub type StartResult<T> = std::result::Result<T, StartError>;
impl From<Error> for StartError {
    fn from(error: Error) -> Self {
        Self::Other(error)
    }
}
impl From<std::io::Error> for StartError {
    fn from(error: std::io::Error) -> Self {
        Self::Other(error.into())
    }
}
impl From<StartRefusal> for StartError {
    fn from(refusal: StartRefusal) -> Self {
        Self::Refused(refusal)
    }
}
fn refused_response(refusal: StartRefusal) -> Response {
    Response::new(Reply::StartRefused {
        case: refusal.case,
        message: refusal.message,
    })
}
impl<B: Backend> Controller<B> {
    pub fn handle(&mut self, tenant: &str, op: Operation) -> Result<Response> {
        validate_name(tenant)?;
        if let Operation::Relay(port) = op {
            validate_port(port)?;
        }
        if let Operation::Remove = op {
            return self.remove(tenant);
        }
        if matches!(
            op,
            Operation::Ensure | Operation::Restore | Operation::PrepareStart
        ) {
            if let Err(refusal) = self.0.check_start(tenant) {
                return Ok(refused_response(refusal));
            }
        }
        let r = self.0.registry(tenant)?;
        r.validate(tenant)?;
        // Stop before recording recovery or consuming files: recording can
        // itself stall on the disk that made snapshot/create time out.
        if self.0.snapshot_uncertain(tenant) {
            self.0.stop(tenant)?;
        }
        self.record_lost_vm(tenant, &r)?;
        let mut restored = self.0.was_restored(tenant);
        // Legacy callers cannot complete the attach/activate handshake. Hide a
        // pending VM from adopt, and refuse legacy starts instead of returning
        // a success that an older host would advertise as ready.
        if matches!(
            op,
            Operation::Ensure | Operation::Restore | Operation::Adopt
        ) {
            let error = match self.0.staged_resize(&r) {
                Ok(None) => None,
                Ok(Some(_)) => Some(Error(
                    "staged resize requires prepare_start, attach_tenant, then activate".into(),
                )),
                Err(error) => Some(error),
            };
            if let Some(error) = error {
                return Ok(refused_response(start_refused(
                    StartRefusalCase::Activation,
                    error,
                )));
            }
        }
        match op {
            Operation::Ensure | Operation::Restore | Operation::PrepareStart => {
                if self.0.running(tenant) {
                    // A failed activation stop may have left a live VM. Never
                    // adopt it while durable provisioning intent is pending.
                    match self.0.staged_resize(&r) {
                        Ok(Some(_)) => {
                            if let Err(error) = self.stop_cleanly(tenant, &r) {
                                return Ok(refused_response(start_refused(
                                    StartRefusalCase::Activation,
                                    error,
                                )));
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            return Ok(refused_response(start_refused(
                                StartRefusalCase::Activation,
                                error,
                            )))
                        }
                    }
                }
                if self.0.unclean(&r)? && self.0.running(tenant) {
                    self.0.stop(tenant)?;
                }
                if !self.0.running(tenant) {
                    restored = match self.wake(tenant, &r) {
                        Ok(restored) => restored,
                        Err(StartError::Refused(refusal)) => return Ok(refused_response(refusal)),
                        Err(StartError::Other(error)) => return Err(error),
                    };
                }
            }
            Operation::Activate => {
                let result = match self.0.staged_resize(&r) {
                    Ok(Some(activation)) if self.0.running(tenant) => {
                        self.activate(tenant, &r, &activation)
                    }
                    Ok(None) => Ok(()),
                    pending => {
                        let mut refusal = start_refused(
                            StartRefusalCase::Activation,
                            pending.err().unwrap_or_else(|| {
                                Error("resize requires a running tenant".into())
                            }),
                        );
                        if let Err(stop) = self.stop_cleanly(tenant, &r) {
                            refusal
                                .message
                                .push_str(&format!("; clean stop failed: {stop}"));
                        }
                        Err(StartError::Refused(refusal))
                    }
                };
                match result {
                    Ok(()) => {}
                    Err(StartError::Refused(refusal)) => return Ok(refused_response(refusal)),
                    Err(StartError::Other(error)) => return Err(error),
                }
            }
            Operation::AttachDisk => self.0.drive(tenant, &r)?,
            Operation::Cpu(vcpus) => self.0.cpu(tenant, &r.cpu_max(vcpus)?)?,
            Operation::Egress => self.0.egress(tenant, &r)?,
            Operation::Adopt => {}
            Operation::Remove => unreachable!("removal returns before the registry is read"),
            Operation::RecoveryComplete => self.0.recovery_complete(tenant, &r)?,
            Operation::Metrics => {
                let (memory_bytes, egress_bytes) = self.0.metrics(tenant, &r)?;
                return Ok(Response::new(Reply::TenantMetrics {
                    memory_bytes,
                    egress_bytes,
                }));
            }
            Operation::Relay(port) => {
                if port != 54 && self.0.staged_resize(&r)?.is_some() {
                    return Err(Error(
                        "resize activation pending; job relay is fenced".into(),
                    ));
                }
                return Ok(Response {
                    reply: Reply::Relay { fd_count: 1 },
                    fds: vec![self.0.relay(tenant, port).map_err(|error| {
                        crate::Error(format!(
                            "open_relay tenant={tenant} port={port} step=open_guest_connection: {error}"
                        ))
                    })?],
                });
            }
            Operation::Stop => {
                self.stop_cleanly(tenant, &r)?;
            }
            Operation::Suspend => {
                if self.0.staged_resize(&r)?.is_some() {
                    return Err(Error(
                        "resize activation pending; snapshot is fenced".into(),
                    ));
                }
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
                    return Err(Error(format!("suspend_prepare_refused: {error}")));
                }
                if let Err(error) = self.0.snapshot(tenant, &r, &key) {
                    if error.0.starts_with(SNAPSHOT_CREATE_UNCERTAIN) {
                        // Closing the API socket does not cancel Firecracker's
                        // writer. Kill and reap it before deleting any output.
                        // Prepare already drained jobs and froze the filesystem,
                        // but the next boot must still perform unclean recovery.
                        self.0.stop(tenant).map_err(|stop| {
                            Error(format!("suspend_create_stop_failed: {error}; stop: {stop}"))
                        })?;
                        // The running marker already forces recovery if this
                        // reason cannot be recorded after the writer is stopped.
                        let record = self.0.mark_unclean(&r, &error.to_string());
                        let cleanup = self.0.consume(&r);
                        eprintln!("tenant {tenant}: suspend_create_failed_vm_stopped action=cold_boot_recovery cause={error}");
                        record?;
                        cleanup?;
                        return Err(Error(format!("suspend_create_failed_vm_stopped: {error}")));
                    }
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
    /// The caller names a tenant only. Its registry entry decides what goes,
    /// so an unknown tenant is refused, and the VM must already be stopped:
    /// the helper checks that itself rather than trusting the caller. Every
    /// tenant operation runs under the helper's one lock, so no suspend or
    /// snapshot can be in progress while this runs.
    fn remove(&mut self, tenant: &str) -> Result<Response> {
        let r = match self.0.registry(tenant) {
            Ok(r) => r,
            Err(error) => {
                return match self.0.resume_removal(tenant)? {
                    Some(quarantine) => Ok(removed(quarantine)),
                    None => Err(error),
                }
            }
        };
        r.validate(tenant)?;
        if self.0.running(tenant) || self.0.vm_present(tenant)? {
            return Err(Error(
                "tenant VM is running; stop it before removing the tenant".into(),
            ));
        }
        let quarantine = self.0.remove(tenant, &r)?;
        Ok(removed(quarantine))
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
    fn stop_cleanly(&mut self, tenant: &str, r: &Registry) -> Result<()> {
        if self.0.running(tenant) {
            let prepared = self.0.prepare_stop(tenant);
            let record = match &prepared {
                Ok(()) => Ok(()),
                Err(error) => self.0.mark_unclean(r, &error.to_string()),
            };
            let prepared = prepared.is_ok();
            if let Err(error) = self.0.stop(tenant) {
                let _ = self.0.thaw(tenant);
                return Err(error);
            }
            record?;
            // Only a stop whose guest flush succeeded is clean. After a failed
            // flush the running marker stays as a second durable recovery record.
            if prepared {
                self.0.clear_running(r)?;
            }
        }
        Ok(())
    }
    fn activate(
        &mut self,
        tenant: &str,
        r: &Registry,
        activation: &ResizeActivation,
    ) -> StartResult<()> {
        let result = (|| {
            self.0
                .apply_resize(tenant, activation)
                .map_err(|e| start_refused(StartRefusalCase::Activation, e))?;
            self.0
                .sync_disk(tenant)
                .map_err(|e| start_refused(StartRefusalCase::Activation, e))?;
            if activation.quota_grew {
                self.0
                    .post_grow_check(r)
                    .map_err(|e| start_refused(StartRefusalCase::PostGrowSpace, e))?;
            }
            self.0
                .complete_resize(r, activation)
                .map_err(|e| start_refused(StartRefusalCase::Activation, e))
        })();
        if let Err(mut refusal) = result {
            if let Err(stop) = self.stop_cleanly(tenant, r) {
                refusal
                    .message
                    .push_str(&format!("; clean stop failed: {stop}"));
            }
            return Err(refusal.into());
        }
        Ok(())
    }
    fn wake(&mut self, tenant: &str, r: &Registry) -> StartResult<bool> {
        let activation = self
            .0
            .staged_resize(r)
            .map_err(|e| start_refused(StartRefusalCase::Activation, e))?;
        let key = self.0.key(r)?;
        let saved = activation.is_none()
            && self
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
                .and_then(|()| self.0.drive(tenant, r).map_err(Into::into));
            let result = match result {
                Err(StartError::Refused(refusal)) => {
                    // Policy failures happened before snapshot/load, so the
                    // saved state is still usable after the operator fixes them.
                    self.0.stop(tenant)?;
                    self.0.clear_running(r)?;
                    return Err(StartError::Refused(refusal));
                }
                result => result,
            };
            if result.is_ok() {
                let durable = (|| {
                    self.0.consume(r)?;
                    self.0.advance(r)
                })();
                if let Err(error) = durable {
                    // A failed durability barrier must not leave an apparently
                    // running, paused guest available for adoption by a reconnect.
                    self.0.stop(tenant)?;
                    return Err(error.into());
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
        let result = (|| -> StartResult<()> {
            self.0.load(tenant, r, false)?;
            self.0.drive(tenant, r)?;
            self.0.consume(r)?;
            self.0.advance(r)?;
            self.0.resume(tenant).map_err(Into::into)
        })();
        if result.is_err() {
            self.0.stop(tenant)?;
            if matches!(result, Err(StartError::Refused(_))) {
                self.0.clear_running(r)?;
            }
        }
        result?;
        Ok(false)
    }
}

fn removed(quarantine: PathBuf) -> Response {
    Response::new(Reply::Removed {
        quarantine: quarantine.display().to_string(),
    })
}

#[cfg(test)]
pub(crate) mod tests;
