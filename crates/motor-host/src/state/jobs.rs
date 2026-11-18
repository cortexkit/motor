//! Durable admission for job VMs. The lifecycle consumes a launch authorization
//! only after its transaction commits; reconnects never issue a second one.
use super::{Admission, Connection, ConnectionState, Error, Host, Refusal};
use crate::claim_log::{ClaimError, RefusalCause};
use crate::config::HostSettings;
use motor_lease::{Claims, Purpose};
use rand::RngCore;
use serde::{Deserialize, Serialize};

pub mod fills;
pub(crate) mod forks;
pub(crate) mod have;
pub mod images;
pub mod recovery;
pub(crate) mod residency;
pub mod storage;

pub(super) const WARM_WORKSPACE_LIMIT: usize = 64;
const TERMINAL_IDLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
const WAITER_MEMORY_HOLD_AFTER_MS: u64 = 60_000;

pub(super) struct MemoryWaiter {
    admission_order: u64,
    ready_since_ms: Option<u64>,
}

pub(super) struct MemoryWaiterHold {
    job: String,
    started_at: std::time::Instant,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Ledger {
    #[serde(default, skip_serializing_if = "super::Table::is_empty")]
    pub fill_sources: super::Table<crate::jobs::fill_source::Slot>,
    #[serde(default, skip_serializing_if = "super::Table::is_empty")]
    pub export_tips: super::Table<crate::jobs::fill_source::Request>,
    /// The helper's store may rotate while the kept tenant disk is unused.
    /// Tenant startup must install the current sets before clearing this fence.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tenant_rollback_pending: bool,
    #[serde(default, skip_serializing_if = "super::Table::is_empty")]
    pub fills: super::Table<fills::Reservation>,
    /// A DAX-less boot requires operator intervention, not an automatic retry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dax_failed: bool,
    /// Disk ids are opaque VM names in placement hints, not account or workspace names.
    pub disks: super::Table<WorkspaceDisk>,
    pub executions: super::Table<Execution>,
    #[serde(default, skip_serializing_if = "super::Table::is_empty")]
    pub bases: super::Table<storage::Base>,
    #[serde(default, skip_serializing_if = "super::Table::is_empty")]
    pub images: super::Table<images::Generation>,
}

impl Ledger {
    /// Folds every table's changes into its base; see `Snapshot::commit`.
    pub(super) fn commit(&mut self) {
        let Self {
            fill_sources,
            export_tips,
            tenant_rollback_pending: _,
            fills,
            dax_failed: _,
            disks,
            executions,
            bases,
            images,
        } = self;
        fill_sources.commit();
        export_tips.commit();
        fills.commit();
        disks.commit();
        executions.commit();
        bases.commit();
        images.commit();
    }
    pub fn is_empty(&self) -> bool {
        !self.tenant_rollback_pending
            && self.fill_sources.is_empty()
            && self.export_tips.is_empty()
            && !self.dax_failed
            && self.fills.is_empty()
            && self.disks.is_empty()
            && self.executions.is_empty()
            && self.bases.is_empty()
            && self.images.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct WorkspaceDisk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<storage::Disk>,
    pub sub: String,
    pub workspace_key_hash: String,
    pub queue: Vec<String>,
    pub attached_job: Option<String>,
    pub quarantined: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<recovery::Recovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_result: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// Oldest-first mirror pins from runner reports, never caller candidates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub have: Vec<motor_protocol::MirrorRef>,
}

impl WorkspaceDisk {
    fn holds_job_disk_slot(&self) -> bool {
        !self.deleted && (self.attached_job.is_some() || !self.queue.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Execution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_reservation_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_replay_reservation_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_completion: Option<storage::ReplayCompletion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relayed_return_paths: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_accepted_at_ms: Option<u64>,
    pub claims: Claims,
    pub disk_id: String,
    pub memory_debit_mib: u64,
    pub guest_ram_mib: u64,
    pub boot_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_started_at_ms: Option<u64>,
    pub runner_hello: bool,
    pub ended: bool,
    /// Fence delayed imports from this VM after a refusal or an unclean stop.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub have_invalidated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<super::settlement::Evidence>,
}

/// Trusted host measurements. The permit budget includes CPU overcommit; the
/// ceiling bounds one job's vCPUs. Guest reports must never populate this sample.
#[derive(Clone, Copy, Debug)]
pub struct HostResources {
    pub at_ms: u64,
    pub available_mem_mib: u64,
    pub job_vm_headroom_mib: u64,
    pub vcpu_ceiling: u32,
    pub permit_budget: u32,
}

#[derive(PartialEq, Eq)]
pub(super) enum HostResourcesFailure {
    MissingOrStale,
    Unavailable(String),
}

// A caller may capture its clock, then wait on I/O or a lock while another
// thread publishes a newer sample. Such a sample is newer than the caller's
// view, not stale; allow the same window ahead as behind.
const HOST_SAMPLE_AHEAD_MS: u64 = super::MEMORY_FRESH_MS;

impl HostResources {
    pub(crate) fn free_mem_mib(&self, host_reserve_mib: u64) -> u64 {
        // A VM can grow by GiB between probes and before reclaim takes effect.
        // Keep the same reserve at both limits, rather than waiting until the
        // slice is nearly full. Take the minimum first: this is one reserve,
        // not two debits against the same available memory.
        self.available_mem_mib
            .min(self.job_vm_headroom_mib)
            .saturating_sub(host_reserve_mib)
    }

    pub(crate) fn memory_pressure(&self, host_reserve_mib: u64) -> bool {
        self.available_mem_mib.min(self.job_vm_headroom_mib) < host_reserve_mib
    }
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
    pub(super) fn update_job_warm_state(&mut self, next: &super::Snapshot) {
        if !matches!(self.settings, HostSettings::PerJob(_)) {
            return;
        }
        let has_terminal = |snapshot: &super::Snapshot, job: &str| {
            snapshot
                .billing
                .get(job)
                .is_some_and(|billing| billing.terminal.is_some() || billing.host_settled)
        };
        for job in next.billing.changed_keys() {
            // A retained execution can outlive its evicted disk; look it up
            // rather than index, so saving never panics on a missing disk.
            if next.per_job.executions.get(job).is_some_and(|execution| {
                next.per_job
                    .disks
                    .get(&execution.disk_id)
                    .is_some_and(|disk| disk.attached_job.as_ref() == Some(job))
            }) && has_terminal(next, job)
                && !has_terminal(&self.snapshot, job)
            {
                self.job_terminal_stored_at
                    .insert(job.clone(), tokio::time::Instant::now());
            }
        }
        for id in next.per_job.disks.changed_keys() {
            if let Some(job) = self
                .snapshot
                .per_job
                .disks
                .get(id)
                .and_then(|disk| disk.attached_job.as_ref())
            {
                if next
                    .per_job
                    .disks
                    .get(id)
                    .and_then(|disk| disk.attached_job.as_ref())
                    != Some(job)
                {
                    self.job_terminal_stored_at.remove(job);
                }
            }
        }
    }

    pub(crate) fn set_dax_failed(&mut self, failed: bool) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        next.per_job.dax_failed = failed;
        self.save(next)
    }

    pub(super) fn job_warm_workspaces(&self) -> Vec<motor_lease::WarmVm> {
        let mut entries: Vec<_> = self
            .snapshot
            .per_job
            .disks
            .iter()
            .filter(|(_, disk)| !disk.deleted)
            .map(|(id, disk)| {
                let terminal_idle = disk.attached_job.as_ref().is_some_and(|job| {
                    self.job_terminal_stored_at
                        .get(job)
                        .is_some_and(|stored| stored.elapsed() < TERMINAL_IDLE_WINDOW)
                        // The attached job stays at the queue head until release.
                        && disk.queue.iter().all(|queued| queued == job)
                }) && !disk.quarantined
                    && disk.recovery.is_none();
                let priority = if disk.attached_job.is_some() && !terminal_idle {
                    3
                } else if !disk.queue.is_empty() && !terminal_idle {
                    2
                } else if disk.quarantined || disk.recovery.is_some() {
                    1
                } else {
                    0
                };
                let busy = priority > 0;
                // Disk recency survives retention of the jobs that used it.
                let used = disk.storage.as_ref().map_or(0, |s| s.last_used_ms);
                (
                    priority,
                    used,
                    motor_lease::WarmVm {
                        tenant: disk.sub.clone(),
                        workspace_key_hash: disk.workspace_key_hash.clone(),
                        vm: id.clone(),
                        busy,
                    },
                )
            })
            .collect();
        entries.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.vm.cmp(&b.2.vm))
        });
        entries
            .into_iter()
            .take(WARM_WORKSPACE_LIMIT)
            .map(|(_, _, entry)| entry)
            .collect()
    }

