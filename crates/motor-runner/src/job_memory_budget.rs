//! The shared jobs slice's ceiling and the operator's declared Linux job budget.
//!
//! Plain data with no systemd calls, so code that every platform compiles can
//! name it. Only Linux reads a real slice (`systemd::read_job_memory_budget`);
//! elsewhere the ceiling stays [`NO_JOB_MEMORY_BUDGET`] and refuses nothing.
//! A slice ceiling alone is not a declaration that the host can afford the
//! per-job memory floor: small hosts deliberately clamp job limits below it.

/// Stands in for the slice's memory ceiling when no usable budget was read.
/// No slice can have this `MemoryMax`, so it refuses nothing.
pub(crate) const NO_JOB_MEMORY_BUDGET: u64 = u64::MAX;

/// What the shared jobs slice allows a job unit to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JobMemoryBudget {
    /// The slice's `MemoryMax`: the most memory the slice as a whole may use.
    pub(crate) max: u64,
    /// The slice's `MemoryHigh`: from here the kernel throttles the whole slice.
    /// No decision reads it, but it is part of the same reading and the policy
    /// check requires it to sit strictly below `max`.
    #[allow(dead_code)]
    pub(crate) high: u64,
}

impl JobMemoryBudget {
    /// The budget with a remembered ceiling. Only the ceiling drives a
    /// decision, so callers that cache it do not keep the whole reading.
    pub(crate) fn from_max(max: u64) -> Option<Self> {
        (max != NO_JOB_MEMORY_BUDGET).then_some(Self { max, high: 0 })
    }
}

/// Only capacity-managed hosts declare a guaranteed Linux job budget. The
/// slice's maximum also includes the Windows allowance, so it cannot stand in
/// for `jobs_min_bytes`. Boat and tenant guests normally have no capacity file.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn read_declared_jobs_min(path: &std::path::Path) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Capacity {
        jobs_min_bytes: u64,
    }

    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<Capacity>(&bytes)
        .ok()
        .map(|capacity| capacity.jobs_min_bytes)
}

#[cfg(test)]
pub(crate) type BudgetProbe =
    std::sync::Arc<dyn Fn() -> std::io::Result<(u64, Option<u64>)> + Send + Sync>;
