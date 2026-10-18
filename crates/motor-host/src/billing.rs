//! Host observations, durable settlement records and the serialized broker outbox.
use crate::state::{Error, Host, LeaseState, Snapshot};
use motor_lease::{Heartbeat, HostHeaders, JobState, RunningJob, Usage, UsageOutcome, WarmVm};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DELIVERY_GRACE_MS: u64 = 60_000;
pub const OUTBOX_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

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
            free_vcpus: self
                .tenants
                .values()
                .map(|t| u64::from(t.vcpu_ceiling.saturating_sub(self.share(&t.tenant, now_ms))))
                .sum(),
            free_mem_mib,
            queued: self
                .snapshot
                .leases
                .values()
                .filter(|l| l.state == LeaseState::Reserved && !l.delivered)
                .count() as u64,
            warm: self
                .tenants
                .iter()
                .filter(|(_, t)| {
                    self.snapshot
                        .vms
                        .get(&t.tenant)
                        .is_some_and(|v| v.initialized_generation.is_some())
                })
                .map(|(vm, t)| WarmVm {
                    tenant: t.tenant.clone(),
                    workspace_key_hash: String::new(),
                    vm: vm.clone(),
                })
                .collect(),
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
            (409, Some("usage_conflict" | "job_settled")) | (404, Some("job_unknown"))
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
            if status == 409
                || ((200..300).contains(&status) && local != reply["settled_vcpu_ms"].as_u64())
            {
                tracing::error!(
                    job_id = job,
                    host_vcpu_ms = local,
                    broker_vcpu_ms = reply["settled_vcpu_ms"].as_u64(),
                    "usage_billing_diverged"
                );
            }
            if final_error {
                tracing::warn!(
                    job_id = job,
                    status,
                    error = reply["error"].as_str(),
                    "usage_reconciliation_required"
                );
            }
        }
        let mut next = self.snapshot.clone();
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
