//! Durable admission for job VMs. The lifecycle consumes a launch authorization
//! only after its transaction commits; reconnects never issue a second one.
use super::{Admission, Connection, ConnectionState, Error, Host, Refusal};
use crate::claim_log::{ClaimError, RefusalCause};
use crate::config::HostSettings;
use motor_lease::{Claims, Purpose};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Ledger {
    /// Disk ids are opaque VM names in placement hints, not account or workspace names.
    pub disks: BTreeMap<String, WorkspaceDisk>,
    pub executions: BTreeMap<String, Execution>,
}

impl Ledger {
    pub fn is_empty(&self) -> bool {
        self.disks.is_empty() && self.executions.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct WorkspaceDisk {
    pub sub: String,
    pub workspace_key_hash: String,
    pub queue: Vec<String>,
    pub attached_job: Option<String>,
    pub quarantined: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Execution {
    pub claims: Claims,
    pub disk_id: String,
    pub memory_debit_mib: u64,
    pub guest_ram_mib: u64,
    pub boot_started: bool,
    pub runner_hello: bool,
    pub ended: bool,
}

/// Trusted host measurements. The permit budget includes CPU overcommit; the
/// ceiling bounds one job's vCPUs. Guest reports must never populate this sample.
#[derive(Clone, Copy, Debug)]
pub struct HostResources {
    pub at_ms: u64,
    pub available_mem_mib: u64,
    pub vcpu_ceiling: u32,
    pub permit_budget: u32,
}

/// The token's optional field enables burst; its numeric value cannot enlarge
/// the contract's burst bound, even for a caller bypassing token verification.
pub fn memory_limit_mib(claims: &Claims) -> Option<u64> {
    claims
        .mem_mib
        .checked_add(if claims.mem_burst_mib.is_some() {
            (claims.mem_mib / 2).min(8192)
        } else {
            0
        })
}

pub fn guest_ram_mib(claims: &Claims) -> Option<u64> {
    let limit = u128::from(memory_limit_mib(claims)?);
    u64::try_from((limit * 4).div_ceil(3)).ok()
}

impl Host {
    pub fn record_host_resources(&mut self, resources: HostResources) {
        self.host_resources = Some(resources);
    }

    pub fn job_free_mem_mib(&self, now_ms: u64) -> u64 {
        let HostSettings::PerJob(settings) = &self.settings else {
            return 0;
        };
        let Some(sample) = self.fresh_host_resources(now_ms) else {
            return 0;
        };
        self.snapshot
            .per_job
            .executions
            .values()
            .filter(|job| !job.ended && !job.runner_hello)
            .fold(
                sample
                    .available_mem_mib
                    .saturating_sub(settings.host_reserve_mib),
                |free, job| free.saturating_sub(job.memory_debit_mib),
            )
    }

    fn fresh_host_resources(&self, now_ms: u64) -> Option<HostResources> {
        self.host_resources.filter(|sample| {
            now_ms >= sample.at_ms && now_ms - sample.at_ms <= super::MEMORY_FRESH_MS
        })
    }

    pub(super) fn claim_job(
        &mut self,
        claims: &Claims,
        now_ms: u64,
    ) -> Result<(u64, Admission), ClaimError> {
        let Some(hash) = claims.workspace_key_hash.as_ref() else {
            return Err(Refusal::Auth.into());
        };
        if self.snapshot.revocations.contains_key(&claims.jti) {
            return Err(Refusal::Auth.into());
        }
        if now_ms / 1000 >= claims.exp {
            return Err(Refusal::Expired.into());
        }
        let ledger = &self.snapshot.per_job;
        if let Some(execution) = ledger.executions.get(&claims.jti) {
            if self.connections.iter().any(|connection| {
                connection.job == claims.jti && connection.purpose == claims.purpose
            }) {
                return Err(Refusal::Auth.into());
            }
            // Placement hints may change on a reconnect. Only the recorded
            // execution selects its disk, and its account/workspace must match.
            if !same_execution(&execution.claims, claims) {
                return Err(Refusal::Auth.into());
            }
            let disk = ledger.disks.get(&execution.disk_id).ok_or(Refusal::Auth)?;
            if disk.sub != claims.sub || &disk.workspace_key_hash != hash {
                return Err(Refusal::Auth.into());
            }
        } else {
            if claims.purpose == Purpose::Attach {
                return Err(Refusal::Auth.into());
            }
            let HostSettings::PerJob(settings) = &self.settings else {
                return Err(Refusal::Auth.into());
            };
            let sample = self
                .fresh_host_resources(now_ms)
                .ok_or(RefusalCause::Memory)?;
            let used = ledger
                .executions
                .values()
                .filter(|job| !job.ended)
                .try_fold(0u32, |used, job| used.checked_add(job.claims.vcpus));
            if claims.vcpus > sample.vcpu_ceiling
                || used
                    .and_then(|used| used.checked_add(claims.vcpus))
                    .is_none_or(|used| used > sample.permit_budget)
            {
                return Err(RefusalCause::Permits.into());
            }
            let debit = claims
                .mem_mib
                .checked_add(settings.boot_reserve_mib)
                .ok_or(RefusalCause::Memory)?;
            if self.job_free_mem_mib(now_ms) < debit {
                return Err(RefusalCause::Memory.into());
            }
            if self.storage_refusing() {
                return Err(RefusalCause::Storage.into());
            }
            // A host has one disk per account/workspace pair. Cold or foreign
            // hints never bypass that disk's queue or attach another pair's data.
            let existing = ledger
                .disks
                .iter()
                .find(|(_, disk)| disk.sub == claims.sub && &disk.workspace_key_hash == hash);
            if existing.is_some_and(|(_, disk)| disk.quarantined) {
                return Err(RefusalCause::QuarantinedWorkspace.into());
            }
            let disk_id = existing.map(|(id, _)| id.clone()).unwrap_or_else(|| loop {
                let mut random = [0u8; 16];
                rand::thread_rng().fill_bytes(&mut random);
                let id = format!("ws-{:032x}", u128::from_le_bytes(random));
                if !ledger.disks.contains_key(&id) {
                    break id;
                }
            });
            let execution = Execution {
                claims: claims.clone(),
                disk_id: disk_id.clone(),
                memory_debit_mib: debit,
                guest_ram_mib: guest_ram_mib(claims).ok_or(RefusalCause::Memory)?,
                boot_started: false,
                runner_hello: false,
                ended: false,
            };
            let mut next = self.snapshot.clone();
            next.per_job
                .disks
                .entry(disk_id)
                .or_insert_with(|| WorkspaceDisk {
                    sub: claims.sub.clone(),
                    workspace_key_hash: hash.clone(),
                    queue: Vec::new(),
                    attached_job: None,
                    quarantined: false,
                })
                .queue
                .push(claims.jti.clone());
            next.per_job
                .executions
                .insert(claims.jti.clone(), execution);
            // Identity, queue position and memory debit become visible together,
            // before lease_ok and before any launch authorization is returned.
            self.save(next)?;
        }
        self.next_connection += 1;
        let id = self.next_connection;
        self.connections.push(Connection {
            id,
            job: claims.jti.clone(),
            purpose: claims.purpose,
            updates: tokio::sync::watch::channel(ConnectionState::Ready).0,
        });
        // Disk contention is an admitted runner-style queue, not a capacity
        // wait before lease_ok. The lifecycle waits in begin_job_boot instead.
        Ok((id, Admission::Ready))
    }

    /// Consumes the sole launch authorization. After restart, a job with
    /// boot_started set must not launch again, even if runner_hello was lost.
    pub fn begin_job_boot(&mut self, job: &str) -> Result<Option<Execution>, Error> {
        let execution = self
            .snapshot
            .per_job
            .executions
            .get(job)
            .ok_or(Refusal::Auth)
            .map_err(Error::Refused)?;
        if execution.boot_started || execution.ended {
            return Ok(None);
        }
        let disk = &self.snapshot.per_job.disks[&execution.disk_id];
        if disk.quarantined
            || disk.attached_job.is_some()
            || disk.queue.first().map(String::as_str) != Some(job)
        {
            return Ok(None);
        }
        let mut next = self.snapshot.clone();
        let execution = next.per_job.executions.get_mut(job).unwrap();
        execution.boot_started = true;
        let launch = execution.clone();
        next.per_job
            .disks
            .get_mut(&launch.disk_id)
            .unwrap()
            .attached_job = Some(job.to_owned());
        self.save(next)?;
        Ok(Some(launch))
    }

    /// Called only after the host's runner handshake, never from telemetry.
    pub fn job_runner_hello(&mut self, job: &str) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get_mut(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if !execution.boot_started || execution.ended {
            return Err(Error::Refused(Refusal::Auth));
        }
        if !execution.runner_hello {
            execution.runner_hello = true;
            self.save(next)?;
        }
        Ok(())
    }

    /// Releases admission only after the lifecycle has confirmed the VM stopped
    /// (or that a queued execution never booted). Retain identity for reconnects.
    pub fn job_stopped(&mut self, job: &str, unclean: bool) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get_mut(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if execution.ended {
            return Ok(());
        }
        execution.ended = true;
        let disk = next.per_job.disks.get_mut(&execution.disk_id).unwrap();
        disk.queue.retain(|queued| queued != job);
        if disk.attached_job.as_deref() == Some(job) {
            disk.attached_job = None;
            disk.quarantined |= unclean;
        }
        self.save(next)
    }
}

fn same_execution(a: &Claims, b: &Claims) -> bool {
    a.jti == b.jti
        && a.sub == b.sub
        && a.workspace_key_hash == b.workspace_key_hash
        && a.placement.host == b.placement.host
        && a.vcpus == b.vcpus
        && a.mem_mib == b.mem_mib
        && a.mem_burst_mib == b.mem_burst_mib
        && a.max_s == b.max_s
        && a.net == b.net
        && a.platform == b.platform
}
