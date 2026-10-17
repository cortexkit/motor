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
    /// Why the job was stopped by its memory limit, when it was. The terminal
    /// record sent to callers (`TerminalRecord` in cortexkit-exec-remote-types)
    /// has no free-form detail field outside refusals, so the text is kept here
    /// and also written to the job's stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_use: Option<ReservationUse>,
    /// Fixed execution end, independent of delayed launcher-accounting delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_ended_ms: Option<u64>,
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
            chunks: Chunks::default(),
            next_seq: 0,
            output_bytes: 0,
            weight,
            cpus: Vec::new(),
            order,
            base: false,
            writer_descriptor: None,
            kill: None,
            stdout_offset: 0,
            stderr_offset: 0,
            durable_output: false,
            monitoring_error: None,
            memory_limit_detail: None,
            reservation_use: None,
            execution_ended_ms: None,
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
        if remote {
            terminal = terminal.with_workspace_changes(Vec::new());
        }
        self.terminal = Some(terminal);
        self.finished_ms = Some(now);
        self.state = JobState::Terminal;
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

pub(crate) fn atomic_json_private(path: &Path, value: &impl Serialize) -> io::Result<()> {
    atomic_json_mode(path, value, true)
}

fn atomic_json_mode(path: &Path, value: &impl Serialize, private: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("record has no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension("tmp");
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
    write_json(&mut file, value)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    File::open(parent)?.sync_all()
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
