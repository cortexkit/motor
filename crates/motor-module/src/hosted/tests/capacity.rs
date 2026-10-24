//! A test double for capacity-driven placement: one first-party host with one
//! registered tenant, and the broker's view of it.
//!
//! The broker selects a host only from what the host's last heartbeat
//! advertised (`free_vcpus` and `free_mem_mib`), checks each lease against
//! those figures with the per-job memory rule `motor_lease::memory_max_mib`,
//! and does not count leases it grants between heartbeats. When the heartbeat
//! no longer leaves room, it answers 503 `no_capacity`. The host admits a run
//! token only while the tenant's permits (`max_weight` minus the vCPUs its
//! running jobs claim) cover the job, and otherwise refuses it `host_full`
//! before any Run frame. Tests send each heartbeat themselves, so a burst of
//! leases within one heartbeat interval is exact rather than timed.

use std::{collections::BTreeMap, time::Instant};

/// One answer the broker double gave a lease request, in arrival order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseAnswer {
    /// Passed the heartbeat check; the fake broker then answered it.
    Selected,
    /// Refused 503 `no_capacity` from the last heartbeat.
    NoCapacity,
}

/// One host decision on a run token, in arrival order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostAnswer {
    Admitted,
    HostFull,
}

pub struct TenantCapacity {
    /// The tenant's permit budget, in claimed vCPUs.
    pub max_weight: u32,
    /// What the host advertises as `free_mem_mib` for this tenant.
    pub free_mem_mib: u64,
    /// The `Retry-After` seconds sent with each 503 `no_capacity`.
    pub retry_after: &'static str,
    /// Claimed vCPUs of each admitted job that has not ended, by job id.
    claims: BTreeMap<String, u32>,
    /// `(free_vcpus, free_mem_mib)` from the last heartbeat; None before any.
    advertised: Option<(u64, u64)>,
    /// Every lease request the double judged: job id, answer, arrival time.
    pub leases: Vec<(String, LeaseAnswer, Instant)>,
    /// Every run token the host judged: job id and decision.
    pub admissions: Vec<(String, HostAnswer)>,
}

impl TenantCapacity {
    pub fn new(max_weight: u32, free_mem_mib: u64) -> Self {
        Self {
            max_weight,
            free_mem_mib,
            retry_after: "1",
            claims: BTreeMap::new(),
            advertised: None,
            leases: Vec::new(),
            admissions: Vec::new(),
        }
    }

    /// The tenant's free permits now: `max_weight` minus every running claim.
    pub fn free_vcpus(&self) -> u64 {
        u64::from(self.max_weight).saturating_sub(self.claims.values().map(|v| u64::from(*v)).sum())
    }

    /// The host sends a heartbeat; the broker's view becomes the host's
    /// current figures until the next one.
    pub fn heartbeat(&mut self) {
        self.advertised = Some((self.free_vcpus(), self.free_mem_mib));
    }

    /// Whether the last heartbeat leaves room for a lease of `vcpus`, by the
    /// broker's rule: enough free vCPUs and `memory_max_mib(vcpus)` of memory.
    pub fn qualifies(&self, vcpus: u32) -> bool {
        self.advertised.is_some_and(|(free_vcpus, free_mem_mib)| {
            u64::from(vcpus) <= free_vcpus && motor_lease::memory_max_mib(vcpus) <= free_mem_mib
        })
    }

    /// The broker's capacity decision for a lease request. None selects the
    /// host and leaves the answer to the fake broker.
    pub(super) fn broker_answer(
        &mut self,
        job_id: &str,
        vcpus: u32,
    ) -> Option<(u16, serde_json::Value, &'static str)> {
        let selected = self.qualifies(vcpus);
        self.leases.push((
            job_id.to_owned(),
            if selected {
                LeaseAnswer::Selected
            } else {
                LeaseAnswer::NoCapacity
            },
            Instant::now(),
        ));
        (!selected).then(|| {
            (
                503,
                serde_json::json!({"error": "no_capacity"}),
                self.retry_after,
            )
        })
    }

    /// The host's admission decision for a run token: whether the job's claim
    /// fits the tenant's free permits. An admitted job holds its claim until
    /// [`Self::end`].
    pub(super) fn admit(&mut self, job_id: &str, vcpus: u32) -> bool {
        let admitted = u64::from(vcpus) <= self.free_vcpus();
        if admitted {
            self.claims.insert(job_id.to_owned(), vcpus);
        }
        self.admissions.push((
            job_id.to_owned(),
            if admitted {
                HostAnswer::Admitted
            } else {
                HostAnswer::HostFull
            },
        ));
        admitted
    }

    /// An admitted job ended on the host and its claim is free again.
    pub(super) fn end(&mut self, job_id: &str) {
        self.claims.remove(job_id);
    }

    /// Jobs currently holding a claim on the tenant.
    pub fn running(&self) -> usize {
        self.claims.len()
    }

    pub fn count(&self, answer: &HostAnswer) -> usize {
        self.admissions.iter().filter(|(_, a)| a == answer).count()
    }
}
