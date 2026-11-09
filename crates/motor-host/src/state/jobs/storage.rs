//! Host-space promises. Filesystem free space already charges all physical
//! blocks, including shared base blocks and legacy tenant disks. Only exclusive
//! workspace extents reduce the promise: overwriting a shared extent needs a
//! new block. The filesystem, not an individual clone, owns that shared charge.
use super::*;
use crate::state::Snapshot;
use std::sync::Arc;

pub const REPLAY_BYTES: u64 = 64 * 1024 * 1024;
pub const RETURN_BYTES: u64 = 128 * 1024 * 1024;
/// Space for both copies of the atomic index, terminal frames and block rounding.
/// The replay store must bound output and index growth within these allowances;
/// admission accounting does not itself enforce the frame store's write limits.
pub const REPLAY_RECORD_BYTES: u64 = 8 * 1024 * 1024;
pub const REPLAY_RESERVATION_BYTES: u64 =
    REPLAY_BYTES + crate::replay::SEGMENT_BYTES + REPLAY_RECORD_BYTES + RETURN_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayCompletion {
    Terminal,
    Quarantined,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Disk {
    pub cap_bytes: u64,
    pub last_used_ms: u64,
    pub growth_reservation_bytes: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub evicting: bool,
}
impl Disk {
    pub fn new(cap_bytes: u64, last_used_ms: u64) -> Self {
        Self {
            cap_bytes,
            last_used_ms,
            growth_reservation_bytes: cap_bytes,
            evicting: false,
        }
    }
}

/// Registered only by the host's clean, detached base publication path.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Base {
    pub sub: String,
    pub last_used_ms: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub evicting: bool,
}

impl Host {
    pub fn set_job_storage(&mut self, control: Arc<dyn crate::helper::Control>) {
        self.job_storage = Some(control);
    }

