use super::*;
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct Hold {
    users: usize,
    revision: u64,
}

struct Donor {
    have: Vec<motor_protocol::MirrorRef>,
    revision: u64,
}

pub(crate) struct Plan {
    pub request: motor_vm_helper::job::Fork,
    donors: BTreeMap<String, Donor>,
}

impl Host {
    pub(crate) fn note_job_repository(&mut self, job: &str, root: &str) -> Result<(), Error> {
        if root.is_empty() {
            return Ok(());
        }
        let disk_id = &self.snapshot.per_job.executions[job].disk_id;
        let key = crate::jobs::fill_source::repository_key(root);
        if self.snapshot.per_job.disks[disk_id].repository_key.as_ref() == Some(&key) {
            return Ok(());
        }
        let mut next = self.snapshot.clone();
        next.per_job.disks.get_mut(disk_id).unwrap().repository_key = Some(key);
        self.save(next)
    }

    pub(crate) fn job_fork_plan(&mut self, job: &str, root: &str) -> anyhow::Result<Option<Plan>> {
        if !matches!(&self.settings, crate::config::HostSettings::PerJob(settings) if settings.fork_checkouts)
        {
            return Ok(None);
        }
        let execution = self
            .snapshot
            .per_job
            .executions
            .get(job)
            .ok_or_else(|| anyhow::anyhow!("unknown job"))?;
        if root.is_empty()
            || execution.claims.cache_write != Some(true)
            || self.snapshot.remote_cache_write_blocked
        {
            return Ok(None);
        }
        let disk = &self.snapshot.per_job.disks[&execution.disk_id];
        anyhow::ensure!(
            !disk.deleted
                && !disk.quarantined
                && disk.recovery.is_none()
                && disk
                    .attached_job
                    .as_deref()
                    .is_none_or(|owner| owner == job)
                && !execution.runner_hello
                && !execution.ended
                && self.job_workspace_is_new(job),
            "forking requires a new, unattached workspace"
        );
        let Some(storage) = &disk.storage else {
            return Ok(None);
        };
        let key = crate::jobs::fill_source::repository_key(root);
        let mut donors: Vec<_> = self
            .snapshot
            .per_job
            .disks
            .iter()
            .filter(|(id, donor)| {
                *id != &execution.disk_id
                    && donor.sub == disk.sub
                    && donor.repository_key.as_ref() == Some(&key)
                    && donor
                        .storage
                        .as_ref()
                        .is_some_and(|s| !s.evicting && s.cap_bytes <= storage.cap_bytes)
                    && donor.attached_job.is_none()
                    && donor.queue.is_empty()
                    && !donor.quarantined
                    && donor.recovery.is_none()
                    && !donor.deleted
                    && self
                        .snapshot
                        .per_job
                        .executions
                        .values()
                        .any(|e| e.disk_id == **id && e.runner_hello && e.ended)
            })
            .collect();
        donors.sort_by(|(a_id, a), (b_id, b)| {
            b.storage
                .as_ref()
                .unwrap()
                .last_used_ms
                .cmp(&a.storage.as_ref().unwrap().last_used_ms)
                .then_with(|| a_id.cmp(b_id))
        });
        donors.truncate(8);
        if donors.is_empty() {
            return Ok(None);
        }
        let request = motor_vm_helper::job::Fork {
            account: disk.sub.clone(),
            disk_id: execution.disk_id.clone(),
            repository_root: root.into(),
            donor_ids: donors.iter().map(|(id, _)| (*id).clone()).collect(),
            cache_write_job_id: Some(job.into()),
        };
        let mut donors: BTreeMap<_, _> = donors
            .into_iter()
            .map(|(id, disk)| {
                (
                    id.clone(),
                    Donor {
                        have: disk.have.clone(),
                        revision: 0,
                    },
                )
            })
            .collect();
        // The helper can publish a clone even if its reply or the later host
        // commit is lost. Persist guest history cleanup before cloning starts.
        let mut next = self.snapshot.clone();
        let disk = next.per_job.disks.get_mut(&request.disk_id).unwrap();
        disk.forked = true;
        disk.forked_from = None;
        disk.have.clear();
        self.save(next)?;
        for (id, donor) in &mut donors {
            let hold = self.fork_holds.entry(id.clone()).or_default();
            hold.users += 1;
            donor.revision = hold.revision;
        }
        Ok(Some(Plan { request, donors }))
    }

