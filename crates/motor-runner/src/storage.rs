use motor_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File},
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

pub const RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
pub const REPLAY_BYTES: u64 = 64 * 1024 * 1024;
/// How long chunked returned files stay available for replay after the job
/// ends. They can be 10 MiB per job, so they are not kept for the whole job
/// history; a later replay sends the manifest with `expired` entries instead.
pub const RETURN_RETENTION_MS: u64 = 60 * 60 * 1000;
/// Prefix of the job-directory subdirectory that holds one collection's
/// returned-file chunks.
pub(crate) const RETURNED_DIRECTORY_PREFIX: &str = "returned-";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobState {
    AwaitingBundles,
    Queued,
    Running,
    Terminal,
    Tombstone,
    Lost,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Chunk {
    pub seq: u64,
    pub stream: OutputStream,
    pub length: u64,
}

/// Cloning a job under the state lock shares its immutable output index. An
/// append copies that index only when it mutates the off-lock working record.
/// Serialization remains the legacy array of chunk descriptors.
#[derive(Clone, Default)]
pub(crate) struct Chunks(Arc<VecDeque<Chunk>>);

impl std::ops::Deref for Chunks {
    type Target = VecDeque<Chunk>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for Chunks {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}
impl Serialize for Chunks {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.as_ref().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Chunks {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        VecDeque::<Chunk>::deserialize(deserializer).map(|chunks| Self(Arc::new(chunks)))
    }
}

/// One chunk of a returned file. Its bytes are the file
/// `<job directory>/<ReturnedFiles::directory>/chunk-<position>`, where
/// position is the chunk's index in `ReturnedFiles::chunks`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReturnedChunk {
    pub seq: u64,
    pub path_index: u32,
    pub offset: u64,
    pub length: u64,
}

/// The chunked returned files of a job that set `Run.return_chunked`.
///
/// The chunk bytes live in their own directory rather than in this record,
/// and are written and synced before the record that names them is saved,
/// as output chunks are. They are deleted `RETURN_RETENTION_MS` after the job
/// ends, while the record, with this index and the manifest, stays for the
/// job's whole history.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReturnedFiles {
    /// Name of the job-directory subdirectory holding the chunk bytes.
    pub directory: String,
    pub chunks: Vec<ReturnedChunk>,
    /// Lists the Run's absolute paths; ck-motor maps them to the caller's.
    pub manifest: ReturnManifest,
    /// Set, and the directory deleted, once the retention window has passed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expired: bool,
}

impl ReturnedFiles {
    pub fn chunk_path(&self, job_directory: &Path, position: usize) -> std::path::PathBuf {
        job_directory
            .join(&self.directory)
            .join(format!("chunk-{position}"))
    }

    /// The manifest a replay sends once the bytes are gone: at its original
    /// seq, each returned entry failed as `expired` with its size, other
    /// failures unchanged, and no returned bytes. Original chunk counts remain
    /// available to check the replay's sequence gap; legacy entries keep none.
    pub fn expired_manifest(&self) -> ReturnManifest {
        let mut manifest = self.manifest.clone();
        for entry in &mut manifest.files {
            if let ReturnManifestOutcome::Returned { size, .. } = entry.outcome {
                entry.outcome = ReturnManifestOutcome::Failed {
                    cause: ReturnPathsFailureCause::Expired,
                    size: Some(size),
                };
            }
        }
        manifest.total_bytes = 0;
        manifest
    }
}

pub(crate) const NO_QUEUE: u64 = u64::MAX;
pub(crate) const PAUSED_QUEUE: u64 = u64::MAX - 1;
pub(crate) fn empty_queue_entry() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(NO_QUEUE))
}
pub(crate) fn empty_queue_elapsed() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(0))
}
pub(crate) fn empty_queue_clock_initialized() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

