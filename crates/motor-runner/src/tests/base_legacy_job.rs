// Job schema frozen from storage.rs at 935b3dbce2cfab34527091473267c1423540ff38.
// Keep this independent of new fields so rollback deserialization is exercised.
use crate::storage::*;
use motor_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc,
    },
};
fn return_reservation_is_zero(bytes: &u64) -> bool {
    *bytes == 0
}
#[derive(Clone, Serialize, Deserialize)]
#[allow(dead_code)] // Runtime-only fields are frozen too, but never inspected.
pub(super) struct LegacyJob {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mappings: Vec<crate::caller_path::CallerRoot>,
    pub request: Option<Run>,
    pub state: JobState,
    pub accepted_ms: u64,
    #[serde(default)]
    pub queued_ms: Option<u64>,
    /// Runtime queue-clock start or paused marker, shared without the runner
    /// state lock so heartbeat writers can report active queue wait. Because
    /// startup refuses queued jobs before subscribers attach, it needs no
    /// persisted queue clock to restore.
    #[serde(skip, default = "empty_queue_entry")]
    pub queue_entry: Arc<AtomicU64>,
    /// Runtime copy of the accumulated queue-wait time already spent.
    #[serde(skip, default = "empty_queue_elapsed")]
    pub queue_elapsed: Arc<AtomicU64>,
    /// Runtime marker indicating whether this job has entered the live queue.
    #[serde(skip, default = "empty_queue_clock_initialized")]
    pub queue_wait_clock_initialized: Arc<AtomicBool>,
    /// A failed disk probe is reported once per queued job, not on every scheduler tick.
    #[serde(skip)]
    pub return_probe_error_logged: Arc<AtomicBool>,
    #[serde(default)]
    pub setup_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub started_ms: Option<u64>,
    /// Sequenced start progress survives restart and output-chunk eviction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<Started>,
    pub process: Option<crate::platform::ProcessIdentity>,
    pub terminal: Option<TerminalRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_result: Option<ReturnResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned: Option<ReturnedFiles>,
    /// Charged at dispatch, reduced to committed bytes and filesystem overhead
    /// when collection finishes, and released after returned directories are deleted.
    #[serde(default, skip_serializing_if = "return_reservation_is_zero")]
    pub return_reserved_bytes: u64,
    pub chunks: Chunks,
    pub next_seq: u64,
    pub output_bytes: u64,
    pub weight: u32,
    /// Retained for inspection after terminal; only live jobs hold these CPUs.
    #[serde(default)]
    pub cpus: Vec<u32>,
    pub order: u64,
    pub base: bool,
    #[serde(default)]
    pub writer_descriptor: Option<String>,
    /// When this fill started, in runner-clock milliseconds. A publication
    /// record ages from here, and an operator reads it as the time the entries
    /// of that fill were written.
    #[cfg(unix)]
    #[serde(default)]
    pub fill_started_ms: Option<u64>,
    /// Store active when fill setup began, updated when the command launches.
    #[cfg(unix)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_identity: Option<StoreIdentity>,
    pub kill: Option<(Killed, u64)>,
    /// Durable isolated stream positions, advanced atomically with replay chunks.
    #[serde(default)]
    pub stdout_offset: u64,
    #[serde(default)]
    pub stderr_offset: u64,
    #[serde(default)]
    pub durable_output: bool,
    /// Launch-time fixed-to-caller paths, retained independently of runner options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_rewrites: Vec<crate::output_rewrite::Rewrite>,
    /// Retained in the record even when replay storage cannot accept a diagnostic.
    #[serde(default)]
    pub monitoring_error: Option<String>,
    /// Last live cgroup sample; systemd can remove the cgroup before finalization.
    #[serde(skip)]
    pub(crate) execution_evidence: Option<crate::engine::terminal_evidence::ExecutionEvidence>,
    /// Memory-limit advice retained for inspection and stderr. The terminal's
    /// existing detail field separately carries bounded resource evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_detail: Option<String>,
    /// Host-verified memory carried on the tenant channel, never a Run field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_stated_mib: Option<u64>,
    /// Accepted memory headroom above stated memory for this job only. Absent
    /// (or legacy null) means no burst was accepted; numeric values never change
    /// stated memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_burst_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_use: Option<ReservationUse>,
    /// Fixed execution end, independent of delayed launcher-accounting delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_ended_ms: Option<u64>,
    /// The launcher's final displayed peak, conservatively rounded up to MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_peak_mib: Option<u64>,
    /// Sampled cgroup peak of anonymous, shared and kernel memory; excludes file
    /// cache. Telemetry only, not an admission, placement or billing input.
    #[serde(
        default,
        skip_serializing_if = "crate::working_memory::WorkingPeak::is_unknown"
    )]
    pub mem_working_peak_mib: crate::working_memory::WorkingPeak,
    /// Working-set integral above `memory_stated_mib`, in MiB-milliseconds.
    /// Absent means not measured or no burst. Null means measured but not
    /// attributable to this job.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::working_memory::OverIntegral::present"
    )]
    pub mem_over_mib_ms: Option<crate::working_memory::OverIntegral>,
    /// Captured from job-private compiler servers before terminal persistence.
    /// Telemetry only; not a billing or admission input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_cache_totals: Option<motor_protocol::RemoteCacheTotals>,
    /// A live accounting reader will finish the terminal lifecycle event. A
    /// restart loses that reader, so recovered records default to no pending read.
    #[serde(skip)]
    pub memory_accounting_pending: bool,
    /// The sibling snapshot directories this job's unit binds at its
    /// siblings' paths, set in the save that records its process and start.
    /// A running job without it was started with each sibling bound from its
    /// host path, by an older runner or in in-place mode, so a main-workspace
    /// job must not check that path out until it ends. Older runners ignore
    /// the field, and a record they rewrite loses it, which only fences more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sibling_sources: Option<Vec<String>>,
    /// Workspace-relative package directories whose node_modules were prepared
    /// by the runner, not the job. Persisted so recovered jobs exclude them too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prepared_javascript_directories: Vec<PathBuf>,
    /// Whether source inode protection was applied before this command started.
    /// Omitted when false, so records of unsealed jobs keep their earlier bytes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_read_only: bool,
}