    pub fn record_host_resources(&mut self, resources: HostResources) {
        self.host_resources = Some(resources);
        self.job_vm_file_mib = None;
        *self.host_resources_failed.get_mut() = None;
    }

    pub(crate) fn host_resources_unavailable(&mut self, detail: &str) {
        self.host_resources = None;
        self.job_vm_file_mib = None;
        let failure = HostResourcesFailure::Unavailable(detail.to_owned());
        if self.host_resources_failed.get_mut().as_ref() != Some(&failure) {
            *self.host_resources_failed.get_mut() = Some(failure);
            tracing::warn!(detail, "host_resources_unavailable");
        }
    }

    pub fn job_free_mem_mib(&self, now_ms: u64) -> u64 {
        self.job_free_mem_mib_except(now_ms, None)
    }

    pub(crate) fn job_free_mem_mib_except(&self, now_ms: u64, booting_job: Option<&str>) -> u64 {
        let HostSettings::PerJob(settings) = &self.settings else {
            return 0;
        };
        let Some(sample) = self.fresh_host_resources(now_ms) else {
            return 0;
        };
        let free = self
            .snapshot
            .per_job
            .executions
            .values()
            .filter(|job| {
                !job.ended
                    && !job.runner_hello
                    && (job.boot_started
                        || (booting_job != Some(job.claims.jti.as_str())
                            && self.job_memory_reservations.contains(&job.claims.jti)))
            })
            .fold(
                sample.free_mem_mib(settings.host_reserve_mib),
                |free, job| free.saturating_sub(job.memory_debit_mib),
            );
        let free = self
            .snapshot
            .per_job
            .disks
            .values()
            .filter_map(|d| d.recovery.as_ref())
            .filter(|r| r.holds_vm_capacity())
            .fold(free, |free, r| free.saturating_sub(r.memory_debit_mib));
        let free = self
            .snapshot
            .per_job
            .fills
            .values()
            .filter(|f| !f.runner_hello)
            .fold(free, |free, f| free.saturating_sub(f.memory_debit_mib));
        free.saturating_sub(self.job_memory_hold(now_ms).held_back_mib)
    }