pub(crate) fn queue_clock_at(entry: &AtomicU64, elapsed: &AtomicU64, now: u64) -> (u64, u64) {
    loop {
        let state = entry.load(Ordering::Acquire);
        let accumulated = elapsed.load(Ordering::Acquire);
        if state == entry.load(Ordering::Acquire) {
            let live = if state == NO_QUEUE || state == PAUSED_QUEUE {
                0
            } else {
                now.saturating_sub(state)
            };
            return (state, accumulated.saturating_add(live));
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Job {
    pub id: Uuid,
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
    #[serde(default)]
    pub fill_started_ms: Option<u64>,
    /// Store active when fill setup began, updated when the command launches.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_use: Option<ReservationUse>,
    /// Fixed execution end, independent of delayed launcher-accounting delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_ended_ms: Option<u64>,
    /// The sibling snapshot directories this job's unit binds at its
    /// siblings' paths, set in the save that records its process and start.
    /// A running job without it was started with each sibling bound from its
    /// host path, by an older runner or in in-place mode, so a main-workspace
    /// job must not check that path out until it ends. Older runners ignore
    /// the field, and a record they rewrite loses it, which only fences more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sibling_sources: Option<Vec<String>>,
}

/// A sidecar preserves damaged records and any independently readable ownership.
/// It has no expiry: only an operator who has reconciled the execution may remove
/// it and repair the original record. Keeping the original unreadable also makes
/// rollback fail closed on runners that do not recognize this sidecar.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct QuarantinedJob {
    pub id: Uuid,
    pub request: Option<Run>,
    pub process: Option<crate::platform::ProcessIdentity>,
    pub observed_ms: u64,
}

impl QuarantinedJob {
    pub fn preserve_rollback_fence(&self, record: &Path) -> io::Result<()> {
        match fs::symlink_metadata(record) {
            // An older runner ignores the quarantine index. If storage loses
            // the original too, an unreadable placeholder preserves its startup
            // refusal. Repeating this migration never changes an existing record.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                atomic_json_private(record, &serde_json::json!({ "quarantined_job": self.id }))
            }
            _ => Ok(()),
        }
    }

    pub fn inspect(id: Uuid, path: &Path, now: u64) -> Self {
        let value = read_json::<serde_json::Value>(path).ok().filter(|value| {
            value.get("id").and_then(|id| id.as_str()) == Some(id.to_string().as_str())
        });
        let request = value
            .as_ref()
            .and_then(|value| value.get("request"))
            .and_then(|request| serde_json::from_value::<Run>(request.clone()).ok())
            .filter(|request| request.job_id == id);
        let process = value
            .as_ref()
            .and_then(|value| value.get("process"))
            .and_then(|process| serde_json::from_value(process.clone()).ok());
        Self {
            id,
            request,
            process,
            observed_ms: now,
        }
    }
}

impl Job {
    pub fn gate_job(&self) -> bool {
        !self.base
            && self
                .request
                .as_ref()
                .is_some_and(|request| request.gate_job)
    }
    pub fn new(id: Uuid, request: Option<Run>, now: u64, order: u64, weight: u32) -> Self {
        Self {
            id,
            request,
            state: JobState::AwaitingBundles,
            accepted_ms: now,
            queued_ms: None,
            queue_entry: empty_queue_entry(),
            queue_elapsed: empty_queue_elapsed(),
            queue_wait_clock_initialized: empty_queue_clock_initialized(),
            setup_ms: None,
            finished_ms: None,
            started_ms: None,
            started: None,
            process: None,
            terminal: None,
            return_result: None,
            returned: None,
            chunks: Chunks::default(),
            next_seq: 0,
            output_bytes: 0,
            weight,
            cpus: Vec::new(),
            order,
            base: false,
            writer_descriptor: None,
            fill_started_ms: None,
            fill_identity: None,
            kill: None,
            stdout_offset: 0,
            stderr_offset: 0,
            durable_output: false,
            monitoring_error: None,
            execution_evidence: None,
            memory_limit_detail: None,
            reservation_use: None,
            execution_ended_ms: None,
            sibling_sources: None,
        }
    }
    pub fn pending(&self) -> bool {
        matches!(self.state, JobState::AwaitingBundles | JobState::Queued)
    }
    pub fn claimed_cpus(&self) -> &[u32] {
        if self.terminal.is_some() {
            &[]
        } else {
            &self.cpus
        }
    }
    pub fn key(&self) -> &str {
        self.request
            .as_ref()
            .map(|r| r.workspace_key.as_str())
            .unwrap_or("")
    }
    pub fn terminal(&mut self, outcome: Outcome, now: u64) {
        let queue_wait_ms = self.queue_wait_ms(now);
        self.queue_entry.store(NO_QUEUE, Ordering::Release);
        let remote = self.started_ms.is_some();
        let bundle_bytes = self
            .request
            .as_ref()
            .map(|r| {
                r.bundles
                    .iter()
                    .chain(r.git_dependencies.iter().filter_map(|d| d.bundle.as_ref()))
                    .map(|b| b.length)
                    .sum()
            })
            .unwrap_or(0);
        let mut terminal = TerminalRecord::new(
            self.id,
            outcome,
            self.started_ms.map(|t| now.saturating_sub(t)).unwrap_or(0),
            queue_wait_ms,
            bundle_bytes,
        )
        .with_ran(if remote { Ran::Remote } else { Ran::None });
        if let Some((killed, _)) = &self.kill {
            terminal = terminal.with_killed(killed.clone());
        }
        if let Some(tree_hash) = self.request.as_ref().map(|r| &r.tree_hash) {
            terminal = terminal.with_tree_hash(tree_hash.clone());
        }
        if remote
            && self.request.as_ref().and_then(|r| r.platform.as_ref()) != Some(&Platform::Macos)
        {
            terminal = terminal.with_workspace_changes(Vec::new());
        }
        self.terminal = Some(terminal);
        self.finished_ms = Some(now);
        self.state = JobState::Terminal;
    }

    /// Finish with proof that no process of the job remains, independently of
    /// whether its exit status is known.
    pub fn terminal_ended(&mut self, outcome: Outcome, now: u64) {
        self.terminal(outcome, now);
        self.terminal = self.terminal.take().map(|t| t.with_execution_ended());
    }

    pub fn queue_wait_ms(&self, now: u64) -> u64 {
        if self.queue_wait_clock_initialized.load(Ordering::Acquire) {
            queue_clock_at(&self.queue_entry, &self.queue_elapsed, now).1
        } else {
            self.setup_ms
                .or(self.started_ms)
                .unwrap_or(now)
                .saturating_sub(self.queued_ms.unwrap_or(self.accepted_ms))
        }
    }

    pub fn start_queue_wait(&self, now: u64) {
        self.queue_elapsed.store(0, Ordering::Release);
        self.queue_wait_clock_initialized
            .store(true, Ordering::Release);
        self.queue_entry.store(now, Ordering::Release);
    }

    pub fn pause_queue_wait(&self, now: u64) {
        let (entry, elapsed) = queue_clock_at(&self.queue_entry, &self.queue_elapsed, now);
        if entry != NO_QUEUE && entry != PAUSED_QUEUE {
            self.queue_entry.store(PAUSED_QUEUE, Ordering::Release);
            self.queue_elapsed.store(elapsed, Ordering::Release);
        }
    }

    pub fn resume_queue_wait(&self, now: u64) {
        if self.state == JobState::Queued && self.queued_ms.is_some() {
            self.queue_wait_clock_initialized
                .store(true, Ordering::Release);
            self.queue_entry.store(now, Ordering::Release);
        }
    }

    pub fn stop_queue_wait(&self, now: u64) {
        self.pause_queue_wait(now);
        self.queue_entry.store(NO_QUEUE, Ordering::Release);
    }

    pub fn record_reservation_use(&mut self, cpu_time_ns: u64, ended_ms: u64) {
        let Some(wall_ms) = self.started_ms.map(|start| ended_ms.saturating_sub(start)) else {
            return;
        };
        if wall_ms == 0 || self.weight == 0 {
            return;
        }
        // cpu_time_ns / (wall_ms * vCPUs) is already in millionths: converting
        // milliseconds to nanoseconds cancels the 1_000_000 ppm multiplier.
        let average_use_ppm = (u128::from(cpu_time_ns)
            / (u128::from(wall_ms) * u128::from(self.weight)))
        .min(u128::from(u64::MAX)) as u64;
        self.reservation_use = Some(ReservationUse {
            cpu_time_ns,
            wall_ms,
            average_use_ppm,
        });
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Workspace {
    #[serde(default)]
    pub kind: WorkspaceKind,
    pub repository: String,
    pub snapshot: String,
    pub siblings: Vec<SiblingSnapshot>,
    pub used_ms: u64,
    /// Per sibling path, the trees whose artifacts may still be in this
    /// workspace's `target/`, used to keep Cargo from reusing them after a
    /// switch to another sibling snapshot; see `sibling_snapshots::freshness`.
    /// Only setups that mounted siblings from snapshots write it. It goes away
    /// with the entry, which is removed when the workspace is deleted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sibling_freshness: BTreeMap<String, crate::sibling_snapshots::freshness::Record>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoreIdentity {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
}

pub(crate) use crate::remote_cache::publishing::Seen;

/// The persisted form of a provisioning store identity. The gateway's identity
/// type lives where it is used to sign; records and comparisons use this one.
pub(crate) fn encode_identity(
    identity: &crate::remote_cache::provisioning::StoreIdentity,
) -> StoreIdentity {
    StoreIdentity {
        endpoint: identity.endpoint.clone(),
        bucket: identity.bucket.clone(),
        prefix: identity.prefix.clone(),
    }
}

impl StoreIdentity {
    /// The same identity in the form the gateway signs and tags requests with.
    #[cfg(test)]
    pub(crate) fn provisioned(&self) -> crate::remote_cache::provisioning::StoreIdentity {
        crate::remote_cache::provisioning::StoreIdentity {
            endpoint: self.endpoint.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
        }
    }
}

/// A key a fill's PUTs stored in the remote store, with the tool whose socket
/// wrote it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PublishedKey {
    pub tool: String,
    pub key: String,
}

/// A fill of one descriptor that published every entry it wrote to the remote
/// store. The operator reads it to check that a repository's cache is fresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PublicationRecord {
    /// The store the fill published into. A record belongs to one identity; a
    /// key rotation keeps it, a prefix or bucket change forgets it.
    pub identity: StoreIdentity,
    /// When the published fill started. Age is measured from here, so a fill
    /// that ran for hours still counts as fresh when it started.
    pub started_ms: u64,
    /// Bare keys (no tenant prefix and no credentials) that the fill's PUTs
    /// stored, in the order they were first stored, up to the gateway's
    /// sample size. Operators read this record, so it must stay free of
    /// anything secret.
    #[serde(default)]
    pub keys: Vec<PublishedKey>,
}

/// The last fill of one descriptor that failed to publish, with the number of
/// consecutive attempts that failed as builds (not merely remote writes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PublishFailure {
    pub started_ms: u64,
    pub attempts: u32,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Repository {
    pub failure_detail: Option<String>,
    pub filled_descriptor: Option<String>,
    pub filled_descriptors: BTreeMap<String, u64>,
    pub recorded_without_writer: BTreeSet<String>,
    pub miss_refills: BTreeSet<String>,
    pub failed_descriptor: Option<String>,
    pub requested_descriptor: Option<String>,
    pub commit: Option<String>,
    pub finished_ms: Option<u64>,
    pub result: Option<RebuildResult>,
    pub desired: Option<BaseRequest>,
    /// Per descriptor, the last fill that published it.
    pub publications: BTreeMap<String, PublicationRecord>,
    /// Per descriptor, the last fill that failed to publish, while no later
    /// fill has published.
    pub publish_failures: BTreeMap<String, PublishFailure>,
}

/// Exact catalog-owned checkout paths awaiting cheap retirement into trash.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyCheckout {
    pub repository: String,
    pub tip: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct BaseRequest {
    pub tip: String,
    pub workspace: String,
    pub siblings: Vec<SiblingSnapshot>,
    #[serde(default)]
    pub git_dependencies: Vec<GitDependency>,
    /// The warm command derived for `tip`; absent in catalogs written before
    /// warm commands were per repository.
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RetainedTransfer {
    pub repository: String,
    pub snapshot: String,
    pub tip: String,
    pub siblings: Vec<SiblingSnapshot>,
    pub finished_ms: u64,
}

fn pending_legacy_generations() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Catalog {
    pub workspaces: BTreeMap<String, Workspace>,
    pub repositories: BTreeMap<String, Repository>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub legacy_checkouts: BTreeMap<String, LegacyCheckout>,
    #[serde(default = "pending_legacy_generations")]
    pub legacy_generations_pending: bool,
    #[serde(default)]
    pub generation_migration_complete: bool,
    pub sibling_commits: BTreeMap<String, String>,
    #[serde(default)]
    pub transferred: BTreeMap<Uuid, RetainedTransfer>,
    /// When each repository mirror was last used, by mirror directory name
    /// (the hash of its repository root). Unused mirrors are retired; see
    /// the runner's `retire_unused_repositories`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mirror_used_ms: BTreeMap<String, u64>,
    /// When a job last finished, for the shared compiler cache's last use.
    /// Jobs mount that cache read-only, so its own timestamps never show
    /// reads; disk eviction ages the cache by this instead. A job still in
    /// flight counts as use at the current time without being recorded here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_cache_used_ms: Option<u64>,
    /// The remote cache's switch state this runner last acted on, so a change
    /// made while it was stopped is still a transition at the first re-read.
    #[serde(default, skip_serializing_if = "Seen::is_default")]
    pub publishing_seen: Seen,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            workspaces: BTreeMap::new(),
            repositories: BTreeMap::new(),
            legacy_checkouts: BTreeMap::new(),
            legacy_generations_pending: true,
            generation_migration_complete: false,
            sibling_commits: BTreeMap::new(),
            transferred: BTreeMap::new(),
            mirror_used_ms: BTreeMap::new(),
            compile_cache_used_ms: None,
            publishing_seen: Seen::default(),
        }
    }
}

