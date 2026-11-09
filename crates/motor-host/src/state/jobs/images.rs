//! Admission-time image pins and space promises, in the same journal transaction
//! as workspace growth, replay, memory and permits. No image IO occurs here.
use super::Ledger;
use crate::{
    claim_log::{ClaimError, RefusalCause},
    state::{Error, Host},
};
use motor_vm_helper::images::Manifest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Generation {
    pub manifest: Manifest,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub current: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub artifacts_ready: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleting: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recovery_holders: BTreeMap<String, String>,
}

pub(super) fn next_generation(ledger: &Ledger) -> Result<Option<String>, ClaimError> {
    let current = ledger
        .images
        .iter()
        .find(|(_, g)| g.current && !g.pending && !g.deleting)
        .map(|(id, _)| id.clone());
    if current.is_none() && !ledger.images.is_empty() {
        return Err(RefusalCause::Storage.into());
    }
    Ok(current)
}

pub(super) fn reserve_account(ledger: &mut Ledger, job: &str) {
    let job = &ledger.executions[job];
    if let Some(id) = &job.image_generation {
        ledger
            .images
            .get_mut(id)
            .unwrap()
            .accounts
            .entry(job.claims.sub.clone())
            .or_insert(false);
    }
}

pub(super) fn reserved_bytes(ledger: &Ledger) -> Option<u64> {
    ledger.images.values().try_fold(0u64, |total, generation| {
        let bytes = if generation.pending && !generation.artifacts_ready {
            generation
                .manifest
                .publication_bytes(generation.accounts.len())?
        } else {
            generation.manifest.toolchain.bytes.checked_mul(
                generation
                    .accounts
                    .values()
                    .filter(|ready| !**ready)
                    .count() as u64,
            )?
        };
        total.checked_add(bytes)
    })
}

fn held(ledger: &Ledger, id: &str) -> bool {
    ledger
        .executions
        .values()
        .any(|e| !e.ended && e.image_generation.as_deref() == Some(id))
        || !ledger.images[id].recovery_holders.is_empty()
}

impl Host {
    /// Reserve the entire publication before the helper performs any image work.
    /// An interrupted publication remains pending and cannot be assigned.
    pub fn begin_image_publication(
        &mut self,
        id: &str,
        manifest: Manifest,
        now_ms: u64,
    ) -> anyhow::Result<()> {
        motor_vm_helper::images::validate_generation(id)?;
        manifest.validate()?;
        anyhow::ensure!(
            matches!(self.settings, crate::config::HostSettings::PerJob(_)),
            "image publication requires per-job mode"
        );
        if let Some(previous) = self.snapshot.per_job.images.get(id) {
            anyhow::ensure!(
                previous.pending && previous.manifest == manifest && !previous.deleting,
                "generation identity already used"
            );
        } else {
            anyhow::ensure!(
                !self.snapshot.per_job.images.values().any(|g| g.pending),
                "another image publication is pending"
            );
        }
        let mut next = self.snapshot.clone();
        let generation = next.per_job.images.entry(id.into()).or_insert(Generation {
            manifest,
            current: false,
            pending: true,
            artifacts_ready: false,
            deleting: false,
            removed: false,
            accounts: BTreeMap::new(),
            recovery_holders: BTreeMap::new(),
        });
        for disk in next.per_job.disks.values().filter(|d| !d.deleted) {
            generation.accounts.entry(disk.sub.clone()).or_insert(false);
        }
        self.reserve_storage(&mut next, "", now_ms)
            .map_err(|e| anyhow::anyhow!("publication storage reservation: {}", e.error))?;
        self.save(next)?;
        Ok(())
    }