    pub(crate) fn job_placeable_mem_mib(&self, now_ms: u64) -> u64 {
        let crate::config::HostSettings::PerJob(settings) = &self.settings else {
            return 0;
        };
        // Admission debits the requested RAM plus boot overhead. Advertise only
        // the RAM a new request can use, not the overhead it must also leave free.
        self.job_free_mem_mib(now_ms)
            .saturating_sub(settings.boot_reserve_mib)
    }

    pub(crate) fn log_job_memory(&self, now_ms: u64, free_mem_mib: u64) {
        let HostSettings::PerJob(settings) = &self.settings else {
            return;
        };
        if self.job_memory_logged.get().is_some_and(|(at_ms, free)| {
            now_ms >= at_ms && now_ms - at_ms < 60_000 && free.abs_diff(free_mem_mib) < 1024
        }) {
            return;
        }
        self.job_memory_logged.set(Some((now_ms, free_mem_mib)));
        let sample = self.fresh_host_resources(now_ms);
        let mut booting_memory_debit_mib = 0u64;
        let mut reserved_memory_debit_mib = 0u64;
        let mut running_memory_debit_mib = 0u64;
        let mut disk_waiters = 0u64;
        for job in self
            .snapshot
            .per_job
            .executions
            .values()
            .filter(|e| !e.ended)
        {
            if !job.boot_started {
                if self.job_memory_reservations.contains(&job.claims.jti) {
                    reserved_memory_debit_mib =
                        reserved_memory_debit_mib.saturating_add(job.memory_debit_mib);
                }
                if !self.job_boot_disk_ready(&job.claims.jti) {
                    disk_waiters += 1;
                }
            } else if job.runner_hello {
                // Running VMs are already charged in the host/cgroup samples;
                // their admission debits are diagnostic, not subtracted again.
                running_memory_debit_mib =
                    running_memory_debit_mib.saturating_add(job.memory_debit_mib);
            } else {
                booting_memory_debit_mib =
                    booting_memory_debit_mib.saturating_add(job.memory_debit_mib);
            }
        }
        let recovery_memory_debit_mib = self
            .snapshot
            .per_job
            .disks
            .values()
            .filter_map(|disk| disk.recovery.as_ref())
            .filter(|recovery| recovery.holds_vm_capacity())
            .fold(0u64, |sum, recovery| {
                sum.saturating_add(recovery.memory_debit_mib)
            });
        let fill_memory_debit_mib = self
            .snapshot
            .per_job
            .fills
            .values()
            .filter(|fill| !fill.runner_hello)
            .fold(0u64, |sum, fill| sum.saturating_add(fill.memory_debit_mib));
        let hold = self.job_memory_hold(now_ms);
        let held_for_waiter_mib = self.job_memory_waiter_hold.as_ref().map_or(0, |hold| {
            self.snapshot.per_job.executions[&hold.job].memory_debit_mib
        });
        tracing::info!(
            free_mem_mib,
            available_mem_mib = sample.map(|s| s.available_mem_mib),
            job_vm_headroom_mib = sample.map(|s| s.job_vm_headroom_mib),
            job_vm_file_mib = sample.and(self.job_vm_file_mib),
            host_reserve_mib = settings.host_reserve_mib,
            booting_memory_debit_mib,
            reserved_memory_debit_mib,
            running_memory_debit_mib,
            running_subtracted_mib = 0u64,
            recovery_memory_debit_mib,
            fill_memory_debit_mib,
            disk_waiters,
            held_back_mib = hold.held_back_mib,
            held_for_waiter_mib,
            resident_samples_stale = hold.resident_samples_stale,
            "job_memory_advertised"
        );
    }