impl Catalog {
    /// Forget every repository's record of what the shared compiler cache
    /// holds. Call whenever the cache's contents go, however that happens:
    /// a repository whose fill is still recorded is never filled again until
    /// its dependencies change, and every job compiles uncached meanwhile.
    pub(crate) fn forget_compiler_cache_fills(&mut self) {
        for repository in self.repositories.values_mut() {
            repository.filled_descriptor = None;
            repository.filled_descriptors.clear();
            repository.recorded_without_writer.clear();
            repository.miss_refills.clear();
        }
    }
}

pub(crate) fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// Rename alone is insufficient: sync the file and its containing directory
/// before acknowledging a state transition to a peer or releasing a launch gate.
pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    atomic_json_mode(path, value, false)
}

pub(crate) fn atomic_job_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    atomic_job_json_and(path, value, |_| Ok(()))
}

fn atomic_job_json_and(
    path: &Path,
    value: &impl Serialize,
    before_publish: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| invalid("job has no directory"))?;
    if directory.try_exists()? {
        return atomic_json(path, value);
    }
    let jobs = directory
        .parent()
        .ok_or_else(|| invalid("job has no root"))?;
    // Keep incomplete first records outside jobs: rollback binaries scan every
    // directory there, even entries whose names are not UUIDs.
    let staging = jobs
        .with_file_name("job-staging")
        .join(Uuid::now_v7().to_string());
    let result = (|| {
        atomic_json(&staging.join("record.json"), value)?;
        before_publish(&staging)?;
        crate::filesystem::rename(&staging, directory)?;
        crate::filesystem::sync_directory(jobs)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

pub(crate) fn atomic_json_private(path: &Path, value: &impl Serialize) -> io::Result<()> {
    atomic_json_mode(path, value, true)
}

fn atomic_json_mode(path: &Path, value: &impl Serialize, private: bool) -> io::Result<()> {
    let mut bytes = Vec::new();
    write_json(&mut bytes, value)?;
    atomic_bytes(path, &bytes, private)
}

/// `atomic_json` for a value already serialized, so a caller can serialize
/// under a lock and write after releasing it. `private` writes it as
/// `atomic_json_private` does (on Unix, readable by its owner only).
pub(crate) fn atomic_bytes(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    atomic_bytes_and(path, bytes, private, || Ok(()))
}

/// `atomic_bytes`, calling `renamed` as soon as the new file has replaced
/// the old one, before the directory is synced. From then on a reader of
/// `path` sees the new bytes, even when this returns an error.
pub(crate) fn atomic_bytes_and(
    path: &Path,
    bytes: &[u8],
    private: bool,
    renamed: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("record has no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    let mut file = if private {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file
    } else {
        File::create(&tmp)?
    };
    #[cfg(windows)]
    let mut file = {
        let _ = private;
        // This directory inherits the installer's SYSTEM/Administrators DACL.
        File::create(&tmp)?
    };
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    crate::filesystem::rename(&tmp, path)?;
    renamed()?;
    crate::filesystem::sync_directory(parent)
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    serde_json::from_slice(&fs::read(path)?).map_err(invalid)
}

pub(crate) fn recover_output_tail(directory: &Path, job: &Job) {
    let committed: BTreeSet<_> = job.chunks.iter().map(|chunk| chunk.seq).collect();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(job_id = %job.id, %error, "cannot inspect uncommitted output tail");
            return;
        }
    };
    // Chunk data precedes index publication. Anything outside the persisted
    // index is an incomplete append or retired history, not a replay record.
    for entry in entries.flatten() {
        let sequence = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("chunk-"))
            .and_then(|sequence| sequence.parse::<u64>().ok());
        if sequence.is_some_and(|sequence| !committed.contains(&sequence)) {
            if let Err(error) = fs::remove_file(entry.path()) {
                tracing::warn!(job_id = %job.id, %error, "cannot remove uncommitted output tail");
            }
        }
    }
    remove_unreferenced_returned(directory, job);
}

