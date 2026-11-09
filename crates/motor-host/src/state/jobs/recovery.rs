//! A disk stays quarantined after its job ends and across host restarts.
use super::*;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Recovery {
    pub id: String,
    pub memory_debit_mib: u64,
    pub guest_ram_mib: u64,
    pub started_at_ms: Option<u64>,
    pub phase: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Reserved,
    Running,
    Stopping,
    Deleting,
}

pub(crate) fn quarantine(snapshot: &mut super::super::Snapshot, job: &str) {
    let e = &snapshot.per_job.executions[job];
    if e.boot_started {
        let disk = snapshot.per_job.disks.get_mut(&e.disk_id).unwrap();
        if disk.deleted {
            return;
        }
        disk.quarantined = true;
        disk.have.clear();
    }
}

impl Host {
    pub(crate) fn recovery_permits(&self) -> u32 {
        self.snapshot
            .per_job
            .disks
            .values()
            .filter(|d| d.recovery.is_some())
            .count() as u32
    }

    pub(crate) fn job_free_vcpus(&self, now_ms: u64) -> u64 {
        let Some(sample) = self.fresh_host_resources(now_ms) else {
            return 0;
        };
        let used = self
            .snapshot
            .per_job
            .executions
            .values()
            .filter(|e| !e.ended)
            .fold(self.recovery_permits(), |n, e| {
                n.saturating_add(e.claims.vcpus)
            });
        u64::from(sample.permit_budget.saturating_sub(used))
    }

    /// Reserve recovery ahead of later claims, but never steal admitted permits.
    pub(crate) fn reserve_recovery(&mut self, disk_id: &str, now_ms: u64) -> Result<(), Error> {
        let disk = &self.snapshot.per_job.disks[disk_id];
        if !disk.quarantined || disk.recovery.is_some() || disk.attached_job.is_some() {
            return Ok(());
        }
        let Some(owner) = self
            .snapshot
            .per_job
            .executions
            .values()
            .filter(|e| e.disk_id == disk_id && e.boot_started)
            .max_by_key(|e| e.boot_started_at_ms)
        else {
            return Ok(());
        };
        if self.job_free_vcpus(now_ms) == 0
            || self.job_free_mem_mib(now_ms) < owner.memory_debit_mib
        {
            return Ok(());
        }
        let recovery = Recovery {
            id: recovery_id(now_ms),
            memory_debit_mib: owner.memory_debit_mib,
            guest_ram_mib: owner.guest_ram_mib,
            started_at_ms: None,
            phase: Phase::Reserved,
        };
        let mut next = self.snapshot.clone();
        let disk = next.per_job.disks.get_mut(disk_id).unwrap();
        disk.have.clear();
        disk.recovery = Some(recovery);
        self.save(next)
    }

    pub(crate) fn recovery_phase(
        &mut self,
        disk_id: &str,
        id: &str,
        phase: Phase,
        at_ms: u64,
        result: Option<&str>,
    ) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let disk = next
            .per_job
            .disks
            .get_mut(disk_id)
            .ok_or(Error::Refused(Refusal::Auth))?;
        let r = disk
            .recovery
            .as_mut()
            .ok_or(Error::Refused(Refusal::Auth))?;
        if r.id != id {
            return Err(Error::Refused(Refusal::Auth));
        }
        // A late guest reply may not reverse a timeout or deletion decision.
        if r.phase == Phase::Deleting && phase != Phase::Deleting {
            return Ok(());
        }
        r.phase = phase;
        if phase == Phase::Running {
            r.started_at_ms.get_or_insert(at_ms);
        }
        disk.have.clear();
        if let Some(result) = result {
            disk.recovery_result = Some(result.into());
        }
        self.save(next)
    }

    pub(crate) fn recovery_finished(&mut self, disk_id: &str, deleted: bool) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let disk = next.per_job.disks.get_mut(disk_id).unwrap();
        disk.have.clear();
        disk.recovery = None;
        disk.quarantined = false;
        disk.deleted = deleted;
        if deleted && !disk.queue.is_empty() {
            let mut fresh = disk.clone();
            fresh.deleted = false;
            fresh.recovery_result = None;
            let queued = std::mem::take(&mut disk.queue);
            let fresh_id = format!("ws-{:032x}", rand::random::<u128>());
            for job in queued {
                next.per_job.executions.get_mut(&job).unwrap().disk_id = fresh_id.clone();
            }
            next.per_job.disks.insert(fresh_id, fresh);
        }
        self.save(next)
    }
}

fn recovery_id(now_ms: u64) -> String {
    let mut bytes = rand::random::<[u8; 16]>();
    bytes[..6].copy_from_slice(&now_ms.to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}