    pub(crate) fn fresh_host_resources(&self, now_ms: u64) -> Option<HostResources> {
        let fresh = self.host_resources.filter(|sample| {
            // Concurrent callers may have captured their clocks before the
            // thread that published this sample. Allow only a bounded skew.
            sample.at_ms.saturating_sub(now_ms) <= HOST_SAMPLE_AHEAD_MS
                && now_ms.saturating_sub(sample.at_ms) <= super::MEMORY_FRESH_MS
        });
        if fresh.is_none() && self.host_resources_failed.borrow().is_none() {
            *self.host_resources_failed.borrow_mut() = Some(HostResourcesFailure::MissingOrStale);
            tracing::warn!(
                now_ms,
                sample_at_ms = self.host_resources.map(|s| s.at_ms),
                "host_resources_missing_or_stale"
            );
        }
        fresh
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
            if !self.snapshot.leases.contains_key(&claims.jti) {
                return Err(Refusal::Auth.into());
            }
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
            if ledger.dax_failed {
                return Err(RefusalCause::DaxRequired.into());
            }
            let HostSettings::PerJob(settings) = &self.settings else {
                return Err(Refusal::Auth.into());
            };
            let existing = ledger.disks.iter().find(|(_, disk)| {
                !disk.deleted && disk.sub == claims.sub && &disk.workspace_key_hash == hash
            });
            if existing.is_some_and(|(_, disk)| disk.quarantined) {
                return Err(RefusalCause::QuarantinedWorkspace.into());
            }
            // Reserve future attachments for queued jobs too, so concurrent
            // claims cannot later hide a running workspace from the warm list.
            // Another job on the same disk shares its existing slot.
            if !existing.is_some_and(|(_, disk)| disk.holds_job_disk_slot())
                && ledger
                    .disks
                    .values()
                    .filter(|disk| disk.holds_job_disk_slot())
                    .count()
                    >= WARM_WORKSPACE_LIMIT
            {
                return Err(RefusalCause::WorkspaceDisks.into());
            }
            let sample = self
                .fresh_host_resources(now_ms)
                .ok_or(RefusalCause::Memory)?;
            let used = ledger
                .executions
                .values()
                .filter(|job| !job.ended)
                .try_fold(
                    self.recovery_permits().saturating_add(self.fill_permits()),
                    |used, job| used.checked_add(job.claims.vcpus),
                );
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
            // A disk-queued job has no VM yet. Recheck memory when it can
            // actually boot instead of refusing or reserving memory now.
            let reserve_memory = !existing.is_some_and(|(_, disk)| disk.holds_job_disk_slot());
            if reserve_memory && self.job_free_mem_mib(now_ms) < debit {
                return Err(RefusalCause::Memory.into());
            }
            // A host has one disk per account/workspace pair. Cold or foreign
            // hints never bypass that disk's queue or attach another pair's data.
            let disk_id = existing.map(|(id, _)| id.clone()).unwrap_or_else(|| loop {
                let mut random = [0u8; 16];
                rand::thread_rng().fill_bytes(&mut random);
                let id = format!("ws-{:032x}", u128::from_le_bytes(random));
                if !ledger.disks.contains_key(&id) {
                    break id;
                }
            });
            let execution = Execution {
                image_generation: images::next_generation(&self.snapshot.per_job)?,
                replay_reservation_bytes: Some(storage::REPLAY_RESERVATION_BYTES),
                final_replay_reservation_bytes: None,
                replay_completion: None,
                relayed_return_paths: None,
                usage_accepted_at_ms: None,
                claims: claims.clone(),
                disk_id: disk_id.clone(),
                memory_debit_mib: debit,
                guest_ram_mib: guest_ram_mib(claims).ok_or(RefusalCause::Memory)?,
                boot_started: false,
                boot_started_at_ms: None,
                runner_hello: false,
                ended: false,
                have_invalidated: false,
                evidence: None,
            };
            let mut next = self.snapshot.clone();
            next.per_job
                .disks
                .entry(disk_id.clone())
                .or_insert_with(|| WorkspaceDisk {
                    repository_key: None,
                    forked_from: None,
                    forked: false,
                    storage: Some(storage::Disk::new(
                        settings.workspace_disk_cap_bytes,
                        now_ms,
                    )),
                    sub: claims.sub.clone(),
                    workspace_key_hash: hash.clone(),
                    queue: Vec::new(),
                    attached_job: None,
                    quarantined: false,
                    recovery: None,
                    recovery_result: None,
                    deleted: false,
                    have: Vec::new(),
                })
                .queue
                .push(claims.jti.clone());
            next.per_job
                .executions
                .insert(claims.jti.clone(), execution);
            images::reserve_account(&mut next.per_job, &claims.jti);
            next.leases.insert(
                claims.jti.clone(),
                super::Lease {
                    claims: claims.clone(),
                    cache_write: false,
                    tenant: claims.jti.clone(),
                    state: super::LeaseState::Reserved,
                    delivered: false,
                    replacing: false,
                    started_at_ms: None,
                    ended_at_ms: None,
                    highest_seq: 0,
                    refusal: None,
                },
            );
            self.reserve_job_storage(&mut next, &disk_id, now_ms)?;
            // Identity, queue position, permits and storage debits become visible together,
            // before admission returns and before any launch authorization is returned.
            self.save(next)?;
            if reserve_memory {
                self.job_memory_reservations.insert(claims.jti.clone());
            } else {
                self.job_memory_waiters.insert(
                    claims.jti.clone(),
                    MemoryWaiter {
                        admission_order: self.next_connection,
                        ready_since_ms: None,
                    },
                );
            }
            if let Some(hinted_vm) = &claims.placement.vm {
                if hinted_vm != &disk_id {
                    tracing::info!(
                        jti = %claims.jti,
                        hinted_vm,
                        actual_disk = %disk_id,
                        "workspace_placement_hint_stale"
                    );
                }
            }
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
        self.begin_job_boot_at(job, self.observed_at_ms)
    }