    pub(crate) fn release_job_fork(&mut self, plan: &Plan) {
        for id in plan.donors.keys() {
            if let Some(hold) = self.fork_holds.get_mut(id) {
                hold.users -= 1;
                if hold.users == 0 {
                    self.fork_holds.remove(id);
                }
            }
        }
    }

    pub(crate) fn job_forked(
        &mut self,
        job: &str,
        plan: &Plan,
        donor_id: &str,
    ) -> anyhow::Result<()> {
        let result = (|| {
            let donor = plan
                .donors
                .get(donor_id)
                .ok_or_else(|| anyhow::anyhow!("unlisted fork donor"))?;
            anyhow::ensure!(
                self.fork_holds
                    .get(donor_id)
                    .is_some_and(|h| h.revision == donor.revision),
                "fork donor changed during clone"
            );
            let current = self
                .snapshot
                .per_job
                .disks
                .get(donor_id)
                .ok_or_else(|| anyhow::anyhow!("fork donor disappeared"))?;
            anyhow::ensure!(
                current.have == donor.have
                    && !current.deleted
                    && !current.quarantined
                    && current.recovery.is_none(),
                "fork donor invalidated during clone"
            );
            let execution = &self.snapshot.per_job.executions[job];
            let disk = &self.snapshot.per_job.disks[&execution.disk_id];
            anyhow::ensure!(
                execution.disk_id == plan.request.disk_id
                    && disk.sub == plan.request.account
                    && disk.forked
                    && !disk.deleted
                    && !disk.quarantined
                    && disk.recovery.is_none()
                    && !execution.ended
                    && !execution.runner_hello
                    && self.job_workspace_is_new(job)
                    && execution.claims.cache_write == Some(true)
                    && !self.snapshot.remote_cache_write_blocked
                    && disk
                        .attached_job
                        .as_deref()
                        .is_none_or(|owner| owner == job),
                "workspace changed during fork"
            );
            let mut next = self.snapshot.clone();
            let disk = next.per_job.disks.get_mut(&plan.request.disk_id).unwrap();
            disk.have = donor.have.clone();
            disk.forked_from = Some(donor_id.into());
            self.save(next)?;
            Ok(())
        })();
        self.release_job_fork(plan);
        result
    }

    // A hold's revision records changes even if a later write restores the prior disk state.
    // Holds are memory-only: a host restart cannot authorize a stale plan.
    pub(in crate::state) fn update_fork_holds(&mut self, next: &super::super::Snapshot) {
        for (id, hold) in &mut self.fork_holds {
            let old = self.snapshot.per_job.disks.get(id);
            let new = next.per_job.disks.get(id);
            let changed = match (old, new) {
                (Some(a), Some(b)) => {
                    a.have != b.have
                        || a.quarantined != b.quarantined
                        || a.recovery != b.recovery
                        || a.deleted != b.deleted
                        || a.repository_key != b.repository_key
                        || a.sub != b.sub
                        || a.storage.as_ref().map(|s| (s.evicting, s.cap_bytes))
                            != b.storage.as_ref().map(|s| (s.evicting, s.cap_bytes))
                }
                _ => true,
            };
            // A saved write of an invalidated execution rejects the donor even
            // when its have list is already empty and its invalidation flag is unchanged.
            if changed
                || next.per_job.executions.changed_keys().any(|job| {
                    next.per_job
                        .executions
                        .get(job)
                        .is_some_and(|e| e.disk_id == *id && e.have_invalidated)
                })
            {
                hold.revision += 1;
            }
        }
    }
}
