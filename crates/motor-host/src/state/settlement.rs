//! Durable first-writer settlement and helper exit receipts for job VMs.
use super::{Error, Host, LeaseState, Refusal, Snapshot};
use motor_lease::{OomKill, Usage, UsageOutcome};
use motor_vm_helper::job::ExitReport;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Evidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner: Option<Winner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Stop>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<Receipt>,
    #[serde(skip_serializing_if = "is_false")]
    pub acknowledged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gone_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<motor_protocol::MemorySample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unclean_stop: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_inputs: Option<UsageInputs>,
    #[serde(skip_serializing_if = "is_false")]
    pub evidence_expired: bool,
}
fn is_false(value: &bool) -> bool {
    !value
}

/// Outcome and observed measurements frozen with the winner. Egress is supplied
/// only by the helper receipt before the single broker usage body is built.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct UsageInputs {
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub outcome: UsageOutcome,
    pub mem_peak_mib: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "nullable_measurement"
    )]
    pub mem_working_peak_mib: Option<Option<u64>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "nullable_measurement"
    )]
    pub mem_over_mib_ms: Option<Option<u64>>,
    pub oom_kill: Option<OomKill>,
}
fn nullable_measurement<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<u64>>, D::Error> {
    Option::<u64>::deserialize(d).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Winner {
    Guest,
    HostStop,
    Exit,
    EvidenceExpired,
}
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopCause {
    HostPressure,
    HostOom,
}
impl StopCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostPressure => "host_pressure",
            Self::HostOom => "host_oom",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Stop {
    pub cause: StopCause,
    pub at_ms: u64,
}
// Durable records tolerate later field additions.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Receipt {
    pub job_id: String,
    pub oom_kill: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_bytes: Option<u64>,
}
impl Receipt {
    pub fn report(&self) -> ExitReport {
        ExitReport {
            job_id: self.job_id.clone(),
            oom_kill: self.oom_kill,
            egress_bytes: self.egress_bytes,
        }
    }
}

impl Host {
    pub fn job_started(&mut self, job: &str, now_ms: u64) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if execution
            .evidence
            .as_ref()
            .is_some_and(|e| e.winner.is_some())
        {
            return Ok(());
        }
        let lease = next
            .leases
            .get_mut(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if !lease.delivered || lease.state == LeaseState::Ended {
            return Err(Error::Refused(Refusal::Auth));
        }
        lease.state = LeaseState::Started;
        lease.started_at_ms.get_or_insert(now_ms);
        next.billing.entry(job.into()).or_default().started_live = true;
        self.save(next)
    }
    pub fn job_stop_intent(&mut self, job: &str, unclean: bool) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let e = next
            .per_job
            .executions
            .get_mut(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        e.evidence
            .get_or_insert_with(Default::default)
            .unclean_stop
            .get_or_insert(unclean);
        self.save(next)
    }
    pub fn job_memory_sample(&mut self, sample: motor_protocol::MemorySample) -> Result<(), Error> {
        let job = sample.jti.to_string();
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get_mut(&job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        let evidence = execution.evidence.get_or_insert_with(Default::default);
        if evidence.winner.is_some() {
            return Ok(());
        }
        evidence.memory = Some(sample);
        self.save(next)
    }

    /// The caller must commit this record before asking the helper to stop.
    pub fn job_host_stop(&mut self, job: &str, cause: StopCause, now_ms: u64) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get_mut(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        let evidence = execution.evidence.get_or_insert_with(Default::default);
        if evidence.stop.is_none() {
            evidence.stop = Some(Stop {
                cause,
                at_ms: now_ms,
            });
        }
        if evidence.winner.is_none() {
            evidence.winner = Some(Winner::HostStop);
            settle_stop(&mut next, job, cause, now_ms);
            defer_usage(&mut next, job);
        }
        self.save(next)?;
        tracing::warn!(jti = job, cause = cause.as_str(), "job_host_stop");
        Ok(())
    }

    /// Sync the full receipt and settlement together before acknowledging the helper.
    pub fn job_exit_received(&mut self, report: &ExitReport, now_ms: u64) -> Result<bool, Error> {
        let mut next = self.snapshot.clone();
        let Some(execution) = next.per_job.executions.get_mut(&report.job_id) else {
            return Ok(false);
        };
        let evidence = execution.evidence.get_or_insert_with(Default::default);
        if let Some(receipt) = &evidence.exit {
            return Ok(receipt.report() == *report);
        }
        evidence.exit = Some(Receipt {
            job_id: report.job_id.clone(),
            oom_kill: report.oom_kill,
            egress_bytes: report.egress_bytes,
        });
        if report.oom_kill > 0 {
            if evidence.stop.is_none() {
                evidence.stop = Some(Stop {
                    cause: StopCause::HostOom,
                    at_ms: now_ms,
                });
            }
            if evidence.winner.is_none() {
                evidence.winner = Some(Winner::HostStop);
                settle_stop(&mut next, &report.job_id, StopCause::HostOom, now_ms);
            }
        } else if evidence.winner.is_none() {
            evidence.winner = Some(Winner::Exit);
            settle_failure(&mut next, &report.job_id, now_ms);
        }
        defer_usage(&mut next, &report.job_id);
        finalize_usage(&mut next, &report.job_id);
        self.save(next)?;
        if report.oom_kill > 0 {
            tracing::warn!(
                jti = report.job_id,
                cause = "host_oom",
                oom_kill = report.oom_kill,
                "job_host_stop"
            );
        }
        Ok(true)
    }

    pub fn job_exit_acknowledged(&mut self, report: &ExitReport) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let evidence = next
            .per_job
            .executions
            .get_mut(&report.job_id)
            .and_then(|e| e.evidence.as_mut())
            .ok_or(Error::Refused(Refusal::Auth))?;
        if evidence.exit.as_ref().is_none_or(|r| r.report() != *report) {
            return Err(Error::Refused(Refusal::Auth));
        }
        if evidence.acknowledged {
            return Ok(());
        }
        evidence.acknowledged = true;
        self.save(next)
    }

