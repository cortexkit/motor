//! Fill reservations are maintenance work, never leases or billable executions.
use super::*;

/// A quiet claim interval, not a promise to preempt an already running fill.
pub const CAPACITY_QUIET_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Reservation {
    pub fill: motor_vm_helper::job::Fill,
    pub vcpus: u32,
    pub memory_debit_mib: u64,
    pub guest_ram_mib: u64,
    pub runner_hello: bool,
    pub image_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<Vec<motor_protocol::MirrorRef>>,
}

#[derive(Clone, Debug)]
pub struct SeedPlan {
    pub request: motor_vm_helper::job::Seed,
    have: Vec<motor_protocol::MirrorRef>,
}

impl Host {
    pub(crate) fn fill_permits(&self) -> u32 {
        self.snapshot
            .per_job
            .fills
            .values()
            .fold(0u32, |n, f| n.saturating_add(f.vcpus))
    }

    pub fn reserve_fill(
        &mut self,
        account: &str,
        repository_key: &str,
        mem_mib: u64,
        now_ms: u64,
    ) -> Result<Option<Reservation>, Error> {
        let HostSettings::PerJob(settings) = &self.settings else {
            return Ok(None);
        };
        if self.snapshot.per_job.dax_failed
            || self.snapshot.draining
            || self.storage_refusing()
            || !self.snapshot.per_job.fills.is_empty()
            || self
                .fill_capacity_refused_at_ms
                .is_some_and(|at| now_ms < at.saturating_add(CAPACITY_QUIET_MS))
            || self.snapshot.per_job.disks.values().any(|d| {
                d.queue
                    .iter()
                    .any(|job| d.attached_job.as_ref() != Some(job))
            })
        {
            return Ok(None);
        }
        let Some(sample) = self.fresh_host_resources(now_ms) else {
            return Ok(None);
        };
        let vcpus = 4.min(sample.vcpu_ceiling).min(sample.permit_budget);
        let Some(debit) = mem_mib.checked_add(settings.boot_reserve_mib) else {
            return Ok(None);
        };
        if vcpus == 0
            || mem_mib == 0
            || self.job_free_vcpus(now_ms) < u64::from(vcpus)
            || self.job_free_mem_mib(now_ms) < debit
        {
            return Ok(None);
        }
        let mut bytes = rand::random::<[u8; 16]>();
        bytes[..6].copy_from_slice(&now_ms.to_be_bytes()[2..]);
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let fill = motor_vm_helper::job::Fill {
            account: account.into(),
            repository_key: repository_key.into(),
            fill_id: uuid::Uuid::from_bytes(bytes).to_string(),
            size_bytes: settings.workspace_disk_cap_bytes,
        };
        fill.validate().map_err(|_| Error::Refused(Refusal::Auth))?;
        let reservation = Reservation {
            fill,
            vcpus,
            memory_debit_mib: debit,
            guest_ram_mib: u64::try_from((u128::from(mem_mib) * 4).div_ceil(3))
                .map_err(|_| Error::Refused(Refusal::HostFull))?,
            runner_hello: false,
            image_generation: images::next_generation(&self.snapshot.per_job)
                .map_err(|e| e.error)?,
            publication: None,
        };
        let mut next = self.snapshot.clone();
        let id = &reservation.fill.fill_id;
        if let Some(generation) = &reservation.image_generation {
            let generation = next.per_job.images.get_mut(generation).unwrap();
            generation
                .recovery_holders
                .insert(id.clone(), account.into());
            generation.accounts.entry(account.into()).or_insert(false);
        }
        next.per_job.fills.insert(id.clone(), reservation.clone());
        for slot in next.per_job.fill_sources.values_mut().filter(|slot| {
            slot.request.account == account
                && crate::jobs::fill_source::repository_key(&slot.request.repository_root)
                    == repository_key
        }) {
            slot.last_fill_started_ms = Some(now_ms);
        }
        if let Err(error) = self.reserve_storage(&mut next, "", now_ms) {
            if error.error.refusal() == Refusal::HostFull {
                return Ok(None);
            }
            return Err(error.error);
        }
        self.save(next)?;
        Ok(Some(reservation))
    }