/// Remove returned-file directories the record does not reference: a
/// collection interrupted before its record was saved, or bytes whose expiry
/// was saved before their deletion finished.
pub(crate) fn remove_unreferenced_returned(directory: &Path, job: &Job) {
    let kept = job
        .returned
        .as_ref()
        .filter(|returned| !returned.expired)
        .map(|returned| returned.directory.as_str());
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(RETURNED_DIRECTORY_PREFIX) && Some(name) != kept {
            if let Err(error) = fs::remove_dir_all(entry.path()) {
                tracing::warn!(job_id = %job.id, %error, "cannot remove unreferenced returned files");
            }
        }
    }
}

fn write_json(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(invalid)?;
    writer.write_all(&bytes)
}

pub(crate) fn mint_ms(id: Uuid) -> u64 {
    id.get_timestamp()
        .map(|t| {
            let (s, n) = t.to_unix();
            s * 1000 + u64::from(n) / 1_000_000
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn first_job_record_is_complete_before_uuid_directory_is_published() {
        let scratch = ScratchDir::new("runner-first-record");
        let jobs = scratch.join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        let job = Job::new(Uuid::now_v7(), None, 123, 1, 2);
        let path = jobs.join(job.id.to_string()).join("record.json");
        let mut inspected = false;
        atomic_job_json_and(&path, &job, |staging| {
            inspected = true;
            assert!(!path.parent().unwrap().exists());
            assert_eq!(fs::read_dir(&jobs)?.count(), 0);
            assert_eq!(read_json::<Job>(&staging.join("record.json"))?.id, job.id);
            Ok(())
        })
        .unwrap();
        assert!(inspected);
        assert_eq!(read_json::<Job>(&path).unwrap().id, job.id);
        let chunk = path.with_file_name("chunk-0");
        fs::write(&chunk, b"retained").unwrap();
        atomic_job_json(&path, &job).unwrap();
        assert_eq!(fs::read(chunk).unwrap(), b"retained");
    }

    #[test]
    fn interrupted_first_job_record_never_leaves_a_canonical_directory() {
        let scratch = ScratchDir::new("runner-interrupted-record");
        let jobs = scratch.join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        let job = Job::new(Uuid::now_v7(), None, 123, 1, 2);
        let path = jobs.join(job.id.to_string()).join("record.json");
        let interrupted = std::panic::catch_unwind(|| {
            let _ = atomic_job_json_and(&path, &job, |_| panic!("interrupted publication"));
        });
        assert!(interrupted.is_err());
        assert!(!path.parent().unwrap().exists());
        assert_eq!(fs::read_dir(&jobs).unwrap().count(), 0);
        assert_eq!(
            fs::read_dir(scratch.join("job-staging")).unwrap().count(),
            1
        );
        atomic_job_json(&path, &job).unwrap();
        assert_eq!(read_json::<Job>(&path).unwrap().id, job.id);
    }

    #[test]
    fn chunk_metadata_clone_shares_until_mutation_and_keeps_legacy_array_format() {
        let mut chunks = Chunks::default();
        chunks.push_back(Chunk {
            seq: 0,
            stream: OutputStream::Stdout,
            length: 7,
        });
        let snapshot = chunks.clone();
        assert!(Arc::ptr_eq(&chunks.0, &snapshot.0));
        let bytes = serde_json::to_vec(&chunks).unwrap();
        let legacy: VecDeque<Chunk> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].seq, 0);
        assert_eq!(legacy[0].length, 7);
        assert_eq!(bytes, serde_json::to_vec(&legacy).unwrap());
        chunks.push_back(Chunk {
            seq: 1,
            stream: OutputStream::Stderr,
            length: 9,
        });
        assert_eq!(snapshot.len(), 1);
        assert_eq!(chunks.len(), 2);
        assert!(!Arc::ptr_eq(&chunks.0, &snapshot.0));
    }

    #[derive(Default)]
    struct CountingWriter {
        bytes: Vec<u8>,
        writes: usize,
    }

    impl Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn json_serialization_uses_one_bulk_write() {
        let value: Vec<_> = (0..10_000).map(|n| format!("record-{n}")).collect();
        let mut writer = CountingWriter::default();
        write_json(&mut writer, &value).unwrap();
        assert_eq!(writer.bytes, serde_json::to_vec(&value).unwrap());
        assert_eq!(
            writer.writes, 1,
            "JSON tokens must not become separate file writes"
        );
    }

    #[test]
    fn json_records_keep_legacy_bytes_and_private_publication_rules() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let scratch = ScratchDir::new("runner-json");
        let path = scratch.join("record.json");
        let mut job = Job::new(Uuid::now_v7(), None, 123, 1, 2);
        job.terminal(Outcome::Exit { code: 0 }, 456);
        let mut legacy_bytes = Vec::new();
        serde_json::to_writer(&mut legacy_bytes, &job).unwrap();
        atomic_json_private(&path, &job).unwrap();
        assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let legacy_job: Job = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        assert_eq!(legacy_job.id, job.id);
        assert_eq!(
            read_json::<Job>(&path).unwrap().next_seq,
            legacy_job.next_seq
        );
        assert!(!path.with_extension("tmp").exists());
        let victim = scratch.join("victim");
        fs::write(&victim, b"untouched").unwrap();
        symlink(&victim, path.with_extension("tmp")).unwrap();
        assert!(atomic_json_private(&path, &job).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");
        assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
    }
}
