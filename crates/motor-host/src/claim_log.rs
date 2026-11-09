use crate::config::HostMode;
use crate::state::{Admission, Error, Refusal};
use motor_lease::{Claims, Purpose};

/// Local admission causes, separate from the requester's wire refusal reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalCause {
    Lease(Refusal),
    Permits,
    Memory,
    Storage,
    WorkspaceDisks,
    RunnerWaking,
    RunnerStartRefused,
    LeaseEntries,
    Suspending,
    StopPending,
    BoundedDrain,
    QuarantinedWorkspace,
    DaxRequired,
}

impl RefusalCause {
    pub fn reason(self) -> Refusal {
        match self {
            Self::Lease(reason) => reason,
            Self::Suspending | Self::StopPending | Self::BoundedDrain => Refusal::Capacity,
            _ => Refusal::HostFull,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lease(reason) => reason.wire(),
            Self::Permits => "permits",
            Self::Memory => "memory",
            Self::Storage => "storage",
            Self::WorkspaceDisks => "workspace_disks",
            Self::RunnerWaking => "runner_waking",
            Self::RunnerStartRefused => "runner_start_refused",
            Self::LeaseEntries => "lease_entries",
            Self::Suspending => "suspending",
            Self::StopPending => "stop_pending",
            Self::BoundedDrain => "bounded_drain",
            Self::QuarantinedWorkspace => "quarantined_workspace",
            Self::DaxRequired => "dax_required",
        }
    }
}

pub(crate) struct ClaimError {
    pub error: Error,
    pub cause: RefusalCause,
}

impl From<Error> for ClaimError {
    fn from(error: Error) -> Self {
        Self {
            cause: RefusalCause::Lease(error.refusal()),
            error,
        }
    }
}

impl From<Refusal> for ClaimError {
    fn from(reason: Refusal) -> Self {
        Error::Refused(reason).into()
    }
}

impl From<RefusalCause> for ClaimError {
    fn from(cause: RefusalCause) -> Self {
        Self {
            error: Error::Refused(cause.reason()),
            cause,
        }
    }
}

/// One record per Run admission attempt, not per Attach or per guest Run frame.
/// Only the job id is logged from the claims; tokens and account data stay out.
pub fn record(mode: HostMode, claims: &Claims, result: Result<Admission, RefusalCause>) {
    if claims.purpose != Purpose::Run {
        return;
    }
    match result {
        Ok(admission) => tracing::info!(
            jti = %claims.jti,
            mode = mode.as_str(),
            result = match admission {
                Admission::Ready => "admitted",
                Admission::Waiting => "waiting",
            },
            "run_claim"
        ),
        Err(cause) => tracing::info!(
            jti = %claims.jti,
            mode = mode.as_str(),
            result = "refused",
            reason = cause.reason().wire(),
            cause = cause.as_str(),
            "run_claim"
        ),
    }
}
