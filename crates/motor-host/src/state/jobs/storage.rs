//! Host-space promises. Filesystem free space already charges all physical
//! blocks, including shared checkout blocks and legacy tenant disks. Only exclusive
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

#[derive(Serialize)]
pub(crate) struct OperatorReservation {
    owner: String,
    bytes: u64,
    expires_at_ms: u64,
}

pub(crate) enum StorageCheck {
    Available {
        free_bytes: u64,
        threshold_bytes: u64,
        reserved_bytes: u64,
    },
    Short(u64),
}

impl Host {
    pub(crate) fn job_relayed_run(&mut self, job: &str, has_returns: bool) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get_mut(job)
            .ok_or(Error::Refused(crate::state::Refusal::Auth))?;
        if execution.relayed_return_paths.is_none() {
            execution.relayed_return_paths = Some(has_returns);
            self.save(next)?;
        }
        Ok(())
    }

    pub(crate) fn job_run_accepted(&mut self, job: &str) -> Result<u64, Error> {
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get_mut(job)
            .ok_or(Error::Refused(crate::state::Refusal::Auth))?;
        let returns = if execution.relayed_return_paths == Some(false) {
            0
        } else {
            RETURN_BYTES
        };
        if !execution.ended {
            execution.replay_reservation_bytes =
                Some(REPLAY_RESERVATION_BYTES - RETURN_BYTES + returns);
        }
        self.save(next)?;
        Ok(returns)
    }
    pub fn set_job_storage(&mut self, control: Arc<dyn crate::helper::Control>) {
        self.job_storage = Some(control);
    }

    fn expire_operator_storage(&mut self, now_ms: u64) {
        self.operator_storage_reservations
            .retain(|_, reservation| reservation.expires_at_ms > now_ms);
    }

    pub(crate) fn operator_storage_reservations(
        &mut self,
        now_ms: u64,
    ) -> Vec<&OperatorReservation> {
        self.expire_operator_storage(now_ms);
        self.operator_storage_reservations.values().collect()
    }

    pub(crate) fn release_operator_storage(&mut self, owner: &str, now_ms: u64) {
        self.expire_operator_storage(now_ms);
        self.operator_storage_reservations.remove(owner);
    }

    pub(crate) fn reserve_operator_storage(
        &mut self,
        owner: &str,
        bytes: u64,
        ttl_s: u64,
        now_ms: impl Fn() -> u64,
    ) -> Result<StorageCheck, Error> {
        if !self.storage_enforced() {
            return Err(Error::Refused(crate::state::Refusal::HostFull));
        }
        let mut next = self.snapshot.clone();
        let check = self
            .check_storage(&mut next, "", now_ms(), Some((owner, bytes)))
            .map_err(|e| e.error)?;
        if matches!(check, StorageCheck::Available { .. }) {
            // Start the TTL after eviction: a slow helper must not consume
            // the time the operator asked to reserve for the build.
            self.operator_storage_reservations.insert(
                owner.into(),
                OperatorReservation {
                    owner: owner.into(),
                    bytes,
                    expires_at_ms: now_ms().saturating_add(ttl_s * 1000),
                },
            );
        }
        Ok(check)
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
        // A disk made under a lower cap grows to the configured one when a job
        // claims it; the helper extends the file before boot. Caps never
        // shrink: the helper refuses a disk larger than the size it is asked
        // for, and the guest filesystem cannot shrink.
        storage.cap_bytes = storage.cap_bytes.max(default_cap);
        storage.growth_reservation_bytes = storage
            .cap_bytes
            .min(settings.workspace_growth_reservation_bytes);
        self.reserve_storage(next, disk_id, now_ms)
    }

    pub(crate) fn reserve_storage(
        &mut self,
        next: &mut Snapshot,
        protected: &str,
        now_ms: u64,
    ) -> Result<(), ClaimError> {
        // Sandboxes without the registered-host storage gate still persist the
        // same promises. Registered hosts never admit without trusted probes.
        if !self.storage_enforced() {
            return Ok(());
        }
        match self.check_storage(next, protected, now_ms, None)? {
            StorageCheck::Available { .. } => Ok(()),
            StorageCheck::Short(_) => Err(RefusalCause::Storage.into()),
        }
    }

    fn check_storage(
        &mut self,
        next: &mut Snapshot,
        protected: &str,
        now_ms: u64,
        replacement: Option<(&str, u64)>,
    ) -> Result<StorageCheck, ClaimError> {
        let HostSettings::PerJob(settings) = &self.settings else {
            return Err(RefusalCause::Storage.into());
        };
        let default_cap = settings.workspace_disk_cap_bytes;
        let growth_bound = settings.workspace_growth_reservation_bytes;
        self.expire_operator_storage(now_ms);
        // Count a replacement in place of the owner's old promise without
        // changing what is held if the new request cannot be admitted.
        let operator_bytes = self
            .operator_storage_reservations
            .iter()
            .filter(|(owner, _)| replacement.is_none_or(|(key, _)| *owner != key))
            .map(|(_, reservation)| u128::from(reservation.bytes))
            .sum::<u128>()
            + u128::from(replacement.map_or(0, |(_, bytes)| bytes));
        let control = self.job_storage.clone().ok_or(RefusalCause::Storage)?;
        let mut attempted = std::collections::BTreeSet::new();
        loop {
            let mut reserved =
                super::images::reserved_bytes(&next.per_job).ok_or(RefusalCause::Storage)?;
            for (id, disk) in next.per_job.disks.iter_mut() {
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
                storage.growth_reservation_bytes = (cap - exclusive).min(growth_bound);
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
            let reserved = u128::from(reserved) + operator_bytes;
            let required = u128::from(threshold) + reserved;
            if u128::from(free) >= required {
                return Ok(StorageCheck::Available {
                    free_bytes: free,
                    threshold_bytes: threshold,
                    reserved_bytes: reserved as u64,
                });
            }
            if !self.evict_job_storage(next, protected, &*control, &mut attempted)? {
                // A request can exceed the size representable by the reply
                // once the threshold and existing promises are added to it.
                let short = required - u128::from(free);
                return Ok(StorageCheck::Short(
                    u64::try_from(short).unwrap_or(u64::MAX),
                ));
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
        let Some((id, disk)) = idle else {
            return Ok(false);
        };
        let (id, account) = (id.clone(), disk.sub.clone());
        attempted.insert(id.clone());
        let before = control
            .available_bytes()
            .map_err(|_| RefusalCause::Storage)?;
        let mut intent = self.snapshot.clone();
        let disk = intent.per_job.disks.get_mut(&id).unwrap();
        disk.deleted = true;
        disk.have.clear();
        let storage = disk.storage.get_or_insert_with(|| Disk::new(0, 0));
        storage.evicting = true;
        storage.growth_reservation_bytes = 0;
        // Fence selection and have replies before the helper deletes anything.
        // A crash leaves an idempotent deletion intent, never a reusable disk.
        self.save(intent)?;
        next.per_job
            .disks
            .insert(id.clone(), self.snapshot.per_job.disks[&id].clone());
        if let Err(error) = control.delete_workspace(&account, &id) {
            // The helper may still hold a VM the host has not re-adopted.
            // Try other safe victims, but keep this disk marked deleted and
            // evicting so deletion can be retried.
            tracing::warn!(disk_id = id, %error, "workspace_disk_eviction_deferred");
            return Ok(true);
        }
        let mut done = self.snapshot.clone();
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
        self.save(done)?;
        let after = control
            .available_bytes()
            .map_err(|_| RefusalCause::Storage)?;
        tracing::info!(
            disk_id = id,
            bytes_freed = after.saturating_sub(before),
            "workspace_disk_evicted"
        );
        Ok(true)
    }
}
