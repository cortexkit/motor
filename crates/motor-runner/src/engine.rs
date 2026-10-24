use crate::{
    backend::{self, setup_step, Backend, BackendKind, Prepared, Setup},
    cpu::CpuTopology,
    platform,
    remote_cache::publication,
    storage::*,
    workspace::{self, Trees},
    writer::declaration::Compilers,
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

/// How long the refs a have reply pins (`workspace::HELD_REFS`) outlive the
/// reply. The bundle built from the reply arrives after the sender builds and
/// uploads it and, on a tenant, after the lease is admitted; all of that is
/// minutes. Six hours covers a slow upload with a wide margin, while a held ref
/// keeps only history the mirror already had, so the cost of waiting is a few
/// small ref files and objects that stay out of compaction a little longer.
const HELD_OFFER_MS: u64 = 6 * 60 * 60 * 1000;

#[path = "accounting.rs"]
mod accounting;
#[path = "disk.rs"]
mod disk;
#[path = "health.rs"]
mod health;
#[path = "migration.rs"]
mod migration;
#[path = "retirement.rs"]
mod retirement;
#[path = "sibling_eviction.rs"]
mod sibling_eviction;
#[path = "terminal_evidence.rs"]
pub(crate) mod terminal_evidence;

/// Disk headroom for concurrent build-target growth and scratch files, not a quota.
pub const DEFAULT_FREE_SPACE_FLOOR_BYTES: u64 = 128 * 1024 * 1024 * 1024;
/// The disk reserve a Boat sandbox keeps (see `Config::disk_reserve_bytes`):
/// enough for terminal records, the catalog and the endpoint's journal to be
/// written after a job has used up every other byte.
pub const BOAT_DISK_RESERVE_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(test)]
type JobWriteHook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
#[cfg(test)]
type CatalogWriteHook = Arc<dyn Fn(CatalogWrite) -> io::Result<()> + Send + Sync>;

/// A step of a catalog write, for `Config::catalog_write`.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogWrite {
    /// Before anything is written.
    Starting,
    /// The new file has replaced the old one; the directory is not synced.
    Renamed,
    /// A snapshot already covered by a rename whose directory sync failed
    /// is about to retry that sync.
    SyncRetry,
}
#[cfg(test)]
type AccountingReportHook = Arc<dyn Fn(Uuid, &[u8]) + Send + Sync>;
pub type FreeSpaceProbe = Arc<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

/// Paths are instance-specific except build_slot_root, which is machine-wide.
#[derive(Clone)]
pub struct Config {
    /// Accept host-authenticated tenant connections over vsock and limit each
    /// job unit to 4096 tasks, with no permission to open AF_VSOCK sockets.
    pub tenant_vm: bool,
    /// Explicit single-tenant Boat compatibility: systemd before 257 cannot
    /// create private PID namespaces. All other job isolation remains enabled.
    pub boat_profile: bool,
    /// Set only on a per-attempt config clone after Boat evicts a reused cache.
    /// Only the Linux backend reads it; Boat sandboxes run Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) boat_cold_materialization: bool,
    #[cfg(target_os = "linux")]
    pub(crate) boat_readiness_withdraw: Arc<dyn Fn() -> io::Result<()> + Send + Sync>,
    #[cfg(target_os = "linux")]
    pub(crate) boat_storage_trim: Arc<dyn Fn() -> io::Result<()> + Send + Sync>,
    /// One-job Boat budget measured at boot, after idle overhead and headroom.
    /// Never used for the fleet profile's per-vCPU memory rule.
    pub boat_memory_max_bytes: Option<u64>,
    #[cfg(target_os = "linux")]
    pub(crate) private_pids: bool,
    #[cfg(target_os = "linux")]
    pub(crate) boat_unset_environment: Vec<String>,
    /// The job account's ids for Boat's PID namespace init and quota filter.
    #[cfg(target_os = "linux")]
    pub(crate) pid_init: Option<crate::isolation::pid_init::JobIdentity>,
    /// Root-private tenant channels instead of vsock, only when explicitly set.
    pub tenant_socket_dir: Option<PathBuf>,
    pub instance: String,
    pub state_root: PathBuf,
    pub build_slot_root: PathBuf,
    pub max_queue_depth: usize,
    pub max_weight: u32,
    /// Keep the fleet's daemon-only core unless the operator explicitly opts out.
    pub reserve_daemon_core: bool,
    pub rustc_slots: u32,
    /// Maximum size of the per-instance compiler cache, in GiB.
    pub sccache_size_gib: u64,
    #[cfg(target_os = "linux")]
    pub(crate) writer_account: Option<crate::isolation::WriterAccount>,
    pub backend: BackendKind,
    pub vm_config: Option<PathBuf>,
    /// Allow an explicitly selected host backend on a macOS development host only.
    pub development_allow_host_backend: bool,
    #[cfg(unix)]
    pub(crate) macos: Option<Arc<crate::macos::VmConfig>>,
    #[cfg(unix)]
    pub(crate) macos_runtime: Arc<crate::macos::Runtime>,
    /// Host CPUs used by the Windows job VM, in CPU-list syntax. Linux placements
    /// also avoid their SMT siblings while the shared slot lock is held.
    pub windows_cpus: Option<String>,
    pub windows_slot_lock: Option<PathBuf>,
    /// Host CPUs Linux jobs may use, in CPU-list syntax. When set, no core is
    /// kept back for the daemon and `max_weight`, not the list's length, is the
    /// largest vCPU claim admitted; a larger claim is refused before start
    /// instead of waiting. Windows avoidance still applies within the list.
    pub linux_cpus: Option<String>,
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
    /// Optional soft reclaim goal; absent preserves twice the admission floor.
    pub free_space_eviction_bytes: Option<u64>,
    /// Size of the preallocated file the runner keeps in its state directory
    /// so that a job which fills the disk on its own can still have its
    /// terminal record written (see `disk.rs`). Zero keeps no reserve; the
    /// Boat profile sets one.
    pub disk_reserve_bytes: u64,
    /// Only catalogued workspaces beneath these roots may be pressure-evicted.
    pub workspace_roots: Vec<PathBuf>,
    /// Authorized destinations for caller workspaces, heads and siblings.
    /// Independent of pressure eviction; Windows never materializes caller paths.
    pub owned_workspace_roots: Vec<PathBuf>,
    /// Maximum time Drop waits for existing workspace ownership to end.
    pub drop_timeout: Duration,
    /// Time limit of one sibling snapshot helper unit running the trusted
    /// Cargo prefetch. systemd stops the unit and its whole cgroup when it is
    /// exceeded, and the setup fails.
    pub snapshot_prefetch_timeout: Duration,
    /// Time limit of one sibling snapshot helper unit running a JavaScript
    /// install while a snapshot is created.
    pub snapshot_javascript_timeout: Duration,
    /// Most entries one workspace's freshness record keeps per sibling; older
    /// closed entries are folded into one `unknown` entry beyond it (see
    /// `sibling_snapshots::freshness`). At least 2.
    pub sibling_freshness_entries: usize,
    /// Most unreferenced sibling snapshots kept per repository; the least
    /// recently used beyond it are deleted after each creation, at startup
    /// and at maintenance.
    pub sibling_snapshot_cap: usize,
    /// How long an unreferenced sibling snapshot may go unused before
    /// maintenance deletes it.
    pub sibling_snapshot_idle: Duration,
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
    /// How long a published fill of the remote cache stays fresh. Every
    /// repository with a local fill record is refilled within this period, so
    /// it must stay well below the store's 30-day lifecycle (see
    /// [`crate::remote_cache::publication::validate_bounds`]).
    pub refresh_period: Duration,
    /// How long a descriptor that failed to publish waits before a fill of it
    /// may be queued again. It must stay below `refresh_period`, and
    /// `refresh_period` plus the fixed 48 h refill bound plus this must stay
    /// below the store's lifecycle.
    pub publish_retry_interval: Duration,
    /// Keep fixtures focused on foreground jobs without using drain to suppress
    /// maintenance: draining also refuses accepted jobs that are still queued.
    #[cfg(test)]
    pub(crate) disable_warm_builds: bool,
    /// Let fixtures advance the scheduler explicitly when observations must not
    /// be consumed by a concurrent background tick.
    #[cfg(test)]
    pub(crate) manual_tick: bool,
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
    /// Called at each step of a catalog write; an error fails the write at
    /// that step.
    #[cfg(test)]
    pub(crate) catalog_write: Option<CatalogWriteHook>,
    /// When set, the next setup thread fails to be created, as
    /// `thread::Builder::spawn` can.
    #[cfg(test)]
    pub(crate) fail_setup_spawn: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    pub(crate) before_replay_read: Option<JobWriteHook>,
    #[cfg(test)]
    pub(crate) systemd_command: Option<crate::systemd::CommandHook>,
    /// Replaces `sibling_snapshot_mode()`, so portable fixtures can run either
    /// sibling mode without the isolated host backend.
    #[cfg(test)]
    pub(crate) sibling_snapshot_mode_override: Option<bool>,
    /// Replaces `in_place_siblings()`, independently of the snapshot mode.
    #[cfg(test)]
    pub(crate) in_place_siblings_override: Option<bool>,
    /// Replaces the job-account ACL step of sibling snapshot creation, which
    /// needs setfacl and the motor-job account, so portable fixtures can
    /// create snapshots.
    #[cfg(test)]
    pub(crate) sibling_snapshot_access: Option<crate::sibling_snapshots::AccessHook>,
    /// Replaces the `systemd-run` call that starts a sibling snapshot helper
    /// unit, which needs sudo and systemd.
    #[cfg(test)]
    pub(crate) snapshot_helper_launcher: Option<crate::sibling_snapshots::helper::Launcher>,
    /// Replaces the host's boot ID, which sibling snapshot metadata records
    /// so that a snapshot published before a host restart is checked again.
    #[cfg(test)]
    pub(crate) boot_id_override: Option<String>,
    /// Replaces the runner's JavaScript environment digest in sibling
    /// snapshot keys, which otherwise runs each package manager.
    #[cfg(test)]
    pub(crate) javascript_environment_override:
        Option<crate::sibling_snapshots::JavascriptEnvironment>,
    #[cfg(test)]
    pub(crate) accounting_report: Option<AccountingReportHook>,
    #[cfg(test)]
    pub(crate) test_processes: Option<Arc<Mutex<crate::test_cleanup::Processes>>>,
    /// Cargo home used by fixture jobs in the non-isolated host test backend.
    #[cfg(test)]
    pub(crate) fixture_cargo_home: Option<PathBuf>,
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
    if error.kind() == io::ErrorKind::TimedOut || MirrorRepaired::is(error) {
        RefusalReason::TransferInterrupted
    } else {
        RefusalReason::BundleRejected
    }
}

/// A bundle fetch failed and the runner then removed unusable refs from the
/// mirror. The job did not run. The next attempt's have reply no longer
/// reports what those refs pointed at, so a retry can send that history.
/// Refused as an interrupted transfer, which callers may retry, with this as
/// the detail.
#[derive(Debug)]
pub(crate) struct MirrorRepaired(pub String);
impl std::fmt::Display for MirrorRepaired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for MirrorRepaired {}
impl MirrorRepaired {
    fn is(error: &io::Error) -> bool {
        error
            .get_ref()
            .is_some_and(|inner| inner.downcast_ref::<Self>().is_some())
    }
}

pub(crate) fn transfer_refusal_detail(error: &io::Error) -> String {
    let detail = error.to_string();
    if error.kind() == io::ErrorKind::TimedOut && !detail.starts_with("transfer_stalled:") {
        format!("transfer_stalled: {detail}")
    } else {
        detail
    }
}

pub(crate) fn workspace_setup_detail(step: &str, error: &io::Error) -> String {
    format!("{step}: {error}")
}

/// Request environment values shorter than this stay visible in refusal details.
pub(crate) const REDACTED_ENV_VALUE_MIN_BYTES: usize = 8;

/// vCPUs a background warm build claims when the host has them.
const WARM_BUILD_VCPUS: u32 = 4;

/// Default time limit of each sibling snapshot helper unit. A cold Cargo
/// registry or JavaScript package cache can take minutes to fill; a helper
/// still running after this is stuck.
const DEFAULT_SNAPSHOT_HELPER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Default of `Config::sibling_snapshot_cap`. With the 24-hour idle expiry
/// it bounds snapshot disk use at about 3 GiB in the worst case for the four
/// repositories used as siblings on the build server, as measured there.
const DEFAULT_SIBLING_SNAPSHOT_CAP: usize = 4;

/// Default of `Config::sibling_snapshot_idle`.
const DEFAULT_SIBLING_SNAPSHOT_IDLE: Duration = Duration::from_secs(24 * 60 * 60);

fn refuse_before_start(
    health: &Mutex<health::Health>,
    job: &mut Job,
    reason: RefusalReason,
    error: impl std::fmt::Display,
    detail: Option<String>,
    retry_after_ms: Option<u64>,
    now: u64,
) {
    if let Some(detail) = detail.as_deref() {
        tracing::warn!(job_id = %job.id, ?reason, error = %error, detail, "job refused_before_start");
    } else {
        tracing::warn!(job_id = %job.id, ?reason, error = %error, "job refused_before_start");
    }
    health.lock().unwrap().refusal(now, &reason);
    // Recovery reuses this helper for cache-building jobs interrupted by a
    // daemon restart. Such a job may already have run: only missing start
    // and process records prove here that no job process ever existed.
    if job.started_ms.is_none() && job.process.is_none() {
        job.terminal_ended(Outcome::RefusedBeforeStart { reason }, now);
    } else {
        job.terminal(Outcome::RefusedBeforeStart { reason }, now);
    }
    if let Some(detail) = detail {
        job.terminal = job.terminal.take().map(|t| t.with_refusal_detail(detail));
    }
    if let Some(retry_after_ms) = retry_after_ms {
        job.terminal = job
            .terminal
            .take()
            .map(|t| t.with_retry_after_ms(retry_after_ms));
    }
}

impl Config {
    pub fn new(state_root: impl Into<PathBuf>, instance: impl Into<String>) -> Self {
        let state_root = state_root.into();
        Self {
            tenant_vm: false,
            boat_profile: false,
            boat_cold_materialization: false,
            #[cfg(target_os = "linux")]
            boat_readiness_withdraw: Arc::new(crate::boat_quota::withdraw_ready),
            #[cfg(target_os = "linux")]
            boat_storage_trim: Arc::new(crate::boat_quota::trim_storage),
            boat_memory_max_bytes: None,
            #[cfg(target_os = "linux")]
            private_pids: true,
            #[cfg(target_os = "linux")]
            boat_unset_environment: Vec::new(),
            #[cfg(target_os = "linux")]
            pid_init: None,
            tenant_socket_dir: None,
            instance: instance.into(),
            build_slot_root: state_root.join("build-slots"),
            state_root,
            max_queue_depth: 64,
            max_weight: 28,
            reserve_daemon_core: true,
            rustc_slots: 24,
            sccache_size_gib: 200,
            #[cfg(target_os = "linux")]
            writer_account: None,
            backend: BackendKind::Host,
            vm_config: None,
            development_allow_host_backend: false,
            #[cfg(unix)]
            macos: None,
            #[cfg(unix)]
            macos_runtime: Arc::default(),
            windows_cpus: None,
            windows_slot_lock: None,
            linux_cpus: None,
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
            free_space_eviction_bytes: None,
            disk_reserve_bytes: 0,
            workspace_roots: vec![PathBuf::from("/Users")],
            owned_workspace_roots: vec![PathBuf::from("/Users")],
            drop_timeout: Duration::from_secs(60),
            snapshot_prefetch_timeout: DEFAULT_SNAPSHOT_HELPER_TIMEOUT,
            snapshot_javascript_timeout: DEFAULT_SNAPSHOT_HELPER_TIMEOUT,
            sibling_freshness_entries: crate::sibling_snapshots::freshness::DEFAULT_RECORD_ENTRIES,
            sibling_snapshot_cap: DEFAULT_SIBLING_SNAPSHOT_CAP,
            sibling_snapshot_idle: DEFAULT_SIBLING_SNAPSHOT_IDLE,
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
            refresh_period: crate::remote_cache::publication::default_refresh_period(),
            publish_retry_interval:
                crate::remote_cache::publication::default_publish_retry_interval(),
            #[cfg(test)]
            disable_warm_builds: false,
            #[cfg(test)]
            manual_tick: false,
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
            catalog_write: None,
            #[cfg(test)]
            fail_setup_spawn: Arc::default(),
            #[cfg(test)]
            before_replay_read: None,
            #[cfg(test)]
            systemd_command: None,
            #[cfg(test)]
            sibling_snapshot_mode_override: None,
            #[cfg(test)]
            in_place_siblings_override: None,
            #[cfg(test)]
            sibling_snapshot_access: None,
            #[cfg(test)]
            snapshot_helper_launcher: None,
            #[cfg(test)]
            boot_id_override: None,
            #[cfg(test)]
            javascript_environment_override: None,
            #[cfg(test)]
            accounting_report: None,
            #[cfg(test)]
            test_processes: None,
            #[cfg(test)]
            fixture_cargo_home: None,
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

    /// Whether one core is kept back for the daemon itself. A tenant VM (a
    /// hosted tenant or a Boat sandbox) is sized exactly for its tenant's jobs,
    /// and the broker places jobs up to that full size, so a tenant runner never
    /// reserves a core. Tenant mode implies this rather than relying on a flag:
    /// a provisioned tenant's runner arguments cannot carry
    /// `--reserve-daemon-core`.
    pub(crate) fn reserves_daemon_core(&self) -> bool {
        self.reserve_daemon_core && !self.tenant_vm
    }

    /// The reservation state named in a vCPU refusal.
    fn daemon_core_reservation(&self) -> &'static str {
        if self.reserves_daemon_core() {
            "enabled"
        } else if self.tenant_vm {
            "disabled in tenant mode"
        } else {
            "explicitly disabled"
        }
    }

    pub(crate) fn shared_compile_cache(&self) -> bool {
        // Hosted runners have no cache writer to populate a shared reader.
        // Even in read-only mode, sccache's C preprocessor tries to create a
        // directory in an empty cache, which fails on the read-only mount.
        !self.boat_profile && !self.tenant_vm
    }
    /// The remote compile cache's active generation and this unit's cache
    /// forwarders, or `None` while the cache is not usable for the unit.
    ///
    /// The gateway serves a generation only when the runner switch, the cache
    /// tenant's `enabled` state and job isolation all hold. Without isolation a
    /// job runs as the runner's own account, which can open the cache keys, so
    /// the switch is ignored there.
    #[cfg(unix)]
    pub(crate) fn unit_remote_cache(
        &self,
        fill: bool,
    ) -> Option<(
        std::sync::Arc<crate::remote_cache::provisioning::ActiveGeneration>,
        Vec<crate::remote_cache::transport::Forwarder>,
    )> {
        // Nothing starts the gateway yet, so no generation is ever served and
        // no unit gets a socket or a forwarder. Wiring the gateway into
        // startup supplies the generation and the unit's forwarders here.
        let _ = (fill, self.remote_cache_available(fill));
        None
    }
    /// Whether the remote compile cache can be on for a job or a fill of this
    /// runner at all, before provisioning is read: the runner switch must be
    /// set, that switch must be honoured (which needs job isolation), and only
    /// the build server may write the cache, so no other role can ever have a
    /// fill with it on.
    ///
    /// This is about the role, not about whether a writer account exists right
    /// now: a role that may write is a role whose unit gets the cache
    /// environment, and a missing account fails the fill itself later.
    #[cfg(unix)]
    pub(crate) fn remote_cache_available(&self, fill: bool) -> Option<()> {
        if !self.isolate_jobs || self.boat_profile || self.tenant_vm {
            return None;
        }
        if fill && self.vm_instance() {
            return None;
        }
        Some(())
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
        if self.macos_instance() {
            return false;
        }
        #[cfg(windows)]
        if self.windows_instance() {
            return false;
        }
        // These runners spend leased CPU time. Even an idle-started fill can
        // consume the next job's quota or hold a setup lock after it arrives.
        if self.tenant_vm || self.boat_profile {
            return false;
        }
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
            BackendKind::MacosVm => Platform::Macos,
        }
    }
    pub(crate) fn windows_instance(&self) -> bool {
        self.backend == BackendKind::WindowsVm
    }
    pub(crate) fn macos_instance(&self) -> bool {
        self.backend == BackendKind::MacosVm
    }
    pub(crate) fn vm_instance(&self) -> bool {
        self.windows_instance() || self.macos_instance()
    }
    fn cleanup_pending(&self, id: Uuid) -> bool {
        #[cfg(unix)]
        if self.macos_instance() {
            return self.macos_runtime.cleanup_pending(id);
        }
        self.windows_runtime.cleanup_pending(id)
    }
    fn backend_degraded(&self) -> bool {
        #[cfg(unix)]
        if self.macos_instance() {
            return self.macos_runtime.degraded();
        }
        false
    }
    fn configure_backend(&mut self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        if !self.macos_instance() && !self.development_allow_host_backend {
            return Err(invalid("macOS hosts require --backend macos-vm; development only: --development-allow-host-backend"));
        }
        #[cfg(unix)]
        if self.macos_instance() {
            self.macos = Some(Arc::new(crate::macos::VmConfig::load(
                self.vm_config
                    .as_deref()
                    .ok_or_else(|| invalid("--backend macos-vm requires --vm-config <file>"))?,
            )?));
            self.max_weight = 8;
            self.free_space_floor_bytes = 20 << 30;
            self.free_space_eviction_bytes = Some(40 << 30);
            self.workspace_roots = vec![self.state_root.clone()];
            self.owned_workspace_roots = vec![self.state_root.clone()];
            self.isolate_jobs = false;
        }
        #[cfg(windows)]
        if !self.windows_instance() || self.tenant_vm || self.boat_profile {
            return Err(invalid(
                "native Windows hosts require the Hyper-V windows-vm backend",
            ));
        }
        if self.windows_instance() {
            let path = self
                .vm_config
                .as_ref()
                .ok_or_else(|| invalid("--backend windows-vm requires --vm-config <file>"))?;
            self.windows = Some(Arc::new(crate::windows::VmConfig::load(path)?));
            #[cfg(unix)]
            {
                self.max_weight = 4;
            }
            #[cfg(windows)]
            {
                self.max_weight = self.windows.as_ref().unwrap().concurrent_jobs * 4;
            }
            self.workspace_roots.clear();
        }
        Ok(())
    }
    pub fn socket_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            self.state_dir().join("runner.sock")
        }
        #[cfg(windows)]
        {
            PathBuf::from(format!(r"\\.\pipe\motor-runner-{}", self.instance))
        }
    }
}

pub(crate) fn oom_memory_limit_detail(_config: &Config, weight: u32, capacity: u32) -> String {
    if let Some(bytes) = _config.boat_memory_max_bytes {
        return format!("the kernel OOM-killed a process and the job was stopped (Boat job memory budget {} bytes; the shared host memory budget also applies). More vCPUs do not increase this sandbox's usable memory.", bytes);
    }
    #[cfg(test)]
    let floor_gib = _config.memory_floor_gib;
    #[cfg(test)]
    let limit = crate::backend::memory_limit_gib(weight, floor_gib);
    #[cfg(not(test))]
    let limit = crate::backend::memory_limit_gib(weight);

    // Doubling a small request can stay at the memory floor. Start from four
    // vCPUs so the recommendation actually buys more memory, and cap it at the
    // largest job this host can admit. When the jobs slice caps job limits, a
    // larger vCPU count cannot buy more memory, so the advice names that cap.
    let mut more = (weight.max(4) * 2).min(capacity);
    if let Ok(budget) = crate::systemd::read_job_memory_budget() {
        // The slice clamps this job's limit, so only a request that still
        // raises the limit below the clamp is worth recommending.
        #[cfg(test)]
        let raised = |vcpus: u32| crate::backend::memory_max(vcpus, floor_gib).min(budget.max);
        #[cfg(not(test))]
        let raised = |vcpus: u32| crate::backend::memory_max(vcpus).min(budget.max);
        if more > weight && raised(more) <= raised(weight) {
            more = weight;
        }
    }
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
        "the kernel OOM-killed a process and the job was stopped (per-job memory limit of {limit} GiB; the shared host memory budget also applies).{advice}"
    )
}

#[test]
fn boat_memory_override_requires_explicit_profile_and_budget() {
    for (boat, bytes) in [
        (false, Some(2 << 30)),
        (true, None),
        (true, Some(512 << 20)),
    ] {
        let mut config = Config::new("/unused", "boat");
        config.boat_profile = boat;
        config.boat_memory_max_bytes = bytes;
        let error = Runner::open(config)
            .err()
            .expect("invalid Boat memory configuration must fail");
        assert!(error
            .to_string()
            .contains("--boat-profile requires an explicit measured"));
    }
    let mut config = Config::new("/unused", "boat");
    config.boat_profile = true;
    config.boat_memory_max_bytes = Some(14 << 30);
    let detail = oom_memory_limit_detail(&config, 8, 8);
    assert!(detail.contains("15032385536 bytes"));
    assert!(!detail.contains("28 GiB"));
}

#[derive(Clone)]
pub(crate) struct QueueProgress {
    entry: Arc<std::sync::atomic::AtomicU64>,
    elapsed: Arc<std::sync::atomic::AtomicU64>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}
impl QueueProgress {
    pub(crate) fn elapsed_ms(&self) -> Option<u64> {
        let (entry, elapsed) = queue_clock_at(&self.entry, &self.elapsed, (self.clock)());
        (entry != NO_QUEUE).then_some(elapsed)
    }
}

struct Inner {
    /// Reserve-file allocation claims the idle interval, then releases the state
    /// lock while writing and syncing the file.
    restoring_reserve: bool,
    writer: Option<Uuid>,
    #[cfg(target_os = "linux")]
    cache_fills: BTreeMap<Uuid, crate::writer::CacheFill>,
    /// Per fill, the write ledgers of its cache sockets, kept until its
    /// publication is decided.
    fill_ledgers: BTreeMap<Uuid, Vec<std::sync::Arc<crate::remote_cache::gateway::WriteLedger>>>,
    base_finishes: BTreeMap<Uuid, BaseFinish>,
    /// Settlement workers cannot share execution's `finalizing` claim: an
    /// exiting launch/monitor may clear that claim while cleanup still runs.
    base_settling: BTreeSet<Uuid>,
    drain_deadline: Option<std::time::Instant>,
    jobs: BTreeMap<Uuid, Job>,
    pending_replays: BTreeSet<Uuid>,
    #[cfg(test)]
    scheduler_visits: std::sync::atomic::AtomicU64,
    catalog: Catalog,
    rotation: BTreeMap<String, u64>,
    order: u64,
    dispatch: u64,
    dropping: BTreeSet<String>,
    evicting_dependencies: BTreeSet<PathBuf>,
    evicting_caches: BTreeSet<PathBuf>,
    maintained_ms: u64,
    /// The remote cache's switch state this runner last acted on; saved with
    /// the catalog so a change made while it was stopped is still a
    /// transition at the first re-read.
    publishing_seen: crate::remote_cache::publishing::Seen,
    /// When the remote cache's refresh timer last ran, in runner-clock
    /// milliseconds. Only a publishing runner uses it.
    refreshed_ms: Option<u64>,
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
    /// Accepted admissions whose record is being saved without the state
    /// lock, by job ID, with their workspace key. They are not in `jobs` until
    /// the save succeeds, but already count toward queue depth, and a
    /// workspace drop or expiry treats them as unfinished jobs.
    admission_saves: BTreeMap<Uuid, String>,
    /// Queued jobs whose terminal record (a withdrawal) is being saved
    /// without the state lock. Dispatch and the drain refusal leave them
    /// alone: the job is about to become a tombstone.
    terminal_saves: BTreeSet<Uuid>,
    /// Jobs whose unit is being spawned and is not yet recorded in the job,
    /// so the quarantine fence cannot yet match the unit to its job.
    launching: BTreeSet<Uuid>,
    /// Queued jobs whose last start attempt failed and was undone.
    start_retries: BTreeMap<Uuid, StartRetry>,
    last_eviction: Option<DiskEviction>,
    eviction_retry_after: BTreeMap<PathBuf, u64>,
    kill_retries: BTreeMap<Uuid, (u32, std::time::Instant)>,
    finalizing: BTreeSet<Uuid>,
    /// A real exit status delivered while the finalization owner is collecting
    /// changes or retrying persistence takes precedence over a monitoring error.
    exit_statuses: BTreeMap<Uuid, backend::ExecutionOutcome>,
    /// A monitor or cancellation handler may prove the unit is inactive even
    /// without its exit code. Retain that proof if another thread is already
    /// collecting changes or persisting the terminal for the same job.
    ended_executions: BTreeSet<Uuid>,
    cpu_times: BTreeMap<Uuid, u64>,
    pending_start_refusals: BTreeSet<Uuid>,
    /// The sibling fences scheduling applies; fixed by the configuration.
    sibling_fences: crate::sibling_snapshots::SiblingFences,
}

