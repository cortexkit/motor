//! Host observations, durable settlement records and the serialized broker outbox.
use crate::state::{Error, Host, LeaseState, Refusal, Snapshot};
use motor_lease::{Heartbeat, HostHeaders, JobState, RunningJob, Usage, UsageOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DELIVERY_GRACE_MS: u64 = 60_000;
pub const OUTBOX_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// How many rejected requests the journal keeps for diagnosis; older ones are dropped.
pub const REJECTED_OUTBOX_LIMIT: usize = 32;
/// How long a finished job's billing and lease records are kept: 13 months,
/// counted as 395 days (365 + 30). The public privacy page promises that host
/// billing records are deleted 13 months after the job finished; a full year
/// of history plus one month leaves the last billing period of that year time
/// to close and be disputed before its evidence goes.
pub const BILLING_RETENTION_MS: u64 = 395 * 24 * 60 * 60 * 1000;
/// At most this many jobs are deleted per retention pass, so one pass after a
/// long outage never holds the host-state mutex (and with it admission) for a
/// large rewrite. Later passes delete the rest.
pub const RETENTION_BATCH: usize = 256;
/// How often the host tick runs a retention pass while nothing is left over.
pub const RETENTION_INTERVAL_MS: u64 = 10 * 60 * 1000;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JobBilling {
    pub started_live: bool,
    pub terminal_live: bool,
    pub ended_observed_ms: Option<u64>,
    pub terminal_received_ms: Option<u64>,
    pub terminal: Option<Value>,
    pub cancelled_at_cap: bool,
    pub host_settled: bool,
    pub synthesized_output: Option<Value>,
    pub hosted_terminal: Option<Value>,
    pub usage: Option<Usage>,
    pub enqueued: bool,
    pub delivered: bool,
    pub overlapping: bool,
    pub memory_peak_bytes: u64,
    pub metrics_observed: bool,
    pub egress_start: Option<u64>,
    pub egress_end: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Request {
    pub path: String,
    pub body: Vec<u8>,
    pub created_ms: u64,
    pub job: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    pub memory_bytes: u64,
    pub egress_bytes: u64,
}

/// No live end and no live start means the guest ran wholly outside observation.
/// Guest-supplied clocks and durations are deliberately not inputs to settlement.
pub(crate) fn finalize_ready(next: &mut Snapshot, now_ms: u64) {
    for (job, lease) in &next.leases {
        if lease.state != LeaseState::Ended {
            continue;
        }
        let b = next.billing.entry(job.clone()).or_default();
        if b.usage.is_some() {
            continue;
        }
        let zero = next.zero_usage.get(job);
        if b.terminal.is_none() && zero.is_none() {
            continue;
        }
        let observed_end = *b.ended_observed_ms.get_or_insert(now_ms);
        if lease.started_at_ms.is_some()
            && !b.overlapping
            && !b.metrics_observed
            && (b.started_live || b.terminal_live)
        {
            continue;
        }
        let start = lease.started_at_ms.unwrap_or(observed_end);
        let mut end = lease
            .ended_at_ms
            .unwrap_or(observed_end)
            .min(start.saturating_add(lease.claims.max_s.saturating_mul(1000)));
        if end < start || (!b.started_live && !b.terminal_live) {
            end = start;
        }
        let outcome = if b.host_settled {
            UsageOutcome::Lost
        } else if lease.started_at_ms.is_none() {
            UsageOutcome::Cancelled
        } else if b.cancelled_at_cap
            || b.terminal
                .as_ref()
                .is_some_and(|t| t["killed"] == "deadline")
        {
            UsageOutcome::Timeout
        } else if b.terminal.as_ref().is_some_and(|t| t["killed"] == "cancel") {
            UsageOutcome::Cancelled
        } else {
            match b
                .terminal
                .as_ref()
                .and_then(|t| t["outcome"]["type"].as_str())
            {
                Some("exit") => UsageOutcome::Exit,
                Some("signal") => UsageOutcome::Signal,
                _ => UsageOutcome::Cancelled,
            }
        };
        let usage = Usage {
            job_id: job.clone(),
            vcpus: lease.claims.vcpus,
            started_at_ms: start,
            ended_at_ms: end,
            mem_peak_mib: (!b.overlapping).then_some(b.memory_peak_bytes.div_ceil(1_048_576)),
            egress_bytes: (!b.overlapping).then_some(
                b.egress_end
                    .saturating_sub(b.egress_start.unwrap_or(b.egress_end)),
            ),
            outcome,
        };
        let billed = u64::from(usage.vcpus)
            .saturating_mul(end.saturating_sub(start))
            .min(
                u64::from(usage.vcpus)
                    .saturating_mul(lease.claims.max_s.max(10))
                    .saturating_mul(1000),
            );
        b.hosted_terminal = Some(json!({"type":"hosted_terminal", "job_id":job,
            "placement":format!("first_party/{}", lease.claims.placement.host),
            "billed_vcpu_ms":billed, "max_runtime_s":lease.claims.max_s}));
        b.usage = Some(usage);
    }
}

pub(crate) fn mark_overlap(next: &mut Snapshot) {
    for (job, lease) in &next.leases {
        if lease.state == LeaseState::Started
            && next.leases.iter().any(|(other, l)| {
                other != job
                    && l.tenant == lease.tenant
                    && matches!(
                        l.state,
                        LeaseState::Reserved | LeaseState::Admitted | LeaseState::Started
                    )
            })
        {
            next.billing.entry(job.clone()).or_default().overlapping = true;
        }
    }
}

impl Host {
    pub fn start_billing(&mut self, now_ms: u64) -> Result<(), Error> {
        self.restart_ms = now_ms;
        self.prior_alive_ms = self.snapshot.last_alive;
        if self.prior_alive_ms > 0 {
            tracing::warn!(
                last_alive = self.prior_alive_ms,
                restart = now_ms,
                "host_observation_outage"
            );
        }
        let mut next = self.snapshot.clone();
        next.counter = next
            .counter
            .saturating_add(1)
            .max(now_ms.saturating_mul(1000));
        self.save(next)
    }
    pub fn reserve_counter(&mut self) -> Result<u64, Error> {
        let mut next = self.snapshot.clone();
        next.counter = next
            .counter
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("host counter exhausted"))?;
        let counter = next.counter;
        self.save(next)?;
        Ok(counter)
    }
    pub fn sample(&mut self, tenant: &str, metrics: Metrics) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        for (job, lease) in &next.leases {
            if lease.tenant == tenant
                && (lease.state == LeaseState::Started
                    || (lease.state == LeaseState::Ended
                        && next.billing.get(job).is_some_and(|b| b.usage.is_none())))
            {
                let b = next.billing.entry(job.clone()).or_default();
                b.metrics_observed = true;
                b.memory_peak_bytes = b.memory_peak_bytes.max(metrics.memory_bytes);
                b.egress_start.get_or_insert(metrics.egress_bytes);
                b.egress_end = metrics.egress_bytes;
            }
        }
        self.save(next)
    }
    pub fn receive_terminal(&mut self, job: &str, frame: Value, now_ms: u64) -> Result<(), Error> {
        self.receive_terminal_observed(job, frame, now_ms, true)
    }
    pub fn receive_terminal_observed(
        &mut self,
        job: &str,
        frame: Value,
        now_ms: u64,
        live: bool,
    ) -> Result<(), Error> {
        if self.provider_managed {
            return Ok(());
        }
        let mut next = self.snapshot.clone();
        let b = next.billing.entry(job.to_owned()).or_default();
        if b.host_settled || b.terminal.is_some() {
            return Ok(());
        }
        b.terminal_received_ms = Some(now_ms);
        b.terminal_live |= live;
        b.terminal = Some(frame);
        finalize_ready(&mut next, now_ms);
        self.save(next)
    }
    pub fn billing_tick(&mut self, now_ms: u64) -> Result<(), Error> {
        if self.provider_managed {
            // Provider usage is measured outside this sandbox. Only cancellation
            // is local: a disconnected requester must not disable the run cap.
            let mut next = self.snapshot.clone();
            for (job, lease) in &next.leases {
                if lease.state == LeaseState::Started
                    && lease.started_at_ms.is_some_and(|started| {
                        now_ms >= started.saturating_add(lease.claims.max_s.saturating_mul(1000))
                    })
                {
                    next.pending_cancels
                        .insert(job.clone(), lease.tenant.clone());
                }
            }
            if next.pending_cancels != self.snapshot.pending_cancels {
                self.save(next)?;
            }
            return Ok(());
        }
        let mut next = self.snapshot.clone();
        for (job, lease) in &mut next.leases {
            let b = next.billing.entry(job.clone()).or_default();
            if lease.state == LeaseState::Started {
                let cap = lease
                    .started_at_ms
                    .unwrap_or(now_ms)
                    .saturating_add(lease.claims.max_s.saturating_mul(1000));
                if let Some(receipt) = b.terminal_received_ms {
                    if now_ms >= receipt.saturating_add(30_000) {
                        lease.state = LeaseState::Ended;
                        lease.ended_at_ms = Some(receipt);
                        b.ended_observed_ms = Some(receipt);
                    }
                } else if now_ms >= cap {
                    if !b.cancelled_at_cap {
                        b.cancelled_at_cap = true;
                        next.pending_cancels
                            .insert(job.clone(), lease.tenant.clone());
                    }
                    if now_ms >= cap.saturating_add(60_000) {
                        settle_lost(lease, b, cap, now_ms);
                        next.vms
                            .entry(lease.tenant.clone())
                            .or_default()
                            .stop_pending = true;
                    }
                }
            }
        }
        finalize_ready(&mut next, now_ms);
        enqueue_usage(&mut next, now_ms);
        if serde_json::to_vec(&next).ok() != serde_json::to_vec(&self.snapshot).ok() {
            self.save(next)?;
        }
        Ok(())
    }
    pub fn lose_tenant(&mut self, tenant: &str, now_ms: u64) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        for (job, lease) in &mut next.leases {
            if lease.tenant == tenant && lease.state != LeaseState::Ended {
                settle_lost(
                    lease,
                    next.billing.entry(job.clone()).or_default(),
                    now_ms,
                    now_ms,
                );
            }
        }
        finalize_ready(&mut next, now_ms);
        self.save(next)?;
        self.guest_unready(tenant);
        Ok(())
    }
    pub fn stops_due(&self) -> Vec<String> {
        self.snapshot
            .vms
            .iter()
            .filter(|(tenant, vm)| {
                vm.stop_pending
                    && !self.snapshot.leases.iter().any(|(job, l)| {
                        &l.tenant == *tenant
                            && (l.state != LeaseState::Ended
                                || (!self.snapshot.zero_usage.contains_key(job)
                                    && self
                                        .snapshot
                                        .billing
                                        .get(job)
                                        .is_none_or(|b| b.terminal.is_none())))
                    })
            })
            .map(|(tenant, _)| tenant.clone())
            .collect()
    }
    pub fn stopped(&mut self, tenant: &str) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let vm = next.vms.entry(tenant.to_owned()).or_default();
        vm.stop_pending = false;
        vm.initialized_generation = None;
        next.pending_cancels.retain(|_, t| t != tenant);
        self.save(next)?;
        self.guest_unready(tenant);
        Ok(())
    }
    pub fn terminal_delivered(&mut self, job: &str, now_ms: u64) -> Result<(), Error> {
        if self.provider_managed {
            return Ok(());
        }
        let mut next = self.snapshot.clone();
        next.billing.entry(job.to_owned()).or_default().delivered = true;
        enqueue_usage(&mut next, now_ms);
        self.save(next)
    }
    pub fn host_settled_frames(&self, job: &str, from_seq: u64) -> Option<Vec<Value>> {
        let l = self.snapshot.leases.get(job)?;
        let b = self.snapshot.billing.get(job)?;
        if !b.host_settled {
            return None;
        }
        let mut frames = Vec::new();
        let seq = l.highest_seq.saturating_add(1);
        if from_seq <= seq {
            frames.push(b.synthesized_output.clone()?);
        }
        frames.push(b.terminal.clone()?);
        frames.push(b.hosted_terminal.clone()?);
        Some(frames)
    }
    pub fn heartbeat(&self, now_ms: u64, free_mem_mib: u64, healthy: bool) -> Heartbeat {
        Heartbeat {
            // While host storage is low, new leases are refused, so no free
            // vCPUs are advertised and the broker places no new work here.
            free_vcpus: if self.storage_refusing() {
                0
            } else {
                self.tenants
                    .values()
                    .map(|t| self.tenant_free_vcpus(t, now_ms))
                    .max()
                    .unwrap_or(0)
            },
            free_mem_mib,
            queued: self
                .snapshot
                .leases
                .values()
                .filter(|l| l.state == LeaseState::Reserved && !l.delivered)
                .count() as u64,
            warm: self.warm_workspaces(now_ms),
            running: self
                .snapshot
                .leases
                .iter()
                .filter(|(_, l)| l.state != LeaseState::Ended)
                .map(|(job, l)| RunningJob {
                    job_id: job.clone(),
                    state: match l.state {
                        LeaseState::Started => JobState::Started,
                        LeaseState::Admitted => JobState::Admitted,
                        _ => JobState::Reserved,
                    },
                    elapsed_ms: l.started_at_ms.map_or(0, |s| {
                        now_ms
                            .saturating_sub(s)
                            .min(l.claims.max_s.saturating_mul(1000))
                    }),
                })
                .collect(),
            healthy,
        }
    }
    pub fn queue_heartbeat(
        &mut self,
        host_id: &str,
        heartbeat: &Heartbeat,
        now_ms: u64,
    ) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        // Replace only an unsigned queued heartbeat; the sender owns its in-flight
        // request separately, so a retry always keeps the original body.
        next.outbox.retain(|r| r.job.is_some());
        next.outbox.push(Request {
            path: format!("/v1/hosts/{host_id}/heartbeat"),
            body: serde_json::to_vec(heartbeat).map_err(std::io::Error::other)?,
            created_ms: now_ms,
            job: None,
        });
        next.last_alive = now_ms;
        self.save(next)
    }
    pub fn expire_outbox(&mut self, now_ms: u64) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        while next
            .outbox
            .first()
            .is_some_and(|r| now_ms.saturating_sub(r.created_ms) >= OUTBOX_MAX_AGE_MS)
        {
            let entry = next.outbox.remove(0);
            tracing::warn!(job_id = entry.job, "usage_outbox_expired");
            next.expired_outbox.push(entry);
        }
        if next.expired_outbox.len() != self.snapshot.expired_outbox.len() {
            self.save(next)?;
        }
        Ok(())
    }
    pub fn finish_request(
        &mut self,
        request: &Request,
        status: u16,
        reply: &Value,
    ) -> Result<bool, Error> {
        if (200..300).contains(&status) && request.job.is_some() {
            let parsed = serde_json::from_value::<motor_lease::UsageReply>(reply.clone());
            if parsed.as_ref().is_err()
                || parsed
                    .as_ref()
                    .is_ok_and(|r| Some(&r.job_id) != request.job.as_ref())
            {
                return Ok(false);
            }
        }
        let final_error = matches!(
            (status, reply["error"].as_str()),
            (409, Some("usage_conflict" | "job_settled" | "job_conflict"))
                | (404, Some("job_unknown"))
        );
        if !(200..300).contains(&status) && !final_error {
            return Ok(false);
        }
        if let Some(job) = &request.job {
            let local = self
                .snapshot
                .billing
                .get(job)
                .and_then(|b| b.hosted_terminal.as_ref())
                .and_then(|h| h["billed_vcpu_ms"].as_u64());
            let moved = status == 409 && reply["error"] == "job_conflict";
            if moved {
                tracing::info!(job_id = job, "usage_ignored_after_replace");
            } else if status == 409
                || ((200..300).contains(&status) && local != reply["settled_vcpu_ms"].as_u64())
            {
                tracing::error!(
                    job_id = job,
                    host_vcpu_ms = local,
                    broker_vcpu_ms = reply["settled_vcpu_ms"].as_u64(),
                    "usage_billing_diverged"
                );
            }
            if final_error && !moved {
                tracing::warn!(
                    job_id = job,
                    status,
                    error = reply["error"].as_str(),
                    "usage_reconciliation_required"
                );
            }
        }
        let mut next = self.snapshot.clone();
        if (200..300).contains(&status) && request.path.ends_with("/heartbeat") {
            for job in reply["ignored"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if let Some(lease) = next.leases.get_mut(job) {
                    // Keep the revocation with the lease, beyond the run token's
                    // lifetime, so a restart cannot admit the old placement.
                    lease.refusal = Some(Refusal::Auth);
                    if lease.state != LeaseState::Ended {
                        next.pending_cancels
                            .insert(job.to_owned(), lease.tenant.clone());
                    }
                }
            }
        }
        if let Some(index) = next
            .outbox
            .iter()
            .position(|r| r.path == request.path && r.body == request.body)
        {
            next.outbox.remove(index);
        }
        self.save(next)?;
        Ok(true)
    }
    /// Runs a retention pass when one is due: every `RETENTION_INTERVAL_MS`,
    /// or again on the next tick while a full batch was deleted, so a backlog
    /// drains in bounded steps.
    pub fn retention_tick(&mut self, now_ms: u64) -> Result<usize, Error> {
        if now_ms < self.retention_due_ms {
            return Ok(0);
        }
        let pruned = self.prune_retention(now_ms)?;
        self.retention_due_ms = if pruned >= RETENTION_BATCH {
            now_ms
        } else {
            now_ms.saturating_add(RETENTION_INTERVAL_MS)
        };
        Ok(pruned)
    }
    /// Deletes the billing and lease records of up to `RETENTION_BATCH` jobs
    /// that finished more than `BILLING_RETENTION_MS` ago, and returns how
    /// many jobs it deleted. A job's records are its lease (which carries the
    /// account identifier), billing state, no-start report, pending cancel,
    /// and every outbox, set-aside or rejected usage request naming it.
    /// Expired replay nonces, job-less set-aside requests (heartbeats) and
    /// closure records of tenants removed longer ago than the retention
    /// period go in the same pass.
    ///
    /// A job is kept, whatever its age, while it is unsettled: its lease has
    /// not ended, its usage report is computed but not yet queued, or a queued
    /// report is still inside its `OUTBOX_MAX_AGE_MS` retry window. It is also
    /// kept while its job token has not expired, because the lease record is
    /// what refuses a replay of that token.
    ///
    /// The deletion is one ordinary snapshot save, so it is exactly as
    /// crash-safe as every other state change: the journal publishes the
    /// whole pruned snapshot with a synced write and rename, or nothing.
    pub fn prune_retention(&mut self, now_ms: u64) -> Result<usize, Error> {
        // Finished strictly before this instant means more than the
        // retention period ago. Before the epoch plus one period nothing is
        // old enough, and the saturated zero matches no finish time.
        let cutoff = now_ms.saturating_sub(BILLING_RETENTION_MS);
        let snapshot = &self.snapshot;
        let retrying: std::collections::BTreeSet<&str> = snapshot
            .outbox
            .iter()
            .filter(|r| now_ms.saturating_sub(r.created_ms) < OUTBOX_MAX_AGE_MS)
            .filter_map(|r| r.job.as_deref())
            .collect();
        let finished_at = |job: &str| -> Option<u64> {
            let lease = snapshot.leases.get(job);
            if let Some(lease) = lease {
                if lease.state != LeaseState::Ended
                    || lease.claims.exp.saturating_mul(1000) > now_ms
                {
                    return None;
                }
            }
            let billing = snapshot.billing.get(job);
            if retrying.contains(job) || billing.is_some_and(|b| b.usage.is_some() && !b.enqueued) {
                return None;
            }
            [
                lease.and_then(|l| l.ended_at_ms),
                billing.and_then(|b| b.ended_observed_ms),
                billing.and_then(|b| b.terminal_received_ms),
                snapshot.zero_usage.get(job).map(|u| u.ended_at_ms),
            ]
            .into_iter()
            .flatten()
            .max()
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut expired = std::collections::BTreeSet::new();
        for job in snapshot
            .leases
            .keys()
            .chain(snapshot.billing.keys())
            .chain(snapshot.zero_usage.keys())
        {
            if expired.len() >= RETENTION_BATCH {
                break;
            }
            if seen.insert(job.as_str()) && finished_at(job).is_some_and(|end| end < cutoff) {
                expired.insert(job.clone());
            }
        }
        // A set-aside request goes with its job, or on its own age once no
        // record of its job is left (a heartbeat, or a job pruned earlier).
        let orphaned = |r: &Request| {
            r.job.as_ref().is_none_or(|job| {
                !snapshot.leases.contains_key(job)
                    && !snapshot.billing.contains_key(job)
                    && !snapshot.zero_usage.contains_key(job)
            })
        };
        let keep = |r: &Request| {
            !r.job.as_ref().is_some_and(|job| expired.contains(job))
                && !(orphaned(r) && r.created_ms < cutoff)
        };
        let now_s = now_ms / 1000;
        let mut next = Snapshot {
            leases: snapshot.leases.clone(),
            nonces: snapshot.nonces.clone(),
            vms: snapshot.vms.clone(),
            zero_usage: snapshot.zero_usage.clone(),
            pending_cancels: snapshot.pending_cancels.clone(),
            billing: snapshot.billing.clone(),
            outbox: snapshot
                .outbox
                .iter()
                .filter(|r| !r.job.as_ref().is_some_and(|job| expired.contains(job)))
                .cloned()
                .collect(),
            expired_outbox: snapshot
                .expired_outbox
                .iter()
                .filter(|r| keep(r))
                .cloned()
                .collect(),
            rejected_outbox: snapshot
                .rejected_outbox
                .iter()
                .filter(|r| keep(r))
                .cloned()
                .collect(),
            counter: snapshot.counter,
            last_alive: snapshot.last_alive,
            closed: snapshot.closed.clone(),
        };
        next.nonces.retain(|_, until| *until >= now_s);
        // A removed tenant's closure record names its account; it goes on
        // the same schedule as that account's billing records.
        next.closed
            .retain(|_, c| c.removed_ms.is_none_or(|removed| removed >= cutoff));
        for job in &expired {
            next.leases.remove(job);
            next.billing.remove(job);
            next.zero_usage.remove(job);
            next.pending_cancels.remove(job);
        }
        let changed = !expired.is_empty()
            || next.nonces.len() != snapshot.nonces.len()
            || next.closed.len() != snapshot.closed.len()
            || next.outbox.len() != snapshot.outbox.len()
            || next.expired_outbox.len() != snapshot.expired_outbox.len()
            || next.rejected_outbox.len() != snapshot.rejected_outbox.len();
        if changed {
            self.save(next)?;
            tracing::info!(jobs = expired.len(), "billing_records_pruned");
        }
        Ok(expired.len())
    }
    /// Moves a request the broker can never accept out of the outbox into
    /// `rejected_outbox`, so it is kept for diagnosis but never resent.
    pub fn reject_request(&mut self, request: &Request) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        next.outbox
            .retain(|r| r.path != request.path || r.body != request.body);
        next.rejected_outbox.push(request.clone());
        let excess = next
            .rejected_outbox
            .len()
            .saturating_sub(REJECTED_OUTBOX_LIMIT);
        next.rejected_outbox.drain(..excess);
        self.save(next)
    }
}