    pub(crate) fn quarantine_workspace(&mut self, job: &str) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        super::recovery::quarantine(&mut next, job);
        self.save(next)
    }

    pub(crate) fn job_replay_closed(
        &mut self,
        job: &str,
        completion: ReplayCompletion,
    ) -> Result<(), Error> {
        let Some(execution) = self.snapshot.per_job.executions.get(job) else {
            return Ok(());
        };
        if execution.replay_completion == Some(completion)
            && execution.final_replay_reservation_bytes.is_none()
        {
            return Ok(());
        }
        let mut next = self.snapshot.clone();
        let execution = next.per_job.executions.get_mut(job).unwrap();
        execution.replay_completion = Some(completion);
        execution.final_replay_reservation_bytes = None;
        self.save(next)
    }

    pub(super) fn reserve_job_storage(
        &mut self,
        next: &mut Snapshot,
        disk_id: &str,
        now_ms: u64,
    ) -> Result<(), ClaimError> {
        let HostSettings::PerJob(settings) = &self.settings else {
            unreachable!()
        };
        let default_cap = settings.workspace_disk_cap_bytes;
        let disk = next.per_job.disks.get_mut(disk_id).unwrap();
        let storage = disk
            .storage
            .get_or_insert_with(|| Disk::new(default_cap, now_ms));
        storage.last_used_ms = storage.last_used_ms.max(now_ms);
        storage.growth_reservation_bytes = storage.cap_bytes;
        // Sandboxes without the registered-host storage gate still persist the
        // same promises. Registered hosts never admit without trusted probes.
        if !self.storage_enforced() {
            return Ok(());
        }
        let control = self.job_storage.clone().ok_or(RefusalCause::Storage)?;
        let mut attempted = std::collections::BTreeSet::new();
        loop {
            let mut reserved = 0u64;
            for (id, disk) in &mut next.per_job.disks {
                let active = !disk.deleted
                    && (disk.attached_job.is_some()
                        || !disk.queue.is_empty()
                        || disk.recovery.is_some());
                if !active {
                    continue;
                }
                let cap = disk.storage.as_ref().map_or(default_cap, |s| s.cap_bytes);
                let exclusive = control
                    .workspace_usage(&disk.sub, id)
                    .ok()
                    .flatten()
                    .unwrap_or(0)
                    .min(cap);
                let storage = disk.storage.get_or_insert_with(|| Disk::new(cap, now_ms));
                storage.growth_reservation_bytes = cap - exclusive;
                reserved = reserved
                    .checked_add(storage.growth_reservation_bytes)
                    .ok_or(RefusalCause::Storage)?;
            }
            for (job, execution) in &next.per_job.executions {
                if execution.ended {
                    reserved = reserved
                        .checked_add(execution.final_replay_reservation_bytes.unwrap_or(0))
                        .ok_or(RefusalCause::Storage)?;
                    continue;
                }
                let bound = execution
                    .replay_reservation_bytes
                    .unwrap_or(REPLAY_RESERVATION_BYTES);
                let allocated = self
                    .journal_directory()
                    .and_then(|root| crate::replay::allocated_bytes(&root, job).ok())
                    .unwrap_or(0);
                reserved = reserved
                    .checked_add(bound.saturating_sub(allocated))
                    .ok_or(RefusalCause::Storage)?;
                // Queued input is host-owned too and may coexist with replay.
                reserved = reserved
                    .checked_add(crate::jobs::SPOOL_LIMIT)
                    .ok_or(RefusalCause::Storage)?;
            }
            // Measure allocations before free space. Growth between probes then
            // only over-reserves; reversing the order could credit it twice.
            let capacity = control.capacity().map_err(|_| RefusalCause::Storage)?;
            let threshold = self
                .storage_threshold(&capacity)
                .ok_or(RefusalCause::Storage)?;
            let free = control
                .available_bytes()
                .map_err(|_| RefusalCause::Storage)?;
            if free
                .checked_sub(threshold)
                .is_some_and(|available| available >= reserved)
            {
                return Ok(());
            }
            if !self.evict_job_storage(next, disk_id, &*control, &mut attempted)? {
                return Err(RefusalCause::Storage.into());
            }
        }
    }

    fn evict_job_storage(
        &mut self,
        next: &mut Snapshot,
        protected: &str,
        control: &dyn crate::helper::Control,
        attempted: &mut std::collections::BTreeSet<String>,
    ) -> Result<bool, ClaimError> {
        let idle = self
            .snapshot
            .per_job
            .disks
            .iter()
            .filter(|(id, disk)| {
                id.as_str() != protected
                    && !attempted.contains(id.as_str())
                    && (!disk.deleted || disk.storage.as_ref().is_some_and(|s| s.evicting))
                    && disk.attached_job.is_none()
                    && disk.queue.is_empty()
                    && !disk.quarantined
                    && disk.recovery.is_none()
                    && !self
                        .snapshot
                        .per_job
                        .executions
                        .values()
                        .any(|e| !e.ended && e.disk_id == **id)
            })
            .min_by_key(|(id, disk)| (disk.storage.as_ref().map_or(0, |s| s.last_used_ms), *id));
        let victim = idle
            .map(|(id, disk)| (id.clone(), disk.sub.clone(), false))
            .or_else(|| {
                self.snapshot
                    .per_job
                    .bases
                    .iter()
                    .filter(|(id, _)| !attempted.contains(id.as_str()))
                    .min_by_key(|(id, base)| (base.last_used_ms, *id))
                    .map(|(id, base)| (id.clone(), base.sub.clone(), true))
            });
        let Some((id, account, base)) = victim else {
            return Ok(false);
        };
        attempted.insert(id.clone());
        let before = control
            .available_bytes()
            .map_err(|_| RefusalCause::Storage)?;
        let mut intent = self.snapshot.clone();
        if base {
            intent.per_job.bases.get_mut(&id).unwrap().evicting = true;
        } else {
            let disk = intent.per_job.disks.get_mut(&id).unwrap();
            disk.deleted = true;
            disk.have.clear();
            let storage = disk.storage.get_or_insert_with(|| Disk::new(0, 0));
            storage.evicting = true;
            storage.growth_reservation_bytes = 0;
        }
        // Fence selection and have replies before the helper deletes anything.
        // A crash leaves an idempotent deletion intent, never a reusable disk.
        self.save(intent)?;
        if base {
            next.per_job
                .bases
                .insert(id.clone(), self.snapshot.per_job.bases[&id].clone());
        } else {
            next.per_job
                .disks
                .insert(id.clone(), self.snapshot.per_job.disks[&id].clone());
        }
        let result = if base {
            control.evict_base(&account, &id)
        } else {
            control.delete_workspace(&account, &id)
        };
        if let Err(error) = result {
            // The helper may still hold a seed lease or an unreleased VM that
            // the host has not re-adopted yet. Try other safe victims, but keep
            // this deletion fenced for an idempotent retry.
            tracing::warn!(disk_id = id, %error, "workspace_disk_eviction_deferred");
            return Ok(true);
        }
        let mut done = self.snapshot.clone();
        if base {
            done.per_job.bases.remove(&id);
            next.per_job.bases.remove(&id);
        } else {
            done.per_job
                .disks
                .get_mut(&id)
                .unwrap()
                .storage
                .as_mut()
                .unwrap()
                .evicting = false;
            next.per_job
                .disks
                .insert(id.clone(), done.per_job.disks[&id].clone());
        }
        self.save(done)?;
        let after = control
            .available_bytes()
            .map_err(|_| RefusalCause::Storage)?;
        tracing::info!(
            disk_id = id,
            role = if base { "base" } else { "job" },
            bytes_freed = after.saturating_sub(before),
            "workspace_disk_evicted"
        );
        Ok(true)
    }
}