    /// Absence is timed from the first host observation, never from VM start or
    /// the previous host's last-alive clock. Persist it across another restart.
    pub fn job_vmm_gone(&mut self, job: &str, now_ms: u64) -> Result<(), Error> {
        let super::super::config::HostSettings::PerJob(settings) = &self.settings else {
            return Ok(());
        };
        let bound = settings.exit_evidence_bound_ms;
        let mut next = self.snapshot.clone();
        let execution = next
            .per_job
            .executions
            .get_mut(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        let evidence = execution.evidence.get_or_insert_with(Default::default);
        if evidence.exit.is_some() {
            return Ok(());
        }
        let first = *evidence.gone_at_ms.get_or_insert(now_ms);
        let expired = now_ms >= first.saturating_add(bound);
        let new_gap = expired && !evidence.evidence_expired;
        if new_gap {
            evidence.evidence_expired = true;
            if evidence.winner.is_none() {
                evidence.winner = Some(Winner::EvidenceExpired);
                settle_failure(&mut next, job, now_ms);
            }
            defer_usage(&mut next, job);
            finalize_usage(&mut next, job);
        }
        self.save(next)?;
        if new_gap {
            tracing::warn!(jti = job, first_gone_ms = first, "job_exit_evidence_gap");
        }
        Ok(())
    }

    pub(crate) fn receive_job_terminal(
        &mut self,
        job: &str,
        mut frame: Value,
        now_ms: u64,
        live: bool,
    ) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let evidence = next
            .per_job
            .executions
            .get_mut(job)
            .unwrap()
            .evidence
            .get_or_insert_with(Default::default);
        if evidence.winner.is_some() {
            return Ok(());
        }
        // A guest can report its own job limit, but cannot attest a host stop.
        if matches!(
            frame["oom_kill"]["cause"].as_str(),
            Some("host_pressure" | "host_oom")
        ) {
            frame
                .as_object_mut()
                .map(|object| object.remove("oom_kill"));
        }
        evidence.winner = Some(Winner::Guest);
        let lease = next
            .leases
            .get_mut(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        lease.state = LeaseState::Ended;
        lease.ended_at_ms = Some(now_ms);
        let b = next.billing.entry(job.into()).or_default();
        b.terminal_received_ms = Some(now_ms);
        b.ended_observed_ms = Some(now_ms);
        b.terminal_live = live;
        b.mem_peak_mib = frame["mem_peak_mib"].as_u64();
        b.mem_working_peak_mib = frame["mem_working_peak_mib"].as_u64();
        b.terminal = Some(frame);
        crate::billing::finalize_ready(&mut next, now_ms);
        defer_usage(&mut next, job);
        self.save(next)
    }
}

pub(crate) fn defer_usage(next: &mut Snapshot, job: &str) {
    if next.per_job.executions[job]
        .evidence
        .as_ref()
        .is_some_and(|e| e.usage_inputs.is_some())
    {
        return;
    }
    let b = next.billing.entry(job.into()).or_default();
    if let Some(usage) = b.usage.take() {
        let evidence = next
            .per_job
            .executions
            .get_mut(job)
            .unwrap()
            .evidence
            .get_or_insert_with(Default::default);
        evidence.usage_inputs.get_or_insert(UsageInputs {
            started_at_ms: usage.started_at_ms,
            ended_at_ms: usage.ended_at_ms,
            outcome: usage.outcome,
            mem_peak_mib: usage.mem_peak_mib,
            mem_working_peak_mib: usage.mem_working_peak_mib,
            mem_over_mib_ms: usage.mem_over_mib_ms,
            oom_kill: usage.oom_kill,
        });
    }
}

pub(crate) fn finalize_usage(next: &mut Snapshot, job: &str) {
    let Some(evidence) = next.per_job.executions[job].evidence.as_ref() else {
        return;
    };
    if evidence.exit.is_none() && !evidence.evidence_expired {
        return;
    }
    let Some(inputs) = evidence.usage_inputs.as_ref() else {
        return;
    };
    let b = next.billing.entry(job.into()).or_default();
    if b.usage.is_some() {
        return;
    }
    let claims = &next.leases[job].claims;
    b.usage = Some(Usage {
        job_id: job.into(),
        vcpus: claims.vcpus,
        started_at_ms: inputs.started_at_ms,
        ended_at_ms: inputs.ended_at_ms,
        outcome: inputs.outcome,
        mem_peak_mib: inputs.mem_peak_mib,
        mem_working_peak_mib: inputs.mem_working_peak_mib,
        mem_over_mib_ms: inputs.mem_over_mib_ms,
        oom_kill: inputs.oom_kill,
        egress_bytes: if evidence.evidence_expired {
            None
        } else {
            evidence.exit.as_ref().and_then(|r| r.egress_bytes)
        },
    });
}

fn settle_stop(next: &mut Snapshot, job: &str, cause: StopCause, now_ms: u64) {
    if next.leases[job].started_at_ms.is_none() {
        settle_failure(next, job, now_ms);
        return;
    }
    let start = next.leases[job].started_at_ms.unwrap();
    let sample = next.per_job.executions[job]
        .evidence
        .as_ref()
        .and_then(|e| e.memory.as_ref());
    let peak = sample.and_then(|s| s.mem_working_peak_mib);
    let over = sample.and_then(|s| s.mem_over_mib_ms).unwrap_or(None);
    let burst = next.leases[job].claims.mem_burst_mib.is_some();
    let b = next.billing.entry(job.into()).or_default();
    b.terminal = Some(
        json!({"type":"terminal","job_id":job,"outcome":{"type":"signal","signal":9},"wall_ms":now_ms.saturating_sub(start),"queue_wait_ms":0,"bundle_bytes":0,"ran":"remote","tree_hash":null,"workspace_changes":null,"oom_kill":{"cause":cause.as_str()},"mem_peak_mib":null,"mem_working_peak_mib":peak,"mem_over_mib_ms":burst.then_some(over)}),
    );
    if !burst {
        b.terminal
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("mem_over_mib_ms");
    }
    b.terminal_live = true;
    b.terminal_received_ms = Some(now_ms);
    b.ended_observed_ms = Some(now_ms);
    let lease = next.leases.get_mut(job).unwrap();
    lease.state = LeaseState::Ended;
    lease.ended_at_ms = Some(now_ms);
    let end = now_ms
        .max(start)
        .min(start.saturating_add(lease.claims.max_s.saturating_mul(1000)));
    b.usage = Some(Usage {
        job_id: job.into(),
        vcpus: lease.claims.vcpus,
        started_at_ms: start,
        ended_at_ms: end,
        outcome: UsageOutcome::Signal,
        mem_peak_mib: None,
        mem_working_peak_mib: Some(peak),
        mem_over_mib_ms: burst.then_some(over),
        egress_bytes: next.per_job.executions[job]
            .evidence
            .as_ref()
            .and_then(|e| e.exit.as_ref())
            .and_then(|r| r.egress_bytes),
        oom_kill: Some(match cause {
            StopCause::HostPressure => OomKill::HostPressure,
            StopCause::HostOom => OomKill::HostOom,
        }),
    });
    b.hosted_terminal = Some(
        json!({"type":"hosted_terminal","job_id":job,"placement":format!("first_party/{}",lease.claims.placement.host),"billed_vcpu_ms":u64::from(lease.claims.vcpus).saturating_mul(end.saturating_sub(start)),"max_runtime_s":lease.claims.max_s}),
    );
}

pub(crate) fn settle_failure(next: &mut Snapshot, job: &str, now_ms: u64) {
    let lease = next.leases.get_mut(job).unwrap();
    if !lease.delivered {
        super::end_zero(next, job, now_ms, Some(Refusal::InternalFailure)).expect("known lease");
        crate::billing::finalize_ready(next, now_ms);
        return;
    }
    let b = next.billing.entry(job.into()).or_default();
    crate::billing::settle_lost(lease, b, now_ms, now_ms);
    b.restart_pending = Some(false);
    let start = lease
        .started_at_ms
        .unwrap_or(now_ms.saturating_sub(lease.claims.max_s.saturating_mul(1000)));
    let end = start.saturating_add(lease.claims.max_s.saturating_mul(1000));
    b.usage = Some(Usage {
        job_id: job.into(),
        vcpus: lease.claims.vcpus,
        started_at_ms: start,
        ended_at_ms: end,
        outcome: UsageOutcome::Lost,
        mem_peak_mib: None,
        mem_working_peak_mib: None,
        mem_over_mib_ms: None,
        egress_bytes: None,
        oom_kill: None,
    });
    b.hosted_terminal = Some(
        json!({"type":"hosted_terminal","job_id":job,"placement":format!("first_party/{}",lease.claims.placement.host),"billed_vcpu_ms":u64::from(lease.claims.vcpus).saturating_mul(lease.claims.max_s).saturating_mul(1000),"max_runtime_s":lease.claims.max_s}),
    );
}