    pub fn image_account_ready(&mut self, id: &str, account: &str) -> anyhow::Result<()> {
        let mut next = self.snapshot.clone();
        let generation = next
            .per_job
            .images
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("unknown generation"))?;
        anyhow::ensure!(!generation.deleting, "generation being deleted");
        let ready = generation
            .accounts
            .get_mut(account)
            .ok_or_else(|| anyhow::anyhow!("account copy has no reservation"))?;
        *ready = true;
        self.save(next)?;
        Ok(())
    }

    pub fn image_artifacts_ready(&mut self, id: &str) -> anyhow::Result<()> {
        let mut next = self.snapshot.clone();
        let generation = next
            .per_job
            .images
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("unknown generation"))?;
        anyhow::ensure!(
            generation.pending && !generation.deleting,
            "generation not publishing"
        );
        generation.artifacts_ready = true;
        self.save(next)?;
        Ok(())
    }

    pub fn finish_image_publication(&mut self, id: &str) -> anyhow::Result<()> {
        let generation = self
            .snapshot
            .per_job
            .images
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unknown generation"))?;
        anyhow::ensure!(
            generation.pending
                && generation.artifacts_ready
                && !generation.deleting
                && generation.accounts.values().all(|ready| *ready),
            "publication is incomplete"
        );
        anyhow::ensure!(
            self.snapshot
                .per_job
                .disks
                .values()
                .filter(|d| !d.deleted)
                .all(|d| generation.accounts.contains_key(&d.sub)),
            "publication must copy newly admitted accounts"
        );
        let mut next = self.snapshot.clone();
        for (key, generation) in &mut next.per_job.images {
            generation.current = key == id;
            if key == id {
                generation.pending = false;
            }
        }
        self.save(next)?;
        Ok(())
    }

    /// Fence GC durably, before asking the helper to remove files. A restart
    /// retries this intent; it never makes a partially deleted image current.
    pub fn begin_image_gc(&mut self, id: &str) -> anyhow::Result<()> {
        let generation = self
            .snapshot
            .per_job
            .images
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unknown generation"))?;
        anyhow::ensure!(
            !generation.current
                && !generation.pending
                && !generation.removed
                && !held(&self.snapshot.per_job, id),
            "generation still referenced or removed"
        );
        let mut next = self.snapshot.clone();
        next.per_job.images.get_mut(id).unwrap().deleting = true;
        self.save(next)?;
        Ok(())
    }

    pub fn finish_image_gc(&mut self, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.snapshot
                .per_job
                .images
                .get(id)
                .is_some_and(|g| g.deleting && !g.current && !g.pending)
                && !held(&self.snapshot.per_job, id),
            "generation not fenced for deletion"
        );
        let mut next = self.snapshot.clone();
        let generation = next.per_job.images.get_mut(id).unwrap();
        generation.accounts.clear();
        generation.removed = true;
        generation.deleting = false;
        self.save(next)?;
        Ok(())
    }

    /// Call only after the helper has confirmed that publication work is gone.
    pub fn abort_image_publication(&mut self, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.snapshot
                .per_job
                .images
                .get(id)
                .is_some_and(|g| g.pending && !g.current)
                && !held(&self.snapshot.per_job, id),
            "publication is referenced"
        );
        let mut next = self.snapshot.clone();
        next.per_job.images.remove(id);
        self.save(next)?;
        Ok(())
    }

    pub fn reserve_recovery_image(
        &mut self,
        job: &str,
        account: &str,
        now_ms: u64,
    ) -> anyhow::Result<String> {
        if let Some((id, _)) = self
            .snapshot
            .per_job
            .images
            .iter()
            .find(|(_, g)| g.recovery_holders.contains_key(job))
        {
            return Ok(id.clone());
        }
        let id = next_generation(&self.snapshot.per_job)
            .map_err(|e| anyhow::anyhow!("recovery image: {}", e.error))?
            .ok_or_else(|| anyhow::anyhow!("no current image"))?;
        let mut next = self.snapshot.clone();
        let generation = next.per_job.images.get_mut(&id).unwrap();
        generation
            .recovery_holders
            .insert(job.into(), account.into());
        generation.accounts.entry(account.into()).or_insert(false);
        self.reserve_storage(&mut next, "", now_ms)
            .map_err(|e| anyhow::anyhow!("recovery storage: {}", e.error))?;
        self.save(next)?;
        Ok(id)
    }

    pub fn release_image_holder(&mut self, job: &str) -> Result<(), Error> {
        if self
            .snapshot
            .per_job
            .executions
            .get(job)
            .is_some_and(|e| !e.ended)
            || self.snapshot.per_job.fills.contains_key(job)
        {
            return Err(Error::Refused(crate::state::Refusal::InternalFailure));
        }
        let mut next = self.snapshot.clone();
        for generation in next.per_job.images.values_mut() {
            generation.recovery_holders.remove(job);
        }
        for (id, generation) in &mut next.per_job.images {
            if generation.pending {
                continue;
            }
            generation.accounts.retain(|account, ready| {
                *ready
                    || generation.recovery_holders.values().any(|a| a == account)
                    || self.snapshot.per_job.executions.values().any(|e| {
                        !e.ended
                            && e.image_generation.as_ref() == Some(id)
                            && &e.claims.sub == account
                    })
            });
        }
        self.save(next)
    }
}
