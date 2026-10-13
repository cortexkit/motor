use crate::{
    backend::{self, setup_step, Backend, BackendKind, Prepared, Setup},
    cpu::CpuTopology,
    platform,
    storage::*,
    workspace::{self, Trees},
};
use fs2::FileExt;
use motor_protocol::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Backfill unused capacity briefly, then drain for the oldest waiting head.
// Rotation alone lets a stream of new workspaces starve a recently used one.
const BACKFILL_GRACE_MS: u64 = 60_000;

#[path = "accounting.rs"]
mod accounting;
#[path = "disk.rs"]
mod disk;
#[path = "health.rs"]
mod health;
#[path = "migration.rs"]
mod migration;

/// Disk headroom for concurrent build-target growth and scratch files, not a quota.
pub const DEFAULT_FREE_SPACE_FLOOR_BYTES: u64 = 128 * 1024 * 1024 * 1024;

#[cfg(test)]
type JobWriteHook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
#[cfg(test)]
type AccountingReportHook = Arc<dyn Fn(Uuid, &[u8]) + Send + Sync>;
pub type FreeSpaceProbe = Arc<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

/// Paths are instance-specific except build_slot_root, which is machine-wide.
#[derive(Clone)]
pub struct Config {
    pub instance: String,
    pub state_root: PathBuf,
    pub build_slot_root: PathBuf,
    pub max_queue_depth: usize,
    pub max_weight: u32,
    pub rustc_slots: u32,
    /// Maximum size of the per-instance compiler cache, in GiB.
    pub sccache_size_gib: u64,
    #[cfg(target_os = "linux")]
    pub(crate) writer_account: Option<crate::isolation::WriterAccount>,
    pub backend: BackendKind,
    pub vm_config: Option<PathBuf>,
    pub(crate) windows: Option<Arc<crate::windows::VmConfig>>,
    pub(crate) windows_runtime: Arc<crate::windows::Runtime>,
    #[cfg(test)]
    pub(crate) windows_command: Option<crate::windows::CommandHook>,
    /// Binary run as the trusted Windows preparation helper. Production uses
    /// the runner's own executable; a test harness binary can't serve, because
    /// it reads `windows-prepare <plan>` as a test filter and exits 0 having
    /// written nothing.
    #[cfg(test)]
    pub(crate) windows_prep_executable: Option<PathBuf>,
    #[cfg(test)]
    pub(crate) windows_vm: Option<crate::windows::VmHook>,
    #[cfg(test)]
    pub(crate) windows_socket_pair: bool,
    #[cfg(test)]
    pub(crate) windows_clock: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    #[cfg(test)]
    pub(crate) windows_phase: Option<Arc<dyn Fn(motor_winagent::Phase) + Send + Sync>>,
    pub free_space_floor_bytes: u64,
    /// Only catalogued workspaces beneath these roots may be pressure-evicted.
    pub workspace_roots: Vec<PathBuf>,
    /// Probe available bytes on the filesystem containing the supplied path.
    pub free_space_probe: FreeSpaceProbe,
    /// Linux production uses systemd services under motor-job. The portable
    /// in-process test backend deliberately leaves this disabled.
    pub isolate_jobs: bool,
    /// Outbound networking is opt-in and requires a probed isolated host backend.
    pub job_network: Option<crate::network::JobNetwork>,
    pub(crate) job_network_unavailable: String,
    #[cfg(test)]
    pub(crate) job_network_hook: Option<crate::network::Hook>,
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub(crate) background_deletes: Arc<std::sync::atomic::AtomicUsize>,
    /// The background warm build's commands, run in order with `&&`. Empty
    /// (the default) derives them per repository from the tree being warmed,
    /// see `workspace::warm_command`; a non-empty list overrides that for
    /// every repository.
    pub base_commands: Vec<String>,
    /// Keep fixtures focused on foreground jobs without using drain to suppress
    /// maintenance: draining also refuses accepted jobs that are still queued.
    #[cfg(test)]
    pub(crate) disable_warm_builds: bool,
    #[cfg(test)]
    pub(crate) writer_unavailable: bool,
    #[cfg(test)]
    pub(crate) before_gate: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    pub(crate) fail_materialize: bool,
    #[cfg(test)]
    pub(crate) cpu_topology: Option<CpuTopology>,
    #[cfg(test)]
    pub(crate) memory_floor_gib: Option<u64>,
    #[cfg(test)]
    pub(crate) before_job_write: Option<JobWriteHook>,
    #[cfg(test)]
    pub(crate) systemd_command: Option<crate::systemd::CommandHook>,
    #[cfg(test)]
    pub(crate) accounting_report: Option<AccountingReportHook>,
    #[cfg(test)]
    pub(crate) test_processes: Option<Arc<Mutex<crate::test_cleanup::Processes>>>,
    /// Called with an operation label so tests can hold filesystem work or the
    /// admission handoff open without holding the runner state lock.
    #[cfg(test)]
    pub(crate) long_work: Option<Arc<dyn Fn(&'static str) + Send + Sync>>,
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) shared_read_roots: Option<(PathBuf, PathBuf, Option<PathBuf>)>,
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(all(test, target_os = "linux"))]
pub(crate) async fn net_operator_case(mode: &str) {
    tests::network::operator_case(mode).await;
}

pub(crate) fn transfer_refusal(error: &io::Error) -> RefusalReason {
    if error.kind() == io::ErrorKind::TimedOut {
        RefusalReason::Unknown("transfer_stalled".into())
    } else {
        RefusalReason::BundleRejected
    }
}

pub(crate) fn workspace_setup_refusal(step: &str, error: &io::Error) -> RefusalReason {
    RefusalReason::Unknown(format!("workspace_setup_failed: {step}: {error}"))
}

/// Request environment values shorter than this stay visible in refusal details.
pub(crate) const REDACTED_ENV_VALUE_MIN_BYTES: usize = 8;

/// vCPUs a background warm build claims when the host has them.
const WARM_BUILD_VCPUS: u32 = 4;

fn refuse_before_start(
    health: &Mutex<health::Health>,
    job: &mut Job,
    reason: RefusalReason,
    error: impl std::fmt::Display,
    detail: Option<String>,
    now: u64,
) {
    if let Some(detail) = detail.as_deref() {
        tracing::warn!(job_id = %job.id, ?reason, error = %error, detail, "job refused_before_start");
    } else {
        tracing::warn!(job_id = %job.id, ?reason, error = %error, "job refused_before_start");
    }
    health.lock().unwrap().refusal(now, &reason);
    job.terminal(Outcome::RefusedBeforeStart { reason }, now);
    if let Some(detail) = detail {
        job.terminal = job.terminal.take().map(|t| t.with_refusal_detail(detail));
    }
}

impl Config {
    pub fn new(state_root: impl Into<PathBuf>, instance: impl Into<String>) -> Self {
        let state_root = state_root.into();
        Self {
            instance: instance.into(),
            build_slot_root: state_root.join("build-slots"),
            state_root,
            max_queue_depth: 64,
            max_weight: 28,
            rustc_slots: 24,
            sccache_size_gib: 200,
            #[cfg(target_os = "linux")]
            writer_account: None,
            backend: BackendKind::Host,
            vm_config: None,
            windows: None,
            windows_runtime: Arc::new(crate::windows::Runtime::default()),
            #[cfg(test)]
            windows_vm: None,
            #[cfg(test)]
            windows_socket_pair: false,
            #[cfg(test)]
            windows_clock: None,
            #[cfg(test)]
            windows_phase: None,
            #[cfg(test)]
            windows_command: None,
            #[cfg(test)]
            windows_prep_executable: None,
            free_space_floor_bytes: DEFAULT_FREE_SPACE_FLOOR_BYTES,
            workspace_roots: vec![PathBuf::from("/Users")],
            free_space_probe: Arc::new(|path| fs2::available_space(path)),
            isolate_jobs: false,
            job_network: None,
            job_network_unavailable: "--job-network veth is not enabled".into(),
            #[cfg(test)]
            job_network_hook: None,
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64
            }),
            background_deletes: Arc::default(),
            base_commands: Vec::new(),
            #[cfg(test)]
            disable_warm_builds: false,
            #[cfg(test)]
            writer_unavailable: false,
            #[cfg(test)]
            before_gate: None,
            #[cfg(test)]
            fail_materialize: false,
            #[cfg(test)]
            cpu_topology: None,
            #[cfg(test)]
            memory_floor_gib: None,
            #[cfg(test)]
            before_job_write: None,
            #[cfg(test)]
            systemd_command: None,
            #[cfg(test)]
            accounting_report: None,
            #[cfg(test)]
            test_processes: None,
            #[cfg(test)]
            long_work: None,
            #[cfg(all(test, target_os = "linux"))]
            shared_read_roots: None,
        }
    }
    pub fn state_dir(&self) -> PathBuf {
        self.state_root.join(&self.instance)
    }
    pub fn sccache_dir(&self) -> PathBuf {
        self.state_dir().join("sccache/fleet")
    }
    /// Endpoint for one trusted Linux build that populates the compiler cache,
    /// independent of the runner's state paths.
    #[cfg(target_os = "linux")]
    pub fn writer_sccache_server_uds(&self, fill: Uuid) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use sha2::{Digest, Sha256};

        // sccache expects the literal backslash-x00 prefix for abstract sockets.
        // Hash the instance to bound the name even for long instance names. Linux
        // abstract sockets belong to the network namespace: the writer unit's
        // private network lets only its own processes reach this server.
        let instance = URL_SAFE_NO_PAD.encode(Sha256::digest(self.instance.as_bytes()));
        format!(r"\x00motor-sccache-{instance}-{fill}")
    }
    pub(crate) fn writer_scratch_dir(&self) -> PathBuf {
        self.state_dir().join("writer-scratch")
    }
    pub(crate) fn writer_enabled(&self) -> bool {
        #[cfg(test)]
        if self.writer_unavailable {
            return false;
        }
        if !self.isolate_jobs {
            // Portable fixtures model maintenance without host accounts.
            return true;
        }
        #[cfg(target_os = "linux")]
        return self.writer_account.is_some();
        #[cfg(not(target_os = "linux"))]
        false
    }
    /// The execution backend, not the runner host OS, determines the platform.
    pub fn served_platform(&self) -> Platform {
        match self.backend {
            BackendKind::Host | BackendKind::Vm => Platform::Linux,
            BackendKind::WindowsVm => Platform::Windows,
        }
    }
    pub(crate) fn windows_instance(&self) -> bool {
        self.backend == BackendKind::WindowsVm
    }
    fn configure_backend(&mut self) -> io::Result<()> {
        if self.windows_instance() {
            let path = self
                .vm_config
                .as_ref()
                .ok_or_else(|| invalid("--backend windows-vm requires --vm-config <file>"))?;
            self.windows = Some(Arc::new(crate::windows::VmConfig::load(path)?));
            self.max_weight = 4;
            self.workspace_roots.clear();
        }
        Ok(())
    }
    pub fn socket_path(&self) -> PathBuf {
        self.state_dir().join("runner.sock")
    }
}

pub(crate) fn oom_memory_limit_detail(_config: &Config, weight: u32, capacity: u32) -> String {
    #[cfg(test)]
    let floor_gib = _config.memory_floor_gib;
    #[cfg(test)]
    let limit = crate::backend::memory_limit_gib(weight, floor_gib);
    #[cfg(not(test))]
    let limit = crate::backend::memory_limit_gib(weight);

    // Doubling a small request can stay at the memory floor. Start from four
    // vCPUs so the recommendation actually buys more memory, and cap it at the
    // largest job this host can admit.
    let more = (weight.max(4) * 2).min(capacity);
    let advice = if more > weight {
        #[cfg(test)]
        let more_limit = crate::backend::memory_limit_gib(more, floor_gib);
        #[cfg(not(test))]
        let more_limit = crate::backend::memory_limit_gib(more);
        format!(
            " Request more vCPUs, for example linux,{more}c, to raise the limit to {more_limit} GiB."
        )
    } else {
        format!(" The job is already at the maximum of {more} vCPUs ({limit} GiB).")
    };
    format!(
        "the job's memory limit of {limit} GiB was reached; the kernel killed a process and the job was stopped.{advice}"
    )
}

#[derive(Clone)]
pub(crate) struct QueueProgress {
    entry: Arc<std::sync::atomic::AtomicU64>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}
impl QueueProgress {
    pub(crate) fn elapsed_ms(&self) -> Option<u64> {
        let entry = self.entry.load(std::sync::atomic::Ordering::Acquire);
        (entry != NO_QUEUE).then(|| (self.clock)().saturating_sub(entry))
    }
}

struct Inner {
    writer: Option<Uuid>,
    #[cfg(target_os = "linux")]
    cache_fills: BTreeMap<Uuid, crate::writer::CacheFill>,
    drain_deadline: Option<std::time::Instant>,
    jobs: BTreeMap<Uuid, Job>,
    catalog: Catalog,
    rotation: BTreeMap<String, u64>,
    order: u64,
    dispatch: u64,
    dropping: BTreeSet<String>,
    evicting_dependencies: BTreeSet<PathBuf>,
    evicting_caches: BTreeSet<PathBuf>,
    maintained_ms: u64,
    /// Workspaces being set up without the state lock held, by workspace key
    /// (`base:<root>` for a warm build): a prepare materializing a workspace,
    /// or a job whose checkout and target are being made ready before launch.
    /// Scheduling treats a reserved key, and the siblings it checks out, as in
    /// use.
    reserved: BTreeMap<String, Reservation>,
    /// Queued jobs whose setup is in flight. They still count toward queue
    /// depth, and toward the weight cap as if they were running.
    starting: BTreeSet<Uuid>,
    transfers: BTreeSet<Uuid>,
    preparing: BTreeMap<Uuid, Prepare>,
    /// Requests between the eligibility check and job insertion. Each call owns
    /// a distinct token, so duplicate calls cannot release each other's paths.
    admitting: BTreeMap<Uuid, Run>,
    last_eviction: Option<DiskEviction>,
    eviction_retry_after: BTreeMap<PathBuf, u64>,
    kill_retries: BTreeMap<Uuid, (u32, std::time::Instant)>,
    finalizing: BTreeSet<Uuid>,
    /// A real exit status delivered while the finalization owner is collecting
    /// changes or retrying persistence takes precedence over a monitoring error.
    exit_statuses: BTreeMap<Uuid, backend::ExecutionOutcome>,
    cpu_times: BTreeMap<Uuid, u64>,
    pending_start_refusals: BTreeSet<Uuid>,
}

fn nested_workspace_detail(
    inner: &Inner,
    workspace: &str,
    repository: &str,
    siblings: &[SiblingSnapshot],
) -> Option<String> {
    // Prepared checkouts may be owned by a job account; compare path components
    // so similarly named sibling directories remain independent workspaces.
    let mut candidates = std::iter::once(("workspace", workspace))
        .chain(
            siblings
                .iter()
                .map(|sibling| ("sibling", sibling.canonical_path.as_str())),
        )
        .chain(std::iter::once(("repository", repository)));
    candidates.find_map(|(kind, candidate)| {
        let candidate_path = Path::new(candidate);
        inner.catalog.workspaces.keys().find_map(|known| {
            let known_path = Path::new(known);
            (candidate_path != known_path && candidate_path.starts_with(known_path)).then(|| {
                format!(
                    "{kind} {candidate} is inside runner workspace {known}; nested workspaces aren't supported, so move it outside any remote workspace"
                )
            })
        })
    })
}

struct Reservation {
    repository: String,
    commit: String,
    siblings: Vec<SiblingSnapshot>,
    since_ms: u64,
}