fn nested_workspace_detail(
    inner: &Inner,
    workspace: &str,
    repository: &str,
    siblings: &[SiblingSnapshot],
) -> Option<String> {
    if inner.sibling_fences == crate::sibling_snapshots::SiblingFences::Legacy {
        // Snapshot mounts must not hide a subtree of another checkout, even
        // when an existing symlink gives the two paths different spellings.
        let candidates = std::iter::once(workspace)
            .chain(std::iter::once(repository))
            .chain(siblings.iter().map(|s| s.canonical_path.as_str()));
        let mut known: Vec<&str> = inner
            .catalog
            .workspaces
            .iter()
            .flat_map(|(key, record)| {
                std::iter::once(key.as_str())
                    .chain(record.siblings.iter().map(|s| s.canonical_path.as_str()))
            })
            .collect();
        for request in inner
            .jobs
            .values()
            // Cache-fill jobs use base:<root> as a scheduling key, not a filesystem
            // path. Writer-path validation checks each fill job's actual checkout cwd.
            .filter(|job| !job.base)
            // Settled terminal jobs no longer own paths; created workspaces are
            // in the catalog. Cancellation can publish a terminal record while
            // setup is still creating its checkout, so keep including that
            // job's requested paths in overlap checks until setup finishes.
            .filter(|job| job.terminal.is_none() || inner.starting.contains(&job.id))
            .filter_map(|j| j.request.as_ref())
            .chain(inner.admitting.values())
        {
            known.push(&request.workspace_key);
            known.extend(request.siblings.iter().map(|s| s.canonical_path.as_str()));
        }
        for request in inner.preparing.values() {
            known.push(&request.workspace_key);
            known.extend(request.siblings.iter().map(|s| s.canonical_path.as_str()));
        }
        for candidate in candidates {
            let path = match workspace::destination(Path::new(candidate)) {
                Ok(path) => path,
                Err(error) => {
                    return Some(format!(
                        "cannot resolve workspace or sibling {candidate}: {error}"
                    ))
                }
            };
            for known in &known {
                let other = match workspace::destination(Path::new(known)) {
                    Ok(path) => path,
                    Err(error) => {
                        return Some(format!(
                            "cannot resolve known workspace or sibling {known}: {error}"
                        ))
                    }
                };
                if path != other && (path.starts_with(&other) || other.starts_with(&path)) {
                    return Some(format!(
                        "path {candidate} overlaps runner workspace or sibling {known}; nested workspaces aren't supported"
                    ));
                }
            }
        }
    }
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

/// What `Runner::queue_base_planned` reads from a repository's mirror for one
/// tip, read by `Runner::base_plan` before the state lock is taken.
struct BasePlan {
    tip: String,
    command: String,
    explicit_command: bool,
    descriptor: io::Result<String>,
    dependency_free: bool,
    /// `None` until read; `queue_base_planned` then reads it itself if it
    /// queues a fill job.
    fill: Option<FillInputs>,
}

/// The inputs of a new warm-build fill job.
struct FillInputs {
    git_dependencies: io::Result<Vec<GitDependency>>,
    /// Read only when `git_dependencies` is `Ok`.
    tree: Option<io::Result<String>>,
}

/// The change tracking of a warm build's compiler cache fill, begun when the
/// build is dispatched. Only Linux tracks fills; elsewhere this type has no
/// values, so there is never a fill to finish.
#[cfg(target_os = "linux")]
type CacheFill = crate::writer::CacheFill;
#[cfg(not(target_os = "linux"))]
enum CacheFill {}

/// The results of the parts of finishing a warm build that wait on child
/// processes and the disk (see `Runner::clean_base`), for
/// `Runner::record_base`.
struct BaseCleanup {
    /// The cache fill's ACL repair, when the build had a fill.
    fill: Option<io::Result<()>>,
    /// Discarding the build's scratch directory, after restoring the
    /// runner's access to it when Linux job isolation is on.
    scratch: io::Result<()>,
    publication: Option<(StoreIdentity, publication::Outcome)>,
}

/// Retained until a fill's terminal and catalog result are durably saved.
/// A failed thread spawn or a worker unwind leaves it available to the timer.
struct BaseFinish {
    request: Run,
    success: bool,
    failure: Option<String>,
    recorded: bool,
    cleanup: Option<BaseCleanup>,
    terminal: Option<Job>,
}

const BASE_SCRATCH_CLEANUP_ATTEMPTS: usize = 3;

/// Clears an in-memory marker that was set under the state lock. Its owner
/// normally clears it with `clear`, under a state lock it already holds; if
/// the owner returns early or unwinds first, dropping this takes the state
/// lock and clears it. It must therefore never be dropped while its thread
/// holds the state lock: create it while no state guard is held, so every
/// guard taken later in the same scope is dropped before it, and clear it
/// with `clear` rather than letting it drop where a guard is held.
struct Marker<F: FnOnce(&mut Inner)> {
    runner: Runner,
    clear: Option<F>,
}

impl<F: FnOnce(&mut Inner)> Marker<F> {
    fn new(runner: &Runner, clear: F) -> Self {
        Self {
            runner: runner.clone(),
            clear: Some(clear),
        }
    }
    fn clear(mut self, inner: &mut Inner) {
        if let Some(clear) = self.clear.take() {
            clear(inner);
        }
    }
}

impl<F: FnOnce(&mut Inner)> Drop for Marker<F> {
    fn drop(&mut self) {
        if let Some(clear) = self.clear.take() {
            let mut inner = self
                .runner
                .core
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            clear(&mut inner);
        }
    }
}

/// A job's queue-wait clock, which `Job` keeps in atomics shared by every
/// copy of the job. Saved before a tentative stop so that undoing the stop
/// restores the clock exactly, wait accrued meanwhile included.
#[derive(Clone, Copy)]
struct QueueClock {
    entry: u64,
    elapsed: u64,
    initialized: bool,
}

impl QueueClock {
    fn of(job: &Job) -> Self {
        use std::sync::atomic::Ordering::Acquire;
        Self {
            entry: job.queue_entry.load(Acquire),
            elapsed: job.queue_elapsed.load(Acquire),
            initialized: job.queue_wait_clock_initialized.load(Acquire),
        }
    }
    fn restore(self, job: &Job) {
        use std::sync::atomic::Ordering::Release;
        // The entry last: readers retry until the entry they read before and
        // after the elapsed time agree.
        job.queue_elapsed.store(self.elapsed, Release);
        job.queue_wait_clock_initialized
            .store(self.initialized, Release);
        job.queue_entry.store(self.entry, Release);
    }
}

/// What `start` reserved for a job, owned by the job's setup thread until
/// the start is settled: the job is Running or refused, or its reservation
/// was released. If the thread unwinds before that and before the job's
/// unit may have been spawned (`Inner::launching`), dropping this puts the
/// job back in the queue, so a panic cannot hold a workspace, permits and
/// CPUs until restart. Once a unit may exist it keeps the reservation
/// instead, since requeuing could run the job twice.
struct PendingStart {
    runner: Runner,
    id: Uuid,
    workspace: String,
    base: bool,
    /// The job as dispatch found it and the workspace's rotation before
    /// `start`. `None` once settled.
    undo: Option<(Job, Option<u64>)>,
}

impl PendingStart {
    /// Undo the dispatch: release the reservation and put the job back in
    /// the queue with its CPUs, setup time and workspace rotation as they
    /// were and its queue clock running again, giving back the writer slot
    /// of a warm build.
    /// `reason` is why the start failed. The job is not dispatched again
    /// before a delay that doubles with each consecutive failure, up to
    /// `START_RETRY_MAX_MS` (see `StartRetry`), so a failure that persists (a full disk, say) does not
    /// redo the whole setup on every scheduler pass.
    fn requeue(&mut self, inner: &mut Inner, reason: &str) {
        let Some((job, rotation)) = self.undo.take() else {
            return;
        };
        inner.starting.remove(&self.id);
        inner.reserved.remove(&self.workspace);
        match rotation {
            Some(previous) => inner.rotation.insert(self.workspace.clone(), previous),
            None => inner.rotation.remove(&self.workspace),
        };
        if self.base {
            if inner.writer == Some(self.id) {
                inner.writer = None;
            }
            #[cfg(target_os = "linux")]
            inner.cache_fills.remove(&self.id);
        }
        // `start` stopped the queue-wait clock, which this job shares with
        // its copies, keeping the wait accrued until then. It runs again
        // from now: the failed attempt's setup time is not queue wait.
        let now = self.runner.now();
        job.resume_queue_wait(now);
        inner.jobs.insert(self.id, job);
        let retry = inner.start_retries.entry(self.id).or_default();
        let first = retry.delay_ms == 0;
        retry.delay_ms = if first {
            START_RETRY_MS
        } else {
            retry.delay_ms.saturating_mul(2).min(START_RETRY_MAX_MS)
        };
        retry.not_before_ms = now.saturating_add(retry.delay_ms);
        // One warning when the job starts failing and whenever the reason
        // changes; repeats of the same failure are logged at debug.
        if first || retry.reason != reason {
            tracing::warn!(job_id = %self.id, reason, retry_in_ms = retry.delay_ms, "job start failed; the job stays queued and is retried");
            retry.reason = reason.to_owned();
        } else {
            tracing::debug!(job_id = %self.id, reason, retry_in_ms = retry.delay_ms, "job start failed again");
        }
    }
    /// The start is settled; there is nothing left to undo.
    fn settled(&mut self) {
        self.undo = None;
    }
}

impl Drop for PendingStart {
    fn drop(&mut self) {
        if self.undo.is_none() {
            return;
        }
        let runner = self.runner.clone();
        let mut inner = runner
            .core
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !inner.starting.contains(&self.id) {
            return;
        }
        // Once its unit may have been spawned (`launching`), requeuing the
        // job could run it twice, so its reservation is kept instead.
        if inner.launching.contains(&self.id) {
            tracing::error!(job_id = %self.id, "job start unwound during launch; its reservation is kept");
            return;
        }
        if inner
            .jobs
            .get(&self.id)
            .is_some_and(|job| job.state == JobState::Queued)
        {
            self.requeue(&mut inner, "job start unwound");
            return;
        }
        // Cancelled or withdrawn while being set up (its terminal is saved),
        // then the setup unwound: there is nothing to requeue, but its
        // reservation goes, as when a launch finds the job ended.
        tracing::error!(job_id = %self.id, "job start unwound after the job ended; its reservation is released");
        inner.starting.remove(&self.id);
        inner.reserved.remove(&self.workspace);
        let request = self.undo.take().and_then(|(job, _)| job.request);
        if let (true, Some(request)) = (self.base, request) {
            if let Err(error) = runner.finish_base(&mut inner, &request, false, None) {
                tracing::error!(job_id = %self.id, %error, "warm build bookkeeping failed");
            }
        }
    }
}

/// The first delay before a job whose start failed is dispatched again; it
/// doubles with each consecutive failure up to `START_RETRY_MAX_MS`.
const START_RETRY_MS: u64 = 1000;
const START_RETRY_MAX_MS: u64 = 60_000;

/// The bounds on the `retry_after_ms` hint a capacity refusal carries, in
/// milliseconds. The hint reports the median recent queue wait, so a caller
/// waits as long as jobs are usually waiting instead of guessing.
///
/// The floor is one backoff step of a job whose start failed
/// (`START_RETRY_MS`): a job that arrives while another is still being set up
/// waits for that setup before it can start, so a hint below it would send the
/// caller back before anything could free the capacity it is waiting for. The
/// ceiling is `START_RETRY_MAX_MS`, the longest the runner itself backs off
/// before retrying a failed start: a median past it says the queue is not
/// clearing on any schedule the runner observes, and a longer hint would keep a
/// caller waiting while its own deadline drains instead of letting it decide.
const CAPACITY_RETRY_MIN_MS: u64 = START_RETRY_MS;
const CAPACITY_RETRY_MAX_MS: u64 = START_RETRY_MAX_MS;

/// Retry state of a queued job whose last start attempt failed.
#[derive(Default)]
struct StartRetry {
    /// Runner clock time before which dispatch does not start the job.
    not_before_ms: u64,
    delay_ms: u64,
    reason: String,
}

/// How far catalog writes got, by snapshot generation.
#[derive(Default)]
struct CatalogWritten {
    /// The newest snapshot renamed into place: the bytes the path holds.
    renamed: u64,
    /// The newest snapshot known durable (its directory synced too).
    synced: u64,
}

/// The catalog as serialized under the state lock, for `Runner::write_catalog`
/// to write after the lock is released.
#[must_use = "a catalog snapshot changes nothing until it is written"]
struct CatalogSnapshot {
    generation: u64,
    bytes: Vec<u8>,
}

struct Reservation {
    repository: String,
    commit: String,
    siblings: Vec<SiblingSnapshot>,
    since_ms: u64,
}

struct QuarantineCleanup {
    blocked: Vec<Uuid>,
    last_error_ms: Option<u64>,
}

struct Core {
    /// Admission/prepare take this before claiming state, never while holding inner.
    reserve_restore: Mutex<()>,
    tenant_events: Option<Arc<crate::tenant::EventLog>>,
    draining: std::sync::atomic::AtomicBool,
    /// Separate from the fsync/state lock so a slow persistence operation cannot
    /// prevent the server from observing its shutdown deadline.
    drain_control: Mutex<Option<DrainWaiter>>,
    config: Config,
    backend: Box<dyn Backend>,
    trees: Trees,
    /// The last-seen state of the remote cache's provisioning, and the
    /// publication state derived from it. Without a configuration that is
    /// publishing, this holds a policy that changes nothing.
    publishing: Mutex<crate::remote_cache::publishing::State>,
    /// Creates sibling snapshots and serializes work on each one; used only
    /// in sibling snapshot mode.
    snapshots: crate::sibling_snapshots::store::SnapshotStore,
    inner: crate::state_lock::StateLock<Inner>,
    job_logs: Mutex<BTreeMap<Uuid, Arc<JobLog>>>,
    /// Ref mutations acquire this before the state lock, which is released
    /// before Git runs. Keep pinning and ownership publication in one turn.
    ref_mutation: Mutex<()>,
    /// Commits each have reply reported, by mirror, with the time (runner
    /// clock) after which their `refs/held/` refs may be pruned. Memory only:
    /// after a restart every connection that received a reply is gone, so the
    /// first prune removes all held refs.
    held_offers: Mutex<BTreeMap<PathBuf, BTreeMap<String, u64>>>,
    /// Immutable until restart after operator reconciliation. Even readable
    /// workspace metadata cannot establish a damaged job's full CPU/cache use.
    quarantined: BTreeMap<Uuid, QuarantinedJob>,
    quarantine_admission: Mutex<Option<String>>,
    quarantine_probe: Mutex<Option<std::time::Instant>>,
    quarantine_cleanup: Mutex<QuarantineCleanup>,
    /// Serialize pressure cleanup, without holding the output/persistence lock.
    disk: Mutex<()>,
    /// Numbers catalog snapshots in the order they were taken under the
    /// state lock (see `Runner::catalog_snapshot`).
    catalog_generation: std::sync::atomic::AtomicU64,
    /// Held while a catalog snapshot is written, so writes never overlap
    /// (they share one temporary file); records how far writes got. The
    /// state lock may be held when taking this lock, but must never be taken
    /// while holding it.
    catalog_written: Mutex<CatalogWritten>,
    health: Mutex<health::Health>,
    repository_usage: Mutex<health::RepositoryUsage>,
    background_deletes: Arc<std::sync::atomic::AtomicUsize>,
    _lock: File,
    rustc_version: String,
    /// Build server compiler versions a declared fill's descriptor extends. They
    /// are read once, at startup, so one runner's descriptors do not move under
    /// a host whose tools change.
    compilers: Compilers,
    cpus: CpuTopology,
}

const REPLAY_PAGE_BYTES: u64 = 1024 * 1024;
const REPLAY_PAGE_CHUNKS: usize = 64;

#[derive(Default)]
struct JobLog {
    /// Acquire before the state lock, never while waiting under it. Serializes
    /// output/index commits with cancellation, deadlines and terminal commits.
    /// Output commits take it through `write_for_output` and cancellation
    /// through `write_ahead_of_output`; other callers lock it directly.
    write: Mutex<()>,
    /// Held by a cancel or kill retry from its last check that finalization
    /// is not claimed until its unit query and stop have been issued, which
    /// run without the state lock. Claiming finalization takes it first, so
    /// a job being finalized is never signalled. Acquire after `write`, if
    /// held, and never while holding the state lock.
    signalling: Mutex<()>,
    /// How many cancels are waiting for `write`. An output commit waits for
    /// this to reach zero before it takes `write`. Without that, a job whose
    /// output arrives faster than it can be committed keeps a cancel waiting
    /// indefinitely: the monitor thread releases `write` after one commit
    /// (three fsyncs) and takes it again for the next within microseconds,
    /// long before the woken cancel thread is scheduled on a busy host.
    cancels_waiting: Mutex<usize>,
    cancels_admitted: std::sync::Condvar,
    retention: Mutex<ReplayRetention>,
}

#[derive(Default)]
struct ReplayRetention {
    readers: usize,
    pinned: BTreeMap<PathBuf, usize>,
    retired: Vec<PathBuf>,
}

struct ReplayLease {
    log: Arc<JobLog>,
    paths: Vec<PathBuf>,
}

fn delete_retired_chunks(paths: Vec<PathBuf>) {
    for path in paths {
        if let Err(error) = fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), %error, "retired output cleanup failed");
        }
    }
}

impl JobLog {
    /// Take `write`. Neither `write` nor `signalling` guards data of its
    /// own: a job's state lives in its record and under the state lock, and
    /// the markers set around a save are cleared by guards when it unwinds.
    /// So a lock poisoned by a panic while it was held (for instance during
    /// a record save) is taken like a healthy one. That does not undo work
    /// the panic interrupted; it keeps every later step for the job (cancel,
    /// withdraw, setup, finish) from panicking too.
    fn lock_write(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// `lock_write` without waiting: `None` while another holder has it.
    fn try_lock_write(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        match self.write.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
    /// Take `signalling`, recovering from poison as `lock_write` does.
    fn lock_signalling(&self) -> std::sync::MutexGuard<'_, ()> {
        self.signalling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Take `write` for a cancel. Output commits that have not started yet
    /// wait until this cancel holds the lock; one already committing
    /// finishes first.
    fn write_ahead_of_output(&self) -> std::sync::MutexGuard<'_, ()> {
        *self.cancels_waiting.lock().unwrap() += 1;
        let write = self.lock_write();
        let mut waiting = self.cancels_waiting.lock().unwrap();
        *waiting -= 1;
        if *waiting == 0 {
            self.cancels_admitted.notify_all();
        }
        drop(waiting);
        write
    }
    /// Take `write` for an output commit, after every waiting cancel.
    fn write_for_output(&self) -> std::sync::MutexGuard<'_, ()> {
        let mut waiting = self.cancels_waiting.lock().unwrap();
        while *waiting > 0 {
            waiting = self.cancels_admitted.wait(waiting).unwrap();
        }
        drop(waiting);
        self.lock_write()
    }
    fn lease(self: &Arc<Self>, paths: Vec<PathBuf>) -> ReplayLease {
        let mut retention = self.retention.lock().unwrap();
        retention.readers += 1;
        for path in &paths {
            *retention.pinned.entry(path.clone()).or_default() += 1;
        }
        ReplayLease {
            log: self.clone(),
            paths,
        }
    }
    fn retire(&self, paths: Vec<PathBuf>) {
        let mut retention = self.retention.lock().unwrap();
        let mut deletable = Vec::new();
        for path in paths {
            if retention.pinned.contains_key(&path) {
                retention.retired.push(path);
            } else {
                deletable.push(path);
            }
        }
        drop(retention);
        delete_retired_chunks(deletable);
    }
}

