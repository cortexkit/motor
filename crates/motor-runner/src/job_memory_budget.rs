//! The shared jobs slice's memory ceiling, as admission and OOM advice see it.
//!
//! Plain data with no systemd calls, so code that every platform compiles can
//! name it. Only Linux reads a real slice (`systemd::read_job_memory_budget`);
//! elsewhere the ceiling stays [`NO_JOB_MEMORY_BUDGET`] and refuses nothing.

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