    pub(crate) fn job_boot_disk_ready(&self, job: &str) -> bool {
        pending_boot(&self.snapshot, job)
            .is_some_and(|e| boot_disk_ready(&self.snapshot.per_job, &self.fork_holds, e))
    }

    pub(super) fn update_job_memory_waiters(&mut self) {
        let snapshot = &self.snapshot;
        let fork_holds = &self.fork_holds;
        self.job_memory_waiters.retain(|job, waiter| {
            let Some(execution) = pending_boot(snapshot, job) else {
                return false;
            };
            // A saved disk change can interrupt readiness between boot attempts.
            if waiter.ready_since_ms.is_some()
                && !boot_disk_ready(&snapshot.per_job, fork_holds, execution)
            {
                waiter.ready_since_ms = None;
            }
            true
        });
        if let Some(hold) = &self.job_memory_waiter_hold {
            let reason = match self.job_memory_waiters.get(&hold.job) {
                Some(waiter) if waiter.ready_since_ms.is_some() => return,
                Some(_) => "not_ready",
                None if snapshot
                    .per_job
                    .executions
                    .get(&hold.job)
                    .is_some_and(|e| e.boot_started && !e.ended) =>
                {
                    "booted"
                }
                None => "ended",
            };
            self.end_job_memory_waiter_hold(reason);
        }
    }