struct Core {
    draining: std::sync::atomic::AtomicBool,
    /// Separate from the fsync/state lock so a slow persistence operation cannot
    /// prevent the server from observing its shutdown deadline.
    drain_control: Mutex<Option<DrainWaiter>>,
    config: Config,
    backend: Box<dyn Backend>,
    trees: Trees,
    inner: Mutex<Inner>,
    /// Serialize pressure cleanup, without holding the output/persistence lock.
    disk: Mutex<()>,
    health: Mutex<health::Health>,
    background_deletes: Arc<std::sync::atomic::AtomicUsize>,
    _lock: File,
    rustc_version: String,
    cpus: CpuTopology,
}

#[derive(Clone)]
pub(crate) struct DrainWaiter(Arc<Mutex<DrainState>>);

struct DrainState {
    deadline: std::time::Instant,
    exit_after: bool,
    result: Option<DrainResult>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DrainResult {
    TimedOut,
    Recoverable { jobs: u64 },
    Resumed,
}

#[derive(Clone)]
pub struct Runner {
    core: Arc<Core>,
}

pub(crate) enum Admission {
    Accepted(u32),
    Refused(RefusalReason),
    Duplicate,
    DiskFull(RunnerDiskFull),
}

impl Runner {
    /// Opening a table performs crash recovery before any channel is served.
    /// An exclusive instance lock prevents a second daemon from recovering live jobs.
    pub fn open(config: Config) -> io::Result<Self> {
        let backend = backend::select(&config)?;
        Self::open_with_backend(config, backend)
    }
    fn open_with_backend(mut config: Config, backend: Box<dyn Backend>) -> io::Result<Self> {
        config.configure_backend()?;
        if config.instance.is_empty()
            || !config
                .instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || config.max_weight == 0
            || config.rustc_slots == 0
            || config.sccache_size_gib == 0
            || config
                .sccache_size_gib
                .checked_mul(1024 * 1024 * 1024)
                .is_none()
            || config
                .workspace_roots
                .iter()
                .any(|p| !workspace::absolute(&p.to_string_lossy()) || p.parent().is_none())
        {
            return Err(invalid("invalid runner configuration"));
        }
        backend.validate(&config)?;
        #[cfg(test)]
        let cpus = config
            .cpu_topology
            .clone()
            .map(Ok)
            .unwrap_or_else(CpuTopology::read)?;
        #[cfg(not(test))]
        let cpus = CpuTopology::read()?;
        let cpus = if let Some(vm) = &config.windows {
            CpuTopology::fixed(vm.cpu_set.clone())?
        } else {
            cpus
        };
        #[cfg(target_os = "linux")]
        if config.isolate_jobs && !config.windows_instance() {
            crate::isolation::validate_account()?;
            #[cfg(test)]
            if let Some((cargo_home, rustup_home, bun_bin)) = &config.shared_read_roots {
                crate::isolation::prepare_shared_read_access_from(
                    cargo_home,
                    rustup_home,
                    bun_bin.as_deref(),
                )?;
            } else {
                crate::isolation::prepare_shared_read_access()?;
            }
            #[cfg(not(test))]
            crate::isolation::prepare_shared_read_access()?;
        }
        let state = config.state_dir();
        fs::create_dir_all(state.join("jobs"))?;
        if config.windows_instance() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(state.join("jobs"), fs::Permissions::from_mode(0o700))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("daemon.lock"))?;
        lock.try_lock_exclusive()?;
        #[cfg(target_os = "linux")]
        if config.isolate_jobs && !config.windows_instance() {
            // A bad cache account must not take the foreground runner down.
            // Keep the validated identity only after its storage is also safe.
            config.writer_account =
                match crate::isolation::validate_writer_account().and_then(|account| {
                    crate::isolation::prepare_writer_shared_read_access()?;
                    Ok(account)
                }) {
                    Ok(account) => Some(account),
                    Err(error) => {
                        tracing::warn!(%error, "dependency writer disabled; cache reads may miss");
                        None
                    }
                };
        }
        if config.windows_instance() {
            use std::os::unix::fs::PermissionsExt;
            lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let trees = Trees {
            dependency_locks: Mutex::new(BTreeMap::new()),
            mirrors: state.join("mirrors"),
            state: state.clone(),
            compacting: Arc::default(),
        };
        fs::create_dir_all(&trees.mirrors)?;
        if config.windows_instance() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&trees.mirrors, fs::Permissions::from_mode(0o700))?;
        }
        backend.initialize(&config)?;
        crate::network::initialize(&mut config);
        if state.join("spools").exists() {
            fs::remove_dir_all(state.join("spools"))?;
        }
        fs::create_dir_all(state.join("spools"))?;
        if config.windows_instance() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(state.join("spools"), fs::Permissions::from_mode(0o700))?;
        }
        let now = (config.clock)();
        let health = Mutex::new(health::Health::new(now));
        let mut jobs = BTreeMap::new();
        let mut recovered = Vec::new();
        let mut lost = Vec::new();
        for entry in fs::read_dir(state.join("jobs"))? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let record = path.join("record.json");
            if !record.exists() {
                fs::remove_dir_all(path)?;
                continue;
            }
            let job: Job = read_json(&record)?;
            jobs.insert(job.id, job);
        }
        let interrupted: Vec<_> = jobs
            .values()
            .filter(|job| job.terminal.is_none())
            .cloned()
            .collect();
        let interrupted_writers: Vec<_> = interrupted
            .iter()
            .filter(|job| job.base && job.writer_descriptor.is_some())
            .cloned()
            .collect();
        // Dependency-writer services may outlive the daemon. Stop their entire
        // process group before removing active build files or starting another
        // cache-writing service; foreground services still use normal recovery.
        for job in interrupted.iter().filter(|job| job.base) {
            if let Some(unit) = job.process.and_then(|p| p.unit) {
                #[cfg(any(target_os = "linux", test))]
                {
                    let name = backend.unit_name(&config, unit);
                    let stopped = backend::systemctl(&config, &["stop", &name])?;
                    if !stopped.status.success()
                        && backend.unit_alive(&config, job.process)? == Some(true)
                    {
                        return Err(io::Error::other("cannot stop surviving dependency writer"));
                    }
                    let _ = backend::systemctl(&config, &["reset-failed", &name]);
                }
                #[cfg(not(any(target_os = "linux", test)))]
                {
                    let _ = unit;
                    backend.cancel(&config, job, true)?;
                }
            } else {
                backend.cancel(&config, job, true)?;
            }
            if job.process.is_some_and(|p| p.unit.is_some())
                && backend.unit_alive(&config, job.process)? != Some(false)
            {
                return Err(io::Error::other("dependency writer survived startup stop"));
            }
        }
        for mut job in interrupted.iter().filter(|job| job.base).cloned() {
            refuse_before_start(
                &health,
                &mut job,
                RefusalReason::Unknown("lost_to_restart".into()),
                "runner restarted during a background fill",
                Some("runner restarted during a background fill".into()),
                now,
            );
            atomic_json(
                &state
                    .join("jobs")
                    .join(job.id.to_string())
                    .join("record.json"),
                &job,
            )?;
            jobs.insert(job.id, job);
        }
        #[cfg(target_os = "linux")]
        if let Some(account) = config.writer_account {
            if let Err(error) =
                crate::isolation::prepare_writer_cache(&config.sccache_dir(), account)
            {
                tracing::warn!(%error, "dependency writer disabled; cache storage is unavailable");
                config.writer_account = None;
            }
        }
        let foreground: Vec<_> = interrupted
            .iter()
            .filter(|job| !job.base)
            .cloned()
            .collect();
        let reattached = backend.reconcile(&config, &foreground)?;
        for mut job in interrupted.into_iter().filter(|job| !job.base) {
            let record = state
                .join("jobs")
                .join(job.id.to_string())
                .join("record.json");
            if (config.windows_instance() && job.started_ms.is_none())
                || (job.pending() && job.started_ms.is_none() && job.process.is_none())
            {
                refuse_before_start(
                    &health,
                    &mut job,
                    RefusalReason::Unknown("lost_to_restart".into()),
                    "runner restarted before the job could start",
                    Some("runner restarted before the job could start".into()),
                    now,
                );
                atomic_json(&record, &job)?;
            } else if reattached.contains(&job.id) {
                // Preserve workspace/weight ownership and resume monitoring.
                job.state = JobState::Running;
                recovered.push(job.id);
            } else {
                job.terminal(Outcome::OutcomeUnknown, now);
                job.state = JobState::Lost;
                lost.push((job.id, job.process));
                atomic_json(&record, &job)?;
            }
            jobs.insert(job.id, job);
        }
        let mut catalog: Catalog = if state.join("catalog.json").exists() {
            read_json(&state.join("catalog.json"))?
        } else {
            Catalog::default()
        };
        if config.windows_instance() {
            // Catalog entries are caller checkout paths on the build server, and
            // those checkouts belong to the Linux instance. The Windows instance
            // works only in its own staging, so it must never touch them, not
            // even to look: an entry from an older catalog is dropped unread.
            catalog.workspaces.clear();
            catalog.sibling_commits.clear();
            catalog.repositories.clear();
        }
        if !config.windows_instance() {
            migration::identify_legacy_checkouts(&mut catalog, &jobs, &config.instance);
        }
        // A saved dependency-writer job also preserves an approved tip if the
        // daemon stopped between persisting the job and its catalog request.
        for job in interrupted_writers {
            let Some(request) = job.request else { continue };
            let repository = catalog
                .repositories
                .entry(request.repository_root.clone())
                .or_default();
            if repository.requested_descriptor.is_none() {
                repository.requested_descriptor = job.writer_descriptor;
                repository.desired = Some(BaseRequest {
                    tip: request.snapshot_commit,
                    workspace: request.cwd,
                    siblings: vec![],
                    git_dependencies: vec![],
                    command: Some(request.command),
                });
            }
        }
        for repository in catalog.repositories.values_mut() {
            // Descriptors are recorded only for clients advertising a trusted
            // default-branch tip; older clients' arbitrary HEAD tips cannot be
            // replayed with write access to the shared compiler cache.
            if repository.requested_descriptor.is_none() {
                repository.desired = None;
            }
            repository.failed_descriptor = None;
            if repository.desired.is_some() {
                repository.requested_descriptor = Some("restart".into());
            }
        }
        if config.windows_instance() {
            atomic_json_private(&state.join("catalog.json"), &catalog)?;
        } else {
            atomic_json(&state.join("catalog.json"), &catalog)?;
        }
        let order = jobs.values().map(|j| j.order).max().unwrap_or(0);
        let rustc_version = if let Some(vm) = &config.windows {
            vm.toolchain_version.clone()
        } else {
            std::process::Command::new("rustc")
                .arg("--version")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?
        };
        let runner = Self {
            core: Arc::new(Core {
                draining: std::sync::atomic::AtomicBool::new(false),
                drain_control: Mutex::new(None),
                background_deletes: config.background_deletes.clone(),
                config,
                backend,
                trees,
                inner: Mutex::new(Inner {
                    writer: None,
                    #[cfg(target_os = "linux")]
                    cache_fills: BTreeMap::new(),
                    drain_deadline: None,
                    jobs,
                    catalog,
                    order,
                    rotation: BTreeMap::new(),
                    dispatch: 0,
                    dropping: BTreeSet::new(),
                    maintained_ms: now,
                    reserved: BTreeMap::new(),
                    starting: BTreeSet::new(),
                    transfers: BTreeSet::new(),
                    preparing: BTreeMap::new(),
                    admitting: BTreeMap::new(),
                    evicting_dependencies: BTreeSet::new(),
                    evicting_caches: BTreeSet::new(),
                    last_eviction: None,
                    eviction_retry_after: BTreeMap::new(),
                    kill_retries: BTreeMap::new(),
                    finalizing: BTreeSet::new(),
                    exit_statuses: BTreeMap::new(),
                    cpu_times: BTreeMap::new(),
                    pending_start_refusals: BTreeSet::new(),
                }),
                disk: Mutex::new(()),
                health,
                _lock: lock,
                rustc_version,
                cpus,
            }),
        };
        #[cfg(test)]
        tests::register_runner(&runner);
        // Deletions a previous run moved aside but did not finish.
        if let Ok(entries) = fs::read_dir(runner.trash()) {
            for entry in entries.flatten() {
                runner.purge(entry.path());
            }
        }
        runner.migrate_legacy_storage()?;
        let scratch = runner.core.config.writer_scratch_dir();
        if scratch.exists() {
            #[cfg(target_os = "linux")]
            if runner.core.config.isolate_jobs {
                crate::isolation::restore_runner_access(&scratch)?;
            }
            runner.discard(&scratch)?;
        }
        runner.maintenance()?;
        for (id, process) in lost {
            runner.recovery_diagnostic(id, process, false)?;
        }
        for id in recovered {
            let runner = runner.clone();
            std::thread::spawn(move || {
                #[cfg(target_os = "linux")]
                let _runtime = crate::isolation::recovered_runtime(&runner.core.trees.state, id)
                    .tracking(runner.core.background_deletes.clone());
                runner.monitor_unit(id);
            });
        }
        let weak = Arc::downgrade(&runner.core);
        std::thread::spawn(move || loop {
            let Some(core) = weak.upgrade() else {
                break;
            };
            let runner = Runner { core };
            if let Err(error) = runner.tick() {
                tracing::error!(%error, "runner dispatch failed");
            }
            drop(runner);
            std::thread::park_timeout(Duration::from_millis(25));
        });
        Ok(runner)
    }
    pub fn config(&self) -> &Config {
        &self.core.config
    }
    pub(crate) fn safe_refusal_detail(
        &self,
        text: impl std::fmt::Display,
        environment_values: impl IntoIterator<Item = String>,
    ) -> String {
        let mut detail = text.to_string();
        // Short values such as "0" or "true" aren't secrets, and replacing them
        // would mangle every number and word in the detail.
        let mut environment_values: Vec<_> = environment_values
            .into_iter()
            .filter(|value| value.len() >= REDACTED_ENV_VALUE_MIN_BYTES)
            .collect();
        environment_values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        for value in environment_values {
            detail = detail.replace(&value, "runner environment");
        }
        let mut internal_paths = vec![
            self.core.trees.state.to_string_lossy().into_owned(),
            self.core
                .config
                .build_slot_root
                .to_string_lossy()
                .into_owned(),
            "/srv/build".to_owned(),
        ];
        internal_paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
        for path in internal_paths {
            if !path.is_empty() {
                detail = detail.replace(&path, "runner state");
            }
        }
        let mut end = detail.len().min(REFUSAL_DETAIL_MAX_BYTES);
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        detail
    }
    pub fn tree_hash(&self, path: &Path) -> io::Result<String> {
        self.core.trees.tree_hash(path)
    }
    /// Admission and drain use the same lock, so every accepted job is included.
    pub fn begin_drain(&self, deadline: Duration, exit_after: bool) {
        self.begin_drain_request(deadline, exit_after);
    }
    pub(crate) fn begin_drain_request(&self, deadline: Duration, exit_after: bool) -> DrainWaiter {
        let requested_deadline = std::time::Instant::now() + deadline;
        let mut inner = self.core.inner.lock().unwrap();
        let mut drain_control = self.core.drain_control.lock().unwrap();
        let waiter = if let Some(waiter) = drain_control.as_ref() {
            let mut state = waiter.0.lock().unwrap();
            if state.result.is_none() && requested_deadline < state.deadline {
                state.deadline = requested_deadline;
                inner.drain_deadline = Some(requested_deadline);
                tracing::info!(
                    deadline_s = deadline.as_secs(),
                    "runner drain deadline shortened"
                );
            }
            // The first request in a drain episode owns exit behavior; repeated
            // requests can shorten the deadline but do not change that choice.
            waiter.clone()
        } else {
            let waiter = DrainWaiter(Arc::new(Mutex::new(DrainState {
                deadline: requested_deadline,
                exit_after,
                result: None,
            })));
            inner.drain_deadline = Some(requested_deadline);
            *drain_control = Some(waiter.clone());
            tracing::info!(deadline_s = deadline.as_secs(), "runner draining");
            waiter
        };
        self.core
            .draining
            .store(true, std::sync::atomic::Ordering::Release);
        waiter
    }
    pub(crate) fn drain_retry_after_ms(&self) -> Option<u64> {
        let waiter = self.core.drain_control.lock().unwrap().clone()?;
        let deadline = waiter.0.lock().unwrap().deadline;
        Some(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u64,
        )
    }
    pub fn drain_should_exit(&self) -> bool {
        let drain_control = self.core.drain_control.lock().unwrap();
        let Some(waiter) = drain_control.as_ref() else {
            return false;
        };
        let exit_after = waiter.0.lock().unwrap().exit_after;
        exit_after && self.drain_result_for(waiter).is_some()
    }
    /// Reopen admission after a deployment closed it. Running jobs continue;
    /// jobs already refused before start remain safe for callers to retry.
    pub fn resume(&self) {
        let mut inner = self.core.inner.lock().unwrap();
        inner.drain_deadline = None;
        if let Some(waiter) = self.core.drain_control.lock().unwrap().take() {
            let mut state = waiter.0.lock().unwrap();
            if state.result.is_none() {
                state.result = Some(DrainResult::Resumed);
            }
        }
        self.core
            .draining
            .store(false, std::sync::atomic::Ordering::Release);
        tracing::info!("runner admission resumed");
    }
    pub fn is_draining(&self) -> bool {
        self.core
            .draining
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn drain_result(&self) -> Option<bool> {
        let waiter = self.core.drain_control.lock().unwrap().clone()?;
        self.drain_result_for(&waiter)
    }
    pub(crate) fn drain_result_for(&self, waiter: &DrainWaiter) -> Option<bool> {
        self.drain_reply_for(waiter).map(|(timed_out, _)| timed_out)
    }
    pub(crate) fn drain_reply_for(&self, waiter: &DrainWaiter) -> Option<(bool, u64)> {
        let mut state = waiter.0.lock().unwrap();
        if let Some(result) = state.result {
            return Some(match result {
                DrainResult::TimedOut => (true, 0),
                DrainResult::Recoverable { jobs } => (false, jobs),
                DrainResult::Resumed => (false, 0),
            });
        }
        if std::time::Instant::now() >= state.deadline {
            state.result = Some(DrainResult::TimedOut);
            return Some((true, 0));
        }
        let inner = self.core.inner.try_lock().ok()?;
        if inner
            .jobs
            .values()
            .all(|j| j.terminal.is_some() || self.restart_recoverable(j))
            && inner.starting.is_empty()
            && inner.reserved.is_empty()
            && inner.preparing.is_empty()
            && inner.admitting.is_empty()
        {
            let jobs = inner.jobs.values().filter(|j| j.terminal.is_none()).count() as u64;
            state.result = Some(DrainResult::Recoverable { jobs });
            Some((false, jobs))
        } else {
            None
        }
    }
    fn restart_recoverable(&self, job: &Job) -> bool {
        // Linux units outlive the daemon and recovery resumes their output
        // monitor. Only a persisted Started proves launch progressed far enough;
        // setup, portable processes and Windows executions cannot reattach.
        cfg!(target_os = "linux")
            && !self.core.config.windows_instance()
            && job.started_ms.is_some()
            && job.started.is_some()
            && job.process.is_some_and(|process| process.unit.is_some())
    }
    fn now(&self) -> u64 {
        (self.core.config.clock)()
    }
    fn record_path(&self, id: Uuid) -> PathBuf {
        self.core
            .trees
            .state
            .join("jobs")
            .join(id.to_string())
            .join("record.json")
    }
    fn save(&self, job: &Job) -> io::Result<()> {
        #[cfg(test)]
        if let Some(write) = &self.core.config.before_job_write {
            write(&self.record_path(job.id))?;
        }
        atomic_json(&self.record_path(job.id), job)
    }
    fn catalog(&self, inner: &Inner) -> io::Result<()> {
        if self.core.config.windows_instance() {
            atomic_json_private(&self.core.trees.state.join("catalog.json"), &inner.catalog)
        } else {
            atomic_json(&self.core.trees.state.join("catalog.json"), &inner.catalog)
        }
    }
    fn unit_alive(&self, process: Option<platform::ProcessIdentity>) -> io::Result<Option<bool>> {
        self.core.backend.unit_alive(&self.core.config, process)
    }
    pub(crate) fn spool_dir(&self, id: Uuid) -> PathBuf {
        self.core.trees.state.join("spools").join(id.to_string())
    }
    pub(crate) fn have(&self, r: Vec<RepositoryCandidates>) -> Vec<RepositoryCandidates> {
        self.core.trees.have(r)
    }

    pub(crate) fn have_dependencies(
        &self,
        urls: Vec<String>,
    ) -> io::Result<Vec<GitDependencyCandidates>> {
        self.core.trees.have_dependencies(urls)
    }
    fn long_work(&self, _label: &'static str) {
        #[cfg(test)]
        if let Some(hook) = &self.core.config.long_work {
            hook(_label);
        }
    }
    fn trash(&self) -> PathBuf {
        self.core.trees.state.join("trash")
    }
    /// Move a directory out of the way now and delete it on a background
    /// thread. Callers hold the state lock: the rename is immediate and frees
    /// the path for reuse, while deleting a workspace or a warm target
    /// (hundreds of thousands of files) takes minutes, and holding the lock
    /// that long stalls every other job, transfer and status request.
    fn discard(&self, path: &Path) -> io::Result<()> {
        if path.symlink_metadata().is_err() {
            return Ok(());
        }
        let name = Uuid::now_v7().to_string();
        fs::create_dir_all(self.trash())?;
        let aside = self.trash().join(&name);
        let aside = match fs::rename(path, &aside) {
            Ok(()) => aside,
            Err(_) => {
                // A workspace on another filesystem can only move within it.
                let aside = path
                    .parent()
                    .ok_or_else(|| invalid("discarded path has no parent"))?
                    .join(format!(".motor-trash-{name}"));
                fs::rename(path, &aside)?;
                aside
            }
        };
        self.purge(aside);
        Ok(())
    }
    fn purge(&self, path: PathBuf) {
        #[cfg(test)]
        let hook = self.core.config.long_work.clone();
        #[cfg(target_os = "linux")]
        let restore_access = self.core.config.isolate_jobs;
        let deletion = crate::cleanup::BackgroundDelete::new(self.core.background_deletes.clone());
        std::thread::spawn(move || {
            let _deletion = deletion;
            #[cfg(test)]
            if let Some(hook) = &hook {
                hook("remove");
            }
            let _access = crate::tree_access::lock(&path);
            #[cfg(target_os = "linux")]
            if restore_access {
                if let Err(error) = crate::isolation::reclaim_runtime(&path) {
                    tracing::error!(path = %path.display(), %error, "cannot reclaim job-owned tree for runner cleanup");
                    return;
                }
            }
            if let Err(error) = fs::remove_dir_all(&path) {
                tracing::error!(path = %path.display(), %error, "deleting a discarded directory failed");
            }
        });
    }
    // Only the Linux isolated tests delete job trees in the background.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn wait_for_background_deletes(&self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let count = self
                .core
                .background_deletes
                .load(std::sync::atomic::Ordering::SeqCst);
            if count == 0 {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{count} background deletions still in flight"
            );
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }

    /// A running job, or a setup in flight, owns this workspace key.
    fn occupied(inner: &Inner, key: &str) -> bool {
        inner.reserved.contains_key(key)
            || inner
                .reserved
                .values()
                .any(|r| Self::workspace_uses_sibling(key, &r.siblings))
            || inner.jobs.values().any(|j| {
                j.state == JobState::Running
                    && (j.key() == key
                        || j.request
                            .as_ref()
                            .is_some_and(|r| Self::workspace_uses_sibling(key, &r.siblings)))
            })
    }

    // A head checkout can also be a different repository's canonical sibling.
    // Treat that shared path as owned in either role, including during setup.
    fn workspace_uses_sibling(key: &str, siblings: &[SiblingSnapshot]) -> bool {
        siblings.iter().any(|s| {
            Path::new(key).starts_with(&s.canonical_path)
                || Path::new(&s.canonical_path).starts_with(key)
        })
    }
    /// Record a workspace a setup created, so drop and retention can delete
    /// it. Its siblings are recorded only when the setup succeeded.
    #[allow(clippy::too_many_arguments)]
    fn note_workspace(
        &self,
        inner: &mut Inner,
        key: &str,
        root: &str,
        commit: &str,
        siblings: &[SiblingSnapshot],
        populated: bool,
        kind: Option<WorkspaceKind>,
    ) {
        if self.core.config.windows_instance() {
            return;
        }
        if populated {
            workspace::record_workspace(
                &mut inner.catalog,
                key,
                root,
                commit,
                siblings,
                self.now(),
            );
        } else if Path::new(key).exists() {
            workspace::record_workspace(&mut inner.catalog, key, root, commit, &[], self.now());
            if let Some(w) = inner.catalog.workspaces.get_mut(key) {
                w.siblings = siblings.to_vec();
            }
        }
        if let Some(w) = inner.catalog.workspaces.get_mut(key) {
            w.kind = kind.unwrap_or_default();
        }
    }

    pub(crate) fn accept(&self, mut request: Run) -> io::Result<Admission> {
        if self.core.config.windows_instance() {
            request.weight_hint = Some(4);
        }
        let (_admission, initial_refusal) = {
            let mut inner = self.core.inner.lock().unwrap();
            if inner.jobs.contains_key(&request.job_id) {
                return Ok(Admission::Duplicate);
            }
            let refusal = self.run_refusal(&inner, &request);
            (self.protect_admission(&mut inner, &request), refusal)
        };
        // Recheck ordinary admission after cleanup; another request may have
        // occupied the workspace or queue while deletion ran off the state lock.
        let disk_full = if initial_refusal.is_none() {
            self.ensure_disk_space()?
        } else {
            None
        };
        self.long_work("admission_record");
        let mut inner = self.core.inner.lock().unwrap();
        if inner.jobs.contains_key(&request.job_id) {
            return Ok(Admission::Duplicate);
        }
        let depth = inner.jobs.values().filter(|j| j.pending()).count();
        inner.order += 1;
        let mut job = Job::new(
            request.job_id,
            Some(request.clone()),
            self.now(),
            inner.order,
            request.weight_hint.unwrap_or(2),
        );
        // If another pressure pass had already chosen this request's workspace
        // for deletion before admission began, the guard could not stop it, so
        // the files may be partly gone. Keep that setup refusal even when the
        // deletion has finished and no overlap is visible by this recheck;
        // otherwise the job would start on a half-deleted workspace.
        let ordinary_refusal = self.run_refusal(&inner, &request).or_else(|| {
            initial_refusal.filter(|(reason, _)| *reason == RefusalReason::WorkspaceSetupFailed)
        });
        let disk_refused = ordinary_refusal.is_none() && disk_full.is_some();
        let refusal = ordinary_refusal.or_else(|| {
            disk_full
                .as_ref()
                .map(|full| (RefusalReason::RunnerDiskFull, full.diagnostic()))
        });
        if let Some((reason, error)) = refusal.as_ref() {
            let detail = if disk_refused {
                disk_full.as_ref().map(RunnerDiskFull::diagnostic)
            } else if *reason == RefusalReason::RunnerDraining {
                Some("runner_draining".into())
            } else {
                Some(self.safe_refusal_detail(error, request.env.values().cloned()))
            };
            refuse_before_start(
                &self.core.health,
                &mut job,
                reason.clone(),
                error,
                detail,
                self.now(),
            );
            job.state = JobState::Tombstone;
        }
        if let Err(error) = self.save(&job) {
            if !disk_refused {
                return Err(error);
            }
            // A completely full filesystem may not even hold a refusal record.
            // Still send the no-start proof rather than dropping the connection.
            tracing::error!(job_id = %job.id, %error, "cannot persist disk refusal");
        }
        if refusal.is_none() {
            self.core
                .health
                .lock()
                .unwrap()
                .record(self.now(), |c| c.accepted += 1);
            tracing::info!(job_id = %job.id, workspace = %request.workspace_key, queue_position = depth, bundle_bytes = request.bundles.iter().map(|b| b.length).sum::<u64>(), "job accepted");
        }
        inner.jobs.insert(job.id, job);
        if let Some(full) = disk_full.filter(|_| disk_refused) {
            return Ok(Admission::DiskFull(full));
        }
        Ok(refusal
            .map(|(reason, _)| Admission::Refused(reason))
            .unwrap_or(Admission::Accepted(depth as u32)))
    }

    fn cpu_refusal(&self, claim: u32) -> Option<(RefusalReason, String)> {
        if claim == 0 || claim > self.core.cpus.capacity() {
            Some((RefusalReason::RunnerFull, format!(
                "vcpu request {claim} is outside the supported range 1..={} (allocatable CPU limit; one physical core is reserved for daemon setup)",
                self.core.cpus.capacity()
            )))
        } else {
            None
        }
    }

    fn run_refusal(&self, inner: &Inner, request: &Run) -> Option<(RefusalReason, String)> {
        if request.network.is_some() {
            if request.network != Some(Network::Outbound) {
                return Some((
                    RefusalReason::NetworkUnsupported,
                    "unknown job network mode".into(),
                ));
            }
            if let Some(detail) = crate::network::unsupported(&self.core.config) {
                return Some((RefusalReason::NetworkUnsupported, detail));
            }
        }
        let requested = request.platform.as_ref().unwrap_or(&Platform::Linux);
        let served = self.core.config.served_platform();
        if matches!(requested, Platform::Unknown(_)) || *requested != served {
            Some((
                RefusalReason::PlatformUnsupported,
                format!(
                    "requested platform {} is unsupported; runner serves {}",
                    String::from(requested.clone()),
                    String::from(served),
                ),
            ))
        } else if let Some(refusal) = self.cpu_refusal(if self.core.config.windows_instance() {
            4
        } else {
            request.weight_hint.unwrap_or(2)
        }) {
            Some(refusal)
        } else if inner.drain_deadline.is_some() {
            Some((RefusalReason::RunnerDraining, "runner is draining".into()))
        } else if (!self.core.config.windows_instance()
            && Self::deleting(inner, &request.workspace_key, &request.siblings))
            || self.dependencies_evicting(inner, &request.git_dependencies)
            || self.core.config.windows.as_ref().is_some_and(|vm| {
                inner
                    .evicting_caches
                    .contains(&vm.caches().disk(&request.workspace_key))
            })
        {
            Some((RefusalReason::WorkspaceSetupFailed, format!(
                "workspace or sibling overlaps deletion in progress: workspace={} dropping={:?} dependency_evictions={:?}",
                request.workspace_key, inner.dropping, inner.evicting_dependencies
            )))
        } else if inner.jobs.values().filter(|j| j.pending()).count()
            >= self.core.config.max_queue_depth
        {
            Some((
                RefusalReason::RunnerFull,
                format!(
                    "queue depth reached limit {}",
                    self.core.config.max_queue_depth
                ),
            ))
        } else if let Err(error) = workspace::validate(
            &request.workspace_key,
            &request.repository_root,
            &request.siblings,
        ) {
            Some((RefusalReason::WorkspaceKeyRejected, error.to_string()))
        } else if !workspace::absolute(&request.cwd)
            || !Path::new(&request.cwd).starts_with(&request.workspace_key)
        {
            Some((
                RefusalReason::WorkspaceKeyRejected,
                format!(
                    "cwd is not an absolute path within the workspace: {}",
                    request.cwd
                ),
            ))
        } else if let Some(detail) = nested_workspace_detail(
            inner,
            &request.workspace_key,
            &request.repository_root,
            &request.siblings,
        ) {
            Some((RefusalReason::WorkspaceSetupFailed, detail))
        } else if let Err(error) = self.validate_writer_paths_with_catalog(
            inner,
            &request.workspace_key,
            Some(&request.cwd),
            &request.siblings,
        ) {
            Some((RefusalReason::WorkspaceKeyRejected, error.to_string()))
        } else if let Err(error) = workspace::validate_dependencies(&request.git_dependencies) {
            Some((RefusalReason::BundleRejected, error.to_string()))
        } else {
            None
        }
    }

    fn validate_writer_paths(
        &self,
        workspace: &str,
        cwd: Option<&str>,
        siblings: &[SiblingSnapshot],
    ) -> io::Result<()> {
        self.validate_writer_paths_with_catalog(
            &self.core.inner.lock().unwrap(),
            workspace,
            cwd,
            siblings,
        )
    }

    fn validate_writer_paths_with_catalog(
        &self,
        inner: &Inner,
        workspace: &str,
        cwd: Option<&str>,
        siblings: &[SiblingSnapshot],
    ) -> io::Result<()> {
        if self.core.config.windows_instance() {
            return Ok(());
        }
        let mut known = Vec::new();
        let mut derive = |workspace: &str, repository: &str| {
            if workspace::absolute(workspace) {
                if let Some(parent) = Path::new(workspace).parent() {
                    known.push(parent.join(format!(
                        ".motor-base-{}-{}",
                        self.core.config.instance,
                        workspace::name(repository)
                    )));
                }
            }
        };
        for (workspace, record) in &inner.catalog.workspaces {
            derive(workspace, &record.repository);
        }
        for (repository, record) in &inner.catalog.repositories {
            derive(repository, repository);
            if let Some(desired) = &record.desired {
                derive(&desired.workspace, repository);
            }
        }
        for request in inner
            .jobs
            .values()
            .filter(|job| job.base)
            .filter_map(|job| job.request.as_ref())
        {
            known.push(PathBuf::from(&request.cwd));
        }
        crate::writer::validate_paths(&self.core.config, workspace, cwd, siblings, &known)
    }

    fn deleting(inner: &Inner, key: &str, siblings: &[SiblingSnapshot]) -> bool {
        let paths: Vec<_> = std::iter::once(PathBuf::from(key))
            .chain(siblings.iter().map(|s| PathBuf::from(&s.canonical_path)))
            .flat_map(|p| {
                let canonical = p.canonicalize().ok();
                std::iter::once(p).chain(canonical)
            })
            .collect();
        inner.dropping.iter().any(|dropping| {
            let path = PathBuf::from(dropping);
            std::iter::once(path.clone())
                .chain(path.canonicalize().ok())
                .any(|d| paths.iter().any(|p| p.starts_with(&d) || d.starts_with(p)))
        })
    }
    fn dependencies_evicting(&self, inner: &Inner, dependencies: &[GitDependency]) -> bool {
        dependencies.iter().any(|d| {
            inner
                .evicting_dependencies
                .contains(&self.core.trees.dependency_mirror(&d.url))
        })
    }
    pub(crate) fn refuse(
        &self,
        id: Uuid,
        reason: RefusalReason,
        error: impl std::fmt::Display,
    ) -> io::Result<()> {
        let mut inner = self.core.inner.lock().unwrap();
        if let Some(job) = inner
            .jobs
            .get_mut(&id)
            .filter(|j| j.pending() && j.started_ms.is_none())
        {
            let detail = self.safe_refusal_detail(
                &error,
                job.request
                    .as_ref()
                    .into_iter()
                    .flat_map(|request| request.env.values().cloned()),
            );
            refuse_before_start(
                &self.core.health,
                job,
                reason,
                error,
                Some(detail),
                self.now(),
            );
            job.state = JobState::Tombstone;
            self.save(job)?;
        }
        Ok(())
    }
    /// Index a job's verified bundles into the mirrors and queue the job. The
    /// fetch runs without the state lock: indexing a large bundle takes from
    /// seconds to minutes, and the scheduler, output persistence and every
    /// other request need that lock meanwhile.
    pub(crate) fn fetch_run(
        &self,
        request: &Run,
        directory: &Path,
        default_branch_tip: bool,
    ) -> io::Result<()> {
        let id = request.job_id;
        let awaiting = |inner: &Inner| {
            inner
                .jobs
                .get(&id)
                .is_some_and(|j| j.state == JobState::AwaitingBundles)
        };
        if !awaiting(&self.core.inner.lock().unwrap()) {
            return Ok(());
        }
        let started = std::time::Instant::now();
        self.long_work("fetch");
        let trees = &self.core.trees;
        let fetched = self
            .validate_writer_paths(
                &request.workspace_key,
                Some(&request.cwd),
                &request.siblings,
            )
            .and_then(|()| trees.validate_dependency_bundles(directory, &request.git_dependencies))
            .and_then(|()| {
                trees.fetch(
                    directory,
                    &request.bundles,
                    &request.repository_root,
                    &request.snapshot_commit,
                    &request.integration_tip,
                    &request.siblings,
                )
            })
            .and_then(|()| trees.import_dependencies(directory, &request.git_dependencies))
            .and_then(|()| {
                trees.pin(
                    &request.repository_root,
                    &request.snapshot_commit,
                    &request.integration_tip,
                    &request.siblings,
                )
            });
        let mut inner = self.core.inner.lock().unwrap();
        // A cancel or withdraw during the fetch already wrote the terminal.
        if !awaiting(&inner) {
            return Ok(());
        }
        let job = inner.jobs.get_mut(&id).unwrap();
        if let Err(error) = fetched {
            let detail = self.safe_refusal_detail(&error, request.env.values().cloned());
            refuse_before_start(
                &self.core.health,
                job,
                transfer_refusal(&error),
                &error,
                Some(detail),
                self.now(),
            );
            job.state = JobState::Tombstone;
            return self.save(job);
        }
        job.state = JobState::Queued;
        job.queued_ms = Some(self.now());
        self.save(job)?;
        job.queue_entry
            .store(job.queued_ms.unwrap(), std::sync::atomic::Ordering::Release);
        tracing::info!(job_id = %id, elapsed_ms = started.elapsed().as_millis() as u64, "bundles fetched; job queued");
        // Legacy clients may name an unmerged job branch; only opted-in tips may queue cache fills.
        if !default_branch_tip {
            return Ok(());
        }
        self.queue_base(
            &mut inner,
            &request.repository_root,
            &request.integration_tip,
            &request.workspace_key,
            &request.siblings,
            &request.git_dependencies,
        )
    }
    pub(crate) fn fetch_prepare(&self, request: &Prepare, directory: &Path) -> io::Result<()> {
        self.allow_prepare()?;
        self.validate_writer_paths(&request.workspace_key, None, &request.siblings)?;
        self.core
            .trees
            .validate_dependency_bundles(directory, &request.git_dependencies)?;
        self.core.trees.fetch(
            directory,
            &request.bundles,
            &request.repository_root,
            &request.base_commit,
            &request.integration_tip,
            &request.siblings,
        )?;
        self.core
            .trees
            .import_dependencies(directory, &request.git_dependencies)?;
        let mut inner = self.core.inner.lock().unwrap();
        inner.catalog.transferred.insert(
            request.transfer_id,
            RetainedTransfer {
                repository: request.repository_root.clone(),
                snapshot: request.base_commit.clone(),
                tip: request.integration_tip.clone(),
                siblings: request.siblings.clone(),
                finished_ms: self.now(),
            },
        );
        self.catalog(&inner)?;
        self.core.trees.pin(
            &request.repository_root,
            &request.base_commit,
            &request.integration_tip,
            &request.siblings,
        )
    }
    fn siblings_blocked(inner: &Inner, siblings: &[SiblingSnapshot]) -> bool {
        inner
            .jobs
            .values()
            .filter(|j| j.state == JobState::Running)
            .any(|j| {
                let Some(r) = j.request.as_ref() else { return false; };
                Self::workspace_uses_sibling(if j.base { &r.cwd } else { &r.workspace_key }, siblings)
                || r.siblings.iter().any(|active| {
                    siblings.iter().any(|s| {
                        s.canonical_path == active.canonical_path && s.commit != active.commit
                    })
                })
            })
            // A setup in flight checks a sibling out unless the catalog already
            // records it at that commit. While it may be doing so, a second
            // setup must not touch that checkout even at the same commit.
            || inner.reserved.iter().any(|(key, r)| {
                Self::workspace_uses_sibling(key, siblings)
                || r.siblings.iter().any(|active| {
                    siblings.iter().any(|s| {
                        s.canonical_path == active.canonical_path
                            && (s.commit != active.commit
                                || inner.catalog.sibling_commits.get(&s.canonical_path)
                                    != Some(&s.commit))
                    })
                })
            })
    }
    /// Materialize a workspace at the caller's base commit without the state
    /// lock. A reservation protects the workspace and its siblings, so a burst
    /// of prepares cannot hold up transfers and job starts.
    pub(crate) fn prepare(&self, r: &Prepare, default_branch_tip: bool) -> io::Result<()> {
        self.allow_prepare()?;
        workspace::validate(&r.workspace_key, &r.repository_root, &r.siblings)?;
        self.validate_writer_paths(&r.workspace_key, None, &r.siblings)?;
        let recorded = {
            let mut inner = self.core.inner.lock().unwrap();
            if Self::deleting(&inner, &r.workspace_key, &r.siblings)
                || Self::occupied(&inner, &r.workspace_key)
                || Self::siblings_blocked(&inner, &r.siblings)
            {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "workspace busy"));
            }
            inner.reserved.insert(
                r.workspace_key.clone(),
                Reservation {
                    repository: r.repository_root.clone(),
                    commit: r.base_commit.clone(),
                    siblings: r.siblings.clone(),
                    since_ms: self.now(),
                },
            );
            inner.catalog.sibling_commits.clone()
        };
        let started = std::time::Instant::now();
        self.long_work("materialize");
        #[cfg(target_os = "linux")]
        if self.core.config.isolate_jobs {
            let workspace_path = Path::new(&r.workspace_key);
            if workspace_path.exists() {
                setup_step(
                    "restore runner access before workspace prepare",
                    crate::isolation::restore_runner_access(workspace_path),
                )?;
                if workspace_path.is_dir() {
                    setup_step(
                        "restore runner ownership before workspace prepare",
                        crate::isolation::restore_runner_checkout_root(workspace_path),
                    )?;
                }
            }
        }
        let tree_hash = workspace::git(
            &self.core.trees.mirror(&r.repository_root),
            &["rev-parse", &format!("{}^{{tree}}", r.base_commit)],
        )?;
        let populated = setup_step(
            "materialize prepared workspace",
            self.core.trees.populate(
                &r.workspace_key,
                &r.repository_root,
                &r.base_commit,
                &r.siblings,
                &recorded,
                &tree_hash,
                &r.base_commit,
            ),
        );
        #[cfg(target_os = "linux")]
        let populated = if self.core.config.isolate_jobs {
            populated.and_then(|()| {
                setup_step(
                    "prepare workspace target",
                    crate::isolation::prepare_job_writable_target(
                        &self.core.trees.state,
                        &Path::new(&r.workspace_key).join("target"),
                    ),
                )?;
                setup_step(
                    "prepare workspace checkout root",
                    crate::isolation::prepare_job_checkout_root(Path::new(&r.workspace_key)),
                )
            })
        } else {
            populated
        };
        let mut inner = self.core.inner.lock().unwrap();
        inner.reserved.remove(&r.workspace_key);
        self.note_workspace(
            &mut inner,
            &r.workspace_key,
            &r.repository_root,
            &r.base_commit,
            &r.siblings,
            populated.is_ok(),
            r.workspace_kind,
        );
        self.catalog(&inner)?;
        if let Err(error) = populated {
            tracing::warn!(workspace = %r.workspace_key, %error, "prepare failed");
            return Err(error);
        }
        tracing::info!(workspace = %r.workspace_key, elapsed_ms = started.elapsed().as_millis() as u64, "workspace prepared");
        if !default_branch_tip {
            return Ok(());
        }
        self.queue_base(
            &mut inner,
            &r.repository_root,
            &r.integration_tip,
            &r.workspace_key,
            &r.siblings,
            &r.git_dependencies,
        )
    }
    pub(crate) fn allow_prepare(&self) -> io::Result<()> {
        if self.core.config.windows_instance() {
            Err(io::Error::new(io::ErrorKind::Unsupported, "Prepare is unsupported by the Windows instance; caller workspaces are never materialized on the host"))
        } else {
            Ok(())
        }
    }
    fn queue_base(
        &self,
        inner: &mut Inner,
        root: &str,
        tip: &str,
        workspace: &str,
        _siblings: &[SiblingSnapshot],
        _git_dependencies: &[GitDependency],
    ) -> io::Result<()> {
        if tip.is_empty()
            || self.core.config.windows_instance()
            || !self.core.config.writer_enabled()
            || inner.drain_deadline.is_some()
        {
            return Ok(());
        }
        #[cfg(test)]
        if self.core.config.disable_warm_builds {
            return Ok(());
        }
        let (command, explicit_command) = if self.core.config.base_commands.is_empty() {
            workspace::warm_plan(&self.core.trees.mirror(root), tip)
        } else {
            (self.core.config.base_commands.join(" && "), true)
        };
        let descriptor = crate::writer::fill::descriptor(
            &self.core.trees.mirror(root),
            tip,
            &self.core.rustc_version,
            &command,
        );
        let repository = inner
            .catalog
            .repositories
            .entry(root.to_string())
            .or_default();
        repository.desired = Some(BaseRequest {
            tip: tip.to_string(),
            workspace: workspace.to_string(),
            siblings: vec![],
            git_dependencies: vec![],
            command: Some(command.clone()),
        });
        let descriptor = match descriptor {
            Ok(descriptor) => descriptor,
            Err(error) => {
                tracing::warn!(%root, %tip, %error, "dependency writer skips unavailable integration tip");
                repository.result = Some(RebuildResult::Failed);
                repository.requested_descriptor = Some(format!("unavailable:{tip}"));
                repository.failed_descriptor = repository.requested_descriptor.clone();
                return self.catalog(inner);
            }
        };
        repository.requested_descriptor = Some(descriptor.clone());
        if !explicit_command
            && crate::writer::fill::dependency_free(&self.core.trees.mirror(root), tip)
                .unwrap_or(false)
        {
            // With no dependency units to cache, recording success is the fill.
            // Remember no-op records so a later explicit command with identical
            // text cannot mistake them for an executed fill.
            if !repository.filled_descriptors.contains_key(&descriptor) {
                repository
                    .recorded_without_writer
                    .insert(descriptor.clone());
            }
            let finished = *repository
                .filled_descriptors
                .entry(descriptor.clone())
                .or_insert(self.now());
            repository.filled_descriptor = Some(descriptor);
            repository.commit = Some(tip.into());
            repository.finished_ms = Some(finished);
            repository.result = Some(RebuildResult::Ok);
            return self.catalog(inner);
        }
        if let Some(finished) = repository
            .filled_descriptors
            .get(&descriptor)
            .copied()
            .filter(|_| {
                !explicit_command || !repository.recorded_without_writer.contains(&descriptor)
            })
        {
            repository.filled_descriptor = Some(descriptor);
            repository.commit = Some(tip.into());
            repository.finished_ms = Some(finished);
            repository.result = Some(RebuildResult::Ok);
            return self.catalog(inner);
        }
        if repository.failed_descriptor.as_ref() == Some(&descriptor) {
            repository.result = Some(RebuildResult::Failed);
            return self.catalog(inner);
        }
        repository.result = Some(RebuildResult::Building);
        if inner.jobs.values().any(|j| {
            j.base
                && j.terminal.is_none()
                && j.request
                    .as_ref()
                    .is_some_and(|r| r.repository_root == root)
        }) {
            return self.catalog(inner);
        }
        if inner.jobs.values().filter(|j| j.pending()).count() >= self.core.config.max_queue_depth {
            return self.catalog(inner);
        }
        let id = Uuid::now_v7();
        let cwd = Path::new(workspace)
            .parent()
            .ok_or_else(|| invalid("workspace parent"))?
            .join(format!(
                ".motor-base-{}-{}",
                self.core.config.instance,
                workspace::name(root)
            ));
        let git_dependencies = match crate::writer::fill::locked_git(&self.core.trees, root, tip)
            .and_then(|dependencies| {
                let (_, missing) = self.core.trees.dependency_routes(&dependencies)?;
                if !missing.is_empty() {
                    return Err(invalid(missing.join("; ")));
                }
                Ok(dependencies)
            }) {
            Ok(dependencies) => dependencies,
            Err(error) => {
                let g = inner.catalog.repositories.get_mut(root).unwrap();
                g.failed_descriptor = Some(descriptor);
                g.result = Some(RebuildResult::Failed);
                tracing::warn!(%root, %tip, %error, "dependency writer skips missing git dependency");
                return self.catalog(inner);
            }
        };
        let tree = workspace::git(
            &self.core.trees.mirror(root),
            &["rev-parse", &format!("{tip}^{{tree}}")],
        )?;
        inner.order += 1;
        let request = Run {
            gate_job: false,
            network: None,
            platform: None,
            workspace_kind: None,
            job_id: id,
            workspace_key: format!("base:{root}"),
            repository_root: root.to_string(),
            cwd: cwd.to_string_lossy().to_string(),
            command: command.clone(),
            env: BTreeMap::new(),
            weight_hint: Some(4),
            timeout: None,
            queue_wait_limit_s: u64::MAX,
            snapshot_commit: tip.to_string(),
            tree_hash: tree,
            integration_tip: tip.to_string(),
            siblings: vec![],
            bundles: vec![],
            git_dependencies,
            base_commit: Some(tip.to_string()),
        };
        // Warm builds use 4 vCPUs, or the whole allocatable set on a smaller
        // host, where a fixed 4 would be refused and the target never warmed.
        let claim = WARM_BUILD_VCPUS.min(self.core.cpus.capacity());
        let mut job = Job::new(id, Some(request), self.now(), inner.order, claim);
        job.base = true;
        job.writer_descriptor = Some(descriptor);
        job.state = JobState::Queued;
        self.save(&job)?;
        tracing::info!(job_id = %id, repository = %root, commit = %tip, %command, "warm build queued");
        inner.jobs.insert(id, job);
        self.catalog(inner)
    }

    pub(crate) fn replay(&self, id: Uuid, from: u64) -> io::Result<Vec<Frame>> {
        self.replay_for_peer(id, from, true)
    }

    pub(crate) fn replay_legacy(&self, id: Uuid, from: u64) -> io::Result<Vec<Frame>> {
        self.replay_for_peer(id, from, false)
    }

    fn replay_for_peer(
        &self,
        id: Uuid,
        from: u64,
        supports_started: bool,
    ) -> io::Result<Vec<Frame>> {
        let mut inner = self.core.inner.lock().unwrap();
        if let std::collections::btree_map::Entry::Vacant(entry) = inner.jobs.entry(id) {
            if self.now().saturating_sub(mint_ms(id)) >= RETENTION_MS {
                let mut job = Job::new(id, None, self.now(), 0, 0);
                job.terminal(Outcome::HistoryExpired, self.now());
                let mut terminal = job.terminal.unwrap();
                terminal.ran = None;
                return Ok(vec![Frame::Terminal(terminal)]);
            }
            let mut job = Job::new(id, None, self.now(), 0, 0);
            refuse_before_start(
                &self.core.health,
                &mut job,
                RefusalReason::TransferInterrupted,
                "attach found no recorded admission or upload",
                Some("attach found no recorded admission or upload".into()),
                self.now(),
            );
            job.state = JobState::Tombstone;
            self.save(&job)?;
            entry.insert(job);
        }
        let job = &inner.jobs[&id];
        // Legacy callers never saw Started, so the replay shown to them must
        // still begin output and the replay cursor at zero. Rewrite the sequence
        // numbers while holding the job lock: a queued job may record Started
        // between attach polls, but not partway through this replay.
        let shift = match (supports_started, &job.started) {
            (false, Some(started)) => {
                if started.seq != 0 || job.chunks.iter().any(|c| c.seq == 0) {
                    tracing::error!(%id, "cannot project legacy replay: Started is not the first sequenced record");
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Started must be sequence zero for legacy replay",
                    ));
                }
                1
            }
            _ => 0,
        };
        let from = from.checked_add(shift).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "legacy replay cursor overflow")
        })?;
        let earliest = job.chunks.front().map(|c| c.seq).unwrap_or(job.next_seq);
        let mut frames = Vec::new();
        let mut output_from = from;
        if let Some(started) = &job.started {
            if supports_started && started.seq >= from {
                frames.push(Frame::Started {
                    job_id: id,
                    seq: started.seq,
                    queue_wait_ms: started.queue_wait_ms,
                    started_at_ms: started.started_at_ms,
                    path_mapping: started.path_mapping.clone(),
                });
                output_from = started.seq + 1;
            }
        }
        for chunk in job.chunks.iter().filter(|c| c.seq >= from) {
            let bytes = fs::read(
                self.record_path(id)
                    .parent()
                    .unwrap()
                    .join(format!("chunk-{}", chunk.seq)),
            )?;
            let output = Output::new(chunk.seq - shift, chunk.stream.clone(), BytePayload(bytes));
            let output = if chunk.seq == earliest && output_from < earliest {
                output.with_truncated_before_seq(earliest - shift)
            } else {
                output
            };
            frames.push(Frame::Output(output));
        }
        if let Some(terminal) = &job.terminal {
            // TerminalRecord in the pinned exec-remote types has no sequence
            // fields; only Output and its truncation boundary need projection.
            frames.push(Frame::Terminal(terminal.clone()));
        }
        Ok(frames)
    }
    pub fn status(&self) -> StatusReply {
        let inner = self.core.inner.lock().unwrap();
        StatusReply::new(
            inner.jobs.values().filter(|j| j.pending()).count() as u32,
            inner
                .jobs
                .values()
                .filter(|j| j.state == JobState::Running)
                .map(|j| RunningJob::new(j.id, j.key(), j.weight))
                .collect(),
            true,
            inner
                .catalog
                .repositories
                .iter()
                .map(|(root, g)| {
                    let mut status = RepositoryStatus::new(root.clone());
                    if let Some(finished_ms) = g.finished_ms {
                        status = status
                            .with_warm_target_age_s(self.now().saturating_sub(finished_ms) / 1000);
                    }
                    if let Some(commit) = &g.commit {
                        status = status.with_published_commit(commit.clone());
                    }
                    if let Some(result) = &g.result {
                        status = status.with_last_rebuild_result(result.clone());
                    }
                    status
                })
                .collect(),
            self.core.rustc_version.clone(),
        )
    }
    fn queue_wait_ms_at(inner: &Inner, id: Uuid, now: u64) -> Option<u64> {
        inner
            .jobs
            .get(&id)
            .filter(|j| j.state == JobState::Queued && !inner.starting.contains(&id))
            .and_then(|j| j.queued_ms.map(|t| now.saturating_sub(t)))
    }
    pub(crate) fn queue_progress(&self, id: Uuid) -> QueueProgress {
        let inner = self.core.inner.lock().unwrap();
        QueueProgress {
            entry: inner
                .jobs
                .get(&id)
                .map(|j| j.queue_entry.clone())
                .unwrap_or_else(empty_queue_entry),
            clock: self.core.config.clock.clone(),
        }
    }
    pub fn job_status(&self, id: Uuid) -> io::Result<JobStatusReply> {
        let inner = self.core.inner.lock().unwrap();
        let job = inner.jobs.get(&id);
        let process = job.and_then(|j| j.process);
        Ok(JobStatusReply {
            job_id: id,
            queue_wait_ms: Self::queue_wait_ms_at(&inner, id, self.now()),
            state: job
                .map(|j| format!("{:?}", j.state).to_lowercase())
                .unwrap_or("not_found".into()),
            terminal: job.and_then(|j| j.terminal.clone()),
            unit: process
                .and_then(|p| p.unit)
                .map(|u| self.core.backend.unit_name(&self.core.config, u)),
            unit_alive: self.unit_alive(process)?,
        })
    }
    pub fn cancel(&self, id: Uuid) -> io::Result<()> {
        let mut inner = self.core.inner.lock().unwrap();
        if self.core.config.windows_instance() && inner.starting.contains(&id) {
            self.core
                .backend
                .cancel(&self.core.config, &inner.jobs[&id], true)?;
        }
        if inner.finalizing.contains(&id) {
            return Ok(());
        }
        let job = inner
            .jobs
            .entry(id)
            .or_insert_with(|| Job::new(id, None, self.now(), 0, 0));
        if job.terminal.is_some() {
            return Ok(());
        }
        if job.state == JobState::Running {
            if job.kill.is_none() {
                job.kill = Some((Killed::Cancel, self.now()));
                self.save(job)?;
                if job.process.is_none() || self.unit_alive(job.process)? == Some(false) {
                    drop(inner);
                    return self.finish(
                        id,
                        Err(io::Error::other("cancel recorded after unit exited")),
                    );
                } else {
                    self.core.backend.cancel(&self.core.config, job, false)?;
                }
            }
        } else {
            job.terminal(Outcome::Cancelled, self.now());
            job.state = JobState::Tombstone;
            self.save(job)?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn withdraw(&self, id: Uuid) -> io::Result<WithdrawOutcome> {
        self.withdraw_for(id, None)
    }
    pub(crate) fn withdraw_for(
        &self,
        id: Uuid,
        reason: Option<RefusalReason>,
    ) -> io::Result<WithdrawOutcome> {
        let mut inner = self.core.inner.lock().unwrap();
        let stalled = reason == Some(RefusalReason::Unknown("transfer_stalled".into()));
        let eligible = inner.jobs.get(&id).is_some_and(|job| {
            (!inner.starting.contains(&id) || self.core.config.windows_instance())
                && job.pending()
                && if stalled {
                    job.state == JobState::AwaitingBundles
                } else {
                    job.state == JobState::Queued
                        && job.queued_ms.is_some_and(|t| {
                            self.now().saturating_sub(t)
                                >= job
                                    .request
                                    .as_ref()
                                    .unwrap()
                                    .queue_wait_limit_s
                                    .saturating_mul(1000)
                        })
                }
        });
        let detail = if stalled {
            "transfer_stalled".into()
        } else {
            self.queue_wait_detail(&inner, id)
        };
        if eligible && self.core.config.windows_instance() && inner.starting.contains(&id) {
            self.core
                .backend
                .cancel(&self.core.config, &inner.jobs[&id], true)?;
        }
        let Some(job) = inner.jobs.get_mut(&id) else {
            return Ok(WithdrawOutcome::Started);
        };
        if eligible {
            let safe_detail = self.safe_refusal_detail(
                &detail,
                job.request
                    .as_ref()
                    .into_iter()
                    .flat_map(|request| request.env.values().cloned()),
            );
            refuse_before_start(
                &self.core.health,
                job,
                RefusalReason::Unknown(detail.clone()),
                &detail,
                Some(safe_detail),
                self.now(),
            );
            job.state = JobState::Tombstone;
            self.save(job)?;
            Ok(WithdrawOutcome::Withdrawn)
        } else {
            Ok(WithdrawOutcome::Started)
        }
    }

    fn held_weight(inner: &Inner) -> u32 {
        inner
            .jobs
            .values()
            .filter(|j| j.state == JobState::Running || inner.starting.contains(&j.id))
            .fold(0u32, |held, j| held.saturating_add(j.weight))
    }

    fn queue_heads(&self, inner: &Inner) -> Vec<Uuid> {
        let mut keys = BTreeMap::<&str, &Job>::new();
        for job in inner
            .jobs
            .values()
            .filter(|j| j.state == JobState::Queued && !inner.starting.contains(&j.id))
        {
            let head = keys.entry(job.key()).or_insert(job);
            if (!job.gate_job(), job.order) < (!head.gate_job(), head.order) {
                *head = job;
            }
        }
        let now = self.now();
        let mut heads: Vec<_> = keys
            .values()
            .map(|j| {
                if j.gate_job() {
                    (0, j.order, 0, j.id)
                } else if j.base {
                    (3, j.order, 0, j.id)
                } else if now.saturating_sub(j.queued_ms.unwrap_or(j.accepted_ms))
                    >= BACKFILL_GRACE_MS
                {
                    (1, j.order, 0, j.id)
                } else {
                    (
                        2,
                        inner.rotation.get(j.key()).copied().unwrap_or(0),
                        j.order,
                        j.id,
                    )
                }
            })
            .collect();
        heads.sort();
        heads.into_iter().map(|(_, _, _, id)| id).collect()
    }

    /// Include both resource owners and scheduler fences in the refusal itself;
    /// the client need not have journal access to explain a local fallback.
    fn queue_wait_detail(&self, inner: &Inner, id: Uuid) -> String {
        let Some(job) = inner.jobs.get(&id) else {
            return "queue_wait_exceeded: unknown job".into();
        };
        let holders: Vec<_> = inner
            .jobs
            .values()
            .filter(|j| j.state == JobState::Running || inner.starting.contains(&j.id))
            .map(|j| {
                format!(
                    "job={} workspace={} permits={} phase={} since_ms={}",
                    j.id,
                    j.key(),
                    j.weight,
                    if inner.starting.contains(&j.id) {
                        "setup"
                    } else {
                        "running"
                    },
                    j.started_ms
                        .or_else(|| inner.reserved.get(j.key()).map(|r| r.since_ms))
                        .unwrap_or(j.accepted_ms)
                )
            })
            .collect();
        let reservations: Vec<_> = inner
            .reserved
            .iter()
            .map(|(key, r)| format!("workspace={key} since_ms={}", r.since_ms))
            .collect();
        let heads = self.queue_heads(inner);
        let pending_ahead: Vec<_> = heads
            .iter()
            .take_while(|head| **head != id)
            .map(|head| {
                format!(
                    "job={} permits={} accepted_ms={}",
                    head, inner.jobs[head].weight, inner.jobs[head].accepted_ms
                )
            })
            .collect();
        let phase = if job.state == JobState::AwaitingBundles {
            "bundle upload/fetch"
        } else if inner.starting.contains(&id) {
            "workspace setup"
        } else if inner.dropping.contains(job.key()) {
            "workspace drop"
        } else if Self::occupied(inner, job.key()) {
            "workspace owner"
        } else if job
            .request
            .as_ref()
            .is_some_and(|r| Self::siblings_blocked(inner, &r.siblings))
        {
            "sibling checkout owner"
        } else if job.weight > self.core.config.max_weight {
            "demand exceeds capacity"
        } else if Self::held_weight(inner).saturating_add(job.weight) > self.core.config.max_weight
        {
            "permits"
        } else {
            "workspace FIFO / admission priority"
        };
        format!("queue_wait_exceeded: waiting_on={phase}; requested_permits={}; held_permits={}/{}; accepted_ms={}; holders=[{}]; reservations=[{}]; heads_ahead=[{}]",
            job.weight, Self::held_weight(inner), self.core.config.max_weight, job.accepted_ms,
            holders.join("; "), reservations.join("; "), pending_ahead.join("; "))
    }
    /// Every step that takes the state lock (and the workspace deletion, which
    /// can take minutes for a large target directory) runs on the blocking
    /// pool: this is called from a connection's async task, and an async
    /// worker parked on the lock stops the runner answering other connections.
    pub async fn drop_workspace(&self, key: &str) -> io::Result<DropReply> {
        let key = key.to_string();
        let ids: Vec<_> = self
            .off_runtime({
                let key = key.clone();
                move |runner| {
                    let mut inner = runner.core.inner.lock().unwrap();
                    inner.dropping.insert(key.clone());
                    Ok(inner
                        .jobs
                        .values()
                        .filter(|j| j.key() == key && j.terminal.is_none())
                        .map(|j| j.id)
                        .collect())
                }
            })
            .await?;
        for &id in &ids {
            self.off_runtime(move |runner| runner.cancel(id)).await?;
        }
        loop {
            let active = self
                .off_runtime({
                    let key = key.clone();
                    move |runner| {
                        let inner = runner.core.inner.lock().unwrap();
                        let active = Self::occupied(&inner, &key);
                        drop(inner);
                        #[cfg(test)]
                        if active {
                            runner.long_work("drop-owner");
                        }
                        Ok(active)
                    }
                })
                .await?;
            if !active {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.off_runtime(move |runner| {
            let _pressure = runner.core.disk.lock().unwrap();
            let mut inner = runner.core.inner.lock().unwrap();
            let dropped = inner.catalog.workspaces.contains_key(&key);
            if dropped {
                runner.discard(Path::new(&key))?;
                inner.catalog.workspaces.remove(&key);
                runner.catalog(&inner)?;
            }
            inner.dropping.remove(&key);
            runner.prune_refs(&inner)?;
            Ok(DropReply::new(dropped, ids))
        })
        .await
    }
    async fn off_runtime<T: Send + 'static>(
        &self,
        call: impl FnOnce(Runner) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let runner = self.clone();
        tokio::task::spawn_blocking(move || call(runner))
            .await
            .map_err(io::Error::other)?
    }
    /// Also exposed to fake-clock tests; retention is relative to both terminal
    /// time and mint time, so recent unknown IDs can never rerun an expired job.
    pub fn maintenance(&self) -> io::Result<()> {
        if self.core.config.windows_instance() {
            self.core.config.windows_runtime.retry_cleanups();
        }
        let mut inner = self.core.inner.lock().unwrap();
        self.retry_start_refusals(&mut inner);
        let expired: Vec<_> = inner
            .jobs
            .values()
            .filter(|j| {
                j.finished_ms.is_some_and(|t| {
                    self.now().saturating_sub(t.max(mint_ms(j.id))) >= RETENTION_MS
                })
            })
            .map(|j| j.id)
            .collect();
        for id in expired {
            fs::remove_dir_all(self.record_path(id).parent().unwrap())?;
            inner.jobs.remove(&id);
        }
        let idle: Vec<_> = inner
            .catalog
            .workspaces
            .iter()
            .filter(|(key, w)| {
                w.kind != WorkspaceKind::Head
                    && self.now().saturating_sub(w.used_ms) >= RETENTION_MS
                    && !inner.reserved.contains_key(*key)
                    && !inner.preparing.values().any(|p| {
                        Path::new(&p.workspace_key).starts_with(key)
                            || Path::new(key).starts_with(&p.workspace_key)
                    })
                    && !Self::deleting(&inner, key, &[])
                    && !inner
                        .jobs
                        .values()
                        .any(|j| j.key() == *key && j.terminal.is_none())
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in idle {
            self.discard(Path::new(&key))?;
            inner.catalog.workspaces.remove(&key);
        }
        let stale_repositories: Vec<_> = inner
            .catalog
            .repositories
            .iter()
            .filter_map(|(repository, record)| {
                let has_unfinished_job = inner.jobs.values().any(|job| {
                    job.terminal.is_none()
                        && job.request.as_ref().is_some_and(|request| {
                            request.repository_root.as_str() == repository.as_str()
                        })
                });
                // Keep writer history and pending requests: they suppress retries,
                // preserve approvals, or record a completed fill.
                let has_writer_state = record.failure_detail.is_some()
                    || record.failed_descriptor.is_some()
                    || record.requested_descriptor.is_some()
                    || record.result.is_some()
                    || record.desired.is_some()
                    || record.commit.is_some()
                    || record.finished_ms.is_some()
                    || !record.recorded_without_writer.is_empty()
                    || !record.miss_refills.is_empty();
                (!Path::new(repository).exists()
                    && !has_unfinished_job
                    && !has_writer_state
                    && record.filled_descriptor.is_none()
                    && record.filled_descriptors.is_empty())
                .then(|| repository.clone())
            })
            .collect();
        for repository in stale_repositories {
            inner.catalog.repositories.remove(&repository);
            tracing::info!(repository = %repository, "removed stale writer repository record");
        }
        let active_transfers = inner.transfers.clone();
        inner.catalog.transferred.retain(|id, t| {
            active_transfers.contains(id) || self.now().saturating_sub(t.finished_ms) < RETENTION_MS
        });
        self.catalog(&inner)?;
        self.prune_refs(&inner)
    }

    fn prune_refs(&self, inner: &Inner) -> io::Result<()> {
        let mut known = BTreeSet::new();
        let mut held = BTreeMap::<String, BTreeSet<String>>::new();
        fn snapshot(
            known: &mut BTreeSet<String>,
            held: &mut BTreeMap<String, BTreeSet<String>>,
            root: &str,
            commit: &str,
            siblings: &[SiblingSnapshot],
        ) {
            known.insert(root.to_string());
            held.entry(root.to_string())
                .or_default()
                .insert(format!("refs/snapshots/{commit}"));
            for s in siblings {
                known.insert(s.canonical_path.clone());
                held.entry(s.canonical_path.clone())
                    .or_default()
                    .insert(format!("refs/snapshots/{}", s.commit));
            }
        }
        for w in inner.catalog.workspaces.values() {
            snapshot(
                &mut known,
                &mut held,
                &w.repository,
                &w.snapshot,
                &w.siblings,
            );
        }
        for job in inner.jobs.values() {
            if let Some(r) = &job.request {
                known.insert(r.repository_root.clone());
                for s in &r.siblings {
                    known.insert(s.canonical_path.clone());
                }
                if !job.base {
                    snapshot(
                        &mut known,
                        &mut held,
                        &r.repository_root,
                        &r.snapshot_commit,
                        &r.siblings,
                    );
                    held.entry(r.repository_root.clone())
                        .or_default()
                        .insert(format!("refs/tips/{}", r.integration_tip));
                }
            }
        }
        for t in inner.catalog.transferred.values() {
            snapshot(
                &mut known,
                &mut held,
                &t.repository,
                &t.snapshot,
                &t.siblings,
            );
            held.entry(t.repository.clone())
                .or_default()
                .insert(format!("refs/tips/{}", t.tip));
        }
        for p in inner.preparing.values() {
            snapshot(
                &mut known,
                &mut held,
                &p.repository_root,
                &p.base_commit,
                &p.siblings,
            );
            held.entry(p.repository_root.clone())
                .or_default()
                .insert(format!("refs/tips/{}", p.integration_tip));
        }
        for r in inner.reserved.values() {
            snapshot(&mut known, &mut held, &r.repository, &r.commit, &r.siblings);
        }
        for (root, g) in &inner.catalog.repositories {
            known.insert(root.clone());
            if let Some(commit) = &g.commit {
                held.entry(root.clone())
                    .or_default()
                    .insert(format!("refs/tips/{commit}"));
            }
        }
        for checkout in inner.catalog.legacy_checkouts.values() {
            known.insert(checkout.repository.clone());
            if let Some(tip) = &checkout.tip {
                held.entry(checkout.repository.clone())
                    .or_default()
                    .insert(format!("refs/tips/{tip}"));
            }
        }
        for job in inner
            .jobs
            .values()
            .filter(|j| j.base && j.terminal.is_none())
        {
            let r = job.request.as_ref().unwrap();
            held.entry(r.repository_root.clone())
                .or_default()
                .insert(format!("refs/tips/{}", r.integration_tip));
            for s in &r.siblings {
                held.entry(s.canonical_path.clone())
                    .or_default()
                    .insert(format!("refs/snapshots/{}", s.commit));
            }
        }
        let mut mirrors: BTreeMap<PathBuf, BTreeSet<String>> = known
            .into_iter()
            .map(|root| {
                (
                    self.core.trees.mirror(&root),
                    held.remove(&root).unwrap_or_default(),
                )
            })
            .collect();
        // Expired records may be the last mention of a repository. Enumerate
        // mirrors too, so their unheld refs are pruned after that record expires.
        for entry in fs::read_dir(&self.core.trees.mirrors)? {
            let path = entry?.path();
            if path.is_dir() {
                mirrors.entry(path).or_default();
            }
        }
        for (mirror, held) in mirrors {
            if !mirror.exists() {
                continue;
            }
            let refs = workspace::git(
                &mirror,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/snapshots",
                    "refs/tips",
                ],
            )?;
            for reference in refs.lines().filter(|r| !held.contains(*r)) {
                workspace::git(&mirror, &["update-ref", "-d", reference])?;
            }
        }
        Ok(())
    }

    fn enforce_kills(&self, inner: &mut Inner, instant: std::time::Instant) -> io::Result<()> {
        let now = self.now();
        for job in inner
            .jobs
            .values_mut()
            .filter(|j| j.state == JobState::Running)
        {
            if job.kill.is_none()
                && job
                    .request
                    .as_ref()
                    .and_then(|r| r.timeout)
                    .is_some_and(|s| {
                        now.saturating_sub(job.started_ms.unwrap()) >= s.saturating_mul(1000)
                    })
            {
                job.kill = Some((Killed::Deadline, now));
                self.save(job)?;
            }
            let Some((_, killed_at)) = job.kill.as_ref() else {
                continue;
            };
            if inner.finalizing.contains(&job.id) {
                continue;
            }
            let retry = inner.kill_retries.entry(job.id).or_insert((0, instant));
            if instant < retry.1 {
                continue;
            }
            // An exited unit needs a terminal, not another sudo/kill invocation.
            let gone = match self.unit_alive(job.process) {
                Ok(alive) => job.process.is_none() || alive == Some(false),
                Err(error) => {
                    tracing::warn!(job_id = %job.id, %error, "cannot inspect cancelled unit; retrying");
                    retry.1 = instant + Duration::from_secs(5);
                    continue;
                }
            };
            if gone {
                // The spawned finish call claims finalization itself. Back off
                // until it can take the lock, rather than spawning every tick.
                retry.1 = instant + Duration::from_secs(5);
                let runner = self.clone();
                let id = job.id;
                std::thread::spawn(move || {
                    if let Err(error) =
                        runner.finish(id, Err(io::Error::other("cancelled unit already exited")))
                    {
                        tracing::error!(%id, %error, "terminal bookkeeping failed");
                    }
                });
                continue;
            }
            let hard = now.saturating_sub(*killed_at) >= 2000;
            // One TERM/stop and at most three KILL attempts, with monotonic
            // backoff. Keep inspecting after exhaustion so exit is still noticed.
            if hard && retry.0 < 3 {
                self.core.backend.cancel(&self.core.config, job, true)?;
                retry.0 += 1;
                retry.1 = instant + Duration::from_millis(500 * (1 << retry.0));
            } else if !hard && retry.0 == 0 {
                self.core.backend.cancel(&self.core.config, job, false)?;
                retry.1 = instant + Duration::from_secs(2);
            } else {
                retry.1 = instant + Duration::from_secs(5);
            }
        }
        Ok(())
    }
    fn tick(&self) -> io::Result<()> {
        let mut inner = self.core.inner.lock().unwrap();
        self.retry_start_refusals(&mut inner);
        let now = self.now();
        if now.saturating_sub(inner.maintained_ms) >= 60_000 {
            inner.maintained_ms = now;
            drop(inner);
            self.maintenance()?;
            inner = self.core.inner.lock().unwrap();
        }
        self.enforce_kills(&mut inner, std::time::Instant::now())?;
        if inner.drain_deadline.is_some() {
            // Do not start queued commands during a deploy. Refuse them durably
            // before acknowledging the drain so callers can retry without the
            // uncertainty of a command interrupted by the binary swap.
            let queued: Vec<_> = inner
                .jobs
                .values()
                .filter(|job| {
                    job.state == JobState::Queued
                        && job.started_ms.is_none()
                        && job.process.is_none()
                        && !inner.starting.contains(&job.id)
                })
                .cloned()
                .collect();
            for mut job in queued {
                refuse_before_start(
                    &self.core.health,
                    &mut job,
                    RefusalReason::RunnerDraining,
                    "runner draining before queued command started",
                    Some("runner draining before queued command started".into()),
                    self.now(),
                );
                self.save(&job)?;
                let request = job.request.clone();
                let base = job.base;
                inner.jobs.insert(job.id, job);
                if base {
                    self.finish_base(&mut inner, request.as_ref().unwrap(), false, None)?;
                }
            }
            return Ok(());
        }
        let desired: Vec<_> = inner
            .catalog
            .repositories
            .iter()
            .filter_map(|(root, g)| {
                if inner.drain_deadline.is_some() {
                    return None;
                }
                let desired = g.desired.as_ref()?;
                (g.requested_descriptor != g.filled_descriptor
                    && (g.failed_descriptor.is_none()
                        || g.failed_descriptor != g.requested_descriptor)
                    && !inner.jobs.values().any(|j| {
                        j.base
                            && j.terminal.is_none()
                            && j.request
                                .as_ref()
                                .is_some_and(|r| r.repository_root == *root)
                    }))
                .then(|| (root.clone(), desired.clone()))
            })
            .collect();
        for (root, desired) in desired {
            self.queue_base(
                &mut inner,
                &root,
                &desired.tip,
                &desired.workspace,
                &desired.siblings,
                &desired.git_dependencies,
            )?;
        }
        self.dispatch(&mut inner, now)
    }

    fn dispatch(&self, inner: &mut Inner, now: u64) -> io::Result<()> {
        loop {
            let heads = self.queue_heads(inner);
            let mut next = None;
            for id in heads {
                let job = &inner.jobs[&id];
                let r = job.request.as_ref().unwrap();
                if job.base
                    && (inner.writer.is_some()
                        || inner
                            .evicting_caches
                            .contains(&self.core.config.sccache_dir())
                        || inner.jobs.values().any(|j| !j.base && j.pending()))
                {
                    continue;
                }
                if (!self.core.config.windows_instance()
                    && Self::deleting(inner, job.key(), &r.siblings))
                    || Self::occupied(inner, job.key())
                    || Self::siblings_blocked(inner, &r.siblings)
                {
                    // Idling unrelated CPUs cannot release a busy workspace.
                    continue;
                }
                let weight = Self::held_weight(inner);
                // An impossible demand must not fence the entire runner. It can
                // still be withdrawn safely, with its capacity mismatch named.
                if job.weight > self.core.config.max_weight {
                    continue;
                }
                if weight.saturating_add(job.weight) > self.core.config.max_weight {
                    if job.gate_job()
                        || (!job.base
                            && now.saturating_sub(job.queued_ms.unwrap_or(job.accepted_ms))
                                >= BACKFILL_GRACE_MS)
                    {
                        return Ok(());
                    }
                    continue;
                }
                next = Some(id);
                break;
            }
            let Some(id) = next else {
                break;
            };
            self.start(inner, id)?;
        }
        Ok(())
    }
    /// Reserve a queued job's workspace and set it up on its own thread.
    /// Checking out a workspace and cloning a warm target can take minutes;
    /// the scheduler keeps the state lock only to reserve and, afterwards, to
    /// launch, so other jobs, transfers and requests proceed meanwhile.
    fn start(&self, inner: &mut Inner, id: Uuid) -> io::Result<()> {
        let job = inner.jobs[&id].clone();
        let r = job.request.clone().unwrap();
        // Internal warm builds bypass external admission, but share its CPU limit.
        if let Some((reason, detail)) = self.cpu_refusal(job.weight) {
            let safe_detail = self.safe_refusal_detail(&detail, r.env.values().cloned());
            let mut refused = job.clone();
            refuse_before_start(
                &self.core.health,
                &mut refused,
                reason,
                &detail,
                Some(safe_detail),
                self.now(),
            );
            self.save(&refused)?;
            inner.jobs.insert(id, refused);
            if job.base {
                self.finish_base(inner, &r, false, None)?;
            }
            return Ok(());
        }
        if job.base {
            inner.writer = Some(id);
            #[cfg(target_os = "linux")]
            if self.core.config.isolate_jobs {
                match crate::writer::CacheFill::begin(&self.core.config) {
                    Ok(fill) => {
                        inner.cache_fills.insert(id, fill);
                    }
                    Err(error) => {
                        let mut failed = job.clone();
                        failed.terminal(Outcome::OutcomeUnknown, self.now());
                        failed.monitoring_error = Some(error.to_string());
                        self.save(&failed)?;
                        inner.jobs.insert(id, failed);
                        return self.finish_base(inner, &r, false, None);
                    }
                }
            }
        }
        let mut queued_job = job.clone();
        queued_job.cpus = self
            .core
            .cpus
            .place(job.weight, inner.jobs.values().map(Job::claimed_cpus));
        queued_job.setup_ms = Some(self.now());
        queued_job
            .queue_entry
            .store(NO_QUEUE, std::sync::atomic::Ordering::Release);
        self.save(&queued_job)?;
        let cpus = queued_job.cpus.clone();
        inner.jobs.insert(id, queued_job);
        inner.starting.insert(id);
        inner.reserved.insert(
            r.workspace_key.clone(),
            Reservation {
                repository: r.repository_root.clone(),
                commit: r.snapshot_commit.clone(),
                siblings: r.siblings.clone(),
                since_ms: self.now(),
            },
        );
        inner.dispatch += 1;
        inner
            .rotation
            .insert(r.workspace_key.clone(), inner.dispatch);
        let setup = Setup {
            id,
            cpus,
            request: r,
            base: job.base,
            recorded: inner.catalog.sibling_commits.clone(),
        };
        let runner = self.clone();
        std::thread::spawn(move || {
            let prepared = runner.ensure_disk_space().and_then(|full| {
                if let Some(full) = full {
                    Err(io::Error::other(format!(
                        "runner_disk_full: free_bytes={} floor_bytes={}",
                        full.free_bytes, full.floor_bytes
                    )))
                } else {
                    runner.setup(&setup)
                }
            });
            if let Err(error) = runner.launch(setup, prepared) {
                tracing::error!(job_id = %id, %error, "job launch failed");
            }
        });
        Ok(())
    }
    /// The long part of starting a job, run without the state lock.
    fn setup(&self, s: &Setup) -> io::Result<Prepared> {
        #[cfg(test)]
        if self.core.config.fail_materialize {
            return Err(io::Error::other(
                "materialize workspace: injected materialization failure",
            ));
        }
        self.long_work("materialize");
        self.core
            .backend
            .materialize(&self.core.config, &self.core.trees, s)
    }

    /// Finish a job's start under the state lock: record its workspace, and
    /// launch it unless setup failed or it was cancelled or withdrawn meanwhile.
    fn launch(&self, s: Setup, prepared: io::Result<Prepared>) -> io::Result<()> {
        let id = s.id;
        let r = s.request.clone();
        let mut inner = self.core.inner.lock().unwrap();
        inner.starting.remove(&id);
        inner.reserved.remove(&r.workspace_key);
        if s.base {
            if prepared.is_ok() {
                workspace::record_siblings(&mut inner.catalog, &r.siblings);
            }
        } else {
            self.note_workspace(
                &mut inner,
                &r.workspace_key,
                &r.repository_root,
                &r.snapshot_commit,
                &r.siblings,
                prepared.is_ok(),
                r.workspace_kind,
            );
        }
        self.catalog(&inner)?;
        let live = inner
            .jobs
            .get(&id)
            .is_some_and(|j| j.state == JobState::Queued);
        let refuse = |inner: &mut Inner,
                      reason: RefusalReason,
                      detail: Option<String>,
                      error: &io::Error|
         -> io::Result<()> {
            let detail = detail.unwrap_or_else(|| error.to_string());
            let safe_detail = self.safe_refusal_detail(detail, r.env.values().cloned());
            let j = inner.jobs.get_mut(&id).unwrap();
            refuse_before_start(
                &self.core.health,
                j,
                reason,
                error,
                Some(safe_detail),
                self.now(),
            );
            j.state = JobState::Tombstone;
            self.save(j)?;
            if s.base {
                self.finish_base(inner, &r, false, None)?;
            }
            Ok(())
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let (reason, detail) = if error.to_string() == "tree_hash_mismatch" {
                    (RefusalReason::TreeHashMismatch, None)
                } else if error.to_string().starts_with("runner_disk_full:") {
                    (RefusalReason::RunnerDiskFull, Some(error.to_string()))
                } else if error.to_string() == "windows_slot_busy" {
                    (RefusalReason::Unknown("windows_slot_busy".into()), None)
                } else {
                    (
                        workspace_setup_refusal("prepare job workspace", &error),
                        None,
                    )
                };
                if live {
                    return refuse(&mut inner, reason, detail, &error);
                }
                if s.base {
                    self.finish_base(&mut inner, &r, false, None)?;
                }
                return Ok(());
            }
        };
        if !live {
            if s.base {
                self.finish_base(&mut inner, &r, false, None)?;
            }
            if self.core.config.windows_instance() {
                inner.starting.insert(id);
                drop(inner);
                if let Some(vm) = &prepared.windows {
                    if let Some(vm) = vm.lock().unwrap().take() {
                        backend::Execution::reap(&vm);
                    }
                }
                self.core.inner.lock().unwrap().starting.remove(&id);
            }
            return Ok(());
        }
        let launch = match self.core.backend.launch(
            &self.core.config,
            &self.core.trees,
            &s,
            &prepared,
            inner.jobs[&id].weight,
        ) {
            Ok(l) => l,
            Err(error) => {
                return refuse(
                    &mut inner,
                    workspace_setup_refusal("launch isolated job", &error),
                    None,
                    &error,
                );
            }
        };
        #[cfg(test)]
        if !self.core.config.windows_instance() {
            if let Some(processes) = &self.core.config.test_processes {
                if let Some(identity) = launch.process {
                    processes.lock().unwrap().register(identity);
                }
            }
        }
        let j = inner.jobs.get_mut(&id).unwrap();
        j.state = JobState::Running;
        j.process = launch.process;
        j.durable_output = launch.durable_output;
        j.started_ms = Some(self.now());
        // Persist start progress with the timeout origin before opening the
        // execution gate; setup and permit reservation do not start the command.
        assert_eq!(
            j.next_seq, 0,
            "Started must be the first sequenced job record"
        );
        let mut started = Started::new(
            j.next_seq,
            j.queue_wait_ms(j.started_ms.unwrap()),
            j.started_ms.unwrap(),
        );
        if self.core.config.windows_instance() {
            started = started.with_path_mapping(PathMapping::new("/", r"C:\m"));
        }
        j.started = Some(started);
        j.next_seq += 1;
        if let Err(error) = self.save(j) {
            if !self.core.config.windows_instance() {
                // An in-memory Started is not evidence of restart recovery if
                // the record write failed. Keep this job holding the drain.
                j.started = None;
                return Err(error);
            }
            j.process = None;
            j.started_ms = None;
            j.started = None;
            j.next_seq = 0;
            j.state = JobState::Queued;
            let mut refused = j.clone();
            refuse_before_start(
                &self.core.health,
                &mut refused,
                RefusalReason::Unknown("start_not_persisted".into()),
                &error,
                Some("Started could not be persisted; the Windows command was not released".into()),
                self.now(),
            );
            refused.state = JobState::Tombstone;
            inner.starting.insert(id);
            inner.finalizing.insert(id);
            // Cleanup can wait for systemd and the I/O thread. It must not hold
            // the state lock used by status, cancellation and the next dispatch.
            drop(inner);
            launch.execution.reap();
            drop(launch);
            let mut inner = self.core.inner.lock().unwrap();
            if self.save(&refused).is_err() {
                inner.pending_start_refusals.insert(id);
            }
            inner.jobs.insert(id, refused);
            inner.starting.remove(&id);
            inner.finalizing.remove(&id);
            return Ok(());
        }
        #[cfg(test)]
        if let Some(hook) = &self.core.config.before_gate {
            hook();
        }
        let mut execution = launch.execution;
        execution.release()?;
        tracing::info!(job_id = %id, workspace = %r.workspace_key, base = s.base, permits = j.weight, since_ms = j.started_ms, "job started");
        drop(inner);
        if launch.durable_output {
            match execution.output() {
                Ok(output) => {
                    std::thread::spawn(move || {
                        let _ = io::copy(&mut { output.stdout }, &mut io::sink());
                    });
                    let runner = self.clone();
                    std::thread::spawn(move || runner.collect_accounting(id, output.stderr));
                }
                Err(error) => {
                    tracing::warn!(job_id = %id, %error, "job CPU accounting unavailable")
                }
            }
            let runner = self.clone();
            std::thread::spawn(move || {
                runner.monitor_unit(id);
                let _ = execution.wait();
                execution.reap();
            });
            return Ok(());
        }
        let output = execution.output()?;
        let stdout = output.stdout;
        let stderr = output.stderr;
        let runner = self.clone();
        std::thread::spawn(move || {
            let reader = |mut file: Box<dyn Read + Send>, stream: OutputStream, runner: Runner| {
                std::thread::spawn(move || {
                    let mut buffer = [0; 16384];
                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => {
                                if let Err(error) = runner.output(id, stream.clone(), &buffer[..n])
                                {
                                    tracing::error!(%error, "output persistence failed");
                                    return Err(error);
                                }
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(error) => return Err(error),
                        }
                    }
                    Ok(())
                })
            };
            let a = reader(stdout, OutputStream::Stdout, runner.clone());
            let b = reader(stderr, OutputStream::Stderr, runner.clone());
            let status = execution.wait();
            let output_error = [a.join(), b.join()]
                .into_iter()
                .find_map(|result| match result {
                    Ok(Err(error)) => Some(error),
                    Err(_) => Some(io::Error::other("output monitor panicked")),
                    _ => None,
                });
            execution.reap();
            if let Err(error) = runner.finish_execution(id, output_error.map_or(status, Err)) {
                tracing::error!(%error, "terminal persistence failed");
            }
        });
        Ok(())
    }
    fn output(&self, id: Uuid, stream: OutputStream, bytes: &[u8]) -> io::Result<()> {
        self.output_at(id, stream, bytes, None)
    }
    fn monitor_unit(&self, id: Uuid) {
        if let Err(error) = self.follow_unit(id) {
            tracing::error!(%id, %error, "durable job monitoring failed");
            // Keep the live unit's workspace reserved even if its output cannot
            // be saved. Only release it after the unit has actually stopped.
            loop {
                let process = self.core.inner.lock().unwrap().jobs[&id].process;
                match self.unit_alive(process) {
                    Ok(Some(true)) | Err(_) => {
                        std::thread::park_timeout(Duration::from_millis(500))
                    }
                    _ => break,
                }
            }
            if let Err(error) = self.finish(id, Err(error)) {
                tracing::error!(%id, %error, "terminal bookkeeping failed");
            }
        }
    }
    fn recovery_diagnostic(
        &self,
        id: Uuid,
        process: Option<platform::ProcessIdentity>,
        alive: bool,
    ) -> io::Result<()> {
        let unit = process
            .and_then(|p| p.unit)
            .map(|id| self.core.backend.unit_name(&self.core.config, id))
            .unwrap_or_else(|| "unrecorded".into());
        self.output(id, OutputStream::Stderr,
            format!("runner recovery: job_id={id} outcome_unknown unit={unit} unit_alive={alive}; never rerun; query exec.status or exec.attach by job_id\n").as_bytes())
    }
    fn follow_unit(&self, id: Uuid) -> io::Result<()> {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::OpenOptionsExt;
        let (process, durable) = {
            let inner = self.core.inner.lock().unwrap();
            let job = &inner.jobs[&id];
            (job.process, job.durable_output)
        };
        let directory = self
            .core
            .trees
            .state
            .join("sandboxes")
            .join(id.to_string())
            .join("output");
        if !durable {
            self.output(id, OutputStream::Stderr,
                format!("runner recovery: job_id={id} reattached unit={} unit_alive=true; legacy pipe output lost\n",
                    process.and_then(|p| p.unit).map(|u| self.core.backend.unit_name(&self.core.config, u)).unwrap_or_default()).as_bytes())?;
        }
        let mut alive = true;
        let mut inspect_at = std::time::Instant::now();
        loop {
            // Output is tailed frequently, but spawning sudo/systemctl for
            // every tail read would turn quiet jobs into a busy polling load.
            if std::time::Instant::now() >= inspect_at {
                alive = match self.unit_alive(process) {
                    Ok(alive) => alive == Some(true),
                    Err(error) => {
                        tracing::warn!(%id, %error, "cannot inspect reattached unit; retrying");
                        std::thread::park_timeout(Duration::from_millis(500));
                        continue;
                    }
                };
                inspect_at = std::time::Instant::now() + Duration::from_millis(500);
            }
            if durable {
                for (name, stream) in [
                    ("stdout", OutputStream::Stdout),
                    ("stderr", OutputStream::Stderr),
                ] {
                    let offset = {
                        let inner = self.core.inner.lock().unwrap();
                        let job = &inner.jobs[&id];
                        if stream == OutputStream::Stdout {
                            job.stdout_offset
                        } else {
                            job.stderr_offset
                        }
                    };
                    let read = || -> io::Result<Vec<u8>> {
                        let mut file = OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_NOFOLLOW)
                            .open(directory.join(name))?;
                        file.seek(SeekFrom::Start(offset))?;
                        let mut bytes = Vec::new();
                        file.take(16384).read_to_end(&mut bytes)?;
                        Ok(bytes)
                    };
                    match read() {
                        Ok(bytes) if !bytes.is_empty() => {
                            self.output_at(id, stream, &bytes, Some(offset + bytes.len() as u64))?;
                            // Drain all remaining bytes before retaining a terminal.
                            continue;
                        }
                        Err(error) if alive => {
                            // A log read failure cannot release a live unit's
                            // workspace. Keep monitoring it until it really ends.
                            tracing::warn!(%id, %error, "reattached output unavailable");
                        }
                        Err(error) => return self.finish(id, Err(error)),
                        _ => {}
                    }
                }
            }
            if !alive {
                if durable {
                    // Repeat while either durable log still has unread bytes.
                    let inner = self.core.inner.lock().unwrap();
                    let job = &inner.jobs[&id];
                    if fs::metadata(directory.join("stdout"))
                        .is_ok_and(|m| m.len() > job.stdout_offset)
                        || fs::metadata(directory.join("stderr"))
                            .is_ok_and(|m| m.len() > job.stderr_offset)
                    {
                        continue;
                    }
                }
                let status = if durable {
                    self.durable_exit_status(process, &directory.join("exit"))
                } else {
                    Err(io::Error::other(
                        "legacy unit ended without retained exit status",
                    ))
                };
                return self.finish(id, status);
            }
            std::thread::park_timeout(Duration::from_millis(25));
        }
    }
    fn durable_exit_status(
        &self,
        process: Option<platform::ProcessIdentity>,
        path: &Path,
    ) -> io::Result<std::process::ExitStatus> {
        use std::os::unix::{fs::OpenOptionsExt, process::ExitStatusExt};
        let durable = (|| {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let mut code = String::new();
            Read::by_ref(&mut file).take(32).read_to_string(&mut code)?;
            let code = code.trim().parse::<i32>().map_err(io::Error::other)?;
            if !(0..=255).contains(&code) {
                return Err(io::Error::other("invalid durable exit code"));
            }
            Ok(std::process::ExitStatus::from_raw(code << 8))
        })();
        durable.or_else(|error| {
            // The wrapper writes a fixed writable inode in a read-only output
            // directory. If it is killed before completing the write, the failed
            // unit retains its status. Read it before finish resets that unit;
            // this also recovers incomplete exit files from older wrappers.
            match self.core.backend.unit_exit_status(&self.core.config, process) {
                Ok(Some(status)) => Ok(status),
                Ok(None) => Err(io::Error::other(format!(
                    "durable exit unavailable: {error}; systemd has no retained exit status"
                ))),
                Err(unit_error) => Err(io::Error::other(format!(
                    "durable exit unavailable: {error}; systemd exit status unavailable: {unit_error}"
                ))),
            }
        })
    }
    fn output_at(
        &self,
        id: Uuid,
        stream: OutputStream,
        bytes: &[u8],
        offset: Option<u64>,
    ) -> io::Result<()> {
        let mut inner = self.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&id).unwrap();
        let path = self
            .record_path(id)
            .parent()
            .unwrap()
            .join(format!("chunk-{}", job.next_seq));
        #[cfg(test)]
        if let Some(write) = &self.core.config.before_job_write {
            write(&path)?;
        }
        let mut file = File::create(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        job.chunks.push_back(Chunk {
            seq: job.next_seq,
            stream: stream.clone(),
            length: bytes.len() as u64,
        });
        job.next_seq += 1;
        job.output_bytes += bytes.len() as u64;
        if let Some(offset) = offset {
            if stream == OutputStream::Stdout {
                job.stdout_offset = offset;
            } else {
                job.stderr_offset = offset;
            }
        }
        let mut removed = Vec::new();
        while job.output_bytes > REPLAY_BYTES {
            let chunk = job.chunks.pop_front().unwrap();
            job.output_bytes -= chunk.length;
            removed.push(chunk.seq);
        }
        self.save(job)?;
        for seq in removed {
            fs::remove_file(
                self.record_path(id)
                    .parent()
                    .unwrap()
                    .join(format!("chunk-{seq}")),
            )?;
        }
        Ok(())
    }
    fn retry_start_refusals(&self, inner: &mut Inner) {
        inner
            .pending_start_refusals
            .retain(|id| self.save(&inner.jobs[id]).is_err());
    }
    fn finish(&self, id: Uuid, status: io::Result<std::process::ExitStatus>) -> io::Result<()> {
        self.finish_execution(id, status.map(backend::ExecutionOutcome::Unix))
    }
    fn collect_accounting(&self, id: Uuid, mut reader: impl Read) {
        let result = (|| -> io::Result<()> {
            let unit = {
                let inner = self.core.inner.lock().unwrap();
                let process = inner.jobs[&id]
                    .process
                    .and_then(|process| process.unit)
                    .ok_or_else(|| io::Error::other("job has no launch unit"))?;
                self.core.backend.unit_name(&self.core.config, process)
            };
            #[cfg(test)]
            let cpu_time_ns = if let Some(observe) = &self.core.config.accounting_report {
                let mut report = Vec::new();
                reader.by_ref().take(65_537).read_to_end(&mut report)?;
                io::copy(&mut reader, &mut io::sink())?;
                observe(id, &report);
                accounting::read_cpu_time_ns(&report[..], &unit)?
            } else {
                accounting::read_cpu_time_ns(reader, &unit)?
            };
            #[cfg(not(test))]
            let cpu_time_ns = accounting::read_cpu_time_ns(&mut reader, &unit)?;
            self.record_cpu_time(id, cpu_time_ns)
        })();
        if let Err(error) = result {
            tracing::warn!(job_id = %id, %error, "job CPU accounting unavailable");
        }
    }

    fn record_cpu_time(&self, id: Uuid, cpu_time_ns: u64) -> io::Result<()> {
        let mut inner = self.core.inner.lock().unwrap();
        let Some(job) = inner.jobs.get(&id) else {
            return Ok(());
        };
        if job.terminal.is_some() {
            if let Some(ended_ms) = job.execution_ended_ms {
                let mut updated = job.clone();
                updated.record_reservation_use(cpu_time_ns, ended_ms);
                self.save(&updated)?;
                inner.jobs.insert(id, updated);
            }
        } else {
            inner.cpu_times.insert(id, cpu_time_ns);
        }
        Ok(())
    }
    fn finish_execution(
        &self,
        id: Uuid,
        status: io::Result<backend::ExecutionOutcome>,
    ) -> io::Result<()> {
        // git status over a large workspace takes a while, so it runs before
        // terminal persistence without the state lock. Claim finalization under
        // that lock so neither cancel nor the scheduler can signal the exited
        // job during collection or persistence retries. It still occupies its
        // workspace until the terminal is committed.
        let job = {
            let mut inner = self.core.inner.lock().unwrap();
            if inner.jobs[&id].terminal.is_some() {
                return Ok(());
            }
            if let Ok(status) = &status {
                inner.exit_statuses.entry(id).or_insert(*status);
            }
            if !inner.finalizing.insert(id) {
                return Ok(());
            }
            inner.jobs[&id].clone()
        };
        let ended_ms = self.now();
        let oom_stopped = self
            .core
            .backend
            .oom_stopped(&self.core.config, job.process)
            .unwrap_or(false);
        let memory_limit_detail = oom_stopped.then(|| {
            oom_memory_limit_detail(&self.core.config, job.weight, self.core.cpus.capacity())
        });
        if let Some(detail) = &memory_limit_detail {
            #[cfg(test)]
            let limit =
                crate::backend::memory_limit_gib(job.weight, self.core.config.memory_floor_gib);
            #[cfg(not(test))]
            let limit = crate::backend::memory_limit_gib(job.weight);
            tracing::warn!(
                job_id = %id,
                memory_limit_gib = %limit,
                vcpus = job.weight,
                "job stopped by its cgroup memory limit"
            );
            if let Err(error) = self.output(
                id,
                OutputStream::Stderr,
                format!("runner: {detail}\n").as_bytes(),
            ) {
                tracing::warn!(job_id = %id, %error, "OOM detail could not be appended to job output");
            }
        }
        let changes = match self.core.backend.collect_changes(&self.core.trees, &job) {
            Ok(changes) => changes,
            Err(error) => {
                tracing::warn!(job_id = %id, %error, "scan_failed");
                None
            }
        };
        let mut status = status;
        let lock_check = if job.base {
            let r = job.request.as_ref().unwrap();
            (|| -> io::Result<()> {
                #[cfg(target_os = "linux")]
                if self.core.config.isolate_jobs {
                    crate::isolation::restore_runner_access(Path::new(&r.cwd))?;
                }
                for (path, bytes) in crate::writer::fill::lockfiles(
                    &self.core.trees.mirror(&r.repository_root),
                    &r.snapshot_commit,
                    Path::new(&r.cwd),
                )? {
                    let original = workspace::blob(
                        &self.core.trees.mirror(&r.repository_root),
                        &format!("{}:{path}", r.snapshot_commit),
                    )?;
                    if bytes != original {
                        return Err(io::Error::other(format!(
                            "{path} differs from integration tip {}",
                            r.snapshot_commit
                        )));
                    }
                }
                Ok(())
            })()
        } else {
            Ok(())
        };
        let writer_stats = if job.base && job.durable_output {
            crate::writer::fill::statistics(&self.core.trees.state, id).map(|_| ())
        } else {
            Ok(())
        };
        let mut fill_failure = Vec::new();
        if let Err(error) = &lock_check {
            fill_failure.push(format!("lockfile integrity: {error}"));
        }
        if let Err(error) = &writer_stats {
            fill_failure.push(format!("writer statistics: {error}"));
        }
        let mut delay = Duration::from_millis(50);
        let reader_descriptor =
            if !job.base && crate::writer::fill::reader_missed(&self.core.trees.state, id) {
                let r = job.request.as_ref().unwrap();
                let command = if self.core.config.base_commands.is_empty() {
                    workspace::warm_command(
                        &self.core.trees.mirror(&r.repository_root),
                        &r.snapshot_commit,
                    )
                } else {
                    self.core.config.base_commands.join(" && ")
                };
                crate::writer::fill::descriptor(
                    &self.core.trees.mirror(&r.repository_root),
                    &r.snapshot_commit,
                    &self.core.rustc_version,
                    &command,
                )
                .ok()
            } else {
                None
            };
        let (mut inner, success, monitoring_error) = loop {
            let mut inner = self.core.inner.lock().unwrap();
            if inner.jobs[&id].terminal.is_some() {
                return Ok(());
            }
            // A monitor that lost the claim can still supply the real status.
            // Recheck on every persistence attempt, under the same lock as save.
            if let Some(exit) = inner.exit_statuses.get(&id) {
                status = Ok(*exit);
            }
            let success = lock_check.is_ok()
                && writer_stats.is_ok()
                && status.as_ref().is_ok_and(|s| s.success());
            let monitoring_error = status.as_ref().err().map(ToString::to_string);
            let outcome = match &status {
                Ok(s) => s.wire(),
                Err(_) => Outcome::OutcomeUnknown,
            };
            let mut finished = inner.jobs[&id].clone();
            finished.execution_ended_ms = Some(ended_ms);
            if let Some(cpu_time_ns) = inner.cpu_times.get(&id) {
                finished.record_reservation_use(*cpu_time_ns, ended_ms);
            }
            finished.terminal(outcome, self.now());
            finished.monitoring_error = monitoring_error.clone();
            finished.memory_limit_detail = memory_limit_detail.as_ref().map(|detail| {
                if let Some(error) = &monitoring_error {
                    format!("{detail} The job's exit status could not be read: {error}.")
                } else {
                    detail.clone()
                }
            });
            if let Some(changes) = &changes {
                finished.terminal.as_mut().unwrap().workspace_changes = Some(changes.clone());
            }
            match self.save(&finished) {
                Ok(()) => {
                    inner.jobs.insert(id, finished);
                    if oom_stopped {
                        self.core
                            .health
                            .lock()
                            .unwrap()
                            .record(self.now(), |counts| {
                                counts.oom_stops += 1;
                            });
                    }
                    inner.kill_retries.remove(&id);
                    inner.finalizing.remove(&id);
                    inner.exit_statuses.remove(&id);
                    inner.cpu_times.remove(&id);
                    break (inner, success, monitoring_error);
                }
                Err(error) => {
                    tracing::warn!(%id, %error, "terminal persistence failed; retrying");
                    // Backoff belongs to this job's monitor, never the scheduler
                    // or the state lock used by admission and other jobs.
                    drop(inner);
                    std::thread::park_timeout(delay);
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            }
        };
        let job = &inner.jobs[&id];
        let request = job.request.as_ref().unwrap().clone();
        let base = job.base;
        tracing::info!(job_id = %id, outcome = ?job.terminal.as_ref().unwrap().outcome, base, "job finished");
        if base {
            if !status.as_ref().is_ok_and(|s| s.success()) {
                fill_failure.push(format!(
                    "command/preflight exit: {}",
                    match &status {
                        Ok(s) => format!("{:?}", s.wire()),
                        Err(e) => e.to_string(),
                    }
                ));
            }
            self.finish_base(
                &mut inner,
                &request,
                success,
                (!fill_failure.is_empty()).then(|| fill_failure.join("; ")),
            )?;
        } else if let Some(w) = inner.catalog.workspaces.get_mut(&request.workspace_key) {
            w.used_ms = self.now();
            if let Some(descriptor) = reader_descriptor {
                if let Some(g) = inner.catalog.repositories.get_mut(&request.repository_root) {
                    // Aggregate misses cannot identify individual units. Restrict
                    // refills to matching graphs and one attempt per approved tip
                    // so a feature/profile the warm command omits cannot loop.
                    if let Some(tip) = g.desired.as_ref().map(|d| &d.tip) {
                        if g.filled_descriptor.as_ref() == Some(&descriptor)
                            && g.miss_refills.insert(format!("{tip}:{descriptor}"))
                        {
                            if let Some(filled) = g.filled_descriptor.take() {
                                g.filled_descriptors.remove(&filled);
                            }
                        }
                    }
                }
            }
            self.catalog(&inner)?;
        }
        self.prune_refs(&inner)?;
        drop(inner);
        if let Some(error) = monitoring_error {
            if let Err(write_error) = self.output(
                id,
                OutputStream::Stderr,
                format!(
                    "runner monitoring: job_id={id} outcome_unknown error={error}; never rerun\n"
                )
                .as_bytes(),
            ) {
                tracing::warn!(%id, %write_error, "monitoring error retained in job record; replay diagnostic unavailable");
            }
        }
        Ok(())
    }

    fn finish_base(
        &self,
        inner: &mut Inner,
        r: &Run,
        success: bool,
        failure: Option<String>,
    ) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        let mut failure = failure;
        #[cfg(target_os = "linux")]
        let success = if let Some(fill) = inner.cache_fills.remove(&r.job_id) {
            if let Err(error) = fill.finish() {
                let detail = format!("cache ACL repair/tracking: {error}");
                failure = Some(failure.map(|f| format!("{f}; {detail}")).unwrap_or(detail));
                false
            } else {
                success
            }
        } else {
            success
        };
        let scratch = self
            .core
            .config
            .writer_scratch_dir()
            .join(r.job_id.to_string());
        if scratch.exists() {
            #[cfg(target_os = "linux")]
            if self.core.config.isolate_jobs {
                crate::isolation::restore_runner_access(&scratch)?;
            }
            self.discard(&scratch)?;
        }
        if inner.writer == Some(r.job_id) {
            inner.writer = None;
        }
        let descriptor = inner.jobs[&r.job_id].writer_descriptor.clone();
        let g = inner
            .catalog
            .repositories
            .get_mut(&r.repository_root)
            .unwrap();
        if success {
            g.failure_detail = None;
            if let Some(descriptor) = &descriptor {
                g.recorded_without_writer.remove(descriptor);
                g.filled_descriptors.insert(descriptor.clone(), self.now());
            }
            if g.requested_descriptor == descriptor {
                g.filled_descriptor = descriptor;
                g.finished_ms = Some(self.now());
                g.commit = g.desired.as_ref().map(|d| d.tip.clone());
                g.result = Some(RebuildResult::Ok);
            } else if g.result != Some(RebuildResult::Ok) {
                // A newer descriptor can be queued while this fill runs. Its
                // status stays building/failed, but this success still becomes
                // the published fallback. Do not replace a newer reused tip.
                g.filled_descriptor = descriptor;
                g.finished_ms = Some(self.now());
                g.commit = Some(r.integration_tip.clone());
            }
        } else {
            let detail = failure
                .or_else(|| {
                    inner.jobs[&r.job_id]
                        .terminal
                        .as_ref()
                        .and_then(|t| t.refusal_detail.clone())
                })
                .or_else(|| inner.jobs[&r.job_id].monitoring_error.clone())
                .unwrap_or_else(|| "command/setup did not complete successfully".into());
            tracing::warn!(job_id = %r.job_id, repository = %r.repository_root, reason = %detail, "dependency writer fill failed");
            g.failure_detail = Some(detail);
            g.failed_descriptor = descriptor.clone();
            if g.requested_descriptor == descriptor {
                g.result = Some(RebuildResult::Failed);
            }
        }
        self.catalog(inner)?;
        if let Some(desired) = inner.catalog.repositories[&r.repository_root]
            .desired
            .clone()
        {
            self.queue_base(
                inner,
                &r.repository_root,
                &desired.tip,
                &desired.workspace,
                &[],
                &[],
            )?;
        }
        Ok(())
    }
}