impl Drop for ReplayLease {
    fn drop(&mut self) {
        let mut retention = self.log.retention.lock().unwrap();
        retention.readers -= 1;
        for path in &self.paths {
            let count = retention.pinned.get_mut(path).unwrap();
            *count -= 1;
            if *count == 0 {
                retention.pinned.remove(path);
            }
        }
        let retired = std::mem::take(&mut retention.retired);
        let (pinned, retired): (Vec<_>, Vec<_>) = retired
            .into_iter()
            .partition(|path| retention.pinned.contains_key(path));
        retention.retired = pinned;
        drop(retention);
        delete_retired_chunks(retired);
    }
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

struct WorkspaceDrop {
    runner: Runner,
    key: String,
}
impl Drop for WorkspaceDrop {
    fn drop(&mut self) {
        self.runner
            .core
            .inner
            .lock()
            .unwrap()
            .dropping
            .remove(&self.key);
    }
}

pub(crate) enum Admission {
    Accepted(u32),
    Refused(RefusalReason),
    Duplicate,
    DiskFull(RunnerDiskFull),
}

#[cfg(unix)]
fn private_state_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| invalid("runner state path contains a NUL byte"))?;
        let acl_size = unsafe {
            libc::getxattr(
                cpath.as_ptr(),
                c"system.posix_acl_access".as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if acl_size >= 0 {
            // chmod changes an extended ACL's mask, disabling named grants.
            // Restrict owning-group/other access and retain the named entries.
            let output = std::process::Command::new("/usr/bin/setfacl")
                .args(["-m", "u::rwx,g::---,o::---", "--"])
                .arg(path)
                .output()?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "make runner state ACL private at {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENODATA | libc::ENOTSUP)) {
            return Err(error);
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
fn private_state_directory(path: &Path) -> io::Result<()> {
    // script/windows-runner/install.ps1 protects C:\ProgramData\MotorRunner.
    // State directories inherit that root DACL; readonly attributes do not
    // express private access on Windows.
    crate::filesystem::device(path).map(|_| ())
}

impl Runner {
    /// The publication policy every fill, reuse decision and record reads, as
    /// the latest re-read of the provisioning files left it.
    pub(crate) fn publishing_policy(&self) -> crate::remote_cache::publication::PublishPolicy {
        self.core
            .publishing
            .lock()
            .unwrap()
            .policy(&self.core.config)
    }
    /// The store identity the active generation addresses while the cache is
    /// publishing, if any.
    pub(crate) fn publishing_identity(&self) -> Option<StoreIdentity> {
        self.core.publishing.lock().unwrap().identity()
    }
    /// Applies one re-read of the provisioning files: stores the new state,
    /// forgets invalidated records, saves the catalog without engine locks and
    /// queues any newly unpublished desired tips immediately.
    // Production startup does not yet run the provisioning watcher.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn observe_publishing(
        &self,
        reread: crate::remote_cache::provisioning::Reread,
    ) -> io::Result<bool> {
        let (catalog, desired) = {
            let mut inner = self.core.inner.lock().unwrap();
            let mut state = self.core.publishing.lock().unwrap();
            let was = state.clone();
            state.observe(reread);
            let changed = state.reconcile(&was, &mut inner.catalog);
            let seen = state.seen();
            let seen_changed = inner.publishing_seen != seen;
            inner.catalog.publishing_seen = seen.clone();
            inner.publishing_seen = seen;
            if !changed.refill && !seen_changed {
                return Ok(false);
            }
            let desired: Vec<_> = if changed.refill {
                inner.refreshed_ms = None;
                inner
                    .catalog
                    .repositories
                    .iter()
                    .filter_map(|(root, record)| {
                        record
                            .desired
                            .clone()
                            .map(|desired| (root.clone(), desired))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            (self.catalog_snapshot(&inner)?, desired)
        };
        self.write_catalog(catalog)?;
        for (root, desired) in desired {
            self.queue_next_base(&root, &desired)?;
        }
        Ok(true)
    }
    /// Opening a table performs crash recovery before any channel is served.
    /// An exclusive instance lock prevents a second daemon from recovering live jobs.
    pub fn open(config: Config) -> io::Result<Self> {
        let backend = backend::select(&config)?;
        Self::open_with_backend(config, backend)
    }
    fn open_with_backend(mut config: Config, backend: Box<dyn Backend>) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        if config.boat_profile {
            config.free_space_floor_bytes = config
                .free_space_floor_bytes
                .max(crate::boat_quota::READY_FREE_BYTES);
            if let Some(goal) = &mut config.free_space_eviction_bytes {
                *goal = (*goal).max(40 << 30);
            }
            let probe = config.free_space_probe.clone();
            config.free_space_probe =
                Arc::new(move |path| Ok(probe(path)?.min(probe(Path::new("/"))?)));
        }
        config.configure_backend()?;
        if config.instance.is_empty()
            || !config
                .instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || config.max_weight == 0
            || config.rustc_slots == 0
            || config.sccache_size_gib == 0
            || config.snapshot_prefetch_timeout.is_zero()
            || config.snapshot_javascript_timeout.is_zero()
            || config.sibling_freshness_entries < 2
            || config
                .sccache_size_gib
                .checked_mul(1024 * 1024 * 1024)
                .is_none()
            || config
                .workspace_roots
                .iter()
                .chain(&config.owned_workspace_roots)
                .any(|p| !workspace::absolute(&p.to_string_lossy()) || p.parent().is_none())
        {
            return Err(invalid("invalid runner configuration"));
        }
        // The remote cache's refresh and retry periods have to leave room for a
        // failed refresh to be retried before the store's lifecycle deletes
        // the entries, so a configuration that breaks that is refused here, at
        // load, rather than after an outage.
        crate::remote_cache::publication::validate_bounds(
            config.refresh_period,
            config.publish_retry_interval,
        )?;
        if config.boat_profile != config.boat_memory_max_bytes.is_some()
            || config
                .boat_memory_max_bytes
                .is_some_and(|bytes| bytes < (1 << 30))
        {
            return Err(invalid("--boat-profile requires an explicit measured --boat-memory-max-bytes budget of at least 1 GiB; other profiles must not override memory"));
        }
        backend.validate(&config)?;
        if config
            .free_space_eviction_bytes
            .is_some_and(|soft| soft <= config.free_space_floor_bytes)
        {
            return Err(invalid(
                "disk eviction threshold must be above the refusal floor",
            ));
        }
        #[cfg(test)]
        let cpus = config
            .cpu_topology
            .clone()
            .map(Ok)
            .unwrap_or_else(CpuTopology::read)?;
        #[cfg(not(test))]
        let cpus = CpuTopology::read()?;
        let cpus = if config.reserves_daemon_core() {
            cpus
        } else {
            cpus.without_daemon_reservation()
        };
        let cpus = if let Some(vm) = &config.windows {
            #[cfg(unix)]
            {
                CpuTopology::fixed(vm.cpu_set.clone())?
            }
            #[cfg(windows)]
            {
                CpuTopology::fixed((0..vm.concurrent_jobs * 4).collect())?
            }
        } else {
            cpus
        };
        let cpus = match (&config.windows_cpus, &config.windows_slot_lock) {
            (None, None) => cpus,
            (Some(list), Some(path)) if config.windows.is_none() => {
                cpus.avoid_windows(&crate::cpu::parse_list(list)?, path.clone())?
            }
            _ => {
                return Err(invalid(
                    "--windows-cpus and --windows-slot-lock must be supplied together on the Linux runner",
                ));
            }
        };
        // Confine after Windows avoidance so the Windows CPUs are checked
        // against every online CPU, not only the listed ones.
        let cpus = match &config.linux_cpus {
            None => cpus,
            Some(_) if config.vm_instance() => {
                return Err(invalid(
                    "--linux-cpus applies only to a runner serving Linux jobs",
                ));
            }
            Some(list) => cpus.confine(&crate::cpu::parse_list(list)?, config.max_weight)?,
        };
        #[cfg(target_os = "linux")]
        if config.isolate_jobs && !config.windows_instance() {
            // The runner's own command hook answers this query in tests, so a
            // test can run the isolated path without a host manager.
            config.private_pids = match crate::backend::systemctl(
                &config,
                &["show", "--property=Version", "--value"],
            ) {
                Ok(output) => crate::systemd::manager_private_pids(&output, config.boat_profile)?,
                Err(error) => return Err(error),
            };
            if config.boat_profile {
                config.boat_unset_environment = crate::systemd::provider_environment_names()?;
            }
            crate::isolation::validate_account()?;
            if config.boat_profile && !config.private_pids {
                // Job units exec the installed runner as their PID namespace
                // init; tests run from the build tree and never reach a unit.
                #[cfg(not(test))]
                crate::isolation::pid_init::check_installed_runner()?;
                config.pid_init = Some(crate::isolation::job_identity()?);
            }
            #[cfg(test)]
            if let Some((cargo_home, rustup_home, bun_bin)) = &config.shared_read_roots {
                crate::isolation::prepare_shared_read_access_from(
                    cargo_home,
                    rustup_home,
                    bun_bin.as_deref(),
                )?;
            } else {
                crate::isolation::prepare_shared_read_access(
                    &crate::isolation::JavascriptTools::for_config(&config),
                )?;
            }
            #[cfg(not(test))]
            crate::isolation::prepare_shared_read_access(
                &crate::isolation::JavascriptTools::for_config(&config),
            )?;
        }
        // The operator's state root may be reached through symlinks (for example
        // when storage lives on another volume). Resolve only that trusted root
        // once; job-controlled descendants must still pass the no-follow walk.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&config.state_root)?;
        }
        #[cfg(windows)]
        fs::create_dir_all(&config.state_root)?;
        #[cfg(unix)]
        {
            config.state_root = fs::canonicalize(&config.state_root)?;
            if config.macos_instance() {
                config.workspace_roots = vec![config.state_root.clone()];
                config.owned_workspace_roots = vec![config.state_root.clone()];
            }
        }
        let state = config.state_dir();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            // Bind may initially inherit a loose umask. A private, owned parent
            // prevents other users reaching the socket before its chmod.
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&state)?;
            let metadata = fs::symlink_metadata(&state)?;
            if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(invalid(
                    "runner state directory must be owned by the daemon and not a symlink",
                ));
            }
            private_state_directory(&state)?;
        }
        #[cfg(windows)]
        {
            fs::create_dir_all(&state)?;
            private_state_directory(&state)?;
        }
        #[cfg(target_os = "linux")]
        if config.isolate_jobs && !config.windows_instance() {
            crate::isolation::prepare_private_state(&state)?;
        }
        fs::create_dir_all(state.join("jobs"))?;
        if config.windows_instance() {
            private_state_directory(&state.join("jobs"))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("daemon.lock"))?;
        lock.try_lock_exclusive()?;
        // Holding the daemon lock, this is the only runner of this instance,
        // so any helper unit of this instance belongs to an earlier run.
        #[cfg(any(target_os = "linux", test))]
        if config.sibling_snapshot_mode() {
            crate::sibling_snapshots::helper::stop_stale(&config)?;
        }
        #[cfg(target_os = "linux")]
        if config.isolate_jobs && !config.windows_instance() {
            // A bad cache account must not take the foreground runner down.
            // Keep the validated identity only after its storage is also safe.
            let javascript = crate::isolation::JavascriptTools::for_config(&config);
            config.writer_account =
                match crate::isolation::validate_writer_account().and_then(|account| {
                    crate::isolation::prepare_writer_shared_read_access(&javascript)?;
                    Ok(account)
                }) {
                    Ok(account) => Some(account),
                    Err(error) => {
                        tracing::warn!(%error, "dependency writer disabled; cache reads may miss");
                        None
                    }
                };
        }
        #[cfg(unix)]
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
            private_state_directory(&trees.mirrors)?;
        }
        let now = (config.clock)();
        let health = Mutex::new(health::Health::new(now));
        let mut jobs = BTreeMap::new();
        let mut quarantined = BTreeMap::new();
        let mut recovered = Vec::new();
        let mut lost = Vec::new();
        // Catalog damage cannot be treated like one damaged execution record:
        // it is the ownership index for every retained workspace and cache.
        let mut catalog: Catalog = match read_json(&state.join("catalog.json")) {
            Ok(catalog) => catalog,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Catalog::default(),
            Err(error) => return Err(invalid(format!("cannot read workspace catalog: {error}"))),
        };
        let quarantine_root = state.join("quarantine");
        match fs::read_dir(&quarantine_root) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry?.path();
                    if path.extension().is_none_or(|extension| extension != "json") {
                        continue;
                    }
                    let job: QuarantinedJob = read_json(&path).map_err(|error| {
                        invalid(format!("cannot read quarantine marker: {error}"))
                    })?;
                    if path.file_stem().and_then(|name| name.to_str())
                        != Some(job.id.to_string().as_str())
                    {
                        return Err(invalid("quarantine identity does not match its filename"));
                    }
                    let record = state
                        .join("jobs")
                        .join(job.id.to_string())
                        .join("record.json");
                    job.preserve_rollback_fence(&record)?;
                    quarantined.insert(job.id, job);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // An unpublished first record cannot have launched a job. Old runners
        // also ignore this separate staging root during a rollback.
        match fs::remove_dir_all(state.join("job-staging")) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for entry in fs::read_dir(state.join("jobs"))? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let record = path.join("record.json");
            let id = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| Uuid::parse_str(name).ok());
            if id.is_some_and(|id| quarantined.contains_key(&id)) {
                continue;
            }
            match read_json::<Job>(&record) {
                Ok(job) => {
                    recover_output_tail(&path, &job);
                    jobs.insert(job.id, job);
                }
                Err(error) => {
                    let id =
                        id.ok_or_else(|| invalid("unreadable job has no directory identity"))?;
                    let job = QuarantinedJob::inspect(id, &record, now);
                    // Publish the fence before skipping the record. Do not move
                    // or rewrite evidence that an older runner must still reject.
                    atomic_json_private(&quarantine_root.join(format!("{id}.json")), &job)?;
                    job.preserve_rollback_fence(&record)?;
                    tracing::error!(%id, %error, "job quarantined; reconcile ownership before admitting more work");
                    quarantined.insert(id, job);
                }
            }
        }
        if quarantined.is_empty() {
            backend.initialize(&config)?;
            if state.join("spools").exists() {
                fs::remove_dir_all(state.join("spools"))?;
            }
        }
        crate::network::initialize(&mut config);
        fs::create_dir_all(state.join("spools"))?;
        if config.windows_instance() {
            private_state_directory(&state.join("spools"))?;
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
                RefusalReason::LostToRestart,
                "runner restarted during a background fill",
                Some("runner restarted during a background fill".into()),
                None,
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
        if let Some(account) = config.writer_account.filter(|_| quarantined.is_empty()) {
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
            if (config.vm_instance() && job.started_ms.is_none())
                || (job.pending() && job.started_ms.is_none() && job.process.is_none())
            {
                refuse_before_start(
                    &health,
                    &mut job,
                    RefusalReason::LostToRestart,
                    "runner restarted before the job could start",
                    Some("runner restarted before the job could start".into()),
                    None,
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
        if config.vm_instance() {
            // Catalog entries are caller checkout paths on the build server, and
            // those checkouts belong to the Linux instance. The Windows instance
            // works only in its own staging, so it must never touch them, not
            // even to look: an entry from an older catalog is dropped unread.
            catalog.workspaces.clear();
            catalog.sibling_commits.clear();
            catalog.repositories.clear();
        }
        if !config.vm_instance() {
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
        // The fill records describe the shared compiler cache's contents. A
        // cache found empty here was emptied while the runner was down: by
        // hand, or by an eviction that moved it aside but did not live to
        // save the catalog. Its fills are gone, so each repository's next
        // pass must fill it again rather than reuse its record. With a writer
        // account the directory always exists by now (prepared just above);
        // without one no job mounts it, so an absent directory is left alone.
        // The shared compiler cache exists only on Linux runners.
        #[cfg(target_os = "linux")]
        if !config.windows_instance() && config.sccache_dir().is_dir() {
            let fleet = config.sccache_dir();
            match crate::compile_cache::holds_entries(&fleet) {
                Ok(true) => {}
                Ok(false) => {
                    if catalog
                        .repositories
                        .values()
                        .any(|repository| !repository.filled_descriptors.is_empty())
                    {
                        tracing::info!("compiler cache is empty; forgetting recorded fills");
                    }
                    catalog.forget_compiler_cache_fills();
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot inspect the compiler cache; keeping recorded fills")
                }
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
            for failure in repository.publish_failures.values_mut() {
                if failure.attempts >= publication::PUBLISH_RETRY_ATTEMPTS {
                    failure.attempts = 0;
                }
            }
            if repository.desired.is_some() {
                repository.requested_descriptor = Some("restart".into());
            }
        }
        // A workspace was last used no earlier than its jobs finished. A crash
        // after a terminal record was saved but before the catalog recorded
        // that use (possible with older runners, which saved the catalog
        // second) would otherwise let the maintenance below expire the
        // workspace of a job that has only just finished.
        for job in jobs
            .values()
            .filter(|job| !job.base && job.started_ms.is_some())
        {
            if let (Some(finished_ms), Some(workspace)) =
                (job.finished_ms, catalog.workspaces.get_mut(job.key()))
            {
                workspace.used_ms = workspace.used_ms.max(finished_ms);
            }
        }
        if config.windows_instance() {
            atomic_json_private(&state.join("catalog.json"), &catalog)?;
        } else {
            atomic_json(&state.join("catalog.json"), &catalog)?;
        }
        let order = jobs.values().map(|j| j.order).max().unwrap_or(0);
        #[cfg(unix)]
        let macos_version = config.macos.as_ref().map(|vm| vm.toolchain_version.clone());
        #[cfg(not(unix))]
        let macos_version: Option<String> = None;
        let rustc_version = if let Some(version) = macos_version {
            version
        } else if let Some(vm) = &config.windows {
            vm.toolchain_version.clone()
        } else {
            std::process::Command::new("rustc")
                .arg("--version")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?
        };
        let repository_usage = health::RepositoryUsage::from_jobs(jobs.values(), now);
        let tenant_events = if config.tenant_vm {
            let log = crate::tenant::EventLog::open(&state)?;
            for job in jobs.values() {
                log.observe(job)?;
            }
            Some(Arc::new(log))
        } else {
            None
        };
        let sibling_fences = config.sibling_fences();
        // The switch is honoured only with job isolation, and only the build
        // server ever writes the cache.
        let publishing_switch = config.isolate_jobs
            && !config.boat_profile
            && !config.tenant_vm
            && !config.vm_instance();
        // A configuration flipped while the runner was stopped is still a
        // transition, not a first read: the switch state the previous run
        // last acted on is restored before the first re-read.
        let mut publishing_state = crate::remote_cache::publishing::State::new(publishing_switch);
        crate::remote_cache::publishing::State::restore(
            &mut publishing_state,
            publishing_switch,
            catalog.publishing_seen.clone(),
        );
        let publishing_seen = catalog.publishing_seen.clone();
        let runner = Self {
            core: Arc::new(Core {
                reserve_restore: Mutex::new(()),
                publishing: Mutex::new(publishing_state),
                tenant_events,
                draining: std::sync::atomic::AtomicBool::new(false),
                drain_control: Mutex::new(None),
                background_deletes: config.background_deletes.clone(),
                config,
                backend,
                trees,
                snapshots: crate::sibling_snapshots::store::SnapshotStore::default(),
                inner: crate::state_lock::StateLock::new(Inner {
                    restoring_reserve: false,
                    writer: None,
                    #[cfg(target_os = "linux")]
                    cache_fills: BTreeMap::new(),
                    fill_ledgers: BTreeMap::new(),
                    base_finishes: BTreeMap::new(),
                    base_settling: BTreeSet::new(),
                    drain_deadline: None,
                    jobs,
                    pending_replays: BTreeSet::new(),
                    #[cfg(test)]
                    scheduler_visits: std::sync::atomic::AtomicU64::new(0),
                    catalog,
                    order,
                    rotation: BTreeMap::new(),
                    dispatch: 0,
                    dropping: BTreeSet::new(),
                    maintained_ms: now,
                    publishing_seen,
                    refreshed_ms: None,
                    reserved: BTreeMap::new(),
                    starting: BTreeSet::new(),
                    transfers: BTreeSet::new(),
                    preparing: BTreeMap::new(),
                    admitting: BTreeMap::new(),
                    admission_saves: BTreeMap::new(),
                    terminal_saves: BTreeSet::new(),
                    launching: BTreeSet::new(),
                    start_retries: BTreeMap::new(),
                    evicting_dependencies: BTreeSet::new(),
                    evicting_caches: BTreeSet::new(),
                    last_eviction: None,
                    eviction_retry_after: BTreeMap::new(),
                    kill_retries: BTreeMap::new(),
                    finalizing: BTreeSet::new(),
                    exit_statuses: BTreeMap::new(),
                    ended_executions: BTreeSet::new(),
                    cpu_times: BTreeMap::new(),
                    pending_start_refusals: BTreeSet::new(),
                    sibling_fences,
                }),
                job_logs: Mutex::new(BTreeMap::new()),
                ref_mutation: Mutex::new(()),
                held_offers: Mutex::new(BTreeMap::new()),
                quarantine_admission: Mutex::new(
                    (!quarantined.is_empty())
                        .then(|| "live unit ownership has not been inspected".into()),
                ),
                quarantine_probe: Mutex::new(None),
                quarantine_cleanup: Mutex::new(QuarantineCleanup {
                    blocked: quarantined.keys().copied().collect(),
                    last_error_ms: None,
                }),
                quarantined,
                disk: Mutex::new(()),
                catalog_generation: std::sync::atomic::AtomicU64::new(0),
                catalog_written: Mutex::new(CatalogWritten::default()),
                health,
                repository_usage: Mutex::new(repository_usage),
                _lock: lock,
                rustc_version,
                compilers: Compilers::probe(&["cc", "c++", "ccache"]),
                cpus,
            }),
        };
        #[cfg(test)]
        tests::register_runner(&runner);
        // Deletions a previous run moved aside but did not finish.
        if runner.core.quarantined.is_empty() {
            if let Ok(entries) = fs::read_dir(runner.trash()) {
                for entry in entries.flatten() {
                    runner.purge(entry.path());
                }
            }
            runner.clean_sibling_snapshot_leftovers();
            runner.migrate_legacy_storage()?;
        }
        let scratch = runner.core.config.writer_scratch_dir();
        if scratch.exists() && runner.core.quarantined.is_empty() {
            #[cfg(target_os = "linux")]
            if runner.core.config.isolate_jobs {
                crate::isolation::restore_runner_access(&scratch)?;
            }
            runner.discard(&scratch)?;
        }
        runner.refresh_quarantine_admission(true);
        runner.check_mirrors();
        runner.maintenance()?;
        for (id, process) in lost {
            let mut job = runner.core.inner.lock().unwrap().jobs[&id].clone();
            let evidence = runner.execution_evidence(&job);
            let detail = runner.explained_terminal(
                &job,
                "restart_execution_not_reattachable",
                "runner restarted; execution and durable exit evidence cannot be reattached",
                &evidence,
            );
            job.terminal = job.terminal.take().map(|t| t.with_refusal_detail(detail));
            runner.save(&job)?;
            runner.core.inner.lock().unwrap().jobs.insert(id, job);
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
        #[cfg(test)]
        if runner.core.config.manual_tick {
            return Ok(runner);
        }
        if runner.core.config.disk_reserve_bytes > 0 {
            if let Err(error) = runner.disk_upkeep() {
                tracing::warn!(%error, "runner disk upkeep failed");
            }
            let weak = Arc::downgrade(&runner.core);
            std::thread::spawn(move || loop {
                std::thread::sleep(disk::DISK_UPKEEP_INTERVAL);
                let Some(core) = weak.upgrade() else {
                    break;
                };
                if let Err(error) = (Runner { core }).disk_upkeep() {
                    tracing::warn!(%error, "runner disk upkeep failed");
                }
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
    fn job_log(&self, id: Uuid) -> Arc<JobLog> {
        self.core
            .job_logs
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .clone()
    }
    fn refresh_quarantine_admission(&self, force: bool) {
        if self.core.quarantined.is_empty() {
            return;
        }
        let Ok(mut probe) = self.core.quarantine_probe.try_lock() else {
            return;
        };
        if !force && probe.is_some_and(|last| last.elapsed() < Duration::from_secs(1)) {
            return;
        }
        *probe = Some(std::time::Instant::now());
        // Service enumeration can block. Snapshot record ownership only after
        // it returns, so a newly recorded launch is not mistaken for an orphan.
        let reason = match self.core.backend.live_units(&self.core.config) {
            Ok(Some(units)) => {
                let inner = self.core.inner.lock().unwrap();
                let accounted: BTreeSet<_> = inner
                    .jobs
                    .values()
                    .filter(|job| job.terminal.is_none())
                    .filter_map(|job| job.process.and_then(|process| process.unit))
                    .map(|unit| self.core.backend.unit_name(&self.core.config, unit))
                    .collect();
                let unknown: Vec<_> = units.difference(&accounted).cloned().collect();
                // A job being launched has a unit it does not name yet. While
                // as many launches are in flight as there are unmatched units,
                // those units may be theirs: keep the previous decision and
                // look again on the next refresh, a second later.
                if !unknown.is_empty() && unknown.len() <= inner.launching.len() {
                    return;
                }
                (!unknown.is_empty())
                    .then(|| format!("unaccounted live job units: {}", unknown.join(", ")))
            }
            Ok(None) => Some(format!(
                "{:?} backend cannot enumerate live execution ownership",
                self.core.config.backend
            )),
            Err(error) => Some(format!("live job unit enumeration failed: {error}")),
        };
        let mut previous = self.core.quarantine_admission.lock().unwrap();
        if *previous != reason {
            if let Some(reason) = &reason {
                tracing::warn!(%reason, "quarantined job admission fence holds");
            } else {
                tracing::info!(
                    "all live units have readable ownership; quarantined IDs remain fenced"
                );
            }
            *previous = reason;
        }
    }
    fn ensure_admission_ownership(&self) -> io::Result<()> {
        match self.core.quarantine_admission.lock().unwrap().as_ref() {
            Some(reason) => Err(invalid(reason)),
            None => Ok(()),
        }
    }
    fn ensure_reconciled(&self) -> io::Result<()> {
        if self.core.quarantined.is_empty() {
            Ok(())
        } else {
            Err(invalid(
                "quarantined job historical ownership must be reconciled before destructive cleanup",
            ))
        }
    }
    fn ensure_job_readable(&self, id: Uuid) -> io::Result<()> {
        if self.core.quarantined.contains_key(&id) {
            Err(invalid(
                "job record is quarantined; execution outcome and ownership are unknown",
            ))
        } else {
            Ok(())
        }
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
            && inner.base_finishes.is_empty()
            && inner.base_settling.is_empty()
            && inner.finalizing.is_empty()
            && inner.writer.is_none()
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
            && !self.core.config.vm_instance()
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
        crate::storage::atomic_job_json(&self.record_path(job.id), job)?;
        if let Some(log) = &self.core.tenant_events {
            log.observe(job)?;
        }
        self.core
            .repository_usage
            .lock()
            .unwrap()
            .record_job(job, self.now());
        Ok(())
    }
    pub(crate) fn tenant_events(&self) -> Option<&crate::tenant::EventLog> {
        self.core.tenant_events.as_deref()
    }
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn tenant_idle(&self) -> bool {
        let inner = self.core.inner.lock().unwrap();
        let idle = inner.jobs.values().all(|job| job.terminal.is_some()) && inner.writer.is_none();
        #[cfg(target_os = "linux")]
        let idle = idle && inner.cache_fills.is_empty();
        idle
    }
    pub(crate) fn has_job_record(&self, id: Uuid) -> bool {
        self.core.quarantined.contains_key(&id)
            || self.core.inner.lock().unwrap().jobs.contains_key(&id)
    }
    pub(crate) fn forget_unadmitted(&self, id: Uuid) -> io::Result<()> {
        // Admission and the bundle fetch save this job's record without the
        // state lock but with its job log lock held; taking that lock here
        // keeps this from deleting the directory during such a save.
        let log = self.job_log(id);
        let _write = log.lock_write();
        let mut inner = self.core.inner.lock().unwrap();
        if inner
            .jobs
            .get(&id)
            .is_some_and(|job| job.state == JobState::AwaitingBundles && job.started_ms.is_none())
        {
            let path = self.record_path(id);
            let directory = path.parent().unwrap();
            std::fs::remove_dir_all(directory)?;
            crate::filesystem::sync_directory(directory.parent().unwrap())?;
            inner.jobs.remove(&id);
            self.core.repository_usage.lock().unwrap().remove(id);
        }
        Ok(())
    }
    pub(crate) fn tenant_admitted(&self, id: Uuid) -> io::Result<()> {
        let inner = self.core.inner.lock().unwrap();
        let log = self
            .tenant_events()
            .ok_or_else(|| io::Error::other("not a tenant runner"))?;
        log.admit(id)?;
        if let Some(job) = inner.jobs.get(&id) {
            log.observe(job)?;
        }
        Ok(())
    }
    /// Save the catalog now, with the state lock held. Callers that can
    /// release the lock first take a `catalog_snapshot` under it and pass
    /// that to `write_catalog` afterwards instead.
    fn catalog(&self, inner: &Inner) -> io::Result<()> {
        self.write_catalog(self.catalog_snapshot(inner)?)
    }
    /// Serialize the catalog for a later `write_catalog`. Call with the state
    /// lock held: the snapshot's generation orders it after every catalog
    /// change made before it.
    fn catalog_snapshot(&self, inner: &Inner) -> io::Result<CatalogSnapshot> {
        Ok(CatalogSnapshot {
            bytes: serde_json::to_vec(&inner.catalog).map_err(invalid)?,
            generation: self
                .core
                .catalog_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1,
        })
    }
    /// Write a catalog snapshot (write, fsync, rename, directory fsync),
    /// with or without the state lock held, and return Ok only once a
    /// snapshot at least this new is durable. A snapshot older than one
    /// already renamed into place is not written: the newer one was taken
    /// later under the state lock, so it already holds this one's changes,
    /// and writing the older one would undo them on disk. If that newer
    /// write's directory sync failed, this one retries the sync instead.
    fn write_catalog(&self, snapshot: CatalogSnapshot) -> io::Result<()> {
        // The watermarks are only ever raised once a write step succeeded,
        // so a panic during a write leaves them low: at worst a later
        // write repeats work. A poisoned lock is taken as a healthy one.
        let mut written = self
            .core
            .catalog_written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if written.synced >= snapshot.generation {
            return Ok(());
        }
        let path = self.core.trees.state.join("catalog.json");
        if written.renamed >= snapshot.generation {
            #[cfg(test)]
            if let Some(hook) = &self.core.config.catalog_write {
                hook(CatalogWrite::SyncRetry)?;
            }
            crate::filesystem::sync_directory(path.parent().unwrap())?;
            written.synced = written.renamed;
            return Ok(());
        }
        self.long_work("catalog_save");
        #[cfg(test)]
        if let Some(hook) = &self.core.config.catalog_write {
            hook(CatalogWrite::Starting)?;
        }
        let written = &mut *written;
        atomic_bytes_and(
            &path,
            &snapshot.bytes,
            self.core.config.windows_instance(),
            || {
                // Once renamed into place these bytes are what the path holds
                // even if the directory sync below fails, so no older snapshot
                // may be written over them from here on.
                written.renamed = snapshot.generation;
                #[cfg(test)]
                if let Some(hook) = &self.core.config.catalog_write {
                    hook(CatalogWrite::Renamed)?;
                }
                Ok(())
            },
        )?;
        written.synced = snapshot.generation;
        Ok(())
    }
    fn unit_alive(&self, process: Option<platform::ProcessIdentity>) -> io::Result<Option<bool>> {
        self.core.backend.unit_alive(&self.core.config, process)
    }
    pub(crate) fn spool_dir(&self, id: Uuid) -> PathBuf {
        self.core.trees.state.join("spools").join(id.to_string())
    }
    /// Report which offered commits the mirrors hold, and pin each reported
    /// commit with a `refs/held/` ref for `HELD_OFFER_MS`. The sender uses the
    /// reply as its bundle's prerequisites; without the pin, a record expiring
    /// before the bundle arrives would prune the only ref that reaches them.
    /// A commit whose ref cannot be written is not reported.
    pub(crate) fn have(&self, r: Vec<RepositoryCandidates>) -> Vec<RepositoryCandidates> {
        // Held across the walk, so no prune can remove a ref the walk relied
        // on before the held refs exist.
        let _refs = self.core.ref_mutation.lock().unwrap();
        let mut held = self.core.trees.have(r);
        let now = self.now();
        let mut offers = self.core.held_offers.lock().unwrap();
        for repository in &mut held {
            if repository.commits.is_empty() {
                continue;
            }
            let mirror = self.core.trees.mirror(&repository.repository);
            let refs: Vec<_> = repository
                .commits
                .iter()
                .map(|commit| (format!("{}{commit}", workspace::HELD_REFS), commit.clone()))
                .collect();
            match workspace::update_refs(&mirror, &refs) {
                Ok(()) => {
                    let offered = offers.entry(mirror).or_default();
                    for commit in &repository.commits {
                        offered.insert(commit.clone(), now.saturating_add(HELD_OFFER_MS));
                    }
                }
                Err(error) => {
                    tracing::warn!(repository = %repository.repository, %error, "cannot pin held commits; reporting none");
                    repository.commits.clear();
                }
            }
        }
        held
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

    /// Jobs counted against `max_queue_depth`: pending jobs, and accepted
    /// admissions whose record is still being saved.
    fn pending_count(inner: &Inner) -> usize {
        inner.jobs.values().filter(|j| j.pending()).count() + inner.admission_saves.len()
    }
    fn scheduler_jobs(inner: &Inner) -> impl Iterator<Item = &Job> {
        inner.jobs.values().inspect(|_| {
            #[cfg(test)]
            inner
                .scheduler_visits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })
    }
    /// A running job, or a setup in flight, owns this workspace key. With
    /// in-place siblings, so does a running or reserved job that checks out
    /// an overlapping path as a sibling. With snapshot siblings, only a
    /// running job started with its siblings bound from their host paths
    /// still does (see `legacy_sibling_user`).
    fn occupied(inner: &Inner, key: &str) -> bool {
        use crate::sibling_snapshots::SiblingFences;
        let in_place = inner.sibling_fences == SiblingFences::InPlace;
        inner.reserved.contains_key(key)
            || (in_place
                && inner
                    .reserved
                    .values()
                    .any(|r| Self::workspace_uses_sibling(key, &r.siblings)))
            || Self::scheduler_jobs(inner).any(|j| {
                j.state == JobState::Running
                    && (j.key() == key
                        || ((in_place || Self::legacy_sibling_user(inner, j))
                            && j.request
                                .as_ref()
                                .is_some_and(|r| Self::workspace_uses_sibling(key, &r.siblings))))
            })
    }

    /// In snapshot mode, whether `job` still binds its siblings from their
    /// host paths: it has no `sibling_sources`, so an older runner, or one
    /// with in-place siblings, started it. Warm builds never mount siblings.
    /// This term only matters until the jobs started before the switch to
    /// snapshots have ended.
    fn legacy_sibling_user(inner: &Inner, job: &Job) -> bool {
        inner.sibling_fences == crate::sibling_snapshots::SiblingFences::Legacy
            && job.state == JobState::Running
            && !job.base
            && job.sibling_sources.is_none()
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
    /// it. Its siblings are recorded only when the setup succeeded, and their
    /// commits as checked out at their host paths only when
    /// `sibling_checkouts` says the setup checked them out there.
    #[allow(clippy::too_many_arguments)]
    fn note_workspace(
        &self,
        inner: &mut Inner,
        key: &str,
        root: &str,
        commit: &str,
        siblings: &[SiblingSnapshot],
        populated: bool,
        sibling_checkouts: bool,
        kind: Option<WorkspaceKind>,
    ) {
        if self.core.config.vm_instance() {
            return;
        }
        if populated && sibling_checkouts {
            workspace::record_workspace(
                &mut inner.catalog,
                key,
                root,
                commit,
                siblings,
                self.now(),
            );
        } else if populated {
            workspace::record_workspace_entry(
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
        if self.core.quarantined.contains_key(&request.job_id) {
            return Ok(Admission::Duplicate);
        }
        if self.core.config.windows_instance() {
            request.weight_hint = Some(4);
        }
        let (_admission, initial_refusal) = {
            let _restore = self.reserve_restore_barrier();
            let mut inner = self.core.inner.lock().unwrap();
            if inner.jobs.contains_key(&request.job_id)
                || inner.pending_replays.contains(&request.job_id)
            {
                return Ok(Admission::Duplicate);
            }
            let refusal = self.run_refusal(&inner, &request);
            (self.protect_admission(&mut inner, &request), refusal)
        };
        // Recheck ordinary admission after cleanup; another request may have
        // occupied the workspace or queue while deletion ran off the state lock.
        let disk_full = if initial_refusal.is_none() {
            self.ensure_admission_disk_space()?
        } else {
            None
        };
        self.long_work("admission_record");
        // This job's `log.write` lock, taken before the state lock. Cancel,
        // withdraw, refuse and attach take the same lock before writing the
        // job's record, so none of them writes one for this ID while this
        // admission's record is being saved below. A duplicate Run for this
        // ID also waits here, and then finds the job.
        let log = self.job_log(request.job_id);
        let _write = log.lock_write();
        let mut inner = self.core.inner.lock().unwrap();
        if inner.jobs.contains_key(&request.job_id)
            || inner.pending_replays.contains(&request.job_id)
        {
            return Ok(Admission::Duplicate);
        }
        let depth = Self::pending_count(&inner);
        inner.order += 1;
        let mut job = Job::new(
            request.job_id,
            Some(request.clone()),
            self.now(),
            inner.order,
            if self.core.config.macos_instance() {
                4
            } else {
                request.weight_hint.unwrap_or(2)
            },
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
                None,
                self.now(),
            );
            job.state = JobState::Tombstone;
        }
        // Save without the state lock: one record save is two fsyncs, and
        // every other admission, status reply and cancel waits for this lock.
        // Until the record is saved and the job inserted, the admission is
        // counted in `admission_saves`, so a concurrent admission cannot take
        // the same queue slot and a workspace drop or expiry still sees it.
        if refusal.is_none() {
            inner
                .admission_saves
                .insert(job.id, request.workspace_key.clone());
        }
        drop(inner);
        let id = job.id;
        let saving = Marker::new(self, move |inner: &mut Inner| {
            inner.admission_saves.remove(&id);
        });
        let saved = self.save(&job);
        let mut inner = self.core.inner.lock().unwrap();
        saving.clear(&mut inner);
        if let Err(error) = saved {
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
        let limit = self.core.cpus.claim_limit();
        if claim == 0 || claim > limit {
            let basis = match &self.core.config.linux_cpus {
                // A confined runner refuses what it could never admit, rather
                // than accepting a claim above max_weight that would wait forever.
                Some(list) => format!(
                    "max_weight limit; Linux jobs share {} CPUs ({list})",
                    self.core.cpus.capacity()
                ),
                None => format!(
                    "allocatable CPU limit; daemon core reservation {}",
                    self.core.config.daemon_core_reservation()
                ),
            };
            Some((
                RefusalReason::RunnerFull,
                format!(
                    "vcpu request {claim} is outside the supported range 1..={limit} ({basis})"
                ),
            ))
        } else {
            None
        }
    }

    /// A Linux job unit takes its memory limit from the shared jobs slice. A
    /// slice too small for the floor every job requests refuses the job before
    /// it starts, instead of letting the slice OOM-kill it on arrival. An
    /// unreadable slice is left to the start path's existing refusal.
    #[cfg(target_os = "linux")]
    fn slice_memory_refusal(&self) -> Option<String> {
        if !self.core.config.isolate_jobs || self.core.config.windows_instance() {
            return None;
        }
        #[cfg(test)]
        let floor = crate::backend::memory_floor(self.core.config.memory_floor_gib);
        #[cfg(not(test))]
        let floor = crate::backend::memory_floor();
        let budget = crate::backend::systemctl(
            &self.core.config,
            &[
                "show",
                "--property=MemoryHigh,MemoryMax",
                "motor-jobs.slice",
            ],
        )
        .ok()
        .and_then(|output| crate::systemd::job_memory_budget(&output))?;
        crate::backend::slice_refusal(floor, budget)
    }

    #[cfg(not(target_os = "linux"))]
    fn slice_memory_refusal(&self) -> Option<String> {
        None
    }

    fn run_refusal(&self, inner: &Inner, request: &Run) -> Option<(RefusalReason, String)> {
        #[cfg(unix)]
        if request.platform == Some(Platform::Macos) {
            if let Err(error) = crate::macos::validate_paths(request) {
                return Some((RefusalReason::RequestRejected, error.to_string()));
            }
        }
        if self.core.config.macos_instance() {
            if request.weight_hint.is_some_and(|hint| hint > 4) {
                return Some((RefusalReason::RequestRejected, "macos_vcpu_limit".into()));
            }
            if !request.return_paths.is_empty() {
                return Some((
                    RefusalReason::RequestRejected,
                    "return_paths_unsupported".into(),
                ));
            }
            if request.return_chunked {
                return Some((
                    RefusalReason::RequestRejected,
                    "return_chunked_unsupported".into(),
                ));
            }
            if request.network.is_some() {
                return Some((
                    RefusalReason::NetworkUnsupported,
                    "network_unsupported".into(),
                ));
            }
        }
        if let Err(code) = validate_return_request(
            &request.workspace_key,
            &request.return_paths,
            request.return_max_bytes,
            request.return_chunked,
        ) {
            return Some((RefusalReason::RequestRejected, code.into()));
        }
        if self.core.config.vm_instance() && !request.return_paths.is_empty() {
            return Some((
                RefusalReason::RequestRejected,
                "return_paths_unsupported".into(),
            ));
        }
        if let Err(error) = self.ensure_admission_ownership() {
            return Some((RefusalReason::WorkspaceSetupFailed, error.to_string()));
        }
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
        } else if let Some(refusal) = self.cpu_refusal(if self.core.config.vm_instance() {
            4
        } else {
            request.weight_hint.unwrap_or(2)
        }) {
            Some(refusal)
        } else if let Some(detail) = self.slice_memory_refusal() {
            Some((RefusalReason::RunnerFull, detail))
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
        } else if Self::pending_count(inner) >= self.core.config.max_queue_depth {
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
            self.core.config.sibling_snapshot_mode(),
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

    fn validate_destinations(
        &self,
        key: &str,
        cwd: Option<&str>,
        siblings: &[SiblingSnapshot],
    ) -> io::Result<()> {
        if self.core.config.vm_instance() {
            return Ok(());
        }
        if self.core.config.sibling_snapshot_mode() {
            workspace::validate_alias_destinations(
                &self.core.config.owned_workspace_roots,
                &workspace::sibling_aliases(key, siblings)?,
            )?;
        }
        workspace::validate_destinations(
            &self.core.config.owned_workspace_roots,
            key,
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
        if self.core.config.vm_instance() {
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
            .filter(|job| {
                job.base
                    && (job.terminal.is_none()
                        || job
                            .request
                            .as_ref()
                            .is_some_and(|r| Path::new(&r.cwd).exists()))
            })
            .filter_map(|job| job.request.as_ref())
        {
            known.push(PathBuf::from(&request.cwd));
        }
        crate::writer::validate_paths(&self.core.config, workspace, cwd, siblings, &known)?;
        if self.core.config.sibling_snapshot_mode() {
            crate::writer::validate_alias_paths(
                &self.core.config,
                &workspace::sibling_aliases(workspace, siblings)?,
                &known,
            )?;
        }
        self.validate_destinations(workspace, cwd, siblings)
    }

    fn deleting(inner: &Inner, key: &str, siblings: &[SiblingSnapshot]) -> bool {
        // Without any drop in progress nothing overlaps; return before the
        // canonicalization below, which touches the filesystem with the state
        // lock held, for every queue head on every dispatch pass.
        if inner.dropping.is_empty() {
            return false;
        }
        // A drop deletes a host path. A job set up in snapshot mode mounts its
        // siblings from snapshots, which no drop of a host path can affect,
        // so only its own workspace counts there.
        let siblings = if inner.sibling_fences == crate::sibling_snapshots::SiblingFences::InPlace {
            siblings
        } else {
            &[]
        };
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
        self.ensure_job_readable(id)?;
        let log = self.job_log(id);
        let _write = log.lock_write();
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
                None,
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
            .and_then(|()| trees.import_dependencies(directory, &request.git_dependencies));
        // Legacy clients may name an unmerged job branch, so only a tip the
        // client marks as the default branch's (`default_branch_tip`) may
        // queue a cache fill. What that needs from the mirror is read here,
        // before the state lock is taken: each read runs git.
        let plan = (default_branch_tip
            && fetched.is_ok()
            && self.warm_builds_enabled(&request.integration_tip))
        .then(|| self.base_plan(&request.repository_root, &request.integration_tip, false));
        let _refs = self.core.ref_mutation.lock().unwrap();
        let fetched = fetched.and_then(|()| {
            trees.pin(
                &request.repository_root,
                &request.snapshot_commit,
                &request.integration_tip,
                &request.siblings,
            )
        });
        let fetched = fetched
            .map_err(|error| self.repair_after_failed_fetch(error, directory, &request.bundles));
        // Held until the job's new record is saved and published. Cancel,
        // withdraw and refuse take it before writing this job's record, so
        // none of them can save a terminal between that save and its
        // publication, which would leave memory and disk disagreeing.
        let log = self.job_log(id);
        let _write = log.lock_write();
        let inner = self.core.inner.lock().unwrap();
        // A cancel or withdraw during the fetch already wrote the terminal.
        if !awaiting(&inner) {
            return Ok(());
        }
        let mut job = inner.jobs[&id].clone();
        // Each record below is saved without the state lock, and published
        // only once saved: until then the job stays AwaitingBundles in memory,
        // so neither dispatch nor status runs ahead of the disk. Cancel,
        // withdraw, refuse and `forget_unadmitted`, the paths that change an
        // AwaitingBundles job, all take `log.write`, held here, so the
        // in-memory job is still the one read here when the saved record
        // replaces it.
        if let Err(error) = fetched {
            let detail = self.safe_refusal_detail(
                transfer_refusal_detail(&error),
                request.env.values().cloned(),
            );
            refuse_before_start(
                &self.core.health,
                &mut job,
                transfer_refusal(&error),
                &error,
                Some(detail),
                None,
                self.now(),
            );
            job.state = JobState::Tombstone;
            drop(inner);
            self.save(&job)?;
            self.core.inner.lock().unwrap().jobs.insert(id, job);
            return Ok(());
        }
        job.state = JobState::Queued;
        let queued_ms = self.now();
        job.queued_ms = Some(queued_ms);
        drop(inner);
        self.save(&job)?;
        let mut inner = self.core.inner.lock().unwrap();
        inner.jobs.insert(id, job);
        let index = DispatchIndex::build(&inner);
        // The clocks change at the current time, not at `queued_ms`, which
        // was taken before the save. This also starts the new job's clock,
        // paused if it waits only for a workspace owner: it is not charged
        // for that owner's time during its own save. Otherwise its queue
        // wait counts from `queued_ms`, when its bundles were pinned.
        self.update_queue_wait_clocks(&inner, self.now(), &index);
        let queued = &inner.jobs[&id];
        if queued
            .queue_wait_clock_initialized
            .load(std::sync::atomic::Ordering::Acquire)
            && queued
                .queue_entry
                .load(std::sync::atomic::Ordering::Acquire)
                != PAUSED_QUEUE
        {
            queued.start_queue_wait(queued_ms);
        }
        tracing::info!(job_id = %id, elapsed_ms = started.elapsed().as_millis() as u64, "bundles fetched; job queued");
        let Some(plan) = plan else {
            return Ok(());
        };
        let catalog = if self.queue_base_planned(
            &mut inner,
            &request.repository_root,
            &request.workspace_key,
            plan,
        )? {
            Some(self.catalog_snapshot(&inner)?)
        } else {
            None
        };
        drop(inner);
        drop(_refs);
        catalog.map_or(Ok(()), |catalog| self.write_catalog(catalog))
    }
    pub(crate) fn fetch_prepare(&self, request: &Prepare, directory: &Path) -> io::Result<()> {
        self.allow_prepare()?;
        self.validate_writer_paths(&request.workspace_key, None, &request.siblings)?;
        self.core
            .trees
            .validate_dependency_bundles(directory, &request.git_dependencies)?;
        if let Err(error) = self.core.trees.fetch(
            directory,
            &request.bundles,
            &request.repository_root,
            &request.base_commit,
            &request.integration_tip,
            &request.siblings,
        ) {
            let _refs = self.core.ref_mutation.lock().unwrap();
            return Err(self.repair_after_failed_fetch(error, directory, &request.bundles));
        }
        self.core
            .trees
            .import_dependencies(directory, &request.git_dependencies)?;
        self.long_work("pin_refs_wait");
        let _refs = self.core.ref_mutation.lock().unwrap();
        self.long_work("pin_refs_git");
        self.core.trees.pin(
            &request.repository_root,
            &request.base_commit,
            &request.integration_tip,
            &request.siblings,
        )?;
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
        let catalog = self.catalog_snapshot(&inner)?;
        drop(inner);
        self.write_catalog(catalog)
    }
    fn siblings_blocked(inner: &Inner, siblings: &[SiblingSnapshot]) -> bool {
        // Snapshot siblings never share a checkout with anything.
        if inner.sibling_fences != crate::sibling_snapshots::SiblingFences::InPlace {
            return false;
        }
        Self::scheduler_jobs(inner)
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
        workspace::validate(
            &r.workspace_key,
            &r.repository_root,
            &r.siblings,
            self.core.config.sibling_snapshot_mode(),
        )?;
        self.validate_writer_paths(&r.workspace_key, None, &r.siblings)?;
        let snapshot_mode = self.core.config.sibling_snapshot_mode();
        let (recorded, prior) = {
            let _restore = self.reserve_restore_barrier();
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
            (
                inner.catalog.sibling_commits.clone(),
                snapshot_mode.then(|| {
                    crate::sibling_snapshots::freshness::Prior::of(&inner.catalog, &r.workspace_key)
                }),
            )
        };
        // Wall clock, not the runner clock: the switch touch compares it with
        // file mtimes.
        let started_at = std::time::SystemTime::now();
        // Released on every exit, including the early errors below.
        let reservation = Marker::new(self, {
            let key = r.workspace_key.clone();
            move |inner: &mut Inner| {
                inner.reserved.remove(&key);
            }
        });
        let started = std::time::Instant::now();
        self.long_work("materialize");
        self.validate_destinations(&r.workspace_key, None, &r.siblings)?;
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
        let populated = match &prior {
            // Snapshot mode never reads or writes a sibling's host path: each
            // sibling comes from its snapshot, touched for this workspace
            // before its freshness record is updated below.
            Some(prior) => setup_step(
                "materialize prepared workspace",
                self.core
                    .trees
                    .populate_main(
                        &r.workspace_key,
                        &r.repository_root,
                        &r.base_commit,
                        &tree_hash,
                        &r.base_commit,
                    )
                    .and_then(|()| {
                        crate::sibling_snapshots::freshness::prepare_siblings(
                            &self.core.config,
                            &self.core.snapshots,
                            &self.core.trees,
                            prior,
                            started_at,
                            &r.siblings,
                            &crate::sibling_snapshots::JavascriptEnvironment::current(
                                &self.core.config,
                            )?,
                        )
                    }),
            ),
            None => setup_step(
                "materialize prepared workspace",
                self.core
                    .trees
                    .populate(
                        &r.workspace_key,
                        &r.repository_root,
                        &r.base_commit,
                        &r.siblings,
                        &recorded,
                        &tree_hash,
                        &r.base_commit,
                    )
                    .map(|()| Vec::new()),
            ),
        };
        #[cfg(target_os = "linux")]
        let populated = if self.core.config.isolate_jobs {
            populated.and_then(|freshness| {
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
                )?;
                Ok(freshness)
            })
        } else {
            populated
        };
        if prior.is_some() {
            // A snapshot may have been created; this Prepare's reservation
            // keeps its own snapshots.
            self.cap_sibling_snapshots(&r.siblings);
        }
        // What a warm build of the tip needs from the mirror is read before
        // the state lock is taken: each read runs git.
        let plan = (default_branch_tip
            && populated.is_ok()
            && self.warm_builds_enabled(&r.integration_tip))
        .then(|| self.base_plan(&r.repository_root, &r.integration_tip, false));
        let mut inner = self.core.inner.lock().unwrap();
        self.note_workspace(
            &mut inner,
            &r.workspace_key,
            &r.repository_root,
            &r.base_commit,
            &r.siblings,
            populated.is_ok(),
            prior.is_none(),
            r.workspace_kind,
        );
        // Only a Prepare whose checkout and touch completed updates the
        // freshness record; a failure before this point leaves it as it was.
        // A record applied here stays correct even if the catalog write below
        // fails: the touch it describes has already happened.
        let populated = populated.map(|freshness| {
            for pending in freshness {
                if !pending.apply(&mut inner.catalog, &r.workspace_key) {
                    tracing::warn!(workspace = %r.workspace_key, "prepared workspace has no catalog entry for its sibling freshness record");
                }
            }
        });
        // The catalog is saved without the state lock. The reservation stays
        // until it is, so no job uses the workspace before the record that
        // drop and retention delete it by is durable.
        let catalog = self.catalog_snapshot(&inner);
        drop(inner);
        let written = catalog.and_then(|catalog| self.write_catalog(catalog));
        let mut inner = self.core.inner.lock().unwrap();
        reservation.clear(&mut inner);
        written?;
        if let Err(error) = populated {
            tracing::warn!(workspace = %r.workspace_key, %error, "prepare failed");
            return Err(error);
        }
        tracing::info!(workspace = %r.workspace_key, elapsed_ms = started.elapsed().as_millis() as u64, "workspace prepared");
        let Some(plan) = plan else {
            return Ok(());
        };
        if !self.queue_base_planned(&mut inner, &r.repository_root, &r.workspace_key, plan)? {
            return Ok(());
        }
        let catalog = self.catalog_snapshot(&inner)?;
        drop(inner);
        self.write_catalog(catalog)
    }
    pub(crate) fn allow_prepare(&self) -> io::Result<()> {
        self.ensure_admission_ownership()?;
        if self.core.config.windows_instance() {
            Err(io::Error::new(io::ErrorKind::Unsupported, "Prepare is unsupported by the Windows instance; caller workspaces are never materialized on the host"))
        } else if self.core.config.macos_instance() {
            Err(io::Error::new(io::ErrorKind::Unsupported, "Prepare is unsupported by the macOS VM instance; caller workspaces are never materialized on the host"))
        } else {
            Ok(())
        }
    }
    /// Record `tip` as the repository's desired warm build and queue a fill
    /// job when one is needed, with the state lock held throughout, then
    /// save the catalog. Callers that can release the lock read the mirror
    /// first (`base_plan`) and call `queue_base_planned` instead.
    #[cfg(test)]
    fn queue_base(
        &self,
        inner: &mut Inner,
        root: &str,
        tip: &str,
        workspace: &str,
        _siblings: &[SiblingSnapshot],
        _git_dependencies: &[GitDependency],
    ) -> io::Result<()> {
        if !self.warm_builds_enabled(tip) || inner.drain_deadline.is_some() {
            return Ok(());
        }
        let plan = self.base_plan(root, tip, false);
        if self.queue_base_planned(inner, root, workspace, plan)? {
            self.catalog(inner)?;
        }
        Ok(())
    }
    /// Whether warm builds may be queued for `tip` at all, from configuration
    /// alone; `queue_base_planned` also checks for a drain under the lock.
    fn warm_builds_enabled(&self, tip: &str) -> bool {
        #[cfg(test)]
        if self.core.config.disable_warm_builds {
            return false;
        }
        !tip.is_empty()
            && !self.core.config.vm_instance()
            && self.core.config.writer_enabled()
            && self.core.quarantined.is_empty()
    }
    /// Read what `queue_base_planned` needs from the repository's mirror.
    /// Each read runs git, which is slow on a loaded host, so callers do this
    /// before taking the state lock where they can. A commit's contents never
    /// change, so the reads stay valid until the plan is used. With
    /// `with_fill`, it also reads the inputs of a new fill job, otherwise
    /// `queue_base_planned` reads them if it queues one.
    /// Read what `queue_base_planned` needs from the repository's mirror.
    /// Each read runs git, which is slow on a loaded host, so callers do this
    /// before taking the state lock where they can. A commit's contents never
    /// change, so the reads stay valid until the plan is used. With
    /// `with_fill`, it also reads the inputs of a new fill job, otherwise
    /// `queue_base_planned` reads them if it queues one.
    fn base_plan(&self, root: &str, tip: &str, with_fill: bool) -> BasePlan {
        let mirror = self.core.trees.mirror(root);
        // The override, the repository's declaration and Cargo's own warm
        // command are all read here so `fill_identity` can tell which of them
        // produced the command the fill will run.
        let overridden = !self.core.config.base_commands.is_empty();
        let (command, declared) = if overridden {
            (self.core.config.base_commands.join(" && "), false)
        } else {
            let (steps, declared) = workspace::warm_steps(&mirror, tip);
            (steps.join(" && "), declared)
        };
        let explicit_command = overridden || declared;
        let (descriptor, dependency_free) = self.fill_identity(&mirror, tip, &command, overridden);
        BasePlan {
            tip: tip.to_owned(),
            command,
            explicit_command,
            descriptor,
            dependency_free,
            fill: with_fill.then(|| self.fill_inputs(root, tip)),
        }
    }
    /// The tip's fill descriptor and whether the dependency-free shortcut may
    /// stand in for a fill.
    ///
    /// Both readers of a descriptor (queueing a fill, and a reader miss asking
    /// for a refill) go through here, so they always agree on it. A declaration
    /// that does not parse fails the tip the way an unavailable tip does.
    /// `Config::base_commands` overrides every command, including a declared
    /// one, so a declaration only takes part while it is in effect.
    fn fill_identity(
        &self,
        mirror: &Path,
        tip: &str,
        command: &str,
        overridden: bool,
    ) -> (io::Result<String>, bool) {
        let declares = !overridden;
        let declaration = if declares {
            // A declaration that does not parse fails the tip: a fill of
            // something other than what the repository declared is worse than
            // no fill, since its descriptor would name inputs nobody checked.
            match crate::writer::declaration::read(mirror, tip) {
                Ok(declaration) => declaration,
                Err(error) => return (Err(error), false),
            }
        } else {
            None
        };
        let compilers = if declares {
            self.core.compilers.clone()
        } else {
            Compilers::default()
        };
        let descriptor = crate::writer::fill::descriptor(
            mirror,
            tip,
            &self.core.rustc_version,
            command,
            declaration.as_ref(),
            &compilers,
        );
        // A declaration's steps are always run, so the shortcut that records a
        // fill without running one only applies to a graph that has neither a
        // declaration nor an override.
        let dependency_free = descriptor.is_ok()
            && crate::writer::fill::dependency_free(mirror, tip, declaration.as_ref())
                .unwrap_or(false);
        (descriptor, dependency_free)
    }
    /// A new fill job's locked git dependencies and, when those are usable,
    /// the tip's tree.
    fn fill_inputs(&self, root: &str, tip: &str) -> FillInputs {
        let git_dependencies = crate::writer::fill::locked_git(&self.core.trees, root, tip)
            .and_then(|dependencies| {
                let (_, missing) = self.core.trees.dependency_routes(&dependencies)?;
                if !missing.is_empty() {
                    return Err(invalid(missing.join("; ")));
                }
                Ok(dependencies)
            });
        let tree = git_dependencies.is_ok().then(|| {
            workspace::git(
                &self.core.trees.mirror(root),
                &["rev-parse", &format!("{tip}^{{tree}}")],
            )
        });
        FillInputs {
            git_dependencies,
            tree,
        }
    }
    /// The decision part of `queue_base`, from a `base_plan`. Changes the
    /// catalog only in memory and returns whether it may have changed it, so
    /// the caller saves it, possibly after releasing the state lock. The
    /// record of a newly queued fill job is saved here, before the job is
    /// inserted.
    fn queue_base_planned(
        &self,
        inner: &mut Inner,
        root: &str,
        workspace: &str,
        plan: BasePlan,
    ) -> io::Result<bool> {
        if inner.drain_deadline.is_some() {
            return Ok(false);
        }
        let settling = Self::base_settlement_pending(inner, root);
        let BasePlan {
            tip,
            command,
            explicit_command,
            descriptor,
            dependency_free,
            fill,
        } = plan;
        let tip = tip.as_str();
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
                return Ok(true);
            }
        };
        repository.requested_descriptor = Some(descriptor.clone());
        if settling {
            // Keep the latest desired tip, but do not queue a successor until
            // the previous attempt's result and cleanup have been settled.
            return Ok(true);
        }
        if !explicit_command && dependency_free {
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
            return Ok(true);
        }
        // One policy decides reuse, the publication gate and the retry cap,
        // and it changes nothing while the remote cache is not publishing.
        let publishing = self.publishing_policy();
        let now = self.now();
        if let Some(finished) = repository
            .filled_descriptors
            .get(&descriptor)
            .copied()
            .filter(|_| {
                !explicit_command || !repository.recorded_without_writer.contains(&descriptor)
            })
            .filter(|_| publication::reusable(&publishing, repository, &descriptor, now))
        {
            repository.filled_descriptor = Some(descriptor);
            repository.commit = Some(tip.into());
            repository.finished_ms = Some(finished);
            repository.result = Some(RebuildResult::Ok);
            return Ok(true);
        }
        if repository.failed_descriptor.as_ref() == Some(&descriptor)
            && !(publishing.publishing()
                && repository.publications.contains_key(&descriptor)
                && repository
                    .publish_failures
                    .get(&descriptor)
                    .is_some_and(|failure| failure.attempts < publication::PUBLISH_RETRY_ATTEMPTS))
        {
            repository.result = Some(RebuildResult::Failed);
            return Ok(true);
        }
        match publication::gate(&publishing, repository, &descriptor, now) {
            publication::Gate::Queue => {}
            publication::Gate::Wait => {
                // The publication failed recently; the refresh timer queues it
                // again once the retry interval has passed.
                repository.result = Some(RebuildResult::Failed);
                return Ok(true);
            }
            publication::Gate::Held => {
                // Repeated failed publications hold this descriptor like a
                // fill that failed as a build until it changes or the runner
                // restarts.
                if repository.failed_descriptor.as_ref() != Some(&descriptor) {
                    repository.failed_descriptor = Some(descriptor.clone());
                    repository.failure_detail = Some(format!(
                        "publishing {descriptor} failed {} times in a row; the descriptor is held until it changes or the runner restarts",
                        publication::PUBLISH_RETRY_ATTEMPTS
                    ));
                }
                repository.result = Some(RebuildResult::Failed);
                return Ok(true);
            }
        }
        repository.result = Some(RebuildResult::Building);
        if inner.jobs.values().any(|j| {
            j.base
                && j.terminal.is_none()
                && j.request
                    .as_ref()
                    .is_some_and(|r| r.repository_root == root)
        }) {
            return Ok(true);
        }
        if Self::pending_count(inner) >= self.core.config.max_queue_depth {
            return Ok(true);
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
        let FillInputs {
            git_dependencies,
            tree,
        } = fill.unwrap_or_else(|| self.fill_inputs(root, tip));
        let git_dependencies = match git_dependencies {
            Ok(dependencies) => dependencies,
            Err(error) => {
                let g = inner.catalog.repositories.get_mut(root).unwrap();
                g.failed_descriptor = Some(descriptor);
                g.result = Some(RebuildResult::Failed);
                tracing::warn!(%root, %tip, %error, "dependency writer skips missing git dependency");
                return Ok(true);
            }
        };
        let tree = tree.expect("the tree is read whenever the git dependencies are usable")?;
        inner.order += 1;
        let request = Run {
            return_paths: Vec::new(),
            return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
            return_chunked: false,
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
        job.fill_started_ms = Some(self.now());
        job.state = JobState::Queued;
        self.save(&job)?;
        tracing::info!(job_id = %id, repository = %root, commit = %tip, %command, "warm build queued");
        inner.jobs.insert(id, job);
        Ok(true)
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
        self.ensure_job_readable(id)?;
        let log = self.job_log(id);
        let job_directory = self.record_path(id).parent().unwrap().to_owned();
        let (
            started,
            terminal,
            return_result,
            chunks,
            (returned_chunks, manifest),
            earliest,
            output_from,
            shift,
            _lease,
        ) = loop {
            let mut inner = self.core.inner.lock().unwrap();
            if let Some(job) = inner.jobs.get(&id) {
                let shift = if let (false, Some(started)) = (supports_started, &job.started) {
                    if started.seq != 0 || job.chunks.iter().any(|chunk| chunk.seq == 0) {
                        return Err(invalid("Started must be sequence zero for legacy replay"));
                    }
                    1
                } else {
                    0
                };
                let from = from.checked_add(shift).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "legacy replay cursor overflow")
                })?;
                let started = job
                    .started
                    .as_ref()
                    .filter(|started| supports_started && started.seq >= from)
                    .cloned();
                let output_from = started.as_ref().map_or(from, |started| started.seq + 1);
                let earliest = job.chunks.front().map_or_else(
                    || job.return_result.as_ref().map_or(job.next_seq, |r| r.seq),
                    |chunk| chunk.seq,
                );
                let mut chunks = Vec::new();
                let mut bytes = 0u64;
                let mut more = false;
                let beginning = job.chunks.partition_point(|chunk| chunk.seq < from);
                for chunk in job.chunks.iter().skip(beginning) {
                    if chunks.len() == REPLAY_PAGE_CHUNKS
                        || bytes.saturating_add(chunk.length) > REPLAY_PAGE_BYTES
                    {
                        if chunks.is_empty() {
                            return Err(invalid("retained output chunk exceeds replay page limit"));
                        }
                        more = true;
                        break;
                    }
                    bytes += chunk.length;
                    chunks.push(chunk.clone());
                }
                // Chunked returned files follow all output, on the same page
                // budget. Once their retention window has passed, a replay
                // sends only the manifest, with returned entries `expired`,
                // which leaves a seq gap where the chunks were.
                let mut returned_chunks = Vec::new();
                let mut manifest = None;
                if let (false, Some(returned)) = (more, &job.returned) {
                    let expired = returned.expired
                        || job.finished_ms.is_some_and(|finished| {
                            self.now().saturating_sub(finished) >= RETURN_RETENTION_MS
                        });
                    if !expired {
                        let beginning = returned.chunks.partition_point(|chunk| chunk.seq < from);
                        for (position, chunk) in returned.chunks.iter().enumerate().skip(beginning)
                        {
                            if chunks.len() + returned_chunks.len() == REPLAY_PAGE_CHUNKS
                                || bytes.saturating_add(chunk.length) > REPLAY_PAGE_BYTES
                            {
                                if chunks.is_empty() && returned_chunks.is_empty() {
                                    return Err(invalid(
                                        "returned file chunk exceeds replay page limit",
                                    ));
                                }
                                more = true;
                                break;
                            }
                            bytes += chunk.length;
                            returned_chunks.push((
                                returned.chunk_path(&job_directory, position),
                                chunk.clone(),
                            ));
                        }
                    }
                    if !more && returned.manifest.seq >= from {
                        manifest = Some(if expired {
                            returned.expired_manifest()
                        } else {
                            returned.manifest.clone()
                        });
                    }
                }
                let terminal = if more { None } else { job.terminal.clone() };
                let return_result = job
                    .return_result
                    .as_ref()
                    .filter(|r| !more && r.seq >= from)
                    .cloned();
                // Take the lease under the index lock: neither retention nor
                // expiry may unlink this snapshot's files before the read ends.
                let lease = log.lease(
                    chunks
                        .iter()
                        .map(|chunk| {
                            self.record_path(id)
                                .parent()
                                .unwrap()
                                .join(format!("chunk-{}", chunk.seq))
                        })
                        .chain(returned_chunks.iter().map(|(path, _)| path.clone()))
                        .collect(),
                );
                break (
                    started,
                    terminal,
                    return_result,
                    chunks,
                    (returned_chunks, manifest),
                    earliest,
                    output_from,
                    shift,
                    lease,
                );
            }
            if self.now().saturating_sub(mint_ms(id)) >= RETENTION_MS {
                let mut job = Job::new(id, None, self.now(), 0, 0);
                job.terminal(Outcome::HistoryExpired, self.now());
                let mut terminal = job.terminal.unwrap();
                terminal.ran = None;
                return Ok(vec![Frame::Terminal(terminal)]);
            }
            drop(inner);
            let _write = log.lock_write();
            inner = self.core.inner.lock().unwrap();
            if inner.jobs.contains_key(&id) {
                continue;
            }
            // Block a concurrent Run for this ID before releasing the state
            // lock. Only the durable tombstone authorizes replying that this
            // ID never started; a failed write must not expose that reply.
            inner.pending_replays.insert(id);
            drop(inner);
            let mut job = Job::new(id, None, self.now(), 0, 0);
            refuse_before_start(
                &self.core.health,
                &mut job,
                RefusalReason::TransferInterrupted,
                "attach found no recorded admission or upload",
                Some("attach found no recorded admission or upload".into()),
                None,
                self.now(),
            );
            job.state = JobState::Tombstone;
            let saved = self.save(&job);
            inner = self.core.inner.lock().unwrap();
            inner.pending_replays.remove(&id);
            saved?;
            inner.jobs.insert(id, job);
        };
        let mut frames = Vec::new();
        if let Some(started) = started {
            frames.push(Frame::Started {
                job_id: id,
                seq: started.seq,
                queue_wait_ms: started.queue_wait_ms,
                started_at_ms: started.started_at_ms,
                path_mapping: started.path_mapping,
            });
        }
        for chunk in chunks {
            let path = self
                .record_path(id)
                .parent()
                .unwrap()
                .join(format!("chunk-{}", chunk.seq));
            #[cfg(test)]
            if let Some(read) = &self.core.config.before_replay_read {
                read(&path)?;
            }
            let mut bytes = Vec::with_capacity(chunk.length as usize);
            File::open(&path)?
                .take(chunk.length + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != chunk.length {
                return Err(invalid(
                    "retained output chunk length differs from its committed index",
                ));
            }
            let output = Output::new(chunk.seq - shift, chunk.stream, BytePayload(bytes));
            let output = if chunk.seq == earliest && output_from < earliest {
                output.with_truncated_before_seq(earliest - shift)
            } else {
                output
            };
            frames.push(Frame::Output(output));
        }
        if let Some(mut result) = return_result {
            result.seq -= shift;
            frames.push(Frame::ReturnResult(result));
        }
        for (path, chunk) in returned_chunks {
            let mut bytes = Vec::with_capacity(chunk.length as usize);
            File::open(&path)?
                .take(chunk.length + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != chunk.length {
                return Err(invalid(
                    "retained returned file chunk length differs from its committed index",
                ));
            }
            frames.push(Frame::ReturnChunk(ReturnChunk::new(
                id,
                chunk.seq - shift,
                chunk.path_index,
                chunk.offset,
                BytePayload(bytes),
            )));
        }
        if let Some(mut manifest) = manifest {
            manifest.seq -= shift;
            frames.push(Frame::ReturnManifest(manifest));
        }
        if let Some(terminal) = terminal {
            frames.push(Frame::Terminal(terminal));
        }
        Ok(frames)
    }
    pub fn status(&self) -> StatusReply {
        let inner = self.core.inner.lock().unwrap();
        self.status_with_jobs(
            &inner,
            inner.jobs.values().filter(|j| j.pending()).count() as u32,
            inner
                .jobs
                .values()
                .filter(|j| j.state == JobState::Running)
                .map(|j| RunningJob::new(j.id, j.key(), j.weight))
                .collect(),
        )
    }
    fn status_with_jobs(
        &self,
        inner: &Inner,
        queue_depth: u32,
        running: Vec<RunningJob>,
    ) -> StatusReply {
        StatusReply::new(
            queue_depth,
            running,
            !self.core.config.backend_degraded(),
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
            .and_then(|j| j.queued_ms.map(|_| j.queue_wait_ms(now)))
    }
    pub(crate) fn queue_progress(&self, id: Uuid) -> QueueProgress {
        let inner = self.core.inner.lock().unwrap();
        QueueProgress {
            entry: inner
                .jobs
                .get(&id)
                .map(|j| j.queue_entry.clone())
                .unwrap_or_else(empty_queue_entry),
            elapsed: inner
                .jobs
                .get(&id)
                .map(|j| j.queue_elapsed.clone())
                .unwrap_or_else(empty_queue_elapsed),
            clock: self.core.config.clock.clone(),
        }
    }
    pub fn job_status(&self, id: Uuid) -> io::Result<JobStatusReply> {
        let inner = self.core.inner.lock().unwrap();
        let index = DispatchIndex::build(&inner);
        let now = self.now();
        self.update_queue_wait_clocks(&inner, now, &index);
        let job = inner.jobs.get(&id);
        let quarantine = self.core.quarantined.get(&id);
        let process = job
            .and_then(|j| j.process)
            .or_else(|| quarantine.and_then(|j| j.process));
        Ok(JobStatusReply {
            job_id: id,
            queue_wait_ms: Self::queue_wait_ms_at(&inner, id, now),
            state: job
                .map(|j| format!("{:?}", j.state).to_lowercase())
                .unwrap_or_else(|| {
                    if quarantine.is_some() {
                        "quarantined"
                    } else {
                        "not_found"
                    }
                    .into()
                }),
            waiting_on: self.queue_wait_phase(&inner, &index, id).map(str::to_owned),
            terminal: job.and_then(|j| j.terminal.clone()),
            unit: process
                .and_then(|p| p.unit)
                .map(|u| self.core.backend.unit_name(&self.core.config, u)),
            unit_alive: self.unit_alive(process)?,
        })
    }
    pub fn cancel(&self, id: Uuid) -> io::Result<()> {
        // Each later step until the unit stop logs the time since the cancel
        // started, so a slow cancel shows which wait it spent time in.
        let started = std::time::Instant::now();
        let elapsed_ms = || started.elapsed().as_millis() as u64;
        tracing::info!(job_id = %id, "cancel received");
        self.ensure_job_readable(id)?;
        #[cfg(unix)]
        if self.core.config.macos_instance() {
            self.core.config.macos_runtime.interrupt(id);
        }
        let log = self.job_log(id);
        let write = log.write_ahead_of_output();
        tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel acquired job log lock");
        let mut inner = self.core.inner.lock().unwrap();
        tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel acquired state lock");
        if self.core.config.vm_instance() && inner.starting.contains(&id) {
            self.core
                .backend
                .cancel(&self.core.config, &inner.jobs[&id], true)?;
        }
        if inner.finalizing.contains(&id) || inner.base_finishes.contains_key(&id) {
            tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel found job already finishing");
            return Ok(());
        }
        let job = inner
            .jobs
            .entry(id)
            .or_insert_with(|| Job::new(id, None, self.now(), 0, 0));
        if job.terminal.is_some() {
            tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel found job already finished");
            return Ok(());
        }
        if job.state == JobState::Running {
            if job.kill.is_none() {
                let mut updated = job.clone();
                updated.kill = Some((Killed::Cancel, self.now()));
                drop(inner);
                self.save(&updated)?;
                tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel recorded");
                let signalling = log.lock_signalling();
                inner = self.core.inner.lock().unwrap();
                inner.jobs.insert(id, updated);
                if inner.finalizing.contains(&id) || inner.base_finishes.contains_key(&id) {
                    tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel found job already finishing");
                    return Ok(());
                }
                let job = inner.jobs[&id].clone();
                // Query and stop the unit without the state lock: each is a
                // systemctl process, slow on a loaded host, and every other
                // job's admission and status reply waits for that lock.
                // `signalling` keeps finalization from being claimed
                // meanwhile, so an exited job being finalized is never
                // signalled.
                drop(inner);
                let unit_exited =
                    job.process.is_some() && self.unit_alive(job.process)? == Some(false);
                if job.process.is_none() || unit_exited {
                    tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel found unit already exited");
                    drop(signalling);
                    drop(write);
                    return self.finish_unobserved_unit(
                        id,
                        "cancel recorded after unit exited",
                        unit_exited,
                    );
                } else {
                    tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel checked unit is alive");
                    self.core.backend.cancel(&self.core.config, &job, false)?;
                    tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel issued unit stop");
                }
            } else {
                tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel already recorded; stop retries belong to the scheduler");
            }
        } else {
            job.terminal_ended(Outcome::Cancelled, self.now());
            job.state = JobState::Tombstone;
            self.save(job)?;
            tracing::info!(job_id = %id, elapsed_ms = elapsed_ms(), "cancel recorded before start");
        }
        Ok(())
    }
    /// Cancel a job whose admission was still saving its record when a
    /// workspace drop chose it, once that save is over. An admission whose
    /// save failed left no job, so nothing is cancelled or written for it.
    /// Returns whether there was a job to cancel.
    fn cancel_admitted(&self, id: Uuid) -> io::Result<bool> {
        let admitted = {
            let log = self.job_log(id);
            // The admission holds this lock until its job is inserted or
            // its save has failed.
            let _write = log.lock_write();
            self.core.inner.lock().unwrap().jobs.contains_key(&id)
        };
        if admitted {
            self.cancel(id)?;
        }
        Ok(admitted)
    }
    #[cfg(test)]
    pub(crate) fn withdraw(&self, id: Uuid) -> io::Result<WithdrawOutcome> {
        self.withdraw_for(id, None, false)
    }
    pub(crate) fn withdraw_for(
        &self,
        id: Uuid,
        reason: Option<RefusalReason>,
        spill: bool,
    ) -> io::Result<WithdrawOutcome> {
        self.ensure_job_readable(id)?;
        let log = self.job_log(id);
        let _write = log.lock_write();
        let mut inner = self.core.inner.lock().unwrap();
        let now = self.now();
        let index = DispatchIndex::build(&inner);
        // Refresh the clock first: the refusal below records the job's
        // queue_wait_ms, and its detail names the holders that were holding it
        // at the withdrawal.
        self.update_queue_wait_clocks(&inner, now, &index);
        let stalled = reason == Some(RefusalReason::Unknown("transfer_stalled".into()));
        let eligible = inner.jobs.get(&id).is_some_and(|job| {
            (!inner.starting.contains(&id) || self.core.config.vm_instance())
                && job.pending()
                && if stalled {
                    job.state == JobState::AwaitingBundles
                } else {
                    // Windows VM boot stays withdrawable after setup reserves the job.
                    // Keep that setup interval separate from paused workspace-owner time.
                    let setup_wait =
                        if self.core.config.vm_instance() && inner.starting.contains(&job.id) {
                            now.saturating_sub(job.setup_ms.unwrap_or(now))
                        } else {
                            0
                        };
                    job.state == JobState::Queued
                        && job.queued_ms.is_some()
                        && job.queue_wait_ms(now).saturating_add(setup_wait)
                            >= job
                                .request
                                .as_ref()
                                .unwrap()
                                .queue_wait_limit_s
                                .saturating_mul(1000)
                }
        });
        let detail = if stalled {
            "transfer_stalled".into()
        } else {
            self.queue_wait_detail_with_index(&inner, &index, id)
        };
        if eligible && self.core.config.vm_instance() && inner.starting.contains(&id) {
            self.core
                .backend
                .cancel(&self.core.config, &inner.jobs[&id], true)?;
        }
        let Some(mut job) = inner.jobs.get(&id).filter(|_| eligible).cloned() else {
            return Ok(WithdrawOutcome::Started);
        };
        // The terminal below stops the queue-wait clock this copy shares
        // with the queued job; kept to restore exactly if the save fails.
        let clock = QueueClock::of(&job);
        let safe_detail = self.safe_refusal_detail(
            &detail,
            job.request
                .as_ref()
                .into_iter()
                .flat_map(|request| request.env.values().cloned()),
        );
        // Why a caller was sent back while the runner could not start it: a
        // job waiting for enough free permits, or for another job to release
        // its workspace or its checkout, may be able to start once that passes.
        // `queue_wait_phase` names the phase from the same dispatch index, so a
        // job behind one that can never fit keeps no hint.
        let retry_after_ms = self.queue_clear_retry_after_ms(&inner, &index, id);
        if spill && !stalled {
            // Keep the legacy outcome so old clients know the job never started,
            // but do not count a move to hosted capacity as a runner refusal.
            let detail = format!(
                "{}{safe_detail}",
                motor_protocol::SPILL_WITHDRAWAL_DETAIL_PREFIX
            );
            tracing::info!(job_id = %id, detail, "job spill_withdrawal");
            self.core
                .health
                .lock()
                .unwrap()
                .record_spill_withdrawal(self.now());
            job.terminal_ended(
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::QueueWaitExceeded,
                },
                self.now(),
            );
            job.terminal = job.terminal.take().map(|t| t.with_refusal_detail(detail));
        } else {
            refuse_before_start(
                &self.core.health,
                &mut job,
                if stalled {
                    RefusalReason::TransferInterrupted
                } else {
                    RefusalReason::QueueWaitExceeded
                },
                &detail,
                Some(safe_detail),
                retry_after_ms,
                self.now(),
            );
        }
        job.state = JobState::Tombstone;
        // Saved without the state lock and published once saved. Meanwhile
        // the job is in `terminal_saves`: it stays queued in its place, so it
        // still holds back the jobs behind it, but dispatch stops at it
        // instead of starting it, and the drain refusal does not write over
        // it. Cancel and refuse wait for `log.write`, held here.
        inner.terminal_saves.insert(id);
        drop(inner);
        // Clears the marker, and gives back the queue clock unless the
        // tombstone was published, also if the save unwinds.
        let saving = Marker::new(self, move |inner: &mut Inner| {
            inner.terminal_saves.remove(&id);
            if let Some(queued) = inner.jobs.get(&id).filter(|job| job.terminal.is_none()) {
                clock.restore(queued);
            }
        });
        let saved = self.save(&job);
        let mut inner = self.core.inner.lock().unwrap();
        if let Err(error) = saved {
            saving.clear(&mut inner);
            return Err(error);
        }
        inner.jobs.insert(id, job);
        saving.clear(&mut inner);
        Ok(WithdrawOutcome::Withdrawn)
    }

    #[cfg(test)]
    fn held_weight(inner: &Inner) -> u32 {
        Self::scheduler_jobs(inner)
            .filter(|j| j.state == JobState::Running || inner.starting.contains(&j.id))
            .fold(0u32, |held, j| held.saturating_add(j.weight))
    }

    /// The first queue head ahead of `id` that stops later heads from being
    /// started for capacity, with whether it is a gate job (see
    /// `holds_back_later_heads`).
    fn blocking_head(
        &self,
        inner: &Inner,
        heads: &[Uuid],
        id: Uuid,
        held: u32,
        now: u64,
    ) -> Option<(Uuid, bool)> {
        heads
            .iter()
            .take_while(|head| **head != id)
            .map(|head| &inner.jobs[head])
            .find(|head| self.holds_back_later_heads(inner, head, held, now))
            .map(|head| (head.id, head.gate_job()))
    }

    /// Whether queue head `head` stops later heads from being started for
    /// capacity, mirroring dispatch's stop rule: a head whose withdrawal is
    /// being saved; or a gate job, a cache fill that may start (the writer
    /// slot is free, the cache is not being evicted, and the fill may pass
    /// queued foreground jobs; see `fill_passes_queue`), or a non-base job
    /// queued past BACKFILL_GRACE_MS, whose weight added to the `held`
    /// (already occupied) permits exceeds the runner's permit limit, or that
    /// is waiting out its start retry delay (such a head keeps its claim on
    /// capacity, as if it were waiting for permits). A head whose earlier
    /// attempt is still being cleaned up is skipped like a checkout conflict
    /// and stops nothing.
    fn holds_back_later_heads(&self, inner: &Inner, head: &Job, held: u32, now: u64) -> bool {
        if inner.terminal_saves.contains(&head.id) {
            return true;
        }
        if self.core.config.cleanup_pending(head.id) {
            return false;
        }
        let fill = head.base
            && inner.writer.is_none()
            && !inner
                .evicting_caches
                .contains(&self.core.config.sccache_dir())
            && (self.fill_passes_queue(head.weight)
                || !Self::scheduler_jobs(inner).any(|job| !job.base && job.pending()));
        let aged = !head.base
            && now.saturating_sub(head.queued_ms.unwrap_or(head.accepted_ms)) >= BACKFILL_GRACE_MS;
        (head.gate_job() || fill || aged)
            && (held.saturating_add(head.weight) > self.core.config.max_weight
                || Self::start_deferred(inner, head.id, now))
    }

    /// The `retry_after_ms` hint for a job refused because its queue wait ran
    /// out, or `None` when waiting cannot help it. Only the wait phases that
    /// clear on their own get one: enough free permits, or a workspace or
    /// checkout that the running job holding it will release. A job waiting
    /// behind a gate job, a cache fill, an older job, or an upload or setup step
    /// has nothing that a wait is known to end. The value is the median queue
    /// wait of jobs started in the last hour (`health.queue_wait_1h.p50_ms`),
    /// clamped to `CAPACITY_RETRY_MIN_MS..=CAPACITY_RETRY_MAX_MS` for the
    /// reasons on those constants, and omitted when no job started in that
    /// window: there is then no wait to report, and a zero would read as one.
    fn queue_clear_retry_after_ms(
        &self,
        inner: &Inner,
        index: &DispatchIndex,
        id: Uuid,
    ) -> Option<u64> {
        let phase = self.queue_wait_phase(inner, index, id)?;
        if !matches!(
            phase,
            "permits" | "workspace owner" | "queued workspace job" | SIBLING_USER_WAIT
        ) {
            return None;
        }
        self.core
            .health
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recent_queue_wait_p50(self.now())
            .map(|p50_ms| p50_ms.clamp(CAPACITY_RETRY_MIN_MS, CAPACITY_RETRY_MAX_MS))
    }

    /// Whether a cache fill of `weight` permits may start while foreground
    /// jobs are queued: only when it leaves at least as many permits as it
    /// takes. A fill's weight is fixed (`WARM_BUILD_VCPUS`, or the whole
    /// host when that is smaller), so on all but the smallest hosts it does.
    fn fill_passes_queue(&self, weight: u32) -> bool {
        weight.saturating_mul(2) <= self.core.config.max_weight
    }

    /// Whether `id`'s last start failed and its retry delay has not passed.
    fn start_deferred(inner: &Inner, id: Uuid, now: u64) -> bool {
        inner
            .start_retries
            .get(&id)
            .is_some_and(|retry| now < retry.not_before_ms)
    }

    fn queue_wait_phase(
        &self,
        inner: &Inner,
        index: &DispatchIndex,
        id: Uuid,
    ) -> Option<&'static str> {
        let job = inner.jobs.get(&id)?;
        if job.state == JobState::AwaitingBundles {
            return Some("bundle upload/fetch");
        }
        if job.state != JobState::Queued {
            return None;
        }
        // An earlier attempt to start the job is still being cleaned up.
        if inner.starting.contains(&id) || self.core.config.cleanup_pending(id) {
            return Some("workspace setup");
        }
        Some(if inner.dropping.contains(job.key()) {
            "workspace drop"
        } else if index.workspaces.contains(job.key()) {
            "workspace owner"
        } else if index.occupied(job.key()) {
            SIBLING_USER_WAIT
        } else if job
            .request
            .as_ref()
            .is_some_and(|r| index.siblings_blocked(&r.siblings, &inner.catalog.sibling_commits))
        {
            "sibling checkout owner"
        } else if index.held_by_queued_workspace(inner, job, self.core.config.max_weight) {
            "queued workspace job"
        } else if job.weight > self.core.config.max_weight {
            "demand exceeds capacity"
        } else if index.weight.saturating_add(job.weight) > self.core.config.max_weight
            || Self::start_deferred(inner, id, self.now())
        {
            // A job waiting out its start retry delay keeps its claim on
            // capacity, as if it were waiting for permits.
            "permits"
        } else {
            "queue order"
        })
    }

    fn update_queue_wait_clocks(&self, inner: &Inner, now: u64, index: &DispatchIndex) {
        let heads = index.heads(inner, now);
        let mut blockers = BTreeMap::<Uuid, Option<(Uuid, bool)>>::new();
        let mut blocking_head = None;
        for id in &heads {
            blockers.insert(*id, blocking_head);
            if blocking_head.is_some() {
                continue;
            }
            let job = &inner.jobs[id];
            if self.holds_back_later_heads(inner, job, index.weight, now) {
                blocking_head = Some((job.id, job.gate_job()));
            }
        }

        let queued: Vec<_> = index
            .queued
            .values()
            .flat_map(|queue| queue.iter().copied())
            .collect();
        for id in queued {
            let job = &inner.jobs[&id];
            let is_workspace_head =
                index.queued.get(job.key()).and_then(|queue| queue.first()) == Some(&id);
            // The running or setup jobs that hold this job's checkout: the
            // owner of the same workspace, and jobs that check out an
            // overlapping path as a sibling (the same host directory).
            let mut holders = BTreeMap::new();
            if is_workspace_head {
                holders.extend(index.workspace_owners.get(job.key()).copied());
                holders.extend(index.checkout_sibling_users(job.key()));
                holders.remove(&id);
            }
            // A job held back only so that an earlier queued workspace job can
            // use their shared checkout first is waiting on a workspace
            // conflict, not on capacity, so its clock pauses as it would
            // behind a running holder of its own checkout. Unlike that holder,
            // the queued job holds no permits, so none are counted as about
            // to be freed when checking that this job would otherwise fit.
            let held = holders.is_empty()
                && is_workspace_head
                && !index.occupied(job.key())
                && index.held_by_queued_workspace(inner, job, self.core.config.max_weight);
            let pause = job.queued_ms.is_some()
                && (!holders.is_empty() || held)
                && job.request.as_ref().is_some_and(|request| {
                    let holder_weight = holders
                        .values()
                        .fold(0u32, |sum, weight| sum.saturating_add(*weight));
                    let blocked_by_head = blockers.get(&id).is_some_and(Option::is_some);
                    !blocked_by_head
                        && !Self::deleting(inner, job.key(), &request.siblings)
                        && job.weight <= self.core.config.max_weight
                        && index
                            .weight
                            .saturating_sub(holder_weight)
                            .saturating_add(job.weight)
                            <= self.core.config.max_weight
                        // The job's own sibling checkouts must be free too,
                        // except where the conflict is with one of the
                        // holders above: two repositories that name each
                        // other as siblings block each other in both roles,
                        // and that is still one checkout conflict. Rebuild
                        // the index without the holders only in that case.
                        && (!index
                            .siblings_blocked(&request.siblings, &inner.catalog.sibling_commits)
                            || (!holders.is_empty()
                                && !DispatchIndex::build_without(
                                    inner,
                                    &holders.keys().copied().collect(),
                                )
                                .siblings_blocked(
                                    &request.siblings,
                                    &inner.catalog.sibling_commits,
                                )))
                });
            let initialized = job
                .queue_wait_clock_initialized
                .load(std::sync::atomic::Ordering::Acquire);
            let entry = job.queue_entry.load(std::sync::atomic::Ordering::Acquire);
            if !initialized {
                job.start_queue_wait(now);
                if pause {
                    job.pause_queue_wait(now);
                }
            } else if pause && entry != PAUSED_QUEUE && entry != NO_QUEUE {
                job.pause_queue_wait(now);
            } else if !pause && entry == PAUSED_QUEUE {
                job.resume_queue_wait(now);
            }
        }
    }

    /// Include both resource owners and scheduler fences in the refusal itself;
    /// the client need not have journal access to explain a local fallback.
    #[cfg(test)]
    fn queue_wait_detail(&self, inner: &Inner, id: Uuid) -> String {
        let index = DispatchIndex::build(inner);
        self.queue_wait_detail_with_index(inner, &index, id)
    }

    fn queue_wait_detail_with_index(
        &self,
        inner: &Inner,
        index: &DispatchIndex,
        id: Uuid,
    ) -> String {
        let Some(job) = inner.jobs.get(&id) else {
            return "unknown job".into();
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
        let now = self.now();
        let heads = index.heads(inner, now);
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
        let mut blocking_head = None;
        let phase = self
            .queue_wait_phase(inner, index, id)
            .unwrap_or("queue order");
        let phase = if phase == "queue order" {
            // This job would fit, so a job ahead of it holds the queue: a gate
            // job, a cache fill, or an older job past its backfill grace,
            // waiting for enough free permits (the scheduler stops smaller
            // jobs overtaking it so it cannot starve). Name that job.
            blocking_head = self.blocking_head(inner, &heads, id, index.weight, now);
            match blocking_head {
                Some((_, true)) => "gate job ahead",
                Some((head, false)) if inner.jobs[&head].base => {
                    "cache fill ahead needs more free permits"
                }
                Some((_, false)) => "older job ahead needs more free permits",
                None => "queue order",
            }
        } else {
            phase
        };
        let blocking = blocking_head
            .map(|(head, _)| {
                format!(
                    "; blocked_by=job={head} permits={}",
                    inner.jobs[&head].weight
                )
            })
            .unwrap_or_default();
        // The jobs holding this checkout as a sibling run in other workspaces,
        // so name them: the holders list alone does not say which one it is.
        let sibling_users = if phase == SIBLING_USER_WAIT {
            let users: Vec<_> = index
                .checkout_sibling_users(job.key())
                .into_iter()
                .map(|(user, weight)| {
                    format!(
                        "job={user} workspace={} permits={weight}",
                        inner.jobs.get(&user).map_or("unknown", |j| j.key())
                    )
                })
                .collect();
            format!("; sibling_users=[{}]", users.join("; "))
        } else {
            String::new()
        };
        format!("waiting_on={phase}{blocking}{sibling_users}; requested_permits={}; held_permits={}/{}; accepted_ms={}; holders=[{}]; reservations=[{}]; heads_ahead=[{}]",
            job.weight, index.weight, self.core.config.max_weight, job.accepted_ms,
            holders.join("; "), reservations.join("; "), pending_ahead.join("; "))
    }
    /// Every step that takes the state lock (and the workspace deletion, which
    /// can take minutes for a large target directory) runs on the blocking
    /// pool: this is called from a connection's async task, and an async
    /// worker parked on the lock stops the runner answering other connections.
    pub async fn drop_workspace(&self, key: &str) -> io::Result<DropReply> {
        self.ensure_reconciled()?;
        self.validate_destinations(key, None, &[])?;
        let key = key.to_string();
        let (guard, mut ids, admitting) = self
            .off_runtime({
                let key = key.clone();
                move |runner| {
                    let mut inner = runner.core.inner.lock().unwrap();
                    if !inner.dropping.insert(key.clone()) {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "workspace drop already in progress",
                        ));
                    }
                    // An admission still saving its record is cancelled too,
                    // once that save is over (see `cancel_admitted`).
                    let ids: Vec<_> = inner
                        .jobs
                        .values()
                        .filter(|j| j.key() == key && j.terminal.is_none())
                        .map(|j| j.id)
                        .collect();
                    let admitting: Vec<_> = inner
                        .admission_saves
                        .iter()
                        .filter(|(_, workspace)| **workspace == key)
                        .map(|(id, _)| *id)
                        .collect();
                    drop(inner);
                    Ok((WorkspaceDrop { runner, key }, ids, admitting))
                }
            })
            .await?;
        for &id in &ids {
            self.off_runtime(move |runner| runner.cancel(id)).await?;
        }
        for id in admitting {
            if self
                .off_runtime(move |runner| runner.cancel_admitted(id))
                .await?
            {
                ids.push(id);
            }
        }
        let deadline = tokio::time::Instant::now() + self.core.config.drop_timeout;
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
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "workspace owner did not stop before drop deadline",
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.off_runtime(move |runner| {
            // The blocking operation owns the fence even if its async caller
            // disconnects. Only this operation may remove its marker.
            let _guard = guard;
            let _pressure = runner.core.disk.lock().unwrap();
            runner.validate_destinations(&key, None, &[])?;
            let mut inner = runner.core.inner.lock().unwrap();
            let dropped = inner.catalog.workspaces.contains_key(&key);
            let catalog = if dropped {
                runner.discard(Path::new(&key))?;
                inner.catalog.workspaces.remove(&key);
                Some(runner.catalog_snapshot(&inner)?)
            } else {
                None
            };
            drop(inner);
            if let Some(catalog) = catalog {
                runner.write_catalog(catalog)?;
            }
            runner.prune_refs()?;
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
    /// Delete chunked returned files `RETURN_RETENTION_MS` after their job
    /// ended. The record keeps the manifest and says the bytes expired; it is
    /// saved first, so a crash before the deletion leaves only bytes that
    /// startup recovery removes, never a record naming missing files. A job
    /// being replayed keeps its bytes until a later pass. Runs without the
    /// state lock held, taking it only to choose and mark each job: the save
    /// fsyncs and the deletion can unlink thousands of files.
    fn expire_returned_files(&self) {
        let now = self.now();
        let due = |job: &Job| {
            job.returned.as_ref().is_some_and(|r| !r.expired)
                && job
                    .finished_ms
                    .is_some_and(|finished| now.saturating_sub(finished) >= RETURN_RETENTION_MS)
        };
        let candidates: Vec<_> = self
            .core
            .inner
            .lock()
            .unwrap()
            .jobs
            .values()
            .filter(|job| due(job))
            .map(|job| job.id)
            .collect();
        for id in candidates {
            let log = self.job_log(id);
            // Every writer of a finished job's record holds this lock, so the
            // record cannot change between the mark below and its save.
            let Some(_write) = log.try_lock_write() else {
                continue;
            };
            let (job, directory) = {
                let mut inner = self.core.inner.lock().unwrap();
                // Replays lease files under the state lock and skip returned
                // files marked expired, so once this check and the mark are
                // made under it, no reader can name the bytes being deleted.
                if log.retention.lock().unwrap().readers != 0 {
                    continue;
                }
                let Some(job) = inner.jobs.get_mut(&id).filter(|job| due(job)) else {
                    continue;
                };
                let returned = job.returned.as_mut().unwrap();
                returned.expired = true;
                let directory = self
                    .record_path(id)
                    .parent()
                    .unwrap()
                    .join(&returned.directory);
                (job.clone(), directory)
            };
            if let Err(error) = self.save(&job) {
                tracing::warn!(%id, %error, "returned file expiry could not be saved; retrying later");
                // The bytes are still there; let replays send them and a
                // later pass retry the expiry.
                if let Some(returned) = self
                    .core
                    .inner
                    .lock()
                    .unwrap()
                    .jobs
                    .get_mut(&id)
                    .and_then(|job| job.returned.as_mut())
                {
                    returned.expired = false;
                }
                continue;
            }
            self.long_work("returned_files_remove");
            match fs::remove_dir_all(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(%id, %error, "expired returned files could not be deleted")
                }
            }
        }
    }
    /// Also exposed to fake-clock tests; retention is relative to both terminal
    /// time and mint time, so recent unknown IDs can never rerun an expired job.
    pub fn maintenance(&self) -> io::Result<()> {
        #[cfg(unix)]
        if self.core.config.macos_instance() {
            self.core
                .config
                .macos_runtime
                .retry_cleanups(&self.core.config);
            let live = {
                let inner = self.core.inner.lock().unwrap();
                inner
                    .jobs
                    .values()
                    .filter(|job| job.terminal.is_none() || inner.starting.contains(&job.id))
                    .map(|job| job.id)
                    .collect()
            };
            self.core.config.macos_runtime.prune_cancelled(&live);
        }
        if self.core.config.windows_instance() {
            self.core.config.windows_runtime.retry_cleanups();
        }
        let mut set_aside = Vec::new();
        let maintained = self.maintain_state(&mut set_aside);
        // Expired job directories were moved aside under the state lock;
        // unlinking their output and returned files can take seconds on a
        // busy disk, so it happens here, with the lock released. Startup
        // deletes whatever a crash leaves in the trash directory.
        for directory in set_aside {
            self.long_work("expired_job_remove");
            if let Err(error) = fs::remove_dir_all(&directory) {
                tracing::warn!(path = %directory.display(), %error, "expired job files could not be deleted; startup retries");
            }
        }
        self.expire_returned_files();
        if !maintained? {
            return Ok(());
        }
        // Also enforces the snapshot cap at startup, which runs maintenance.
        self.maintain_sibling_snapshots();
        self.prune_refs()?;
        self.retire_unused_repositories()
    }
    /// Move an expired job's directory into the trash directory, so the
    /// caller can delete it after releasing the state lock. Only directories
    /// are moved; for anything else this fails and `maintain_state` tries to
    /// delete the path in place, which refuses a non-directory as before.
    fn set_aside_job_directory(&self, id: Uuid, directory: &Path) -> io::Result<PathBuf> {
        if !directory.symlink_metadata()?.is_dir() {
            return Err(io::Error::other("expired job path is not a directory"));
        }
        fs::create_dir_all(self.trash())?;
        let aside = self.trash().join(format!("job-{id}-{}", Uuid::now_v7()));
        fs::rename(directory, &aside)?;
        Ok(aside)
    }
    /// The part of `maintenance` that runs under the state lock. Returns
    /// whether repository maintenance may follow (not while quarantined).
    /// Expired job directories it moves aside are pushed to `set_aside` for
    /// deletion once the lock is released; one that cannot be moved is
    /// deleted in place, under the lock, as before.
    fn maintain_state(&self, set_aside: &mut Vec<PathBuf>) -> io::Result<bool> {
        let mut inner = self.core.inner.lock().unwrap();
        self.retry_start_refusals(&mut inner);
        let expired: Vec<_> = inner
            .jobs
            .values()
            .filter(|j| {
                !inner.base_finishes.contains_key(&j.id)
                    && j.finished_ms.is_some_and(|t| {
                        self.now().saturating_sub(t.max(mint_ms(j.id))) >= RETENTION_MS
                    })
            })
            .map(|j| j.id)
            .collect();
        for id in expired {
            let log = self.job_log(id);
            let Some(_write) = log.try_lock_write() else {
                continue;
            };
            if log.retention.lock().unwrap().readers != 0 {
                continue;
            }
            // Once the record leaves `jobs` under this lock, no replay can
            // lease the directory's files, so the deletion may run later.
            // Where the directory cannot be moved aside, delete it here.
            let directory = self.record_path(id).parent().unwrap().to_owned();
            let removed = match self.set_aside_job_directory(id, &directory) {
                Ok(aside) => {
                    set_aside.push(aside);
                    Ok(())
                }
                Err(_) => fs::remove_dir_all(&directory),
            };
            match removed {
                Ok(()) => {
                    inner.jobs.remove(&id);
                    self.core.repository_usage.lock().unwrap().remove(id);
                }
                Err(error) => {
                    tracing::warn!(%id, %error, "expired job deletion failed; retaining its record and continuing maintenance")
                }
            }
        }
        let Inner {
            jobs,
            start_retries,
            ..
        } = &mut *inner;
        start_retries.retain(|id, _| jobs.get(id).is_some_and(Job::pending));
        self.core.job_logs.lock().unwrap().retain(|id, log| {
            inner.jobs.contains_key(id)
                || inner.pending_replays.contains(id)
                || Arc::strong_count(log) > 1
        });
        if !self.core.quarantined.is_empty() {
            return Ok(false);
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
                        .admission_saves
                        .values()
                        .any(|workspace| workspace == *key)
                    && !inner
                        .jobs
                        .values()
                        .any(|j| j.key() == *key && j.terminal.is_none())
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in idle {
            if let Err(error) = self.validate_destinations(&key, None, &[]) {
                tracing::warn!(workspace = %key, %error, "workspace expiry refused outside owned roots");
                continue;
            }
            if let Err(error) = self.discard(Path::new(&key)) {
                tracing::warn!(workspace = %key, %error, "expired workspace deletion failed; retaining ownership and continuing maintenance");
                continue;
            }
            inner.catalog.workspaces.remove(&key);
        }
        // A repository inside another runner workspace can no longer be admitted,
        // but an entry recorded before that refusal existed still fences the
        // outer workspace through its derived fill checkout. Remove such entries
        // once nothing depends on them. A missing path alone isn't stale: main
        // checkouts used only through linked worktrees never exist on this host,
        // and their failed-fill records stop a failing fill from rerunning.
        let nested_in_workspace = |repository: &str| {
            let path = Path::new(repository);
            inner.catalog.workspaces.keys().any(|workspace| {
                let workspace = Path::new(workspace);
                path != workspace && path.starts_with(workspace)
            })
        };
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
                let fill_in_progress = record.result.as_ref() == Some(&RebuildResult::Building);
                // Preserve a new descriptor until its first fill attempt reports a result.
                let waiting_for_first_attempt = record.result.is_none()
                    && record
                        .requested_descriptor
                        .as_ref()
                        .is_some_and(|requested| {
                            record.failed_descriptor.as_ref() != Some(requested)
                        });
                (nested_in_workspace(repository)
                    && !has_unfinished_job
                    && !fill_in_progress
                    && !waiting_for_first_attempt
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
        let catalog = self.catalog_snapshot(&inner)?;
        drop(inner);
        self.write_catalog(catalog)?;
        Ok(true)
    }

    /// Check the mirrors a failed bundle fetch targeted and remove refs git
    /// cannot use (see `workspace::repair_mirror`), including refs at the
    /// bundles' prerequisites when those are damaged. One such ref fails every
    /// later fetch into that mirror, so leaving it would refuse every job for
    /// the repository. Returns `MirrorRepaired` when something was removed,
    /// otherwise the original error. Callers hold `ref_mutation`.
    fn repair_after_failed_fetch(
        &self,
        error: io::Error,
        directory: &Path,
        bundles: &[BundleManifest],
    ) -> io::Error {
        // A stalled transfer says nothing about the mirror.
        if error.kind() == io::ErrorKind::TimedOut {
            return error;
        }
        let mut repositories = BTreeMap::<&str, Vec<String>>::new();
        for bundle in bundles {
            let prerequisites =
                workspace::bundle_prerequisites(&directory.join(bundle.index.to_string()))
                    .unwrap_or_default();
            repositories
                .entry(&bundle.repository)
                .or_default()
                .extend(prerequisites);
        }
        let mut repaired = Vec::new();
        for (repository, prerequisites) in repositories {
            let removed = self.repair_mirror(repository, &prerequisites);
            if !removed.is_empty() {
                repaired.push(format!("{repository}: {}", removed.join(", ")));
            }
        }
        if repaired.is_empty() {
            return error;
        }
        io::Error::other(MirrorRepaired(format!(
            "mirror_repaired: the bundle fetch failed ({error}); removed unusable refs: {}; retry the job and the runner will ask for the history they pinned",
            repaired.join("; ")
        )))
    }

    /// Remove one mirror's unusable refs and forget the have-reply offers among
    /// them, logging what was found. Returns the removed refs with reasons.
    fn repair_mirror(&self, repository: &str, commits: &[String]) -> Vec<String> {
        let mirror = self.core.trees.mirror(repository);
        if !mirror.is_dir() {
            return Vec::new();
        }
        let removed = match workspace::repair_mirror(&mirror, commits) {
            Ok(removed) => removed,
            Err(error) => {
                tracing::error!(mirror = %mirror.display(), %error, "mirror ref check failed");
                return Vec::new();
            }
        };
        if removed.is_empty() {
            return removed;
        }
        tracing::warn!(mirror = %mirror.display(), removed = ?removed, "removed unusable mirror refs");
        if let Some(offers) = self.core.held_offers.lock().unwrap().get_mut(&mirror) {
            offers.retain(|commit, _| {
                let held = format!("{}{commit}", workspace::HELD_REFS);
                !removed
                    .iter()
                    .any(|entry| entry.starts_with(&format!("{held} ")))
            });
        }
        removed
    }

    /// Startup check of every mirror: an unclean stop can leave a ref file
    /// empty or a ref pointing at an object that was never written to disk,
    /// and either one fails every bundle fetch into that mirror. Only ref
    /// targets and their root trees are read, so this stays cheap however
    /// long a mirror's history is.
    fn check_mirrors(&self) {
        let _refs = self.core.ref_mutation.lock().unwrap();
        let Ok(entries) = fs::read_dir(&self.core.trees.mirrors) else {
            return;
        };
        for entry in entries.flatten() {
            let mirror = entry.path();
            if !mirror.is_dir() {
                continue;
            }
            match workspace::repair_mirror(&mirror, &[]) {
                Ok(removed) if removed.is_empty() => {}
                Ok(removed) => {
                    tracing::warn!(mirror = %mirror.display(), removed = ?removed, "startup removed unusable mirror refs")
                }
                Err(error) => {
                    tracing::error!(mirror = %mirror.display(), %error, "startup mirror ref check failed")
                }
            }
        }
    }

    fn prune_refs(&self) -> io::Result<()> {
        if !self.core.quarantined.is_empty() {
            return Ok(());
        }
        let _refs = self.core.ref_mutation.lock().unwrap();
        let inner = self.core.inner.lock().unwrap();
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
        drop(inner);
        // Commits a recent have reply reported stay pinned until their bundle
        // can have arrived; expired offers are forgotten and their refs pruned.
        let now = self.now();
        let mut offers = self.core.held_offers.lock().unwrap();
        offers.retain(|_, commits| {
            commits.retain(|_, expires| *expires > now);
            !commits.is_empty()
        });
        for (mirror, commits) in offers.iter() {
            mirrors.entry(mirror.clone()).or_default().extend(
                commits
                    .keys()
                    .map(|commit| format!("{}{commit}", workspace::HELD_REFS)),
            );
        }
        drop(offers);
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
            self.long_work("prune_refs_git");
            let refs = workspace::git(
                &mirror,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/snapshots",
                    "refs/tips",
                    workspace::HELD_REFS,
                ],
            )?;
            let stale: Vec<_> = refs
                .lines()
                .filter(|reference| !held.contains(*reference))
                .map(str::to_owned)
                .collect();
            workspace::delete_refs(&mirror, &stale)?;
        }
        Ok(())
    }

    fn enforce_kills(&self, instant: std::time::Instant) -> io::Result<()> {
        let now = self.now();
        let ids: Vec<_> = self
            .core
            .inner
            .lock()
            .unwrap()
            .jobs
            .values()
            .filter(|job| job.state == JobState::Running)
            .map(|job| job.id)
            .collect();
        for id in ids {
            let log = self.job_log(id);
            // A pending output commit owns this record. Retry next tick instead
            // of waiting for its storage while holding the shared state lock.
            let Some(_write) = log.try_lock_write() else {
                continue;
            };
            let _signalling = log.lock_signalling();
            let mut inner = self.core.inner.lock().unwrap();
            let Some(job) = inner
                .jobs
                .get(&id)
                .filter(|job| job.state == JobState::Running)
            else {
                continue;
            };
            if job.kill.is_none()
                && job
                    .request
                    .as_ref()
                    .and_then(|request| request.timeout)
                    .is_some_and(|seconds| {
                        now.saturating_sub(job.started_ms.unwrap()) >= seconds.saturating_mul(1000)
                    })
            {
                let mut updated = job.clone();
                // Storage contention may defer this pass past the deadline.
                // Keep the original expiry time so a late pass cannot restart
                // the TERM-to-KILL grace period for an already-overdue command.
                let deadline = job.started_ms.unwrap().saturating_add(
                    job.request
                        .as_ref()
                        .unwrap()
                        .timeout
                        .unwrap()
                        .saturating_mul(1000),
                );
                updated.kill = Some((Killed::Deadline, deadline));
                drop(inner);
                self.save(&updated)?;
                inner = self.core.inner.lock().unwrap();
                inner.jobs.insert(id, updated);
            }
            if inner.finalizing.contains(&id) || inner.base_finishes.contains_key(&id) {
                continue;
            }
            let job = inner.jobs[&id].clone();
            let Some((_, killed_at)) = job.kill else {
                continue;
            };
            let (attempts, due) = *inner.kill_retries.entry(id).or_insert((0, instant));
            if instant < due {
                continue;
            }
            // Query and signal the unit without the state lock: each is a
            // systemctl process, slow on a loaded host, and every admission,
            // status reply and cancel waits for that lock. `_signalling`
            // keeps finalization from being claimed meanwhile, and `_write`
            // keeps any other pass from retrying this job, so the retry state
            // read above is still this job's when it is written back below.
            drop(inner);
            let next = match self.unit_alive(job.process) {
                Err(error) => {
                    tracing::warn!(job_id = %id, %error, "cannot inspect cancelled unit; retrying");
                    (attempts, instant + Duration::from_secs(5))
                }
                Ok(alive) if job.process.is_none() || alive == Some(false) => {
                    let runner = self.clone();
                    std::thread::spawn(move || {
                        if let Err(error) = runner.finish_unobserved_unit(
                            id,
                            "cancelled unit already exited",
                            alive == Some(false),
                        ) {
                            tracing::error!(%id, %error, "terminal bookkeeping failed");
                        }
                    });
                    (attempts, instant + Duration::from_secs(5))
                }
                Ok(_) => {
                    let hard = now.saturating_sub(killed_at) >= 2000;
                    if hard && attempts < 3 {
                        self.core.backend.cancel(&self.core.config, &job, true)?;
                        (
                            attempts + 1,
                            instant + Duration::from_millis(500 * (1 << (attempts + 1))),
                        )
                    } else if !hard && attempts == 0 {
                        self.core.backend.cancel(&self.core.config, &job, false)?;
                        (attempts, instant + Duration::from_secs(2))
                    } else {
                        (attempts, instant + Duration::from_secs(5))
                    }
                }
            };
            if let Some(retry) = self.core.inner.lock().unwrap().kill_retries.get_mut(&id) {
                *retry = next;
            }
        }
        Ok(())
    }
    fn tick(&self) -> io::Result<()> {
        self.refresh_quarantine_admission(false);
        self.enforce_kills(std::time::Instant::now())?;
        let mut inner = self.core.inner.lock().unwrap();
        self.retry_start_refusals(&mut inner);
        for id in inner.base_finishes.keys().copied().collect::<Vec<_>>() {
            self.start_base_finish(&mut inner, id)?;
        }
        let mut now = self.now();
        if now.saturating_sub(inner.maintained_ms) >= 60_000 {
            inner.maintained_ms = now;
            drop(inner);
            // Production logs at info level, so the per-hold debug events of
            // `StateLock` are not written; one line a minute names the
            // longest holder whenever any hold was slow.
            if let Some(longest) = self.core.inner.take_longest() {
                tracing::info!(
                    site = %longest.site,
                    held_ms = longest.held.as_millis() as u64,
                    slow_holds = longest.slow_holds,
                    "longest state lock hold since the previous report"
                );
            }
            self.maintenance()?;
            inner = self.core.inner.lock().unwrap();
            // Maintenance ran without the state lock; take the time again.
            now = self.now();
        }
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
                        && !inner.terminal_saves.contains(&job.id)
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
                    None,
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
        // Queue warm builds for repositories that want one. What queuing one
        // needs from the mirror is read without the state lock (each read runs
        // git); then the list is made again under the lock, and only
        // repositories still wanting the same build are queued.
        let policy = self.publishing_policy();
        // The refresh timer runs on its own schedule: it needs no job arrival,
        // and it queues through the publication gate like every other caller.
        let refresh_due = policy.publishing()
            && inner
                .refreshed_ms
                .is_none_or(|last| now.saturating_sub(last) >= publication::REFRESH_TICK_MS);
        if refresh_due {
            inner.refreshed_ms = Some(now);
        }
        let mut wanted: Vec<(String, BaseRequest)> = Self::desired_bases(&inner)
            .into_iter()
            .filter(|(_, desired)| self.warm_builds_enabled(&desired.tip))
            .collect();
        if refresh_due {
            for (root, desired) in self.refresh_bases(&inner, &policy, now) {
                if !wanted.iter().any(|(queued, _)| *queued == root) {
                    wanted.push((root, desired));
                }
            }
        }
        let desired = wanted;
        let mut queued = Ok(false);
        if !desired.is_empty() {
            drop(inner);
            self.long_work("base_plan");
            let plans: Vec<_> = desired
                .into_iter()
                .map(|(root, desired)| {
                    let plan = self.base_plan(&root, &desired.tip, true);
                    (root, desired, plan)
                })
                .collect();
            inner = self.core.inner.lock().unwrap();
            // Dispatch below changes queue clocks at the current time, not at
            // one taken before the mirror reads.
            now = self.now();
            if inner.drain_deadline.is_some() {
                // A drain began meanwhile. Start nothing: the next tick
                // refuses the queued jobs first.
                return Ok(());
            }
            let mut current = Self::desired_bases(&inner);
            if refresh_due {
                current.extend(self.refresh_bases(&inner, &self.publishing_policy(), now));
            }
            for (root, desired, plan) in plans {
                if !current.iter().any(|(r, d)| {
                    *r == root && d.tip == desired.tip && d.workspace == desired.workspace
                }) {
                    continue;
                }
                match self.queue_base_planned(&mut inner, &root, &desired.workspace, plan) {
                    Ok(changed) => queued = queued.map(|before| before || changed),
                    Err(error) => {
                        queued = Err(error);
                        break;
                    }
                }
            }
        }
        // The catalog is saved after the state lock is released, as is each
        // started job's setup record (see `start`).
        let catalog = match &queued {
            Ok(false) => None,
            _ => Some(self.catalog_snapshot(&inner)?),
        };
        let dispatched = if queued.is_ok() {
            self.dispatch(&mut inner, now)
        } else {
            Ok(())
        };
        drop(inner);
        if let Some(catalog) = catalog {
            self.write_catalog(catalog)?;
        }
        queued?;
        dispatched
    }

    /// Repositories the refresh timer wants a recache fill of now: a local fill
    /// record whose publication is no longer fresh, and no fill already queued
    /// or running for them. The fill is at the repository's recorded `desired`
    /// tip, with no fetch. A dependency-free shortcut record has nothing to
    /// publish and is never queued here.
    fn refresh_bases(
        &self,
        inner: &Inner,
        policy: &publication::PublishPolicy,
        now: u64,
    ) -> Vec<(String, BaseRequest)> {
        inner
            .catalog
            .repositories
            .iter()
            .filter(|(root, repository)| {
                !Self::base_settlement_pending(inner, root)
                    && !inner.jobs.values().any(|j| {
                        j.base
                            && j.terminal.is_none()
                            && j.request
                                .as_ref()
                                .is_some_and(|r| r.repository_root == **root)
                    })
                    && repository
                        .filled_descriptor
                        .as_deref()
                        .is_some_and(|descriptor| {
                            publication::expired(policy, repository, descriptor, now)
                        })
            })
            .filter_map(|(root, repository)| {
                repository
                    .desired
                    .clone()
                    .map(|desired| (root.clone(), desired))
            })
            .collect()
    }

    /// Repositories whose desired warm build is not filled, has not failed
    /// and has no fill job yet, with that build.
    fn desired_bases(inner: &Inner) -> Vec<(String, BaseRequest)> {
        inner
            .catalog
            .repositories
            .iter()
            .filter_map(|(root, g)| {
                let desired = g.desired.as_ref()?;
                (g.requested_descriptor != g.filled_descriptor
                    && !Self::base_settlement_pending(inner, root)
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
            .collect()
    }

    fn base_settlement_pending(inner: &Inner, root: &str) -> bool {
        inner
            .base_finishes
            .values()
            .any(|pending| pending.request.repository_root == root)
    }

    fn dispatch(&self, inner: &mut Inner, now: u64) -> io::Result<()> {
        if inner.restoring_reserve {
            return Ok(());
        }
        if self.ensure_admission_ownership().is_err() {
            return Ok(());
        }
        let mut index = DispatchIndex::build(inner);
        self.update_queue_wait_clocks(inner, now, &index);
        loop {
            // A Boat lease owns the entire sandbox, regardless of CPU weight
            // or checkout. Count setup reservations as well as running jobs.
            if self.core.config.boat_profile && !index.workspace_owners.is_empty() {
                break;
            }
            let mut next = None;
            for id in index.heads(inner, now) {
                if inner.terminal_saves.contains(&id) {
                    // This head's withdrawal is being saved. Until it is
                    // settled the job may yet stay queued, so it keeps its
                    // place: nothing it would have held back starts, and
                    // neither does it.
                    return Ok(());
                }
                let job = &inner.jobs[&id];
                let request = job.request.as_ref().unwrap();
                // One cache fill at a time (the writer slot), and none while
                // the cache is being evicted. A fill does not wait for queued
                // foreground jobs: under steady load the queue never empties,
                // and an emptied cache would then never be filled again. On a
                // host so small that the fill would take over half of it, it
                // still waits for the foreground queue to empty.
                if job.base
                    && (inner.writer.is_some()
                        || !inner.base_finishes.is_empty()
                        || inner
                            .evicting_caches
                            .contains(&self.core.config.sccache_dir())
                        || (index.foreground_pending != 0 && !self.fill_passes_queue(job.weight)))
                {
                    continue;
                }
                if (!self.core.config.windows_instance()
                    && Self::deleting(inner, job.key(), &request.siblings))
                    || index.occupied(job.key())
                    || index.siblings_blocked(&request.siblings, &inner.catalog.sibling_commits)
                    || index.held_by_queued_workspace(inner, job, self.core.config.max_weight)
                {
                    // A checkout conflict cannot be resolved by idling unrelated CPUs.
                    continue;
                }
                // An earlier attempt to start the job is still being cleaned
                // up (a Windows VM whose stop maintenance retries) and owns
                // paths named by the job's ID, which a new attempt would
                // reuse. Like a checkout conflict, this holds back no one
                // else: it can last until the VM is confirmed stopped.
                #[cfg(unix)]
                if self.core.config.macos_instance()
                    && self.core.config.macos_runtime.workspace_busy(job.key(), id)
                {
                    continue;
                }
                if self.core.config.cleanup_pending(id) {
                    continue;
                }
                if job.weight > self.core.config.max_weight {
                    continue;
                }
                // A job whose last start failed waits out its retry delay
                // (at most START_RETRY_MAX_MS) as if it were waiting for
                // permits: a gate or aged head still fences capacity.
                if index.weight.saturating_add(job.weight) > self.core.config.max_weight
                    || Self::start_deferred(inner, id, now)
                {
                    // Gates, the fill that may start and aged heads stop
                    // backfill only for capacity, after their checkout
                    // conflicts have already been excluded. Running jobs keep
                    // their permits; the fill waits for permits to come free.
                    if job.gate_job()
                        || job.base
                        || now.saturating_sub(job.queued_ms.unwrap_or(job.accepted_ms))
                            >= BACKFILL_GRACE_MS
                    {
                        return Ok(());
                    }
                    continue;
                }
                next = Some(id);
                break;
            }
            let Some(id) = next else { break };
            self.start(inner, id, &index.claims)?;
            index.started(inner, id);
            self.update_queue_wait_clocks(inner, now, &index);
        }
        Ok(())
    }
    /// Reserve a queued job's workspace and set it up on its own thread.
    /// Checking out a workspace and cloning a warm target can take minutes;
    /// the scheduler keeps the state lock only to reserve and, afterwards, to
    /// launch, so other jobs, transfers and requests proceed meanwhile.
    fn start(
        &self,
        inner: &mut Inner,
        id: Uuid,
        claims: &BTreeMap<Uuid, Vec<u32>>,
    ) -> io::Result<()> {
        if inner.restoring_reserve {
            return Ok(());
        }
        let mut job = inner.jobs[&id].clone();
        if job.base {
            job.fill_started_ms = Some(self.now());
            job.fill_identity = self.publishing_identity();
        }
        let r = job.request.clone().unwrap();
        // Recovered warm builds bypass external admission. Boat must never run
        // them: writer units expose a shared cache outside the job project.
        let refusal = if self.core.config.boat_profile && job.base {
            Some((
                RefusalReason::WorkspaceSetupFailed,
                "Boat jobs cannot use dependency writer units".to_owned(),
            ))
        } else {
            self.cpu_refusal(job.weight)
        };
        if let Some((reason, detail)) = refusal {
            let safe_detail = self.safe_refusal_detail(&detail, r.env.values().cloned());
            let mut refused = job.clone();
            refuse_before_start(
                &self.core.health,
                &mut refused,
                reason,
                &detail,
                Some(safe_detail),
                None,
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
                        return self.fail_cache_fill(inner, &job, error);
                    }
                }
            }
        }
        let mut queued_job = job.clone();
        let placement = self
            .core
            .cpus
            .place_with_status(job.weight, claims.values().map(Vec::as_slice));
        tracing::info!(job_id = %id, cache_fill = job.base, requested_cpus = job.weight,
            cpus = ?placement.cpus, windows_active = ?placement.windows_active,
            avoided_cpus = ?placement.avoided_cpus, "placed job CPUs");
        queued_job.cpus = placement.cpus;
        let setup_ms = self.now();
        queued_job.setup_ms = Some(setup_ms);
        queued_job.stop_queue_wait(setup_ms);
        queued_job
            .queue_entry
            .store(NO_QUEUE, std::sync::atomic::Ordering::Release);
        // Everything that grants this job its permits, CPUs and workspace is
        // reserved here, under the state lock, so no other dispatch pass,
        // prepare or admission can take them. The setup record itself is
        // saved by the setup thread, before any setup work and without the
        // state lock (see `persist_setup`).
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
        let rotation = inner
            .rotation
            .insert(r.workspace_key.clone(), inner.dispatch);
        // In snapshot mode no sibling is checked out in place, so the
        // in-place sibling commits are not needed; the freshness record and
        // the start time the switch touch needs are taken here instead,
        // while the workspace is reserved. A warm build mounts no siblings.
        let snapshot_mode = self.core.config.sibling_snapshot_mode();
        let setup = Setup {
            id,
            cpus,
            base: job.base,
            recorded: if snapshot_mode {
                BTreeMap::new()
            } else {
                inner.catalog.sibling_commits.clone()
            },
            snapshots: (snapshot_mode && !job.base).then(|| {
                crate::sibling_snapshots::JobSiblings::reserve(&inner.catalog, &r.workspace_key)
            }),
            request: r,
        };
        // The undo reaches the setup thread as plain data and becomes a
        // `PendingStart` only there. If the thread cannot be created, the
        // closure is dropped on this thread, which holds the state lock, and
        // a `PendingStart` dropped here would take that lock again: it is a
        // plain, non-reentrant mutex, so the scheduler would hang holding it.
        // The copy kept here undoes the dispatch under the lock already held.
        let workspace = setup.request.workspace_key.clone();
        let base = job.base;
        let undo = (job, rotation);
        let kept = undo.clone();
        let runner = self.clone();
        let thread_workspace = workspace.clone();
        let spawned = self.spawn_setup(move || {
            let mut setup = setup;
            let mut pending = PendingStart {
                runner: runner.clone(),
                id,
                workspace: thread_workspace,
                base,
                undo: Some(undo),
            };
            if !runner.persist_setup(&setup, &mut pending) {
                return;
            }
            let prepared = runner.setup(&mut setup);
            if let Err(error) = runner.launch(setup, prepared, &mut pending) {
                tracing::error!(job_id = %id, %error, "job launch failed");
            }
            pending.settled();
        });
        if let Err(error) = spawned {
            let mut pending = PendingStart {
                runner: self.clone(),
                id,
                workspace,
                base,
                undo: Some(kept),
            };
            pending.requeue(inner, &format!("setup thread not started: {error}"));
        }
        Ok(())
    }
    /// Run a job's setup on a new thread. Thread creation can fail (too many
    /// threads, no memory); the caller then undoes the dispatch.
    fn spawn_setup(&self, work: impl FnOnce() + Send + 'static) -> io::Result<()> {
        #[cfg(test)]
        if self
            .core
            .config
            .fail_setup_spawn
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            // As `Builder::spawn` does when it fails: the closure is dropped
            // on the calling thread.
            drop(work);
            return Err(io::Error::other("injected thread creation failure"));
        }
        std::thread::Builder::new().spawn(work).map(drop)
    }
    /// Save the setup record of a job `start` reserved, before any of its
    /// setup work, and return whether setup may go ahead. The save runs
    /// without the state lock, under the job log lock that cancel, withdraw
    /// and refuse take before writing this job's record, so none of them
    /// saves in between. A job one of them ended before this took that lock
    /// is not set up, and its reservation is released. If the save fails, the
    /// job is put back in the queue as dispatch found it (see
    /// `PendingStart::requeue`), so a later pass can retry.
    fn persist_setup(&self, s: &Setup, pending: &mut PendingStart) -> bool {
        let log = self.job_log(s.id);
        let _write = log.lock_write();
        let record = {
            let mut inner = self.core.inner.lock().unwrap();
            match inner.jobs.get(&s.id) {
                Some(job) if job.state == JobState::Queued => job.clone(),
                _ => {
                    // Cancelled or withdrawn since dispatch; that terminal
                    // is already saved.
                    pending.settled();
                    inner.starting.remove(&s.id);
                    inner.reserved.remove(&s.request.workspace_key);
                    if s.base {
                        if let Err(error) = self.finish_base(&mut inner, &s.request, false, None) {
                            tracing::error!(job_id = %s.id, %error, "warm build bookkeeping failed");
                        }
                    }
                    return false;
                }
            }
        };
        let Err(error) = self.save(&record) else {
            return true;
        };
        pending.requeue(
            &mut self.core.inner.lock().unwrap(),
            &format!("setup record not saved: {error}"),
        );
        false
    }
    /// The long part of starting a job, run without the state lock. In
    /// snapshot mode it resolves and touches the job's sibling snapshots
    /// before materializing the workspace, holding the workspace's
    /// reservation and no lock.
    fn setup(&self, s: &mut Setup) -> io::Result<Prepared> {
        crate::setup_log::materialize(s.id, s.base, self.core.tenant_events.clone(), || {
            if let Some(full) =
                crate::setup_log::phase("disk admission", || self.ensure_disk_space())?
            {
                #[cfg(target_os = "linux")]
                if self.core.config.boat_profile {
                    if let Err(error) = (self.core.config.boat_readiness_withdraw)() {
                        tracing::error!(%error, "failed to withdraw Boat readiness after setup disk shortage");
                    }
                }
                return Err(io::Error::other(format!(
                    "runner_disk_full: free_bytes={} floor_bytes={}",
                    full.free_bytes, full.floor_bytes
                )));
            }
            if let Some(snapshots) = &mut s.snapshots {
                let r = &s.request;
                // Validated before a sibling's mirror is read for it.
                workspace::validate_destinations(
                    &self.core.config.owned_workspace_roots,
                    &r.workspace_key,
                    Some(&r.cwd),
                    &r.siblings,
                )?;
                let resolved = setup_step(
                    "prepare sibling snapshots",
                    crate::setup_log::phase("prepare sibling snapshots", || {
                        snapshots.resolve(
                            &self.core.config,
                            &self.core.snapshots,
                            &self.core.trees,
                            &r.siblings,
                        )
                    }),
                );
                // Whatever the outcome, a snapshot may have been created;
                // this setup's reservation keeps its own snapshots.
                self.cap_sibling_snapshots(&r.siblings);
                resolved?;
            }
            #[cfg(target_os = "linux")]
            if self.core.config.boat_profile {
                match self.materialize(s) {
                    Ok(prepared) => return Ok(prepared),
                    Err(error) if crate::boat_quota::cold_cache(&error).is_some() => {
                        let request = crate::boat_quota::cold_cache(&error).unwrap();
                        tracing::info!(job_id = %s.id, workspace = %s.request.workspace_key, initial_bytes = ?request.initial_bytes, cap_bytes = request.cap_bytes, minimum_growth_bytes = request.minimum_growth, "evicting Boat cache before one cold materialization retry");
                        self.evict_boat_setup_workspace(s)?;
                        let mut cold = self.core.config.clone();
                        cold.boat_cold_materialization = true;
                        return self.materialize_config(s, &cold).map_err(|error| {
                            if let Some(request) = crate::boat_quota::cold_cache(&error) {
                                if let Err(withdraw) = (self.core.config.boat_readiness_withdraw)() {
                                    tracing::error!(%withdraw, "failed to withdraw Boat readiness after cold quota shortage");
                                }
                                io::Error::other(format!("runner_disk_full: cold_quota_cap_bytes={} minimum_growth_bytes={}; command was not started", request.cap_bytes, request.minimum_growth))
                            } else { error }
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
            self.materialize(s)
        })
    }

    fn materialize(&self, s: &Setup) -> io::Result<Prepared> {
        self.materialize_config(s, &self.core.config)
    }
    fn materialize_config(&self, s: &Setup, config: &Config) -> io::Result<Prepared> {
        #[cfg(test)]
        if self.core.config.fail_materialize {
            return Err(io::Error::other(
                "materialize workspace: injected materialization failure",
            ));
        }
        self.long_work("materialize");
        self.core.backend.materialize(config, &self.core.trees, s)
    }

    /// `launch` for a job that tests set up by hand, with no dispatch to undo.
    #[cfg(test)]
    pub(crate) fn launch_without_dispatch(
        &self,
        s: Setup,
        prepared: io::Result<Prepared>,
    ) -> io::Result<()> {
        let mut pending = PendingStart {
            runner: self.clone(),
            id: s.id,
            workspace: s.request.workspace_key.clone(),
            base: s.base,
            undo: None,
        };
        self.launch(s, prepared, &mut pending)
    }

    /// Give up a prepared job that will not run: shut down its Windows VM,
    /// waiting for the VM's I/O thread, then drop the rest. Dropping a
    /// `PreparedVm` alone only signals that thread to stop.
    fn release_prepared(&self, prepared: Prepared) {
        if let Some(vm) = &prepared.vm {
            if let Some(vm) = vm.lock().unwrap().take() {
                vm.reap();
            }
        }
        drop(prepared);
    }

    /// Finish a job's start: record its workspace, and launch it unless setup
    /// failed or it was cancelled or withdrawn meanwhile. On the launch path
    /// the catalog write, the launch itself (a process spawn) and the Started
    /// record save run without the state lock; a warm build that does not
    /// launch still saves the catalog under it, in `finish_base`. The job stays in `starting`, and keeps its
    /// workspace reservation, until it is Running in memory or its start is
    /// given up, so no dispatch pass starts it again and no other job takes
    /// its workspace meanwhile. Cancel and withdraw wait for this job's log
    /// lock, held throughout.
    fn launch(
        &self,
        mut s: Setup,
        prepared: io::Result<Prepared>,
        pending: &mut PendingStart,
    ) -> io::Result<()> {
        let id = s.id;
        let log = self.job_log(id);
        let _write = log.lock_write();
        let r = s.request.clone();
        let snapshots = s.snapshots.take();
        let sibling_sources = snapshots.as_ref().map(|snapshots| snapshots.sources());
        let mut inner = self.core.inner.lock().unwrap();
        if s.base {
            // A warm build never checks its siblings out, and in snapshot
            // mode nothing records in-place sibling commits any more.
            if prepared.is_ok() && !self.core.config.sibling_snapshot_mode() {
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
                snapshots.is_none(),
                r.workspace_kind,
            );
            // Only a setup that completed, and so completed its switch
            // touch, updates the workspace's sibling freshness record.
            if let Some(snapshots) = snapshots.filter(|_| prepared.is_ok()) {
                snapshots.apply(&mut inner.catalog, &r.workspace_key);
            }
        }
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
            // Logs must not bypass the redaction and size limit applied to the wire detail.
            refuse_before_start(
                &self.core.health,
                j,
                reason,
                &safe_detail,
                Some(safe_detail.clone()),
                None,
                self.now(),
            );
            j.state = JobState::Tombstone;
            self.save(j)?;
            if s.base {
                self.finish_base(inner, &r, false, None)?;
            }
            Ok(())
        };
        // A failed setup, or a job ended while it was being set up, gives
        // back its reservation and finishes here.
        if !live || prepared.is_err() {
            pending.settled();
            let prepared = match prepared {
                Ok(prepared) => {
                    // The Windows slot stays this job's (`starting`) until
                    // its prepared VM has shut down.
                    drop(inner);
                    self.release_prepared(prepared);
                    inner = self.core.inner.lock().unwrap();
                    Ok(())
                }
                Err(error) => Err(error),
            };
            inner.starting.remove(&id);
            inner.reserved.remove(&r.workspace_key);
            let finished = match prepared {
                Err(error) if live => {
                    let (reason, detail) = if error.to_string() == "tree_hash_mismatch"
                        || error.to_string().starts_with("tree_hash_mismatch:")
                    {
                        (RefusalReason::TreeHashMismatch, None)
                    } else if error.to_string().starts_with("runner_disk_full:") {
                        (RefusalReason::RunnerDiskFull, Some(error.to_string()))
                    } else if error.to_string() == "windows_slot_busy" {
                        (RefusalReason::RunnerFull, None)
                    } else {
                        (
                            RefusalReason::WorkspaceSetupFailed,
                            Some(workspace_setup_detail("prepare job workspace", &error)),
                        )
                    };
                    refuse(&mut inner, reason, detail, &error)
                }
                _ if s.base => self.finish_base(&mut inner, &r, false, None),
                _ => Ok(()),
            };
            // A warm build's `finish_base` saved the catalog already;
            // otherwise it is written after the state lock is released.
            let catalog = (!s.base).then(|| self.catalog_snapshot(&inner)).transpose();
            drop(inner);
            let written =
                catalog.and_then(|catalog| catalog.map_or(Ok(()), |c| self.write_catalog(c)));
            return written.and(finished);
        }
        let Ok(prepared) = prepared else {
            unreachable!("a failed setup returned above");
        };
        let catalog = self.catalog_snapshot(&inner);
        let mut job = inner.jobs[&id].clone();
        drop(inner);
        if let Err(error) = catalog.and_then(|catalog| self.write_catalog(catalog)) {
            // Nothing ran: put the job back in the queue as dispatch found
            // it. The prepared workspace (and Windows VM) goes first, so it
            // never outlives the reservation that covered it.
            self.release_prepared(prepared);
            pending.requeue(
                &mut self.core.inner.lock().unwrap(),
                &format!("catalog not saved: {error}"),
            );
            return Err(error);
        }
        // From here until the job's process is recorded, a live unit of this
        // job can exist that the job does not name yet.
        self.core.inner.lock().unwrap().launching.insert(id);
        tracing::info!(job_id = %id, cache_fill = s.base, "handing materialized job to execution");
        let launch = match self.core.backend.launch(
            &self.core.config,
            &self.core.trees,
            &s,
            &prepared,
            job.weight,
        ) {
            Ok(l) => l,
            Err(error) => {
                pending.settled();
                self.release_prepared(prepared);
                let mut inner = self.core.inner.lock().unwrap();
                inner.launching.remove(&id);
                inner.starting.remove(&id);
                inner.reserved.remove(&r.workspace_key);
                return refuse(
                    &mut inner,
                    RefusalReason::WorkspaceSetupFailed,
                    Some(workspace_setup_detail("launch isolated job", &error)),
                    &error,
                );
            }
        };
        #[cfg(test)]
        if !self.core.config.vm_instance() {
            if let Some(processes) = &self.core.config.test_processes {
                if let Some(identity) = launch.process {
                    processes.lock().unwrap().register(identity);
                }
            }
        }
        job.state = JobState::Running;
        job.process = launch.process;
        job.durable_output = launch.durable_output;
        // A fill's write sockets record what its PUTs did, so its publication
        // is decided from them once no PUT is in flight any more.
        if job.base {
            self.core
                .inner
                .lock()
                .unwrap()
                .fill_ledgers
                .insert(id, launch.cache_ledgers);
        }
        // Saved with the process and Started below, before the execution gate
        // opens: a job with this field mounts no sibling host path, so no
        // sibling fence applies to it, even after a restart.
        job.sibling_sources = sibling_sources;
        let started_ms = self.now();
        job.started_ms = Some(started_ms);
        if job.base {
            job.fill_started_ms = Some(started_ms);
            job.fill_identity = self.publishing_identity();
        }
        // Persist start progress with the timeout origin before opening the
        // execution gate; setup and permit reservation do not start the command.
        assert_eq!(
            job.next_seq, 0,
            "Started must be the first sequenced job record"
        );
        let queue_wait_ms = job.queue_wait_ms(started_ms);
        let mut started = Started::new(job.next_seq, queue_wait_ms, started_ms);
        if self.core.config.windows_instance() {
            started = started.with_path_mapping(PathMapping::new("/", r"C:\m"));
        }
        job.started = Some(started);
        job.next_seq += 1;
        let weight = job.weight;
        // Saved before the job is Running in memory, so no replay shows a
        // Started that is not durable.
        let saved = self.save(&job);
        let mut inner = self.core.inner.lock().unwrap();
        // The start is settled either way: the job is published Running, or
        // `failed_launch` stops its process and refuses it.
        pending.settled();
        inner.reserved.remove(&r.workspace_key);
        inner.launching.remove(&id);
        if let Err(error) = saved {
            // The job stays in `starting`, now with its launched process
            // recorded, while `failed_launch` stops that process.
            job.started = None;
            inner.jobs.insert(id, job);
            inner.finalizing.insert(id);
            drop(inner);
            return self.failed_launch(id, launch.execution, error, true, "start_not_persisted");
        }
        inner.starting.remove(&id);
        inner.start_retries.remove(&id);
        inner.jobs.insert(id, job);
        tracing::info!(job_id = %id, cache_fill = s.base, "job Started recorded");
        self.core
            .health
            .lock()
            .unwrap()
            .record_queue_wait(started_ms, queue_wait_ms);
        let mut execution = launch.execution;
        // Pipe handles must exist before the command can fill its output pipes.
        let output = match execution.output() {
            Ok(output) => Some(output),
            Err(error) if launch.durable_output => {
                tracing::warn!(job_id = %id, %error, "job CPU accounting unavailable");
                None
            }
            Err(error) => {
                inner.starting.insert(id);
                inner.finalizing.insert(id);
                drop(inner);
                return self.failed_launch(id, execution, error, true, "output_unavailable");
            }
        };
        #[cfg(test)]
        if let Some(hook) = &self.core.config.before_gate {
            hook();
        }
        if let Err(error) = execution.release() {
            inner.starting.insert(id);
            inner.finalizing.insert(id);
            drop(inner);
            return self.failed_launch(id, execution, error, false, "release_failed");
        }
        tracing::info!(job_id = %id, workspace = %r.workspace_key, base = s.base, permits = weight, since_ms = started_ms, "job started");
        drop(inner);
        if launch.durable_output {
            if let Some(output) = output {
                std::thread::spawn(move || {
                    let _ = io::copy(&mut { output.stdout }, &mut io::sink());
                });
                let runner = self.clone();
                std::thread::spawn(move || runner.collect_accounting(id, output.stderr));
            }
            let runner = self.clone();
            std::thread::spawn(move || {
                runner.monitor_unit(id);
                let _ = execution.wait();
                execution.reap();
            });
            return Ok(());
        }
        let output = output.unwrap();
        let stdout = output.stdout;
        let stderr = output.stderr;
        let runner = self.clone();
        std::thread::spawn(move || {
            let reader = |mut file: Box<dyn Read + Send>, stream: OutputStream, runner: Runner| {
                std::thread::spawn(move || {
                    let mut buffer = [0; 16384];
                    let mut persistence_error = None;
                    let mut dropped_bytes = 0u64;
                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => {
                                if persistence_error.is_none() {
                                    if let Err(error) =
                                        runner.output(id, stream.clone(), &buffer[..n])
                                    {
                                        persistence_error = Some(error);
                                    }
                                }
                                // Keep the pipe flowing after storage fails; the
                                // execution still owns another read descriptor.
                                if persistence_error.is_some() {
                                    dropped_bytes += n as u64;
                                }
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(error) => return Err(error),
                        }
                    }
                    if let Some(error) = persistence_error {
                        tracing::error!(job_id = %id, %error, dropped_bytes, "output persistence failed; pipe drained");
                        Err(error)
                    } else {
                        Ok(())
                    }
                })
            };
            let a = reader(stdout, OutputStream::Stdout, runner.clone());
            let b = reader(stderr, OutputStream::Stderr, runner.clone());
            let status = execution.wait();
            let execution_ended = status.is_ok();
            let output_error = [a.join(), b.join()]
                .into_iter()
                .find_map(|result| match result {
                    Ok(Err(error)) => Some(error),
                    Err(_) => Some(io::Error::other("output monitor panicked")),
                    _ => None,
                });
            execution.reap();
            if let Err(error) =
                runner.finish_execution(id, output_error.map_or(status, Err), execution_ended)
            {
                tracing::error!(%error, "terminal persistence failed");
            }
        });
        Ok(())
    }
    #[cfg(target_os = "linux")]
    fn fail_cache_fill(&self, inner: &mut Inner, job: &Job, error: io::Error) -> io::Result<()> {
        let mut failed = job.clone();
        failed.terminal_ended(Outcome::OutcomeUnknown, self.now());
        failed.monitoring_error = Some(error.to_string());
        let evidence = self.execution_evidence(&failed);
        let detail = self.explained_terminal(
            &failed,
            "cache_fill_setup_failed",
            &error.to_string(),
            &evidence,
        );
        failed.terminal = failed
            .terminal
            .take()
            .map(|t| t.with_refusal_detail(detail));
        // Preserve the intended terminal before any fallible disk work. The
        // settlement worker saves it without holding the scheduler state lock.
        inner
            .base_finishes
            .entry(job.id)
            .or_insert_with(|| BaseFinish {
                request: job.request.as_ref().unwrap().clone(),
                success: false,
                failure: Some(error.to_string()),
                recorded: false,
                cleanup: None,
                terminal: Some(failed),
            });
        self.start_base_finish(inner, job.id)
    }
    fn failed_launch(
        &self,
        id: Uuid,
        mut execution: Box<dyn backend::Execution>,
        error: io::Error,
        unreleased: bool,
        reason: &str,
    ) -> io::Result<()> {
        let mut finished = {
            let mut inner = self.core.inner.lock().unwrap();
            inner.starting.insert(id);
            inner.finalizing.insert(id);
            inner.jobs[&id].clone()
        };
        self.long_work("launch-cleanup");
        let mut delay = Duration::from_millis(50);
        loop {
            match self
                .core
                .backend
                .cancel(&self.core.config, &finished, true)
                .and_then(|()| execution.abort())
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::warn!(job_id = %id, %error, "launch cleanup stop failed; retaining ownership");
                    std::thread::park_timeout(delay);
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            }
        }
        // Windows reap abandons the pre-Start I/O thread and joins VM cleanup.
        // Host executions must be waited before their process is reaped.
        if !self.core.config.vm_instance() {
            let _ = execution.wait();
        }
        execution.reap();
        drop(execution);
        if !self.core.config.vm_instance()
            && finished
                .process
                .is_some_and(|process| process.unit.is_some())
        {
            // Reaping the launcher does not prove its service's descendants
            // stopped. Keep workspace ownership until systemd confirms it.
            while self
                .core
                .backend
                .unit_alive(&self.core.config, finished.process)
                .ok()
                != Some(Some(false))
            {
                let _ = self.core.backend.cancel(&self.core.config, &finished, true);
                std::thread::park_timeout(delay);
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
        let no_start = unreleased && (cfg!(target_os = "linux") || self.core.config.vm_instance());
        if no_start {
            finished.process = None;
            finished.started_ms = None;
            finished.started = None;
            finished.next_seq = 0;
            refuse_before_start(
                &self.core.health,
                &mut finished,
                RefusalReason::WorkspaceSetupFailed,
                &error,
                Some(format!("execution gate was not opened: {reason}")),
                None,
                self.now(),
            );
            finished.state = JobState::Tombstone;
        } else {
            // A release failure may have partially opened the gate. Do not
            // promise that the command never ran without a backend proof.
            finished.terminal(Outcome::OutcomeUnknown, self.now());
        }
        finished.monitoring_error = Some(error.to_string());
        if !no_start {
            let evidence = self.execution_evidence(&finished);
            let detail = self.explained_terminal(
                &finished,
                "launch_gate_release_failed",
                &format!("{reason}: {error}"),
                &evidence,
            );
            finished.terminal = finished
                .terminal
                .take()
                .map(|t| t.with_refusal_detail(detail));
        }
        let mut delay = Duration::from_millis(50);
        loop {
            let saved = self.save(&finished);
            if saved.is_ok() || self.core.config.vm_instance() {
                let mut inner = self.core.inner.lock().unwrap();
                if saved.is_err() {
                    inner.pending_start_refusals.insert(id);
                }
                inner.jobs.insert(id, finished.clone());
                inner.starting.remove(&id);
                inner.finalizing.remove(&id);
                if finished.base {
                    self.finish_base(&mut inner, finished.request.as_ref().unwrap(), false, None)?;
                }
                return Ok(());
            }
            tracing::warn!(job_id = %id, error = %saved.unwrap_err(), "launch cleanup record failed; retrying");
            std::thread::park_timeout(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }
    fn output(&self, id: Uuid, stream: OutputStream, bytes: &[u8]) -> io::Result<()> {
        self.output_at(id, stream, bytes, None)
    }
    fn monitor_unit(&self, id: Uuid) {
        if let Err(error) = self.follow_unit(id) {
            tracing::error!(%id, %error, "durable job monitoring failed");
            // Keep the live unit's workspace reserved even if its output cannot
            // be saved. Only release it after the unit has actually stopped.
            let unit_exited = loop {
                let process = self.core.inner.lock().unwrap().jobs[&id].process;
                match self.unit_alive(process) {
                    Ok(Some(true)) | Err(_) => {
                        std::thread::park_timeout(Duration::from_millis(500))
                    }
                    Ok(alive) => break alive == Some(false),
                }
            };
            if disk::storage_full(&error) {
                let mut inner = self.core.inner.lock().unwrap();
                inner
                    .jobs
                    .get_mut(&id)
                    .unwrap()
                    .execution_evidence
                    .get_or_insert_with(Default::default)
                    .output_storage_full = true;
                drop(inner);
                self.release_disk_reserve();
            }
            let job = self.core.inner.lock().unwrap().jobs[&id].clone();
            // Replay failures do not invalidate independent exit evidence.
            let result = if job.durable_output {
                let directory = self
                    .core
                    .trees
                    .state
                    .join("sandboxes")
                    .join(id.to_string())
                    .join("output");
                self.finish_ended_unit(id, job.process, &directory, Some(&error), unit_exited)
            } else {
                self.finish_execution(
                    id,
                    Err(io::Error::other(format!("monitor_output: {error}"))),
                    unit_exited,
                )
            };
            if let Err(error) = result {
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
            format!("runner recovery: job_id={id} outcome_unknown unit={unit} unit_alive={alive}; runner will not retry the job automatically; query exec.status or exec.attach by job_id\n").as_bytes())
    }
    fn follow_unit(&self, id: Uuid) -> io::Result<()> {
        use std::io::{Seek, SeekFrom};
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
        let mut unit_exited = false;
        let mut inspect_at = std::time::Instant::now();
        #[cfg(target_os = "linux")]
        let mut polling = crate::systemd::PollBackoff::monitoring();
        loop {
            // Output is tailed frequently, but spawning systemctl for
            // every tail read would turn quiet jobs into a busy polling load.
            if std::time::Instant::now() >= inspect_at {
                let job = self.core.inner.lock().unwrap().jobs[&id].clone();
                let evidence = self.execution_evidence(&job);
                self.core
                    .inner
                    .lock()
                    .unwrap()
                    .jobs
                    .get_mut(&id)
                    .unwrap()
                    .execution_evidence = Some(evidence);
                let was_alive = alive;
                alive = match self.unit_alive(process) {
                    Ok(alive) => {
                        unit_exited = alive == Some(false);
                        alive == Some(true)
                    }
                    Err(error) => {
                        tracing::warn!(%id, %error, "cannot inspect reattached unit; retrying");
                        std::thread::park_timeout(Duration::from_millis(500));
                        continue;
                    }
                };
                #[cfg(target_os = "linux")]
                let delay = polling.next_delay();
                #[cfg(not(target_os = "linux"))]
                let delay = Duration::from_millis(500);
                inspect_at = std::time::Instant::now() + delay;
                if was_alive && !alive {
                    let kill = self.core.inner.lock().unwrap().jobs[&id].kill.clone();
                    if let Some((killed, at)) = kill {
                        tracing::info!(job_id = %id, killed = ?killed,
                            since_kill_ms = self.now().saturating_sub(at), "killed job's unit exit observed");
                    }
                }
            }
            let mut output_unavailable = false;
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
                        let mut file = crate::filesystem::read_nofollow(&directory.join(name))?;
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
                        Err(error) => {
                            output_unavailable = true;
                            tracing::warn!(%id, %error, "ended unit output unavailable; recovering exit status");
                        }
                        _ => {}
                    }
                }
            }
            if !alive {
                if durable && !output_unavailable {
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
                if durable {
                    return self.finish_ended_unit(id, process, &directory, None, unit_exited);
                }
                return self.finish_execution(
                    id,
                    Err(io::Error::other(
                        "legacy_unit_ended: no retained exit status",
                    )),
                    unit_exited,
                );
            }
            std::thread::park_timeout(Duration::from_millis(25));
        }
    }
    fn finish_unobserved_unit(&self, id: Uuid, reason: &str, unit_exited: bool) -> io::Result<()> {
        let job = self.core.inner.lock().unwrap().jobs[&id].clone();
        if job.durable_output {
            let directory = self
                .core
                .trees
                .state
                .join("sandboxes")
                .join(id.to_string())
                .join("output");
            self.finish_ended_unit(
                id,
                job.process,
                &directory,
                Some(&io::Error::other(reason.to_owned())),
                unit_exited,
            )
        } else {
            self.finish_execution(id, Err(io::Error::other(reason.to_owned())), unit_exited)
        }
    }
    fn finish_ended_unit(
        &self,
        id: Uuid,
        process: Option<platform::ProcessIdentity>,
        directory: &Path,
        monitor_error: Option<&io::Error>,
        unit_exited: bool,
    ) -> io::Result<()> {
        let (status, exit_file) = self.durable_exit_status(process, &directory.join("exit"));
        #[cfg(target_os = "linux")]
        let status = if self.unproven_pid_bootstrap_status(&status, &exit_file) {
            if let Some(detail) = self.pid_namespace_setup_failure(
                id,
                directory,
                &status,
                matches!(exit_file, Ok(None)),
            ) {
                return self.refuse_unstarted_unit(id, detail);
            }
            // An init status without its root-only report does not prove that
            // the wrapper never ran, so it must not authorize an automatic retry.
            Err(io::Error::other(format!("pid_bootstrap_unproven: Boat PID namespace bootstrap status {}: wrapper exit status and complete root-only setup evidence unavailable; execution status unknown", status.as_ref().ok().and_then(|s| s.code()).unwrap())))
        } else {
            status
        };
        #[cfg(not(target_os = "linux"))]
        let _ = exit_file;
        let status = status.map_err(|error| match monitor_error {
            Some(monitor) => io::Error::other(format!("monitor_output: {monitor}; {error}")),
            None => error,
        });
        self.finish_execution(id, status.map(backend::ExecutionOutcome::Unix), unit_exited)
    }
    #[cfg(target_os = "linux")]
    fn unproven_pid_bootstrap_status(
        &self,
        status: &io::Result<std::process::ExitStatus>,
        exit_file: &io::Result<Option<u8>>,
    ) -> bool {
        self.core.config.boat_profile
            && !self.core.config.private_pids
            && matches!(
                status.as_ref().ok().and_then(|status| status.code()),
                Some(
                    crate::isolation::pid_init::SETUP_FAILED
                        | crate::isolation::pid_init::SUPERVISOR_FAILED
                )
            )
            && !matches!(exit_file, Ok(Some(_)))
    }
    /// A Boat job's PID namespace init that fails exits 226 before the wrapper
    /// exists, so the gate byte is never read and the command never runs. It
    /// proves this by the line it wrote to the root-only setup-error file,
    /// which nothing writes once the wrapper starts. Returns that line when
    /// all of this holds and the wrapper wrote no exit code.
    #[cfg(target_os = "linux")]
    fn pid_namespace_setup_failure(
        &self,
        id: Uuid,
        directory: &Path,
        status: &io::Result<std::process::ExitStatus>,
        empty_or_missing_exit: bool,
    ) -> Option<String> {
        use crate::isolation::pid_init;
        if !self.core.config.boat_profile || self.core.config.private_pids {
            return None;
        }
        if status.as_ref().ok()?.code() != Some(pid_init::SETUP_FAILED) {
            return None;
        }
        if !empty_or_missing_exit {
            return None;
        }
        let mut text = String::new();
        crate::filesystem::read_nofollow(&directory.join(pid_init::SETUP_ERROR_FILE))
            .ok()?
            .take(4096)
            .read_to_string(&mut text)
            .ok()?;
        // A torn write, including a prefix without its step/error, is not
        // authenticated evidence of a complete bootstrap failure report.
        let line = text.split_once('\n')?.0;
        let (step, error) = line
            .strip_prefix(pid_init::SETUP_FAILURE_PREFIX)?
            .split_once(": ")?;
        if step.is_empty() || error.is_empty() {
            return None;
        }
        // Output could mean job code ran. Without proof that nothing ran,
        // leave execution unknown rather than mark the refusal retry-safe.
        let inner = self.core.inner.lock().unwrap();
        let job = inner.jobs.get(&id)?;
        (job.terminal.is_none() && job.chunks.is_empty()).then(|| line.to_owned())
    }
    /// Record a job whose unit ended before its command could start as
    /// refused before start, which callers may safely retry. Its Started
    /// record is withdrawn, as when the launch itself fails before the gate.
    #[cfg(target_os = "linux")]
    fn refuse_unstarted_unit(&self, id: Uuid, detail: String) -> io::Result<()> {
        let process = self.core.inner.lock().unwrap().jobs[&id].process;
        // Reading the result also resets the failed unit so systemd unloads
        // it, as the normal finish path does.
        let _ = self.core.backend.oom_stopped(&self.core.config, process);
        let mut finished = {
            // As in `finish_execution`: wait out a stop signalling this unit.
            let log = self.job_log(id);
            let _signalling = log.lock_signalling();
            let mut inner = self.core.inner.lock().unwrap();
            if inner.jobs[&id].terminal.is_some()
                || inner.base_finishes.contains_key(&id)
                || !inner.finalizing.insert(id)
            {
                return Ok(());
            }
            inner.jobs[&id].clone()
        };
        let safe = self.safe_refusal_detail(
            &detail,
            finished
                .request
                .as_ref()
                .into_iter()
                .flat_map(|request| request.env.values().cloned()),
        );
        finished.process = None;
        finished.started_ms = None;
        finished.started = None;
        finished.next_seq = 0;
        refuse_before_start(
            &self.core.health,
            &mut finished,
            RefusalReason::WorkspaceSetupFailed,
            &detail,
            Some(safe),
            None,
            self.now(),
        );
        finished.state = JobState::Tombstone;
        let log = self.job_log(id);
        let mut delay = Duration::from_millis(50);
        loop {
            let write = log.lock_write();
            match self.save(&finished) {
                Ok(()) => {
                    let mut inner = self.core.inner.lock().unwrap();
                    inner.jobs.insert(id, finished.clone());
                    inner.finalizing.remove(&id);
                    inner.starting.remove(&id);
                    inner.exit_statuses.remove(&id);
                    inner.ended_executions.remove(&id);
                    inner.kill_retries.remove(&id);
                    inner.cpu_times.remove(&id);
                    if finished.base {
                        self.finish_base(
                            &mut inner,
                            finished.request.as_ref().unwrap(),
                            false,
                            None,
                        )?;
                    }
                    let catalog = self.catalog_snapshot(&inner)?;
                    drop(inner);
                    return self.write_catalog(catalog);
                }
                Err(error) => {
                    tracing::warn!(job_id = %id, %error, "refusal record failed; retrying");
                    drop(write);
                    std::thread::park_timeout(delay);
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            }
        }
    }
    fn durable_exit_status(
        &self,
        process: Option<platform::ProcessIdentity>,
        path: &Path,
    ) -> (io::Result<std::process::ExitStatus>, io::Result<Option<u8>>) {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        // Observe the job-writable inode once. Classification must consume
        // this same bounded observation, never race it with a second read.
        let exit_file = (|| {
            let file = match crate::filesystem::read_nofollow(path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            let mut code = String::new();
            file.take(33).read_to_string(&mut code)?;
            if code.len() > 32 {
                return Err(io::Error::other("oversized durable exit file"));
            }
            if code.trim().is_empty() {
                return Ok(None);
            }
            code.trim()
                .parse::<u8>()
                .map(Some)
                .map_err(io::Error::other)
        })();
        let durable = match &exit_file {
            Ok(Some(code)) => {
                #[cfg(unix)]
                {
                    Ok(std::process::ExitStatus::from_raw(i32::from(*code) << 8))
                }
                #[cfg(windows)]
                {
                    Ok(std::process::ExitStatus::from_raw(u32::from(*code)))
                }
            }
            Ok(None) => Err(io::Error::other("missing or empty durable exit file")),
            Err(error) => Err(io::Error::other(error.to_string())),
        };
        let status = durable.or_else(|error| {
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
        });
        (status, exit_file)
    }
    fn output_at(
        &self,
        id: Uuid,
        stream: OutputStream,
        bytes: &[u8],
        offset: Option<u64>,
    ) -> io::Result<()> {
        let log = self.job_log(id);
        let _write = log.write_for_output();
        let directory = self.record_path(id).parent().unwrap().to_owned();
        let mut consumed = 0usize;
        for bytes_part in bytes.chunks(REPLAY_PAGE_BYTES as usize) {
            let mut job = self
                .core
                .inner
                .lock()
                .unwrap()
                .jobs
                .get(&id)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "output job expired"))?;
            let path = directory.join(format!("chunk-{}", job.next_seq));
            #[cfg(test)]
            if let Some(write) = &self.core.config.before_job_write {
                write(&path)?;
            }
            let mut file = File::create(&path)?;
            file.write_all(bytes_part)?;
            file.sync_all()?;
            job.chunks.push_back(Chunk {
                seq: job.next_seq,
                stream: stream.clone(),
                length: bytes_part.len() as u64,
            });
            job.next_seq += 1;
            job.output_bytes += bytes_part.len() as u64;
            consumed += bytes_part.len();
            if let Some(end) = offset {
                let committed_offset = end.saturating_sub((bytes.len() - consumed) as u64);
                if stream == OutputStream::Stdout {
                    job.stdout_offset = committed_offset;
                } else {
                    job.stderr_offset = committed_offset;
                }
            }
            let mut retired = Vec::new();
            while job.output_bytes > REPLAY_BYTES {
                let chunk = job.chunks.pop_front().unwrap();
                job.output_bytes -= chunk.length;
                retired.push(directory.join(format!("chunk-{}", chunk.seq)));
            }
            // The index is the commit boundary for both replay sequences and
            // durable stream offsets. A failed index save leaves an unreferenced
            // tail, never a visible cursor pointing at uncommitted data.
            self.save(&job)?;
            let previous = self.core.inner.lock().unwrap().jobs.insert(id, job);
            drop(previous);
            log.retire(retired);
        }
        Ok(())
    }
    fn retry_start_refusals(&self, inner: &mut Inner) {
        inner.pending_start_refusals.retain(|id| {
            let log = self.job_log(*id);
            let Some(_write) = log.try_lock_write() else {
                return true;
            };
            self.save(&inner.jobs[id]).is_err()
        });
    }
    #[cfg(test)]
    fn finish(&self, id: Uuid, status: io::Result<std::process::ExitStatus>) -> io::Result<()> {
        let execution_ended = status.is_ok();
        self.finish_execution(
            id,
            status.map(backend::ExecutionOutcome::Unix),
            execution_ended,
        )
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
        let log = self.job_log(id);
        let _write = log.lock_write();
        let mut inner = self.core.inner.lock().unwrap();
        let Some(job) = inner.jobs.get(&id) else {
            return Ok(());
        };
        if job.terminal.is_some() {
            if let Some(ended_ms) = job.execution_ended_ms {
                let mut updated = job.clone();
                updated.record_reservation_use(cpu_time_ns, ended_ms);
                drop(inner);
                self.save(&updated)?;
                inner = self.core.inner.lock().unwrap();
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
        execution_ended: bool,
    ) -> io::Result<()> {
        // git status over a large workspace takes a while, so it runs before
        // terminal persistence without the state lock. Claim finalization under
        // that lock so neither cancel nor the scheduler can signal the exited
        // job during collection or persistence retries. It still occupies its
        // workspace until the terminal is committed.
        let finish_started = std::time::Instant::now();
        let job = {
            // Wait out a cancel or deadline stop that is querying or
            // signalling the unit without the state lock (see
            // `JobLog::signalling`) before claiming finalization.
            let log = self.job_log(id);
            let _signalling = log.lock_signalling();
            let mut inner = self.core.inner.lock().unwrap();
            if inner.jobs[&id].terminal.is_some() {
                return Ok(());
            }
            if let Ok(status) = &status {
                inner.exit_statuses.entry(id).or_insert(*status);
            }
            if execution_ended || status.is_ok() {
                inner.ended_executions.insert(id);
            }
            if inner.base_finishes.contains_key(&id) || !inner.finalizing.insert(id) {
                return Ok(());
            }
            inner.jobs[&id].clone()
        };
        let _finalizing = Marker::new(self, move |inner| {
            inner.finalizing.remove(&id);
        });
        self.finish_step(id, &job.kill, "finalization claimed", finish_started);
        let ended_ms = self.now();
        let evidence = self.execution_evidence(&job);
        let oom_stopped = evidence.oom_killed()
            || self
                .core
                .backend
                .oom_stopped(&self.core.config, job.process)
                .unwrap_or(false);
        self.finish_step(id, &job.kill, "unit result read", finish_started);
        let memory_limit_detail = oom_stopped.then(|| {
            oom_memory_limit_detail(&self.core.config, job.weight, self.core.cpus.claim_limit())
        });
        if let Some(detail) = &memory_limit_detail {
            #[cfg(test)]
            let limit =
                crate::backend::memory_limit_gib(job.weight, self.core.config.memory_floor_gib);
            #[cfg(not(test))]
            let limit = crate::backend::memory_limit_gib(job.weight);
            let limit = self
                .core
                .config
                .boat_memory_max_bytes
                .map(|bytes| format!("{:.3}", bytes as f64 / (1_u64 << 30) as f64))
                .unwrap_or(limit);
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
        self.finish_step(id, &job.kill, "workspace changes collected", finish_started);
        let lost_status = status.as_ref().err().map(ToString::to_string);
        let mut status = status;
        // A cgroup OOM kill is a definite resource termination even when the
        // wrapper and init died before retaining their own wait status.
        #[cfg(unix)]
        if status.is_err() && oom_stopped {
            use std::os::unix::process::ExitStatusExt;
            status = Ok(backend::ExecutionOutcome::Unix(
                std::process::ExitStatus::from_raw(libc::SIGKILL),
            ));
        }
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
        // Settling a warm build waits on child processes and the disk (see
        // `clean_base`), seconds on a loaded host, so that part runs here,
        // without the state lock. Until `record_base` runs below, the build
        // still holds the writer slot, so no other warm build starts and the
        // compiler cache is not evicted meanwhile; and it has no terminal
        // yet, so no second warm build is queued for its repository.
        let base_cleanup = job.base.then(|| {
            let (fill, ledgers) = {
                let mut inner = self.core.inner.lock().unwrap();
                (
                    Self::take_cache_fill(&mut inner, id),
                    inner.fill_ledgers.remove(&id).unwrap_or_default(),
                )
            };
            self.long_work("base_cleanup");
            let mut cleanup = self.clean_base(id, fill);
            cleanup.publication = job.fill_identity.clone().map(|identity| {
                let outcome = self.fill_publication(&identity, id, &ledgers);
                (identity, outcome)
            });
            cleanup
        });
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
                let mirror = self.core.trees.mirror(&r.repository_root);
                let (steps, declared) = if self.core.config.base_commands.is_empty() {
                    workspace::warm_steps(&mirror, &r.snapshot_commit)
                } else {
                    (self.core.config.base_commands.clone(), false)
                };
                let (command, declared) = (steps.join(" && "), declared);
                let overridden = !self.core.config.base_commands.is_empty();
                let _ = declared;
                self.fill_identity(&mirror, &r.snapshot_commit, &command, overridden)
                    .0
                    .ok()
            } else {
                None
            };
        if let Some(exit) = self.core.inner.lock().unwrap().exit_statuses.get(&id) {
            status = Ok(*exit);
        }
        if let Err(error) = &status {
            if let Err(write_error) = self.output(
                id,
                OutputStream::Stderr,
                format!(
                    "runner monitoring: job_id={id} outcome_unknown error={error}; runner will not retry the job automatically\n"
                )
                .as_bytes(),
            ) {
                tracing::warn!(%id, %write_error, "monitoring error retained in job record; replay diagnostic unavailable");
            }
        }
        let return_result = job
            .request
            .as_ref()
            .filter(|r| !r.return_paths.is_empty() && !r.return_chunked)
            .map(|request| {
                let seq = self.core.inner.lock().unwrap().jobs[&id].next_seq;
                crate::return_paths::collect(request, seq)
            });
        let job_directory = self.record_path(id).parent().unwrap().to_owned();
        let returned = job
            .request
            .as_ref()
            .filter(|r| !r.return_paths.is_empty() && r.return_chunked)
            .map(|request| crate::return_paths::collect_chunked(request, &job_directory));
        self.finish_step(id, &job.kill, "returned files collected", finish_started);
        let log = self.job_log(id);
        // The last persistence error, logged at WARN only when it changes.
        let mut failing: Option<String> = None;
        let (mut inner, success) = loop {
            let write = log.lock_write();
            let mut inner = self.core.inner.lock().unwrap();
            self.finish_step(
                id,
                &job.kill,
                "job log and state locks acquired",
                finish_started,
            );
            if inner.jobs[&id].terminal.is_some() {
                if let Some(returned) = &returned {
                    // Another finisher committed the terminal; these bytes
                    // were never referenced by the record.
                    let _ = fs::remove_dir_all(job_directory.join(&returned.directory));
                }
                return Ok(());
            }
            // A monitor that lost the claim can still supply the real status.
            // Check it on every attempt and after disk I/O, before exposing the
            // terminal snapshot to replay readers.
            if let Some(exit) = inner.exit_statuses.get(&id) {
                status = Ok(*exit);
            }
            let success = lock_check.is_ok()
                && writer_stats.is_ok()
                && status.as_ref().is_ok_and(|s| s.success());
            let monitoring_error = status.as_ref().err().map(ToString::to_string);
            #[cfg(unix)]
            let macos_cancelled =
                self.core.config.macos_instance() && self.core.config.macos_runtime.cancelled(id);
            #[cfg(not(unix))]
            let macos_cancelled = false;
            let outcome = match &status {
                Ok(s) => s.wire(),
                Err(_) if macos_cancelled => Outcome::Cancelled,
                Err(_) => Outcome::OutcomeUnknown,
            };
            let mut finished = inner.jobs[&id].clone();
            #[cfg(unix)]
            if self.core.config.macos_instance()
                && finished.kill.is_none()
                && self.core.config.macos_runtime.cancelled(id)
            {
                finished.kill = Some((Killed::Cancel, self.now()));
            }
            if let Some(mut result) = return_result.clone() {
                // Save the bounded collection result with the command's finished
                // state so replay cannot expose one without the other after restart.
                crate::return_paths::set_seq(
                    &mut result,
                    finished.next_seq,
                    job.request.as_ref().unwrap().return_max_bytes,
                );
                finished.next_seq += 1;
                finished.return_result = Some(result);
            }
            if let Some(returned) = &returned {
                // Chunk bytes are already synced; this save commits their
                // seqs, the manifest and the terminal together.
                finished.returned = Some(returned.commit(id, &mut finished.next_seq));
            }
            finished.execution_ended_ms = Some(ended_ms);
            if let Some(cpu_time_ns) = inner.cpu_times.get(&id) {
                finished.record_reservation_use(*cpu_time_ns, ended_ms);
            }
            if inner.ended_executions.contains(&id) {
                finished.terminal_ended(outcome, self.now());
            } else {
                finished.terminal(outcome, self.now());
            }
            finished.monitoring_error = monitoring_error.clone();
            let explanation = if let Some(error) = &monitoring_error {
                Some(self.explained_terminal(&job, "exit_evidence_unavailable", error, &evidence))
            } else if oom_stopped {
                Some(self.explained_terminal(
                    &job,
                    "out_of_memory",
                    &format!(
                        "out of memory: the job ran; a cgroup process was OOM-killed; {}",
                        lost_status.as_deref().unwrap_or("retained exit status")
                    ),
                    &evidence,
                ))
            } else if evidence.disk_full() {
                Some(self.explained_terminal(
                    &job,
                    "disk_full",
                    "disk full: the job ran; retained exit status is authoritative",
                    &evidence,
                ))
            } else if self.core.config.boat_profile {
                Some(self.explained_terminal(
                    &job,
                    "retained_exit",
                    "retained job exit status",
                    &evidence,
                ))
            } else {
                None
            };
            if let Some(detail) = explanation {
                finished.terminal = finished
                    .terminal
                    .take()
                    .map(|t| t.with_refusal_detail(detail));
            }
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
            // A job that is not a warm build, and whose workspace is in the
            // catalog, updates that workspace's last use (and any warm-cache
            // miss). The catalog is written before the terminal record, and
            // the terminal is published only after both, so at every point a
            // crash leaves the catalog at least as new as the terminal record:
            // startup never finds a finished job whose workspace looks unused
            // since before it ran. A catalog ahead of an unfinished job only
            // keeps its workspace longer. Either save failing retries both,
            // with the job still being finalized; repeating the update only
            // moves the last-use time forward. Both saves run without the
            // state lock.
            //
            // Every job also counts as a use of the shared compiler cache,
            // which jobs mount read-only so nothing on disk shows their reads;
            // disk eviction ages the cache by this time. A warm build's catalog
            // is saved with its result, below.
            inner.catalog.compile_cache_used_ms = Some(self.now());
            let catalog = if job.base {
                Ok(None)
            } else {
                self.note_finished_workspace(
                    &mut inner,
                    job.request.as_ref().unwrap(),
                    reader_descriptor.as_ref(),
                );
                self.catalog_snapshot(&inner).map(Some)
            };
            drop(inner);
            let saved = catalog
                .and_then(|catalog| catalog.map_or(Ok(()), |catalog| self.write_catalog(catalog)))
                .and_then(|()| self.save(&finished));
            self.finish_step(id, &job.kill, "terminal record written", finish_started);
            inner = self.core.inner.lock().unwrap();
            let terminal = finished.terminal.as_ref().unwrap();
            if saved.is_ok()
                && (inner
                    .exit_statuses
                    .get(&id)
                    .is_some_and(|exit| exit.wire() != terminal.outcome)
                    || (inner.ended_executions.contains(&id) && !terminal.execution_ended()))
            {
                continue;
            }
            match saved {
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
                    if !job.base {
                        inner.finalizing.remove(&id);
                    }
                    inner.exit_statuses.remove(&id);
                    inner.ended_executions.remove(&id);
                    inner.cpu_times.remove(&id);
                    if failing.is_some() {
                        tracing::info!(%id, "terminal persisted after failed attempts");
                    }
                    break (inner, success);
                }
                Err(error) => {
                    let message = error.to_string();
                    if failing.as_ref() != Some(&message) {
                        tracing::warn!(%id, %error, "terminal persistence failed; the job stays finalizing and is retried");
                        failing = Some(message);
                    } else {
                        tracing::debug!(%id, %error, "terminal persistence failed again");
                    }
                    // A job that used up the disk on its own leaves no room
                    // for its terminal record, and nothing else frees space
                    // while it stays unfinished. The reserve exists for this.
                    if disk::storage_full(&error) {
                        self.release_disk_reserve();
                    }
                    // Backoff belongs to this job's monitor, never the scheduler
                    // or the state lock used by admission and other jobs.
                    drop(inner);
                    drop(write);
                    std::thread::park_timeout(delay);
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            }
        };
        let kill = job.kill.clone();
        let job = &inner.jobs[&id];
        let request = job.request.as_ref().unwrap().clone();
        let base = job.base;
        tracing::info!(job_id = %id, outcome = ?job.terminal.as_ref().unwrap().outcome, base,
            finish_ms = finish_started.elapsed().as_millis() as u64, "job finished");
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
            // The warm build's result reaches the catalog with its terminal,
            // under this lock hold; the catalog is saved, and the next warm
            // build is planned from the mirror, after it is released.
            let cleanup = base_cleanup.expect("a warm build is cleaned up before its terminal");
            if cleanup.scratch.is_err() {
                inner.base_finishes.entry(id).or_insert_with(|| BaseFinish {
                    request,
                    success,
                    failure: (!fill_failure.is_empty()).then(|| fill_failure.join("; ")),
                    recorded: false,
                    cleanup: Some(cleanup),
                    terminal: None,
                });
                let spawned = self.start_base_finish(&mut inner, id);
                drop(inner);
                spawned?;
                return Ok(());
            }
            self.record_base(
                &mut inner,
                &request,
                success,
                (!fill_failure.is_empty()).then(|| fill_failure.join("; ")),
                cleanup,
            )?;
            let catalog = self.catalog_snapshot(&inner)?;
            let next = inner.catalog.repositories[&request.repository_root]
                .desired
                .clone()
                .filter(|desired| {
                    self.warm_builds_enabled(&desired.tip) && inner.drain_deadline.is_none()
                });
            drop(inner);
            self.finish_step(id, &kill, "state lock released", finish_started);
            self.write_catalog(catalog)?;
            if let Some(desired) = next {
                self.queue_next_base(&request.repository_root, &desired)?;
            }
        } else {
            drop(inner);
            // Replay waits for the state lock, so the terminal reaches clients
            // only after this point.
            self.finish_step(id, &kill, "state lock released", finish_started);
        }
        self.prune_refs()?;
        Ok(())
    }

    /// Note in the catalog that a finished job (not a warm build) last used
    /// its workspace now, and handle a warm-cache miss it reported. A
    /// workspace the catalog does not know is left alone.
    fn note_finished_workspace(
        &self,
        inner: &mut Inner,
        request: &Run,
        reader_descriptor: Option<&String>,
    ) {
        let Some(w) = inner.catalog.workspaces.get_mut(&request.workspace_key) else {
            return;
        };
        w.used_ms = self.now();
        if let Some(descriptor) = reader_descriptor {
            if let Some(g) = inner.catalog.repositories.get_mut(&request.repository_root) {
                // Aggregate misses cannot identify individual units. Restrict
                // refills to matching graphs and one attempt per approved tip
                // so a feature/profile the warm command omits cannot loop.
                if let Some(tip) = g.desired.as_ref().map(|d| &d.tip) {
                    if g.filled_descriptor.as_ref() == Some(descriptor)
                        && g.miss_refills.insert(format!("{tip}:{descriptor}"))
                    {
                        if let Some(filled) = g.filled_descriptor.take() {
                            g.filled_descriptors.remove(&filled);
                        }
                    }
                }
            }
        }
    }

    /// Log one step of finishing a job, with the time since finishing began.
    /// A job stopped by a cancel or deadline logs at INFO, with the time since
    /// that kill was recorded, so the interval between a kill and its
    /// terminal can be split into steps; other jobs log at DEBUG.
    fn finish_step(
        &self,
        id: Uuid,
        kill: &Option<(Killed, u64)>,
        step: &'static str,
        started: std::time::Instant,
    ) {
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match kill {
            Some((killed, at)) => tracing::info!(
                job_id = %id,
                killed = ?killed,
                step,
                elapsed_ms,
                since_kill_ms = self.now().saturating_sub(*at),
                "job finishing step"
            ),
            None => tracing::debug!(job_id = %id, step, elapsed_ms, "job finishing step"),
        }
    }

    /// Schedule settlement of a warm build that already has its terminal.
    /// Called under the state lock; the worker drains sockets, reads stats,
    /// cleans scratch and saves the catalog without engine locks. The writer
    /// slot stays reserved through cleanup. Normal completion performs the
    /// same unlocked work in `finish_execution` before publishing its terminal.
    fn finish_base(
        &self,
        inner: &mut Inner,
        r: &Run,
        success: bool,
        failure: Option<String>,
    ) -> io::Result<()> {
        inner
            .base_finishes
            .entry(r.job_id)
            .or_insert_with(|| BaseFinish {
                request: r.clone(),
                success,
                failure,
                recorded: false,
                cleanup: None,
                terminal: None,
            });
        self.start_base_finish(inner, r.job_id)
    }

    /// Claim a pending settlement while holding state, then do its work on a
    /// separate thread. No cleanup resources move until that thread starts.
    fn start_base_finish(&self, inner: &mut Inner, id: Uuid) -> io::Result<()> {
        if !inner.base_settling.insert(id) {
            return Ok(());
        }
        let runner = self.clone();
        let spawned = self.spawn_setup(move || {
            if let Err(error) = runner.settle_base(id) {
                tracing::warn!(job_id = %id, %error, "warm build settlement will be retried");
            }
        });
        if spawned.is_err() {
            inner.base_settling.remove(&id);
        }
        spawned
    }

    fn settle_base(&self, id: Uuid) -> io::Result<()> {
        let marker = Marker::new(self, move |inner| {
            if let Some(pending) = inner.base_finishes.get_mut(&id) {
                if !pending.recorded {
                    // An interrupted worker may have consumed its write ledger
                    // or ACL result. Retrying must not turn missing evidence
                    // into a successful publication.
                    pending.success = false;
                    pending.failure.get_or_insert_with(|| {
                        "fill settlement was interrupted before recording its result".into()
                    });
                }
            }
            inner.base_settling.remove(&id);
        });
        let (request, success, failure, recorded, prepared, terminal) = {
            let mut inner = self.core.inner.lock().unwrap();
            let pending = inner.base_finishes.get_mut(&id).unwrap();
            (
                pending.request.clone(),
                pending.success,
                pending.failure.clone(),
                pending.recorded,
                pending.cleanup.take(),
                pending.terminal.clone(),
            )
        };
        if let Some(terminal) = terminal {
            let mut delay = Duration::from_millis(50);
            loop {
                self.long_work("base_terminal_save");
                match self.save(&terminal) {
                    Ok(()) => break,
                    Err(error) => {
                        tracing::warn!(job_id = %id, %error, "fill setup terminal save will be retried");
                        std::thread::park_timeout(delay);
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                }
            }
            let mut inner = self.core.inner.lock().unwrap();
            inner.jobs.insert(id, terminal);
            inner.base_finishes.get_mut(&id).unwrap().terminal = None;
        }
        if !recorded {
            let (fill, ledgers, identity) = {
                let mut inner = self.core.inner.lock().unwrap();
                (
                    Self::take_cache_fill(&mut inner, id),
                    inner.fill_ledgers.remove(&id).unwrap_or_default(),
                    inner.jobs[&id].fill_identity.clone(),
                )
            };
            self.long_work("base_cleanup");
            let mut cleanup = prepared.unwrap_or_else(|| self.clean_base(id, fill));
            let mut delay = Duration::from_millis(50);
            for _ in 1..BASE_SCRATCH_CLEANUP_ATTEMPTS {
                let Err(error) = &cleanup.scratch else {
                    break;
                };
                tracing::warn!(job_id = %id, %error, "warm build scratch cleanup will be retried");
                std::thread::park_timeout(delay);
                delay = (delay * 2).min(Duration::from_secs(5));
                self.long_work("base_scratch_retry");
                cleanup.scratch = self.clean_base(id, None).scratch;
            }
            if cleanup.publication.is_none() {
                cleanup.publication = identity.map(|identity| {
                    let outcome = self.fill_publication(&identity, id, &ledgers);
                    (identity, outcome)
                });
            }
            let mut inner = self.core.inner.lock().unwrap();
            self.record_base(&mut inner, &request, success, failure, cleanup)?;
            // Re-running record_base would count the same failed build twice.
            inner.base_finishes.get_mut(&id).unwrap().recorded = true;
        }
        let mut delay = Duration::from_millis(50);
        loop {
            let catalog = self.catalog_snapshot(&self.core.inner.lock().unwrap())?;
            match self.write_catalog(catalog) {
                Ok(()) => break,
                Err(error) => {
                    tracing::warn!(job_id = %id, %error, "warm build catalog save will be retried");
                    std::thread::park_timeout(delay);
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            }
        }
        let next = {
            let mut inner = self.core.inner.lock().unwrap();
            inner.base_finishes.remove(&id);
            marker.clear(&mut inner);
            inner.catalog.repositories[&request.repository_root]
                .desired
                .clone()
        };
        if let Some(next) = next {
            self.queue_next_base(&request.repository_root, &next)?;
        }
        Ok(())
    }

    /// Take the cache fill begun when warm build `id` was dispatched, if any,
    /// to finish it with `clean_base`.
    fn take_cache_fill(inner: &mut Inner, id: Uuid) -> Option<CacheFill> {
        #[cfg(target_os = "linux")]
        return inner.cache_fills.remove(&id);
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (inner, id);
            None
        }
    }

    /// The parts of settling warm build `id` that wait on child processes and
    /// the disk. Call without engine locks: finishing the cache fill repairs the ACL of
    /// each cache entry the build changed, one setfacl each, and the build's
    /// scratch directory is moved to the trash, after (when Linux job
    /// isolation is on) a recursive setfacl restores the runner's access.
    fn clean_base(&self, id: Uuid, fill: Option<CacheFill>) -> BaseCleanup {
        #[cfg(target_os = "linux")]
        let fill = fill.map(CacheFill::finish);
        #[cfg(not(target_os = "linux"))]
        let fill = fill.map(|fill| match fill {});
        let scratch = self.core.config.writer_scratch_dir().join(id.to_string());
        let scratch = (|| {
            if scratch.exists() {
                #[cfg(target_os = "linux")]
                if self.core.config.isolate_jobs {
                    crate::isolation::restore_runner_access(&scratch)?;
                }
                self.discard(&scratch)?;
            }
            Ok(())
        })();
        BaseCleanup {
            fill,
            scratch,
            publication: None,
        }
    }

    /// Record a settled warm build in memory, from `clean_base`'s results:
    /// give back the writer slot and record the fill's result in the
    /// catalog. The caller saves the catalog. After bounded cleanup retries,
    /// a scratch error fails the attempt but releases the writer; the leftover
    /// directory remains for startup garbage collection.
    fn record_base(
        &self,
        inner: &mut Inner,
        r: &Run,
        success: bool,
        failure: Option<String>,
        cleanup: BaseCleanup,
    ) -> io::Result<()> {
        let mut failure = failure;
        let success = if let Some(Err(error)) = cleanup.fill {
            let detail = format!("cache ACL repair/tracking: {error}");
            failure = Some(failure.map(|f| format!("{f}; {detail}")).unwrap_or(detail));
            false
        } else {
            success
        };
        let success = if let Err(error) = cleanup.scratch {
            let detail = format!("scratch cleanup deferred to garbage collection: {error}");
            failure = Some(
                failure
                    .map(|previous| format!("{previous}; {detail}"))
                    .unwrap_or(detail),
            );
            false
        } else {
            success
        };
        if inner.writer == Some(r.job_id) {
            inner.writer = None;
        }
        let descriptor = inner.jobs[&r.job_id].writer_descriptor.clone();
        let mut fill_failure_detail = None;
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
                g.filled_descriptor = descriptor.clone();
                g.finished_ms = Some(self.now());
                g.commit = g.desired.as_ref().map(|d| d.tip.clone());
                g.result = Some(RebuildResult::Ok);
            } else if g.result != Some(RebuildResult::Ok) {
                // A newer descriptor can be queued while this fill runs. Its
                // status stays building/failed, but this success still becomes
                // the published fallback. Do not replace a newer reused tip.
                g.filled_descriptor = descriptor.clone();
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
            fill_failure_detail = Some(detail.clone());
            g.failure_detail = Some(detail);
            // A fill that failed as a fill keeps today's hold on its descriptor.
            // With a publication record, the publish-failure marker below is what
            // holds it, and after the retry cap leaves it to this same hold.
            g.failed_descriptor = descriptor.clone();
            if g.requested_descriptor == descriptor {
                g.result = Some(RebuildResult::Failed);
            }
        }
        // Drain and statistics collection run without the state lock. After
        // reacquiring it, recheck the identity so a provisioning change
        // cannot install an old store's publication after forgetting it.
        if let (Some(descriptor), Some((identity, mut outcome))) = (descriptor, cleanup.publication)
        {
            let started_ms = inner.jobs[&r.job_id]
                .fill_started_ms
                .unwrap_or_else(|| self.now());
            if self.publishing_identity().as_ref() == Some(&identity) {
                if !success {
                    outcome = publication::Outcome::Failed {
                        reason: fill_failure_detail
                            .unwrap_or_else(|| "the fill failed as a build".into()),
                    };
                }
                let g = inner
                    .catalog
                    .repositories
                    .get_mut(&r.repository_root)
                    .unwrap();
                publication::record(
                    g,
                    &descriptor,
                    started_ms,
                    &outcome,
                    Some(&identity),
                    success,
                );
                match &outcome {
                    publication::Outcome::Published { keys } => tracing::info!(
                        job_id = %r.job_id,
                        repository = %r.repository_root,
                        %descriptor,
                        keys = keys.len(),
                        "remote cache fill published"
                    ),
                    publication::Outcome::Failed { reason } => tracing::warn!(
                        job_id = %r.job_id,
                        repository = %r.repository_root,
                        %descriptor,
                        reason = %reason,
                        "remote cache fill did not publish"
                    ),
                }
            }
        }
        Ok(())
    }

    /// Whether fill `id` published every entry it wrote, and which keys it
    /// wrote, or why it did not.
    ///
    /// The decision reads the write sockets the gateway served this fill: it
    /// waits until no PUT is in flight, then fails the fill if any PUT was
    /// refused, answered by a breaker or limit, given a non-2xx upstream
    /// status, or ended before the store's 2xx; and if any PUT was signed under
    /// a store identity other than the one the fill started with, or that
    /// identity is no longer the active one. The fill must also have run
    /// successfully, since a fill that failed as a build wrote only part of
    /// what it would have.
    ///
    /// Only the tools the plan ran decide the result: statistics of another
    /// tool on the host matter neither way. GET and HEAD outcomes, such as the
    /// `.sccache_check` probe on an empty prefix, never affect it.
    fn fill_publication(
        &self,
        identity: &StoreIdentity,
        id: Uuid,
        ledgers: &[Arc<crate::remote_cache::gateway::WriteLedger>],
    ) -> publication::Outcome {
        self.long_work("publication_drain");
        let (mut keys, mut reason): (Vec<PublishedKey>, Option<String>) = (Vec::new(), None);
        for ledger in ledgers {
            if let Err(error) = ledger.drained() {
                reason = reason.or(Some(error));
            }
            let summary = ledger.summary();
            if summary.in_flight > 0 {
                reason = reason.or(Some(format!(
                    "{} {} write(s) still in flight",
                    summary.in_flight,
                    ledger.tool().id()
                )));
            }
            if summary.puts_failed > 0 {
                reason = reason.or(Some(format!(
                    "{} {} write(s) failed",
                    summary.puts_failed,
                    ledger.tool().id()
                )));
            }
            if summary
                .identities
                .iter()
                .any(|signed| signed.recorded() != *identity)
            {
                reason = reason.or_else(|| {
                    Some(format!(
                        "{} was signed for another store",
                        ledger.tool().id()
                    ))
                });
            }
            for key in summary.key_sample {
                keys.push(PublishedKey {
                    tool: ledger.tool().id().to_string(),
                    key,
                });
            }
        }
        // The tools the plan ran are the ones with a write socket. Their final
        // statistics must exist and report no remote write failure; a tool on
        // the host that the plan did not run has no socket and so decides
        // nothing.
        for ledger in ledgers {
            let statistics = match ledger.tool() {
                crate::remote_cache::gateway::Tool::Sccache => {
                    crate::writer::fill::sccache_write_failures(&self.core.trees.state, id)
                }
                crate::remote_cache::gateway::Tool::Ccache => {
                    crate::writer::fill::ccache_write_failures(&self.core.trees.state, id)
                }
            };
            match statistics {
                Ok(0) => {}
                Ok(failures) => {
                    reason = reason.or(Some(format!(
                        "{} reported {} remote write failure(s)",
                        ledger.tool().id(),
                        failures
                    )));
                }
                Err(error) => {
                    reason = reason.or(Some(format!("{} statistics: {error}", ledger.tool().id())));
                }
            }
        }
        match reason {
            None => publication::Outcome::Published { keys },
            Some(reason) => publication::Outcome::Failed { reason },
        }
    }

    /// Queue the warm build that repository `root` wants after one of its
    /// warm builds finished, as `queue_base` does, but plan it from the
    /// mirror before taking the state lock. `desired` is the build the repository wanted
    /// when the finished one was settled; if it wants another by the time
    /// the lock is taken, nothing is queued here (the request that changed
    /// it queues that one, or the scheduler's next pass does).
    fn queue_next_base(&self, root: &str, desired: &BaseRequest) -> io::Result<()> {
        self.long_work("base_plan");
        let plan = self.base_plan(root, &desired.tip, true);
        let mut inner = self.core.inner.lock().unwrap();
        let current = inner
            .catalog
            .repositories
            .get(root)
            .and_then(|repository| repository.desired.as_ref());
        if !current.is_some_and(|current| {
            current.tip == desired.tip && current.workspace == desired.workspace
        }) {
            return Ok(());
        }
        if !self.queue_base_planned(&mut inner, root, &desired.workspace, plan)? {
            return Ok(());
        }
        let catalog = self.catalog_snapshot(&inner)?;
        drop(inner);
        self.write_catalog(catalog)
    }
}

/// The `waiting_on` value for a job whose own workspace has no job of its own
/// running, but whose directory jobs in other workspaces have checked out in
/// place as a sibling. It is kept apart from `workspace owner`, which means a
/// job in this same workspace, so a client can tell an earlier job of its own
/// from another repository's job that uses this checkout.
const SIBLING_USER_WAIT: &str = "sibling user of this checkout";

/// Build from retained state once per dispatch, then update only the selected
/// job's weight, checkout reservations and CPU claims after each start.
#[derive(Default)]
struct DispatchIndex {
    weight: u32,
    foreground_pending: usize,
    /// For each repository root, the `order` of its most recently accepted
    /// foreground job, retained records included. The next cache fill is the
    /// one for the repository whose jobs arrived most recently.
    newest_arrivals: BTreeMap<String, u64>,
    workspaces: BTreeSet<String>,
    sibling_paths: BTreeSet<String>,
    running_paths: BTreeSet<String>,
    reserved_paths: BTreeSet<String>,
    running_siblings: BTreeMap<String, BTreeSet<String>>,
    reserved_siblings: BTreeMap<String, BTreeSet<String>>,
    claims: BTreeMap<Uuid, Vec<u32>>,
    queued: BTreeMap<String, Vec<Uuid>>,
    /// Current running/setup holder and permit weight for each exact workspace.
    workspace_owners: BTreeMap<String, (Uuid, u32)>,
    /// Running/setup jobs that check out each sibling path, with their permit
    /// weights. A sibling is checked out in place at its canonical path, so
    /// these jobs also hold any workspace at an overlapping path. With
    /// snapshot siblings only running legacy sibling users are listed (see
    /// `Runner::legacy_sibling_user`).
    sibling_users: BTreeMap<String, BTreeMap<Uuid, u32>>,
    /// Whether siblings are checked out in place, so that sibling users and
    /// main-workspace jobs of one path exclude each other.
    in_place: bool,
}

impl DispatchIndex {
    fn build(inner: &Inner) -> Self {
        Self::build_without(inner, &BTreeSet::new())
    }
    /// The index as it would be if the `excluded` running or setup jobs had
    /// finished, their setup reservations included.
    fn build_without(inner: &Inner, excluded: &BTreeSet<Uuid>) -> Self {
        let mut index = Self {
            in_place: inner.sibling_fences == crate::sibling_snapshots::SiblingFences::InPlace,
            ..Self::default()
        };
        let mut excluded_reservations = BTreeSet::new();
        for job in Runner::scheduler_jobs(inner) {
            if excluded.contains(&job.id) {
                if inner.starting.contains(&job.id) {
                    excluded_reservations.insert(job.key().to_owned());
                }
                continue;
            }
            if job.state == JobState::Running || inner.starting.contains(&job.id) {
                index.weight = index.weight.saturating_add(job.weight);
                index
                    .workspace_owners
                    .insert(job.key().to_owned(), (job.id, job.weight));
                if index.in_place || Runner::legacy_sibling_user(inner, job) {
                    index.add_sibling_user(job);
                }
            }
            if !job.base && job.pending() {
                index.foreground_pending += 1;
            }
            if !job.claimed_cpus().is_empty() {
                index.claims.insert(job.id, job.claimed_cpus().to_vec());
            }
            if job.state == JobState::Queued && !inner.starting.contains(&job.id) {
                index
                    .queued
                    .entry(job.key().to_owned())
                    .or_default()
                    .push(job.id);
            }
            if job.state == JobState::Running {
                index.workspaces.insert(job.key().to_owned());
                if let Some(request) = &job.request {
                    index.running_paths.insert(if job.base {
                        request.cwd.clone()
                    } else {
                        request.workspace_key.clone()
                    });
                    for sibling in &request.siblings {
                        if index.in_place || Runner::legacy_sibling_user(inner, job) {
                            index.sibling_paths.insert(sibling.canonical_path.clone());
                        }
                        if index.in_place {
                            index
                                .running_siblings
                                .entry(sibling.canonical_path.clone())
                                .or_default()
                                .insert(sibling.commit.clone());
                        }
                    }
                }
            }
        }
        for (key, reservation) in &inner.reserved {
            if !excluded_reservations.contains(key) {
                index.reserve(key, &reservation.siblings);
            }
        }
        for job in inner.jobs.values().filter(|job| !job.base) {
            if let Some(request) = &job.request {
                let newest = index
                    .newest_arrivals
                    .entry(request.repository_root.clone())
                    .or_default();
                *newest = (*newest).max(job.order);
            }
        }
        for queue in index.queued.values_mut() {
            queue.sort_by_key(|id| {
                let job = &inner.jobs[id];
                (!job.gate_job(), job.order)
            });
        }
        index
    }
    fn reserve(&mut self, key: &str, siblings: &[SiblingSnapshot]) {
        self.workspaces.insert(key.to_owned());
        self.reserved_paths.insert(key.to_owned());
        // A setup in flight in snapshot mode never touches a sibling's path.
        if !self.in_place {
            return;
        }
        for sibling in siblings {
            self.sibling_paths.insert(sibling.canonical_path.clone());
            self.reserved_siblings
                .entry(sibling.canonical_path.clone())
                .or_default()
                .insert(sibling.commit.clone());
        }
    }
    fn add_sibling_user(&mut self, job: &Job) {
        for sibling in job.request.iter().flat_map(|request| &request.siblings) {
            self.sibling_users
                .entry(sibling.canonical_path.clone())
                .or_default()
                .insert(job.id, job.weight);
        }
    }
    fn overlap(a: &str, b: &str) -> bool {
        Path::new(a).starts_with(b) || Path::new(b).starts_with(a)
    }
    /// Running/setup jobs that hold `key` because one of their sibling
    /// checkouts overlaps it, with their permit weights.
    fn checkout_sibling_users(&self, key: &str) -> BTreeMap<Uuid, u32> {
        self.sibling_users
            .iter()
            .filter(|(path, _)| Self::overlap(key, path))
            .flat_map(|(_, users)| users.iter().map(|(id, weight)| (*id, *weight)))
            .collect()
    }
    fn occupied(&self, key: &str) -> bool {
        self.workspaces.contains(key)
            || self
                .sibling_paths
                .iter()
                .any(|path| Self::overlap(key, path))
    }
    fn siblings_blocked(
        &self,
        siblings: &[SiblingSnapshot],
        catalog: &BTreeMap<String, String>,
    ) -> bool {
        // Snapshot siblings never share a checkout with anything.
        self.in_place
            && siblings.iter().any(|sibling| {
                self.running_paths
                    .iter()
                    .chain(&self.reserved_paths)
                    .any(|path| Self::overlap(path, &sibling.canonical_path))
                    || self
                        .running_siblings
                        .get(&sibling.canonical_path)
                        .is_some_and(|commits| {
                            commits.iter().any(|commit| commit != &sibling.commit)
                        })
                    || self
                        .reserved_siblings
                        .get(&sibling.canonical_path)
                        .is_some_and(|commits| {
                            catalog.get(&sibling.canonical_path) != Some(&sibling.commit)
                                || commits.iter().any(|commit| commit != &sibling.commit)
                        })
            })
    }
    /// Whether a queued workspace job holds `job` back from starting.
    ///
    /// A sibling is checked out in place at its canonical path, the same host
    /// directory a job whose main workspace is that path uses, so the two
    /// cannot run together. Jobs that use the path as a sibling at one commit
    /// may overlap one another, so under steady load they would keep the
    /// directory busy and the main workspace job would never start. The next
    /// job to start on a workspace therefore holds back every job that
    /// declares an overlapping sibling and was accepted after it. Holding
    /// only later arrivals keeps the waits acyclic: two repositories that
    /// name each other as siblings cannot hold each other back. Cache-fill
    /// jobs and jobs that can never fit the runner do not hold anyone. With
    /// snapshot siblings there is no shared checkout, so nobody is held.
    fn held_by_queued_workspace(&self, inner: &Inner, job: &Job, max_weight: u32) -> bool {
        if !self.in_place {
            return false;
        }
        let Some(request) = job.request.as_ref() else {
            return false;
        };
        if request.siblings.is_empty() {
            return false;
        }
        self.queued.iter().any(|(key, queue)| {
            let Some(head) = queue.first().map(|id| &inner.jobs[id]) else {
                return false;
            };
            head.id != job.id
                && !head.base
                && head.weight <= max_weight
                && head.order < job.order
                && request
                    .siblings
                    .iter()
                    .any(|sibling| Self::overlap(key, &sibling.canonical_path))
        })
    }
    /// The head of each workspace queue, in the order dispatch tries them:
    /// gates; then a cache fill, for the repository whose foreground jobs
    /// arrived most recently (there is one writer slot, so at most one of
    /// them starts); then heads that have waited past the backfill grace;
    /// then the rest, rotating between workspaces.
    fn heads(&self, inner: &Inner, now: u64) -> Vec<Uuid> {
        let mut heads: Vec<_> = self
            .queued
            .values()
            .filter_map(|queue| queue.first())
            .map(|id| {
                let job = &inner.jobs[id];
                if job.gate_job() {
                    (0, job.order, 0, *id)
                } else if job.base {
                    let newest = job
                        .request
                        .as_ref()
                        .and_then(|request| self.newest_arrivals.get(&request.repository_root))
                        .copied()
                        .unwrap_or(0);
                    (1, u64::MAX - newest, job.order, *id)
                } else if now.saturating_sub(job.queued_ms.unwrap_or(job.accepted_ms))
                    >= BACKFILL_GRACE_MS
                {
                    (2, job.order, 0, *id)
                } else {
                    (
                        3,
                        inner.rotation.get(job.key()).copied().unwrap_or(0),
                        job.order,
                        *id,
                    )
                }
            })
            .collect();
        heads.sort();
        heads.into_iter().map(|(_, _, _, id)| id).collect()
    }
    fn started(&mut self, inner: &Inner, id: Uuid) {
        let job = &inner.jobs[&id];
        let key = job.key();
        let queue = self.queued.get_mut(key).unwrap();
        queue.retain(|queued| *queued != id);
        if queue.is_empty() {
            self.queued.remove(key);
        }
        if !job.base && !job.pending() {
            self.foreground_pending -= 1;
        }
        if job.claimed_cpus().is_empty() {
            self.claims.remove(&id);
        } else {
            self.claims.insert(id, job.claimed_cpus().to_vec());
        }
        if inner.starting.contains(&id) {
            self.weight = self.weight.saturating_add(job.weight);
            self.workspace_owners
                .insert(key.to_owned(), (job.id, job.weight));
            // A job just started is set up by this runner, so it is never a
            // legacy sibling user.
            if self.in_place {
                self.add_sibling_user(job);
            }
            let request = job.request.as_ref().unwrap();
            self.reserve(key, &request.siblings);
        }
    }
}