    fn end_job_memory_waiter_hold(&mut self, reason: &str) {
        let hold = self
            .job_memory_waiter_hold
            .take()
            .expect("active waiter hold");
        self.job_memory_reservations.remove(&hold.job);
        tracing::info!(jti = %hold.job, reason, held_ms = hold.started_at.elapsed().as_millis() as u64, "job_memory_hold_ended");
    }

    fn maybe_hold_job_memory(&mut self, job: &str, now_ms: u64, debit_mib: u64, free_mib: u64) {
        if self.job_memory_waiter_hold.is_some() {
            return;
        }
        let Some(waiter) = self.job_memory_waiters.get(job) else {
            return;
        };
        let Some(ready_since_ms) = waiter.ready_since_ms else {
            return;
        };
        let waited_ms = now_ms.saturating_sub(ready_since_ms);
        if waited_ms < WAITER_MEMORY_HOLD_AFTER_MS
            || self.job_memory_waiters.values().any(|other| {
                other.ready_since_ms.is_some_and(|since| {
                    (since, other.admission_order) < (ready_since_ms, waiter.admission_order)
                })
            })
        {
            return;
        }
        self.job_memory_reservations.insert(job.to_owned());
        self.job_memory_waiter_hold = Some(MemoryWaiterHold {
            job: job.to_owned(),
            started_at: std::time::Instant::now(),
        });
        tracing::info!(jti = %job, debit_mib, waited_ms, free_mib, "job_memory_hold_started");
    }

    pub fn begin_job_boot_at(
        &mut self,
        job: &str,
        now_ms: u64,
    ) -> Result<Option<Execution>, Error> {
        let debit_mib = self
            .snapshot
            .per_job
            .executions
            .get(job)
            .ok_or(Refusal::Auth)
            .map_err(Error::Refused)?
            .memory_debit_mib;
        if !self.job_boot_disk_ready(job) {
            if self
                .job_memory_waiters
                .get(job)
                .is_some_and(|waiter| waiter.ready_since_ms.is_some())
            {
                self.update_job_memory_waiters();
            }
            return Ok(None);
        }
        if let Some(waiter) = self.job_memory_waiters.get_mut(job) {
            waiter.ready_since_ms.get_or_insert(now_ms);
        }
        // Replace this claim's reservation with the boot debit under the host lock.
        // Other claims' reservations still count, including those not booting yet.
        let free_mib = self.job_free_mem_mib_except(now_ms, Some(job));
        if free_mib < debit_mib {
            self.maybe_hold_job_memory(job, now_ms, debit_mib, free_mib);
            return Ok(None);
        }
        let mut next = self.snapshot.clone();
        let execution = next.per_job.executions.get_mut(job).unwrap();
        execution.boot_started = true;
        execution.boot_started_at_ms = Some(now_ms);
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
        self.job_stopped_at(job, unclean, self.observed_at_ms)
    }

    pub fn job_stopped_at(&mut self, job: &str, unclean: bool, now_ms: u64) -> Result<(), Error> {
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
        execution.replay_reservation_bytes = None;
        // A virtual machine can exit before its synthetic terminal is committed. Retain
        // only the record allowance until replay is durable or quarantined.
        execution.final_replay_reservation_bytes = execution
            .replay_completion
            .is_none()
            .then_some(storage::REPLAY_RECORD_BYTES);
        let disk = next.per_job.disks.get_mut(&execution.disk_id).unwrap();
        if execution.runner_hello {
            disk.forked_from = None;
            disk.forked = false;
        }
        disk.queue.retain(|queued| queued != job);
        if disk.attached_job.as_deref() == Some(job) {
            disk.attached_job = None;
            disk.quarantined |= unclean;
            if unclean {
                disk.have.clear();
                execution.have_invalidated = true;
            }
        }
        if let Some(storage) = &mut disk.storage {
            storage.last_used_ms = storage.last_used_ms.max(now_ms);
            if disk.queue.is_empty() && disk.attached_job.is_none() && disk.recovery.is_none() {
                storage.growth_reservation_bytes = 0;
            }
        }
        let evidence = execution.evidence.get_or_insert_with(Default::default);
        // This path also releases claims whose helper launch never happened.
        // Those claims cannot produce a receipt and never consumed VM egress.
        if evidence.exit.is_none() && evidence.gone_at_ms.is_none() {
            evidence.evidence_expired = true;
            super::settlement::finalize_usage(&mut next, job);
        }
        self.save(next)
    }