    pub fn fill_runner_hello(&mut self, id: &str) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        next.per_job
            .fills
            .get_mut(id)
            .ok_or(Error::Refused(Refusal::Auth))?
            .runner_hello = true;
        self.save(next)
    }

    /// Only the guest seam's verified-import/publication result supplies these pins.
    pub fn fill_publication(
        &mut self,
        id: &str,
        have: Vec<motor_protocol::MirrorRef>,
    ) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let fill = next
            .per_job
            .fills
            .get_mut(id)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if fill.publication.is_none() {
            fill.publication = Some(have);
        }
        self.save(next)
    }

    /// Called only after the helper has promoted or deleted the detached disk.
    pub fn fill_finished(&mut self, id: &str, published: bool, now_ms: u64) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let fill = next
            .per_job
            .fills
            .remove(id)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if published {
            let have = fill.publication.ok_or(Error::Refused(Refusal::Auth))?;
            for base in next.per_job.bases.values_mut().filter(|b| {
                b.sub == fill.fill.account
                    && b.repository_key.as_deref() == Some(&fill.fill.repository_key)
            }) {
                base.evicting = true;
            }
            next.per_job.bases.insert(
                fill.fill.disk_id(),
                storage::Base {
                    sub: fill.fill.account,
                    repository_key: Some(fill.fill.repository_key),
                    have,
                    last_used_ms: now_ms,
                    evicting: false,
                },
            );
        }
        for image in next.per_job.images.values_mut() {
            image.recovery_holders.remove(id);
        }
        self.save(next)
    }

    /// No possession is advertised until the helper has synced the reflink clone.
    /// A crash before the journal write leaves a conservative empty have record.
    pub fn seed_job_from_base(
        &mut self,
        job: &str,
        repository_key: &str,
        now_ms: u64,
    ) -> anyhow::Result<bool> {
        let Some(plan) = self.job_seed_plan(job, repository_key)? else {
            return Ok(false);
        };
        let control = self
            .job_storage
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("storage unavailable"))?;
        control.seed_workspace(&plan.request)?;
        self.job_seeded(job, &plan, now_ms)?;
        Ok(true)
    }

    /// A prior boot attempt may already have created or warmed this disk.
    pub(crate) fn job_workspace_is_new(&self, job: &str) -> bool {
        let execution = &self.snapshot.per_job.executions[job];
        !self
            .snapshot
            .per_job
            .executions
            .values()
            .any(|e| e.disk_id == execution.disk_id && e.boot_started && e.claims.jti != job)
    }

    /// Take this plan under the host lock, then call the helper after releasing it.
    pub(crate) fn job_seed_plan(
        &self,
        job: &str,
        repository_key: &str,
    ) -> anyhow::Result<Option<SeedPlan>> {
        let execution = self
            .snapshot
            .per_job
            .executions
            .get(job)
            .ok_or_else(|| anyhow::anyhow!("unknown job"))?;
        let disk_id = execution.disk_id.clone();
        let disk = &self.snapshot.per_job.disks[&disk_id];
        anyhow::ensure!(
            !disk.deleted
                && !disk.quarantined
                && disk
                    .attached_job
                    .as_deref()
                    .is_none_or(|owner| owner == job)
                && !execution.runner_hello
                && !execution.ended
                && self.job_workspace_is_new(job),
            "seeding requires a new, unattached workspace"
        );
        let Some((base_id, base)) = self
            .snapshot
            .per_job
            .bases
            .iter()
            .filter(|(_, b)| {
                !b.evicting
                    && b.sub == disk.sub
                    && b.repository_key.as_deref() == Some(repository_key)
            })
            .max_by_key(|(_, b)| b.last_used_ms)
        else {
            return Ok(None);
        };
        Ok(Some(SeedPlan {
            request: motor_vm_helper::job::Seed {
                account: disk.sub.clone(),
                repository_key: repository_key.into(),
                base_id: base_id.clone(),
                disk_id,
            },
            have: base.have.clone(),
        }))
    }

    /// Advertise possession only after the helper has durably published the clone.
    pub(crate) fn job_seeded(
        &mut self,
        job: &str,
        plan: &SeedPlan,
        now_ms: u64,
    ) -> anyhow::Result<()> {
        let execution = &self.snapshot.per_job.executions[job];
        let disk = &self.snapshot.per_job.disks[&execution.disk_id];
        anyhow::ensure!(
            execution.disk_id == plan.request.disk_id
                && disk.sub == plan.request.account
                && !disk.deleted
                && !disk.quarantined
                && !execution.ended
                && disk
                    .attached_job
                    .as_deref()
                    .is_none_or(|owner| owner == job),
            "workspace changed during seed"
        );
        let mut next = self.snapshot.clone();
        next.per_job
            .disks
            .get_mut(&plan.request.disk_id)
            .unwrap()
            .have = plan.have.clone();
        if let Some(base) = next.per_job.bases.get_mut(&plan.request.base_id) {
            base.last_used_ms = now_ms;
        }
        self.save(next)?;
        Ok(())
    }
}