fn settle_lost(lease: &mut crate::state::Lease, b: &mut JobBilling, end: u64, observed: u64) {
    lease.state = LeaseState::Ended;
    lease.ended_at_ms = Some(end);
    b.ended_observed_ms = Some(observed);
    b.terminal_live = true;
    b.host_settled = true;
    let seq = lease.highest_seq.saturating_add(1);
    b.synthesized_output = Some(
        json!({"type":"output", "seq":seq, "stream":"stdout", "bytes":"", "truncated_before_seq":seq}),
    );
    b.terminal = Some(
        json!({"type":"terminal", "job_id":lease.claims.jti, "outcome":{"type":"outcome_unknown"},
        "wall_ms":0, "queue_wait_ms":0, "bundle_bytes":0, "ran":"remote", "tree_hash":null, "workspace_changes":null}),
    );
}
fn enqueue_usage(next: &mut Snapshot, now_ms: u64) {
    for (job, b) in &mut next.billing {
        let Some(usage) = b.usage.as_ref() else {
            continue;
        };
        if !b.enqueued
            && (b.delivered
                || b.ended_observed_ms
                    .is_some_and(|e| now_ms >= e.saturating_add(DELIVERY_GRACE_MS)))
        {
            let host = &next.leases[job].claims.placement.host;
            next.outbox.push(Request {
                path: format!("/v1/hosts/{host}/usage"),
                body: serde_json::to_vec(usage).expect("usage serialization"),
                created_ms: now_ms,
                job: Some(job.clone()),
            });
            b.enqueued = true;
        }
    }
}

pub fn sign_request(
    host: &mut Host,
    host_id: &str,
    request: &Request,
    now_ms: u64,
    key: &ed25519_dalek::SigningKey,
) -> anyhow::Result<HostHeaders> {
    let counter = host.reserve_counter()?;
    Ok(HostHeaders::sign(
        host_id,
        "POST",
        &request.path,
        &request.body,
        now_ms,
        counter,
        key,
    )?)
}