    /// A stopped VM cannot prove a delivered command never ran. Keep delivery
    /// evidence and use the lost rule even when no Started frame was observed.
    pub fn job_boot_failed(&mut self, job: &str, now_ms: u64) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get_mut(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        let evidence = execution.evidence.get_or_insert_with(Default::default);
        if evidence.winner.is_some() {
            return Ok(());
        }
        evidence.winner = Some(super::settlement::Winner::HostStop);
        super::settlement::settle_failure(&mut next, job, now_ms);
        super::settlement::defer_usage(&mut next, job);
        self.save(next)
    }

    pub fn job_refused(
        &mut self,
        job: &str,
        frame: serde_json::Value,
        now_ms: u64,
    ) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        have::transfer_terminal(&mut next, job, &frame);
        if next
            .leases
            .get(job)
            .is_some_and(|lease| lease.state != super::LeaseState::Ended && !lease.delivered)
        {
            super::end_zero(&mut next, job, now_ms, Some(Refusal::InternalFailure))?;
            next.billing.entry(job.to_owned()).or_default().terminal = Some(frame);
            next.per_job
                .executions
                .get_mut(job)
                .unwrap()
                .evidence
                .get_or_insert_with(Default::default)
                .winner = Some(super::settlement::Winner::Guest);
            crate::billing::finalize_ready(&mut next, now_ms);
            super::settlement::defer_usage(&mut next, job);
            self.save(next)?;
        }
        Ok(())
    }

    /// A sole owner may cancel an upload it had to close under backpressure.
    /// Once any result or usage can have escaped the host, keep it immutable.
    pub(crate) fn cancel_interrupted_upload(&mut self, job: &str) -> Result<bool, Error> {
        let Some(execution) = self.snapshot.per_job.executions.get(job) else {
            return Ok(false);
        };
        let lease = &self.snapshot.leases[job];
        let Some(billing) = self.snapshot.billing.get(job) else {
            return Ok(false);
        };
        if lease.state != super::LeaseState::Ended
            || execution.boot_started
            || lease.delivered
            || lease.started_at_ms.is_some()
            || billing.delivered
            || billing.enqueued
            || !self.snapshot.zero_usage.contains_key(job)
            || !billing.terminal.as_ref().is_some_and(|terminal| {
                terminal["ran"] == "none"
                    && terminal["outcome"]["type"] == "refused_before_start"
                    && terminal["outcome"]["reason"] == "transfer_interrupted"
            })
        {
            return Ok(false);
        }
        let mut next = self.snapshot.clone();
        let terminal = next
            .billing
            .get_mut(job)
            .unwrap()
            .terminal
            .as_mut()
            .unwrap();
        terminal["outcome"] = serde_json::json!({"type":"cancelled"});
        terminal.as_object_mut().unwrap().remove("refusal_detail");
        self.save(next)?;
        Ok(true)
    }
}

fn pending_boot<'a>(snapshot: &'a super::Snapshot, job: &str) -> Option<&'a Execution> {
    // Cancellation ends the lease before execution cleanup finishes, so both
    // records must still permit a boot when authorization is issued.
    let lease = snapshot.leases.get(job)?;
    let execution = snapshot.per_job.executions.get(job)?;
    (lease.state != super::LeaseState::Ended && !execution.boot_started && !execution.ended)
        .then_some(execution)
}

fn boot_disk_ready(
    ledger: &Ledger,
    fork_holds: &std::collections::BTreeMap<String, forks::Hold>,
    execution: &Execution,
) -> bool {
    let disk = &ledger.disks[&execution.disk_id];
    !fork_holds.contains_key(&execution.disk_id)
        && !disk.quarantined
        && disk.attached_job.is_none()
        && disk.queue.first() == Some(&execution.claims.jti)
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
