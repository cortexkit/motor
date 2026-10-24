// Share the exact caller types with routing clients; only runner transfer and
// framing types are defined locally.
pub use cortexkit_exec_remote_types::types::*;
// The shared checker for chunked returned files, so runner tests and ck-motor
// verify a stream exactly as callers do.
pub use cortexkit_exec_remote_types::assembler::{
    ReturnAssembler, ReturnStreamError, VerifiedReturn,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleManifest {
    pub index: u32,
    /// Main/sibling repository path, or the URL of a git dependency bundle.
    pub repository: String,
    pub length: u64,
    /// SHA-256 of the raw bundle bytes, in lowercase hexadecimal.
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiblingSnapshot {
    pub canonical_path: String,
    pub commit: String,
    pub tree_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitDependency {
    pub url: String,
    pub commits: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<BundleManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitDependencyCandidates {
    pub url: String,
    pub commits: Vec<String>,
}

/// Runner request: env has already been filtered, queue wait is resolved, and
/// siblings are snapshots rather than the caller's list of canonical paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Run {
    pub job_id: Uuid,
    /// Prioritize this queued Run ahead of workers without preempting running jobs.
    /// ck-motor sets it for daemon-attested direct callers (including ck motor run),
    /// only if the runner's Hello enables gate_jobs; older runners receive no mark.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate_job: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    pub workspace_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_kind: Option<WorkspaceKind>,
    pub repository_root: String,
    pub cwd: String,
    pub command: String,
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight_hint: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub return_paths: Vec<String>,
    #[serde(
        default = "default_return_max_bytes",
        skip_serializing_if = "is_default_return_max_bytes"
    )]
    pub return_max_bytes: u64,
    /// Return the files as `ReturnChunk` frames followed by one
    /// `ReturnManifest`, instead of one `ReturnResult`, and allow
    /// `return_max_bytes` up to `RETURN_CHUNKED_MAX_BYTES_CAP`. ck-motor sets it
    /// only for a runner whose Hello advertises `return_chunked`; an older
    /// runner ignores the field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub return_chunked: bool,
    pub queue_wait_limit_s: u64,
    pub snapshot_commit: String,
    pub tree_hash: String,
    pub integration_tip: String,
    pub siblings: Vec<SiblingSnapshot>,
    pub bundles: Vec<BundleManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_dependencies: Vec<GitDependency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prepare {
    pub transfer_id: Uuid,
    pub workspace_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_kind: Option<WorkspaceKind>,
    pub repository_root: String,
    pub base_commit: String,
    pub integration_tip: String,
    pub siblings: Vec<SiblingSnapshot>,
    pub bundles: Vec<BundleManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_dependencies: Vec<GitDependency>,
}

/// Workspace lifetime only; both kinds obey the same runner path and disk rules.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    #[default]
    Task,
    Head,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryCandidates {
    pub repository: String,
    pub commits: Vec<String>,
}

/// A run refusal identifies a job; a prepare refusal identifies a transfer.
/// Semantic restrictions on reasons and IDs are enforced by the executor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Refusal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_id: Option<Uuid>,
    pub reason: RefusalReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_full: Option<RunnerDiskFull>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Runner-only admission detail. The reason tag remains readable by v1 clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerDiskFull {
    pub free_bytes: u64,
    pub floor_bytes: u64,
}

impl RunnerDiskFull {
    pub fn diagnostic(&self) -> String {
        format!(
            "runner_disk_full: free_bytes={} floor_bytes={}",
            self.free_bytes, self.floor_bytes
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskEviction {
    pub path: String,
    /// Allocated inode bytes, not unique XFS extents reclaimed. Reflinks share blocks.
    pub bytes: u64,
    pub last_used_ms: u64,
    pub evicted_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerDiskStatus {
    /// None means the filesystem probe failed, never that the disk is unlimited.
    pub free_bytes: Option<u64>,
    pub floor_bytes: u64,
    pub last_eviction: Option<DiskEviction>,
}

/// Additive runner wire status; the public exec.status {} reply uses only status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerStatusReply {
    #[serde(flatten)]
    pub status: StatusReply,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<RunnerDiskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<RunnerHealth>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowsGuestAgentReadiness {
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_sha256: Option<String>,
    pub release_agent_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Read-only runner telemetry. In-memory counters reset on restart; repository
/// usage is rebuilt from retained records.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerHealth {
    pub runner_version: String,
    pub since_ms: u64,
    pub window_minutes: u32,
    /// Null means the state-filesystem probe failed, not zero free space.
    pub free_bytes: Option<u64>,
    pub queue_depth: u32,
    pub running: Vec<HealthRunningJob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_job_units: Option<u64>,
    /// IDs blocking disk cleanup at its latest evaluation, initially all
    /// quarantines. Ownerless markers with no sandbox are omitted only after
    /// a fresh complete inventory accounts for every live execution unit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup_blocked_by_quarantine: Vec<Uuid>,
    pub counts: HealthCounts,
    /// Retained completed jobs over ten minutes at under 25% of reserved CPUs.
    /// Persisted job measurements survive restarts, unlike the in-memory counts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub underused_reservations: Vec<HealthUnderusedReservation>,
    /// Finished job records in the rolling 24-hour window, grouped by repository basename.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repository_usage: BTreeMap<String, HealthRepositoryUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_1m: Option<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_available_bytes: Option<u64>,
    /// Includes permits reserved for jobs whose workspace setup is in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permits_held: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permits_cap: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_wait_1h: Option<HealthQueueWait>,
    /// Reports the sealed image's guest-agent hash and whether it matches the release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_guest_agent: Option<WindowsGuestAgentReadiness>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthQueueWait {
    /// Nearest-rank percentiles of the capacity wait reported in Started.
    /// Zero means no jobs started in the reporting window.
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub refusals: u64,
    /// Jobs withdrawn by the client to try hosted capacity, not runner refusals.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub spill_withdrawals: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthRepositoryUsage {
    /// Reserved CPU time from each job's recorded start to finish.
    pub reserved_vcpu_seconds: u64,
    /// Finished job records in the reporting window.
    pub job_count: u64,
    /// Finished jobs grouped by requested CPU reservation size; keys are decimal vCPU counts.
    pub jobs_by_vcpu_size: BTreeMap<String, u64>,
    /// Jobs refused because they exceeded their queue-wait budget.
    pub queue_wait_refusals: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReservationUse {
    pub cpu_time_ns: u64,
    pub wall_ms: u64,
    /// CPU time / (wall time * vCPUs), in millionths; 1_000_000 means 100%.
    pub average_use_ppm: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthUnderusedReservation {
    pub job_id: Uuid,
    pub workspace_key: String,
    pub vcpus: u32,
    #[serde(flatten)]
    pub usage: ReservationUse,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthRunningJob {
    pub job_id: Uuid,
    pub weight: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthCounts {
    pub accepted: u64,
    pub refusals: BTreeMap<String, u64>,
    pub evictions: u64,
    /// Allocated inode bytes measured by the guard, not unique reclaimed extents.
    pub eviction_bytes: u64,
    pub eviction_failures: u64,
    pub soft_goal_unmet_passes: u64,
    pub skipped_recent: u64,
    #[serde(default)]
    pub oom_stops: u64,
}

impl std::ops::Deref for RunnerStatusReply {
    type Target = StatusReply;
    fn deref(&self) -> &Self::Target {
        &self.status
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawOutcome {
    Withdrawn,
    Started,
}

/// Prefix on the legacy queue-wait terminal detail for a move to hosted capacity.
pub const SPILL_WITHDRAWAL_DETAIL_PREFIX: &str = "spill_withdrawal: ";

// Generate the tag probe from the same declaration as Frame so new variants
// cannot be mistaken for future records when their payload is malformed.
macro_rules! define_frames {
    ($($variant:ident $( { $($fields:tt)* } )? $( ( $payload:ty ) )?,)*) => {
        /// One SSH/Unix-socket frame. Version 1 uses `type` as its discriminator;
        /// variant fields are inline. Extra fields never reject a known type.
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum Frame {
            $($variant $( { $($fields)* } )? $( ( $payload ) )?,)*
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub(crate) enum FrameTag {
            $($variant,)*
            #[serde(other)]
            Unknown,
        }
    };
}

define_frames! {
    VerifiedLease(crate::VerifiedLease),
    Event(crate::TenantEvent),
    Ack { event_seq: u64 },
    Hello {
        protocol_version: u32,
        /// Opt in to sequenced Started records; absence preserves the legacy output cursor.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        started: bool,
        /// The client selects integration tips from the local default branch, not its job branch.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        default_branch_tip: bool,
        /// The peer understands Run.gate_job: direct CLI callers queue ahead of workers,
        /// without stopping or changing the CPU reservation of any running job.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        gate_jobs: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        return_paths: bool,
        /// The runner accepts `Run.return_chunked` and answers it with
        /// `ReturnChunk` and `ReturnManifest` frames.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        return_chunked: bool,
        /// The runner mounts sibling snapshots below its configured workspace roots,
        /// not just beside the repository root. Absent on in-place and Windows runners.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        sibling_paths: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        platforms: Vec<String>,
    },
    Have {
        repositories: Vec<RepositoryCandidates>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        git_dependencies: Vec<String>,
    },
    HaveReply {
        repositories: Vec<RepositoryCandidates>,
        /// Presence, including an empty list, advertises mirror support.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_dependencies: Option<Vec<GitDependencyCandidates>>,
    },
    Run(Run),
    Accepted(Accepted),
    Started {
        job_id: Uuid,
        seq: u64,
        queue_wait_ms: u64,
        started_at_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path_mapping: Option<PathMapping>,
    },
    Refusal(Refusal),
    BundleChunk {
        index: u32,
        offset: u64,
        data: BytePayload,
    },
    Output(Output),
    ReturnResult(ReturnResult),
    // The answer to a Run with `return_chunked`: chunks of the returned
    // files, then one manifest. Manifest paths are the Run's absolute
    // `return_paths`; ck-motor maps them back to the caller's paths.
    ReturnChunk(ReturnChunk),
    ReturnManifest(ReturnManifest),
    Heartbeat {
        /// Elapsed time waiting for scheduler capacity on a job stream. When
        /// absent, the client clears its local queue deadline rather than
        /// timing upload, setup or a runner that cannot report queue timing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue_wait_ms: Option<u64>,
    },
    Withdraw {
        job_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<RefusalReason>,
        /// The client is leaving the capacity queue to try a hosted runner.
        /// Older runners ignore this field and retain queue-wait accounting.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        spill: bool,
    },
    WithdrawReply {
        outcome: WithdrawOutcome,
    },
    Attach(AttachRequest),
    Terminal(TerminalRecord),
    HostedTerminal(HostedTerminal),
    Prepare(Prepare),
    PrepareAccepted {
        transfer_id: Uuid,
    },
    Prepared {
        transfer_id: Uuid,
    },
    Drop(DropRequest),
    DropReply(DropReply),
    Status,
    StatusReply(RunnerStatusReply),
    JobStatus {
        job_id: Uuid,
    },
    JobStatusReply(JobStatusReply),
    Drain {
        deadline_s: u64,
        exit_after: bool,
    },
    DrainReply {
        timed_out: bool,
        /// Linux unit-backed jobs left running for recovery after the swap.
        /// Older daemons omit the count and older clients ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restart_recoverable_jobs: Option<u64>,
    },
    Resume,
    ResumeReply {
        resumed: bool,
    },
    Cancel(CancelRequest),
    CancelReply(CancelReply),
}

pub const RETURN_MAX_BYTES: u64 = 262144;

pub fn default_return_max_bytes() -> u64 {
    RETURN_MAX_BYTES
}

fn is_default_return_max_bytes(value: &u64) -> bool {
    *value == RETURN_MAX_BYTES
}

/// Validate lexical bounds before acquiring a lease or admitting a job. Files
/// may not exist until the command runs; symlinks are checked when collecting.
pub fn validate_return_paths(
    workspace: &str,
    paths: &[String],
    cap: u64,
) -> Result<(), &'static str> {
    validate_return_request(workspace, paths, cap, false)
}

/// `validate_return_paths` for a request that may set `return_chunked`, which
/// raises the byte cap from `RETURN_MAX_BYTES` to `RETURN_CHUNKED_MAX_BYTES_CAP`.
pub fn validate_return_request(
    workspace: &str,
    paths: &[String],
    cap: u64,
    chunked: bool,
) -> Result<(), &'static str> {
    use std::path::{Component, Path};
    let root = Path::new(workspace);
    let limit = if chunked {
        RETURN_CHUNKED_MAX_BYTES_CAP
    } else {
        RETURN_MAX_BYTES
    };
    if cap > limit || paths.len() > 64 {
        return Err("return_paths_exceeded");
    }
    for path in paths {
        let path_ref = Path::new(path);
        if path.len() > 4096
            || !path_ref.is_absolute()
            || !root.is_absolute()
            || !path_ref.starts_with(root)
            || path_ref
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err("return_paths_exceeded");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReturnedFile {
    pub path: String,
    pub sha256: String,
    /// Standard padded base64 of the file's raw bytes.
    pub data: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReturnFailureCause {
    OverCap,
    Missing,
    Escape,
    NotRegular,
    FrameLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReturnPathsFailed {
    pub measured_bytes: u64,
    pub cap_bytes: u64,
    pub cause: ReturnFailureCause,
}

/// File collection result ordered by seq among the job's events, immediately
/// before its terminal record. Collection failure does not change the exit outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReturnResult {
    pub job_id: Uuid,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned_files: Option<Vec<ReturnedFile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_paths_failed: Option<ReturnPathsFailed>,
}

/// Host-observed usage. A host sends this after the runner's terminal result and
/// replays the same billing figure whenever a client reattaches to the job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostedTerminal {
    pub job_id: Uuid,
    pub placement: String,
    pub billed_vcpu_ms: u64,
    pub max_runtime_s: u64,
}

/// Per-job lifecycle inspection. Aggregate status retains its v1 shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobStatusReply {
    pub job_id: Uuid,
    pub state: String,
    /// Current scheduler phase or blocker while the job has not started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
    pub terminal: Option<TerminalRecord>,
    pub unit: Option<String>,
    pub unit_alive: Option<bool>,
    /// Present while the job remains queued; it counts time spent waiting for
    /// shared scheduler capacity, excluding time paused behind a workspace owner
    /// or held for an earlier queued job whose main workspace overlaps one of this
    /// job's sibling checkout paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<u64>,
}
