//! Execution-specific work lives here; scheduling, persistence and transport
//! remain in the engine. A backend must never fall back to host execution.
use crate::{
    caller_path as paths,
    engine::Config,
    platform,
    storage::*,
    workspace::{self, Trees},
};
use motor_protocol::*;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::PathBuf,
    process::ExitStatus,
    str::FromStr,
};

pub(crate) fn setup_step<T>(step: &str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| {
        io::Error::other(SetupError {
            step: step.to_owned(),
            error,
        })
    })
}

#[derive(Debug)]
struct SetupError {
    step: String,
    error: io::Error,
}
impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.error)
    }
}
impl std::error::Error for SetupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// How this runner instance runs jobs; each instance serves one platform.
/// Host runs Linux jobs on the build server in a sandboxed systemd unit as the
/// unprivileged job account. Vm and WindowsVm run each job inside a VM instead,
/// and WindowsVm also applies the Windows instance's own limits and layout.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackendKind {
    #[default]
    Host,
    Vm,
    WindowsVm,
    MacosVm,
}

impl FromStr for BackendKind {
    type Err = io::Error;

    fn from_str(value: &str) -> io::Result<Self> {
        match value {
            "host" => Ok(Self::Host),
            "vm" => Ok(Self::Vm),
            "windows-vm" => Ok(Self::WindowsVm),
            "macos-vm" => Ok(Self::MacosVm),
            _ => Err(invalid(
                "unknown backend; expected host, vm, windows-vm or macos-vm",
            )),
        }
    }
}

pub(crate) fn select(config: &Config) -> io::Result<Box<dyn Backend>> {
    match config.backend {
        BackendKind::Host => Ok(Box::new(HostBackend)),
        BackendKind::Vm if config.vm_config.is_none() => {
            Err(invalid("--backend vm requires --vm-config <file>"))
        }
        BackendKind::Vm => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "VM backend is not available in this build; use --backend host",
        )),
        BackendKind::WindowsVm if config.vm_config.is_none() => {
            Err(invalid("--backend windows-vm requires --vm-config <file>"))
        }
        BackendKind::WindowsVm => Ok(Box::new(crate::windows::WindowsBackend)),
        #[cfg(unix)]
        BackendKind::MacosVm => Ok(Box::new(crate::macos::MacosBackend)),
        #[cfg(not(unix))]
        BackendKind::MacosVm => Err(invalid("macos-vm requires macOS")),
    }
}

/// One job's reserved setup, carried to the materialization thread without
/// holding the engine's state lock.
pub(crate) struct Setup {
    pub id: Uuid,
    pub cpus: Vec<u32>,
    pub request: Run,
    pub base: bool,
    /// Sibling checkout paths mapped to their commits when setup was reserved.
    /// Checkout compares these with the requested commits before reusing files.
    /// Empty in sibling snapshot mode, which never checks a sibling out.
    pub recorded: BTreeMap<String, String>,
    /// In sibling snapshot mode, for a job that is not a warm build: its
    /// siblings' snapshots, resolved by the setup thread before
    /// materialization. `None` when siblings are checked out in place.
    pub snapshots: Option<crate::sibling_snapshots::JobSiblings>,
}

pub(crate) struct Prepared {
    pub host_cwd: PathBuf,
    pub target: PathBuf,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub wrapper: Option<PathBuf>,
    pub sandbox: Option<Box<dyn IsolationLauncher>>,
    pub vm: Option<std::sync::Mutex<Option<Box<dyn Execution>>>>,
}

/// Owns the prepared service and mount configuration. Tests can substitute a
/// launcher to observe dispatch without requiring systemd or a job account.
pub(crate) trait IsolationLauncher: Send + Sync {
    fn prepared_javascript_directories(&self) -> &[PathBuf] {
        &[]
    }
    #[cfg(unix)]
    fn cache_generation(
        &self,
    ) -> Option<(
        &crate::remote_cache::provisioning::ActiveGeneration,
        &[crate::remote_cache::transport::Forwarder],
    )> {
        None
    }
    fn launch(
        &self,
        command: &str,
        cwd: &std::path::Path,
        env: &[(String, String)],
        cpus: &[u32],
        requested_vcpus: u32,
    ) -> io::Result<platform::Launch>;
}

#[cfg(target_os = "linux")]
impl IsolationLauncher for crate::isolation::Sandbox {
    fn prepared_javascript_directories(&self) -> &[PathBuf] {
        &self.prepared_javascript_directories
    }
    fn cache_generation(
        &self,
    ) -> Option<(
        &crate::remote_cache::provisioning::ActiveGeneration,
        &[crate::remote_cache::transport::Forwarder],
    )> {
        self.cache_generation()
    }
    fn launch(
        &self,
        command: &str,
        cwd: &std::path::Path,
        env: &[(String, String)],
        cpus: &[u32],
        requested_vcpus: u32,
    ) -> io::Result<platform::Launch> {
        self.launch_durable(command, cwd, env, cpus, requested_vcpus)
    }
}

/// The engine persists the process identity before opening the execution gate.
/// It drains stdout and stderr before persisting the final job outcome.
pub(crate) struct Launch {
    pub process: Option<platform::ProcessIdentity>,
    pub durable_output: bool,
    pub execution: Box<dyn Execution>,
    /// The write ledgers of the unit's cache sockets; empty for a unit that is
    /// not a fill or has no remote cache.
    #[cfg(unix)]
    pub cache_ledgers: Vec<std::sync::Arc<crate::remote_cache::gateway::WriteLedger>>,
}

pub(crate) struct Output {
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
}

pub(crate) trait Execution: Send {
    fn release(&mut self) -> io::Result<()>;
    fn output(&self) -> io::Result<Output>;
    /// Stop resources owned by this execution that backend cancellation cannot
    /// address, before waiting or reaping the still-owned child.
    fn abort(&self) -> io::Result<()> {
        Ok(())
    }
    fn wait(&mut self) -> io::Result<ExecutionOutcome>;
    fn reap(&self);
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExecutionOutcome {
    Unix(ExitStatus),
    WindowsExit {
        code: u32,
    },
    #[cfg(unix)]
    Signal {
        signal: i32,
    },
}
impl ExecutionOutcome {
    pub(crate) fn success(&self) -> bool {
        match self {
            Self::Unix(status) => status.success(),
            Self::WindowsExit { code } => *code == 0,
            #[cfg(unix)]
            Self::Signal { .. } => false,
        }
    }
    pub(crate) fn wire(self) -> motor_protocol::Outcome {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        match self {
            Self::Unix(status) => status
                .code()
                .map(|code| motor_protocol::Outcome::Exit { code })
                .unwrap_or(motor_protocol::Outcome::Signal {
                    signal: {
                        #[cfg(unix)]
                        {
                            status.signal().unwrap_or(9)
                        }
                        #[cfg(windows)]
                        {
                            9
                        }
                    },
                }),
            Self::WindowsExit { code } => windows_outcome(code),
            #[cfg(unix)]
            Self::Signal { signal } => motor_protocol::Outcome::Signal { signal },
        }
    }
}

fn windows_outcome(code: u32) -> motor_protocol::Outcome {
    motor_protocol::Outcome::WindowsExit { code }
}

pub(crate) trait Backend: Send + Sync {
    fn validate(&self, _config: &Config) -> io::Result<()> {
        Ok(())
    }
    fn initialize(&self, _config: &Config) -> io::Result<()> {
        Ok(())
    }
    /// None means this backend cannot prove that its live execution list is
    /// complete. Only isolated host services have an authoritative unit listing.
    fn live_units(
        &self,
        _config: &Config,
    ) -> io::Result<Option<std::collections::BTreeSet<String>>> {
        Ok(None)
    }
    fn unit_name(&self, config: &Config, id: Uuid) -> String {
        match config.backend {
            BackendKind::WindowsVm => format!("motor-windows-{}-{id}.service", config.instance),
            _ => host_unit_name(id),
        }
    }
    #[cfg(any(target_os = "linux", test))]
    fn unit_pattern(&self, config: &Config) -> String {
        match config.backend {
            BackendKind::WindowsVm => format!("motor-windows-{}-*", config.instance),
            _ => "motor-job-*".into(),
        }
    }
    #[cfg(any(target_os = "linux", test))]
    fn failed_job_units(&self, config: &Config) -> io::Result<u64> {
        crate::systemd::failed_job_units(&self.unit_pattern(config), |args| systemctl(config, args))
    }
    fn materialize(&self, config: &Config, trees: &Trees, setup: &Setup) -> io::Result<Prepared>;
    fn launch(
        &self,
        config: &Config,
        trees: &Trees,
        setup: &Setup,
        prepared: &Prepared,
        weight: u32,
    ) -> io::Result<Launch>;
    /// `hard` escalates cancellation after the engine's grace period.
    /// The job id is available even for backends without a host process group.
    fn cancel(&self, config: &Config, job: &Job, hard: bool) -> io::Result<()>;
    fn unit_alive(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>>;
    fn oom_stopped(
        &self,
        _config: &Config,
        _process: Option<platform::ProcessIdentity>,
    ) -> Option<bool> {
        None
    }
    fn unit_exit_status(
        &self,
        _config: &Config,
        _process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<std::process::ExitStatus>> {
        Ok(None)
    }
    fn collect_changes(
        &self,
        config: &Config,
        trees: &Trees,
        job: &Job,
    ) -> io::Result<Option<Vec<String>>>;
    /// Called under the exclusive instance lock before serving jobs. Returns
    /// the ids whose execution or durable output can be monitored after restart.
    fn reconcile(&self, config: &Config, interrupted: &[Job]) -> io::Result<Vec<Uuid>>;
}

pub(crate) fn host_unit_name(id: Uuid) -> String {
    format!("motor-job-{id}.service")
}

// Small CPU requests still need enough memory for Rust builds. Use a 16 GiB
// floor, then scale at 3.5 GiB per vCPU to leave host headroom for larger jobs.
const MEMORY_BYTES_PER_VCPU: u64 = 7 * (1 << 29);
const MEMORY_FLOOR_GIB: u64 = 16;

fn memory_max_with_floor(vcpus: u32, floor_gib: u64) -> u64 {
    (u64::from(vcpus) * MEMORY_BYTES_PER_VCPU).max(floor_gib.saturating_mul(1 << 30))
}

#[cfg(not(test))]
pub(crate) fn memory_max(vcpus: u32) -> u64 {
    memory_max_with_floor(vcpus, MEMORY_FLOOR_GIB)
}

#[cfg(test)]
pub(crate) fn memory_max(vcpus: u32, floor_gib: Option<u64>) -> u64 {
    memory_max_with_floor(vcpus, floor_gib.unwrap_or(MEMORY_FLOOR_GIB))
}

#[cfg(test)]
pub(crate) fn memory_floor(floor_gib: Option<u64>) -> u64 {
    floor_gib
        .unwrap_or(MEMORY_FLOOR_GIB)
        .saturating_mul(1 << 30)
}

/// The smallest memory limit any job asks for: the floor every job unit gets.
#[cfg(not(test))]
pub(crate) fn memory_floor() -> u64 {
    MEMORY_FLOOR_GIB * (1 << 30)
}

/// The memory limit a Linux job unit asks for. A request larger than the
/// shared jobs slice can never be honoured, so the unit asks for the slice's
/// own `MemoryMax`; an overrun still ends as the slice's OOM kill.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn job_memory_limit(requested: u64, slice_ceiling: u64) -> u64 {
    crate::job_memory_budget::JobMemoryBudget::from_max(slice_ceiling)
        .map_or(requested, |slice| requested.min(slice.max))
}

/// Only an operator-declared budget promises the per-job floor. A smaller
/// declaration (or a live slice too small to honour it) is a setup fault, not a
/// capacity shortage that waiting can clear. Undeclared hosts simply clamp job
/// limits to the slice, even when that puts them below the floor.
pub(crate) fn slice_admission_refusal(
    floor: u64,
    slice_ceiling: u64,
    declared_min: Option<u64>,
) -> Option<(RefusalReason, String)> {
    let declared_min = declared_min?;
    let source = if declared_min < floor {
        format!("the declared jobs_min_bytes={declared_min}")
    } else if slice_ceiling < floor {
        format!("the shared jobs slice MemoryMax={slice_ceiling}")
    } else {
        return None;
    };
    Some((
        RefusalReason::WorkspaceSetupFailed,
        format!(
            "{source} is below the {} GiB memory floor every job unit requests; raise jobs_min_bytes in /etc/motor-host/capacity.json and re-run provision.py apply-jobs-slice",
            format_memory_limit_gib(floor)
        ),
    ))
}

#[test]
fn a_declared_budget_below_the_job_floor_is_refused_and_an_undeclared_clamp_is_not() {
    // 16 GiB is the floor every job unit requests.
    let floor = 17_179_869_184;
    let (reason, detail) = slice_admission_refusal(floor, 40 << 30, Some(floor - 1)).unwrap();
    assert_eq!(reason, RefusalReason::WorkspaceSetupFailed);
    assert_eq!(
        detail,
        "the declared jobs_min_bytes=17179869183 is below the 16 GiB memory floor every job unit requests; raise jobs_min_bytes in /etc/motor-host/capacity.json and re-run provision.py apply-jobs-slice"
    );
    // At exactly the floor, and above it, jobs run: a clamped job is an
    // ordinary in-slice OOM, not a refusal.
    assert_eq!(slice_admission_refusal(floor, floor, Some(floor)), None);
    assert_eq!(
        slice_admission_refusal(floor, 40 << 30, Some(24 << 30)),
        None
    );
    assert_eq!(slice_admission_refusal(floor, floor - 1, None), None);
}

fn format_memory_limit_gib(bytes: u64) -> String {
    let whole = bytes / (1 << 30);
    if bytes.is_multiple_of(1 << 30) {
        whole.to_string()
    } else {
        format!("{whole}.5")
    }
}

#[cfg(not(test))]
pub(crate) fn memory_limit_gib(vcpus: u32) -> String {
    format_memory_limit_gib(memory_max(vcpus))
}

#[cfg(test)]
pub(crate) fn memory_limit_gib(vcpus: u32, floor_gib: Option<u64>) -> String {
    format_memory_limit_gib(memory_max(vcpus, floor_gib))
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn systemctl(config: &Config, args: &[&str]) -> io::Result<std::process::Output> {
    #[cfg(test)]
    if let Some(run) = &config.systemd_command {
        return run(args);
    }
    crate::systemd::systemctl(args, config.windows_instance() || config.isolate_jobs)
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn live_job_units(config: &Config) -> io::Result<std::collections::BTreeSet<String>> {
    let output = systemctl(
        config,
        &[
            "list-units",
            "--all",
            "--type=service",
            "--no-legend",
            "--plain",
            "--no-pager",
            "motor-job-*",
        ],
    )?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "list live job units: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    parse_live_job_units(&output.stdout)
}

/// Each row ends with the unit's description, which systemd-run fills with the
/// job's whole command line, so a row can be as long as any command a caller
/// sends. Only the four leading fields are read; the bounds cover the listing
/// as a whole, never one row, so one long command can't refuse every job.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_live_job_units(
    stdout: &[u8],
) -> io::Result<std::collections::BTreeSet<String>> {
    if stdout.len() > 64 * 1024 * 1024 {
        return Err(io::Error::other("live job unit listing exceeds size limit"));
    }
    let text = std::str::from_utf8(stdout).map_err(io::Error::other)?;
    let mut units = std::collections::BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        if index >= 10_000 {
            return Err(io::Error::other("live job unit listing exceeds row limit"));
        }
        let mut fields = line.split_whitespace();
        let Some(unit) = fields.next() else { continue };
        let load = fields.next();
        let active = fields.next();
        let sub = fields.next();
        if !unit.starts_with("motor-job-")
            || !unit.ends_with(".service")
            || load.is_none()
            || active.is_none()
            || sub.is_none()
        {
            return Err(io::Error::other("malformed live job unit listing"));
        }
        if !matches!(active, Some("inactive" | "failed")) {
            units.insert(unit.to_string());
        }
    }
    Ok(units)
}

pub(crate) struct HostBackend;

#[cfg(target_os = "linux")]
fn verify_boat_storage(users: &Path, state: &Path, workspace: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let check = || -> io::Result<()> {
        let device = std::fs::metadata(state)?.dev();
        if std::fs::metadata(users)?.dev() != device || std::fs::metadata("/")?.dev() == device {
            return Err(io::Error::other("/Users is not on the runner state disk"));
        }
        let directory = std::fs::metadata(workspace)?;
        if !directory.is_dir() || directory.dev() != device {
            return Err(io::Error::other(
                "workspace is not a directory on the runner state disk",
            ));
        }
        Ok(())
    };
    check().map_err(|error| {
        io::Error::other(format!(
            "Boat storage verification failed before job launch: {error}; workspace={}; stop admission and re-establish storage with the runner stopped",
            workspace.display()
        ))
    })
}

impl Backend for HostBackend {
    #[cfg(target_os = "linux")]
    fn live_units(
        &self,
        config: &Config,
    ) -> io::Result<Option<std::collections::BTreeSet<String>>> {
        if !config.isolate_jobs || config.windows_instance() {
            return Ok(None);
        }
        live_job_units(config).map(Some)
    }
    fn materialize(&self, config: &Config, trees: &Trees, s: &Setup) -> io::Result<Prepared> {
        let r = &s.request;
        // A mapped job's trees live under a host root; every filesystem step
        // below uses their host forms, and only the sandbox sees caller paths.
        let mapped = paths::mapped_run(config, r);
        // In-place population and repair use caller-spelled sibling paths.
        // Mapped non-base jobs with siblings must resolve snapshots first.
        if mapped && !s.base && !r.siblings.is_empty() && s.snapshots.is_none() {
            return Err(invalid("mapped siblings require resolved snapshots"));
        }
        let host = paths::host_run(config, r);
        let host_siblings = paths::host_siblings(config, &r.siblings);
        let validate = || -> io::Result<()> {
            if mapped {
                let authorized = paths::validate_mapped_run(config, r, s.base)?;
                if !s.base {
                    crate::writer::validate_paths(
                        config,
                        paths::host(&authorized.host_workspace),
                        authorized.host_cwd.as_deref().map(paths::host),
                        &authorized.host_siblings,
                        &[],
                    )?;
                }
                return Ok(());
            }
            workspace::validate_destinations(
                &config.owned_workspace_roots,
                crate::caller_path::host_str(if s.base {
                    paths::run(r).caller_cwd.text()
                } else {
                    paths::request(r).caller_workspace_key.text()
                }),
                Some(paths::run(r).host_cwd),
                &r.siblings,
            )?;
            if config.sibling_snapshot_mode() && !s.base {
                workspace::validate_request(paths::request(r), &r.siblings, true)?;
                workspace::validate_alias_destinations(
                    &config.owned_workspace_roots,
                    &workspace::sibling_aliases(paths::request(r).host_workspace, &r.siblings)?,
                )?;
            }
            if !s.base {
                crate::writer::validate_paths(
                    config,
                    paths::request(r).host_workspace,
                    Some(paths::run(r).host_cwd),
                    &r.siblings,
                    &[],
                )?;
            }
            Ok(())
        };
        if s.base {
            validate()?;
            if !config.writer_enabled() {
                return Err(io::Error::other("dependency writer account is unavailable"));
            }
        } else {
            // These checks read inside the workspace before the access reclaim
            // below runs, and the previous job may have locked the runner out
            // of it (see `workspace_access`).
            crate::workspace_access::validate_with_repair(
                config,
                paths::host(&host.host_workspace),
                &host_siblings,
                validate,
            )?;
        }
        let path = host.host_workspace.clone();
        #[cfg(target_os = "linux")]
        if config.boat_profile && path.exists() {
            // Bound hostile cached trees before permission repair or checkout
            // starts a separate, potentially much larger traversal.
            setup_step(
                "preflight Boat cached tree",
                crate::boat_quota::preflight_cached_tree(&path).map_err(|error| {
                    crate::boat_quota::cache_walk_error(error, config.boat_cold_materialization)
                }),
            )?;
        }
        #[cfg(target_os = "linux")]
        if config.isolate_jobs && path.exists() {
            crate::setup_log::phase("restore workspace access", || {
                crate::isolation::restore_runner_access(&path)
            })
            .map_err(|error| io::Error::other(format!("restore workspace access: {error}")))?;
            if path.is_dir() {
                crate::isolation::restore_runner_checkout_root(&path).map_err(|error| {
                    io::Error::other(format!("restore workspace root ownership: {error}"))
                })?;
                if s.base && path.join(".git").is_dir() {
                    // Writer sandbox setup owns the checkout root and .git as
                    // motor-cache. Git's repository safety checks include both
                    // owners, so restore them before the daemon synchronizes.
                    crate::isolation::restore_runner_checkout_root(&path.join(".git")).map_err(
                        |error| {
                            io::Error::other(format!(
                                "restore writer Git directory ownership: {error}"
                            ))
                        },
                    )?;
                }
            }
        }
        // A sibling checkout can also be another job's own workspace (for
        // example a repository's main checkout), which that job's sandbox left
        // owned by the job account. Git refuses a repository whose root another
        // user owns, so take each sibling back as the workspace above. This is
        // separate from the workspace check because a job's first run has no
        // workspace yet while its siblings can already exist. Admission never
        // lets a sibling be prepared while a job is running in it, so no job is
        // affected. A job that mounts its siblings from snapshots never reads
        // or writes their host paths, so it leaves them alone. Writer sandboxes
        // mount no siblings, so base builds need no sibling repair either.
        #[cfg(target_os = "linux")]
        if config.isolate_jobs && !s.base && s.snapshots.is_none() {
            for sibling in &r.siblings {
                let sibling = paths::sibling(sibling).host.as_path();
                if !sibling.is_dir() {
                    continue;
                }
                crate::isolation::restore_runner_access(sibling).map_err(|error| {
                    io::Error::other(format!(
                        "restore sibling {} access: {error}",
                        sibling.display()
                    ))
                })?;
                crate::isolation::restore_runner_checkout_root(sibling).map_err(|error| {
                    io::Error::other(format!(
                        "restore sibling {} root ownership: {error}",
                        sibling.display()
                    ))
                })?;
            }
        }
        if s.base {
            setup_step(
                "checkout workspace",
                trees.sync_workspace(
                    paths::request(r).caller_repository_root.text(),
                    crate::caller_path::host(&path),
                    &r.snapshot_commit,
                    &r.tree_hash,
                    &r.snapshot_commit,
                ),
            )?;
        } else {
            let caller_commit = r.base_commit.clone().unwrap_or_else(|| {
                workspace::resolve_caller_commit(
                    &trees.mirror(paths::request(r).caller_repository_root.text()),
                    &r.snapshot_commit,
                    &r.tree_hash,
                )
            });
            setup_step(
                "populate workspace",
                crate::setup_log::phase("populate workspace", || match &s.snapshots {
                    // Each sibling comes from its snapshot, mounted at the
                    // sibling's path by the sandbox: only the main workspace
                    // is checked out on the host.
                    Some(_) => trees.populate_main(
                        paths::host(&path),
                        paths::request(r).caller_repository_root.text(),
                        &r.snapshot_commit,
                        &r.tree_hash,
                        &caller_commit,
                    ),
                    None => trees.populate(
                        paths::host(&path),
                        paths::request(r).caller_repository_root.text(),
                        &r.snapshot_commit,
                        &r.siblings,
                        &s.recorded,
                        &r.tree_hash,
                        &caller_commit,
                    ),
                }),
            )?;
        }
        let computed = setup_step(
            "verify workspace tree",
            crate::setup_log::phase("verify workspace tree", || trees.tree_hash(&path)),
        )?;
        if computed != r.tree_hash {
            return Err(Trees::tree_hash_mismatch(&path, &r.tree_hash, &computed));
        }
        let cwd = setup_step("resolve job cwd", fs::canonicalize(&host.host_cwd))?;
        if !cwd.starts_with(setup_step(
            "resolve workspace path",
            fs::canonicalize(&path),
        )?) {
            return Err(invalid("cwd outside workspace"));
        }
        // A mapped job sees its cwd at the caller form of this host path, and
        // that form is derived from the configured host root's spelling. The
        // no-symlink checks above make the resolved path that same spelling.
        if mapped && cwd != host.host_cwd {
            return Err(invalid("mapped cwd does not resolve to its host path"));
        }
        let target = if s.base {
            let target = config.writer_scratch_dir().join(s.id.to_string());
            setup_step("create writer scratch", fs::create_dir_all(&target))?;
            target
        } else {
            path.join("target")
        };
        let wrapper = trees.state.join("prefrontal-build-slot");
        #[cfg(target_os = "linux")]
        let wrapper = if config.isolate_jobs {
            use sha2::{Digest, Sha256};
            // Hash the supervisor source embedded in this runner so a binary
            // compiled from an uploaded tree is never reused, and changes to
            // the embedded source invalidate the cache.
            trees.state.join(format!(
                "prefrontal-build-slot-{:x}",
                Sha256::digest(include_str!("../../prefrontal-build-slot/src/main.rs"))
            ))
        } else {
            wrapper
        };
        let source = path.join("crates/prefrontal-build-slot/src/main.rs");
        let mut wrapper = if config.shared_compile_cache() && source.exists() {
            #[cfg(target_os = "linux")]
            let source = if config.isolate_jobs {
                // Uploaded Rust compiled as the daemon could use include_str!
                // to embed files the daemon can read, including its SSH keys,
                // in a binary the job later runs. Compile only the supervisor
                // source embedded in this trusted runner executable.
                let source = wrapper.with_extension("rs");
                let temporary = source.with_extension(format!("{}.tmp", s.id));
                setup_step(
                    "write trusted compiler supervisor source",
                    fs::write(
                        &temporary,
                        include_str!("../../prefrontal-build-slot/src/main.rs"),
                    ),
                )?;
                setup_step(
                    "install compiler supervisor source",
                    fs::rename(temporary, &source),
                )?;
                source
            } else {
                source
            };
            setup_step(
                "build compiler-slot wrapper",
                crate::setup_log::phase("build compiler supervisor", || {
                    platform::build_wrapper(
                        &source,
                        &wrapper,
                        &config.build_slot_root,
                        config.rustc_slots,
                    )
                }),
            )?;
            Some(wrapper)
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let sandbox = if config.isolate_jobs {
            let lockfile = workspace::git(
                &trees.mirror(paths::request(r).caller_repository_root.text()),
                &["show", &format!("{}:Cargo.lock", r.snapshot_commit)],
            )
            .unwrap_or_default();
            let (routes, missing) =
                trees.dependency_fetch_routes(&r.git_dependencies, &lockfile)?;
            if s.base && !missing.is_empty() {
                return Err(invalid(missing.join("; ")));
            }
            let locks: Vec<_> = routes
                .values()
                .map(|mirror| trees.dependency_lock(mirror))
                .collect();
            let _guards: Vec<_> = locks
                .iter()
                .map(|lock| {
                    crate::setup_log::lock_wait("dependency_mirror", || lock.lock().unwrap())
                })
                .collect();
            trees.refresh_dependency_aliases(&lockfile, &routes)?;
            // In snapshot mode the sandbox mounts each sibling's snapshot at
            // the sibling's path, and the Cargo prefetch sees the same view.
            let snapshots =
                s.snapshots
                    .as_ref()
                    .map(|snapshots| crate::isolation::SnapshotSiblings {
                        config,
                        siblings: snapshots.mounts(),
                    });
            let remote_plan = config.unit_remote_cache(s.base);
            let mut remote = None;
            if let (Some((generation, forwarders)), Some(runtime)) =
                (&remote_plan, &config.remote_cache_runtime)
            {
                let cargo = path.join("Cargo.toml").is_file();
                let declaration = if s.base && config.base_commands.is_empty() {
                    crate::writer::declaration::read(
                        &trees.mirror(paths::request(r).caller_repository_root.text()),
                        &r.snapshot_commit,
                    )
                    .ok()
                    .flatten()
                } else {
                    None
                };
                let tools: Vec<_> = forwarders
                    .iter()
                    .map(|f| f.tool())
                    .filter(|tool| match tool {
                        crate::remote_cache::gateway::Tool::Sccache => {
                            cargo
                                || (s.base
                                    && declaration.as_ref().is_some_and(|d| {
                                        d.fills.iter().any(|f| {
                                            f.tool == crate::writer::declaration::Tool::Sccache
                                        })
                                    }))
                        }
                        crate::remote_cache::gateway::Tool::Ccache => {
                            !s.base
                                || declaration.as_ref().is_some_and(|d| {
                                    d.fills
                                        .iter()
                                        .any(|f| f.tool == crate::writer::declaration::Tool::Ccache)
                                })
                        }
                    })
                    .collect();
                if !tools.is_empty() {
                    remote = runtime.transport(&generation.tenant, &tools).ok();
                }
            }
            let sandbox = setup_step(
                "prepare isolated job sandbox",
                crate::setup_log::phase("prepare isolated sandbox", || {
                    if s.base {
                        crate::isolation::Sandbox::prepare_writer(
                            s.id,
                            &trees.state,
                            paths::JobTree::new(&path, paths::caller_checkout(r)),
                            &target,
                            wrapper.as_deref(),
                            &config.build_slot_root,
                            config.rustc_slots,
                            &routes,
                            &r.tree_hash,
                            &crate::isolation::JavascriptTools::for_config(config),
                            config.boat_profile,
                            config.tenant_vm,
                            config.shared_compile_cache(),
                            remote.as_ref(),
                        )
                    } else {
                        crate::isolation::Sandbox::prepare_with_dependencies(
                            s.id,
                            &trees.state,
                            paths::JobTree::new(&path, paths::caller_checkout(r)),
                            &target,
                            &r.siblings,
                            wrapper.as_deref(),
                            &config.build_slot_root,
                            config.rustc_slots,
                            &routes,
                            &r.tree_hash,
                            paths::request(r).caller_repository_root.text(),
                            config.tenant_vm,
                            config.shared_compile_cache(),
                            snapshots.as_ref(),
                            &crate::isolation::JavascriptTools::for_config(config),
                            remote.as_ref(),
                        )
                    }
                    // When a job ends, the engine checks whether systemd stopped its
                    // unit for exceeding the memory limit (Result=oom-kill), then
                    // resets the unit so systemd unloads it. Keep it loaded until then.
                    .map(|sandbox| {
                        let sandbox = sandbox
                            .with_cache_generation(remote_plan.as_ref().map(|(g, _)| g.clone()));
                        #[cfg(test)]
                        let sandbox = sandbox.with_memory_floor_gib(config.memory_floor_gib);
                        sandbox
                            .retaining_failed_units()
                            .in_tenant_vm(config.tenant_vm)
                            .with_boat_profile(
                                config.boat_profile,
                                config.private_pids,
                                config.boat_memory_max_bytes,
                            )
                            .with_pid_init(config.pid_init.clone())
                            .without_provider_environment(&config.boat_unset_environment)
                    })
                    .and_then(|sandbox| {
                        #[cfg(test)]
                        if config.job_network_hook.is_some() {
                            return Ok(sandbox);
                        }
                        if r.network == Some(Network::Outbound) {
                            sandbox.with_network()
                        } else {
                            Ok(sandbox)
                        }
                    })
                    .map_err(|e| {
                        if missing.is_empty() {
                            e
                        } else {
                            invalid(format!("{e}; {}", missing.join("; ")))
                        }
                    })
                }),
            )?;
            let sandbox = sandbox.tracking_cleanup(config.background_deletes.clone());
            let sandbox = if config.boat_profile {
                let sandbox = sandbox.with_boat_temporary_paths()?;
                let runtime = trees.state.join("sandboxes").join(s.id.to_string());
                let spool = trees.state.join("spools").join(s.id.to_string());
                fs::create_dir_all(&spool)?;
                setup_step(
                    "install Boat job disk quota",
                    crate::boat_quota::prepare(
                        &trees.state,
                        s.id,
                        crate::boat_quota::job_roots(&path, &target, &runtime, &spool),
                        config.boat_cold_materialization,
                    ),
                )?;
                sandbox
            } else {
                sandbox
            };
            wrapper = sandbox.wrapper.clone();
            Some(Box::new(sandbox) as Box<dyn IsolationLauncher>)
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let sandbox = {
            let _ = &mut wrapper;
            None
        };
        Ok(Prepared {
            host_cwd: cwd,
            target,
            wrapper,
            sandbox,
            vm: None,
        })
    }
    fn launch(
        &self,
        config: &Config,
        trees: &Trees,
        s: &Setup,
        prepared: &Prepared,
        weight: u32,
    ) -> io::Result<Launch> {
        let r = &s.request;
        #[cfg(target_os = "linux")]
        if config.boat_profile && config.isolate_jobs {
            setup_step(
                "verify Boat storage",
                verify_boat_storage(Path::new("/Users"), &config.state_dir(), &prepared.host_cwd),
            )?;
        }
        #[cfg(target_os = "linux")]
        if config.boat_profile {
            let workspace = paths::host_run(config, r).host_workspace;
            setup_step(
                "verify Boat quota lease",
                crate::boat_quota::validate_for_launch(
                    &trees.state,
                    s.id,
                    &crate::boat_quota::job_roots(
                        &workspace,
                        &prepared.target,
                        &trees.state.join("sandboxes").join(s.id.to_string()),
                        &trees.state.join("spools").join(s.id.to_string()),
                    ),
                ),
            )?;
        }
        let mut env = BTreeMap::new();
        #[cfg(test)]
        if r.network.is_some() {
            if let Some(hook) = &config.job_network_hook {
                setup_step("network attach", hook(crate::network::Phase::Attach(s.id)))?;
            }
        }
        for &key in JOB_BASE_ENV {
            if let Ok(value) = std::env::var(key) {
                env.insert(key.to_string(), value);
            }
        }
        for (k, v) in &r.env {
            if crate::allowed_env(k) {
                env.insert(k.clone(), v.clone());
            }
        }
        #[cfg(test)]
        if !config.isolate_jobs {
            if let Some(cargo_home) = &config.fixture_cargo_home {
                // Fixture Cargo jobs must not load configuration from the developer's home.
                env.insert(
                    "CARGO_HOME".into(),
                    cargo_home.to_string_lossy().into_owned(),
                );
                env.insert("CARGO_NET_OFFLINE".into(), "true".into());
            }
        }
        // Match test and compile concurrency to the permits reserved for this job, not to
        // the host's CPU count or an unrelated daemon's worker budget.
        for key in [
            "RUST_TEST_THREADS",
            "NEXTEST_TEST_THREADS",
            "CARGO_BUILD_JOBS",
        ] {
            env.entry(key.to_string())
                .or_insert_with(|| weight.to_string());
        }
        for key in [
            "SUBC_LAUNCH_NONCE",
            "SUBC_LAUNCH_NONCE_FD",
            "SUBC_MODULE_ID",
            "SSH_AUTH_SOCK",
        ] {
            env.remove(key);
        }
        let caller_commit = r.base_commit.clone().unwrap_or_else(|| {
            workspace::resolve_caller_commit(
                &trees.mirror(paths::request(r).caller_repository_root.text()),
                &r.snapshot_commit,
                &r.tree_hash,
            )
        });
        env.insert("CK_REMOTE_SOURCE_COMMIT".to_string(), caller_commit);
        // An isolated job sees its trees at caller paths; only the test-only
        // unisolated configuration runs a mapped job at its host paths.
        let job_path = |host: &std::path::Path| {
            if config.isolate_jobs {
                paths::caller_form(config, host)
            } else {
                host.to_path_buf()
            }
        };
        env.insert(
            "CARGO_TARGET_DIR".to_string(),
            job_path(&prepared.target).to_string_lossy().to_string(),
        );
        compile_cache_environment(config, s, prepared, &mut env)?;
        let env = env.into_iter().collect::<Vec<_>>();
        let launch = if config.isolate_jobs {
            let sandbox = prepared
                .sandbox
                .as_ref()
                .ok_or_else(|| io::Error::other("isolated job has no prepared sandbox"))?;
            sandbox.launch(
                &s.request.command,
                &job_path(&prepared.host_cwd),
                &env,
                &s.cpus,
                weight,
            )
        } else {
            // The non-isolated host backend is development-only and does not pin CPUs.
            platform::launch(&s.request.command, &prepared.host_cwd, &env)
        }?;
        #[cfg(target_os = "linux")]
        let cache_ledgers = launch.cache_ledgers();
        #[cfg(all(unix, not(target_os = "linux")))]
        let cache_ledgers = Vec::new();
        Ok(Launch {
            process: Some(launch.identity),
            durable_output: launch.durable_output,
            execution: Box::new(launch),
            #[cfg(unix)]
            cache_ledgers,
        })
    }

    fn cancel(&self, config: &Config, job: &Job, hard: bool) -> io::Result<()> {
        if let Some(process) = job.process {
            #[cfg(test)]
            if let (Some(run), Some(unit)) = (&config.systemd_command, process.unit) {
                let _ =
                    crate::systemd::signal(&self.unit_name(config, unit), hard, |args| run(args));
                return Ok(());
            }
            let _ = config;
            platform::signal(process, hard);
        }
        Ok(())
    }

    fn unit_alive(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        #[cfg(test)]
        if let (Some(run), Some(unit)) = (&config.systemd_command, process.and_then(|p| p.unit)) {
            return crate::systemd::alive(&self.unit_name(config, unit), |args| run(args))
                .map(Some);
        }
        let _ = config;
        platform::unit_alive(process)
    }

    fn oom_stopped(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> Option<bool> {
        #[cfg(any(target_os = "linux", test))]
        {
            let unit = process
                .and_then(|process| process.unit)
                .map(|id| self.unit_name(config, id))?;
            let result = crate::systemd::oom_killed(&unit, |args| systemctl(config, args)).ok()?;
            // OOM failures otherwise remain loaded after the result is read; clearing
            // the failure lets systemd collect the inactive transient unit again.
            let _ = systemctl(config, &["reset-failed", &unit]);
            Some(result)
        }
        #[cfg(not(any(target_os = "linux", test)))]
        {
            let _ = (config, process);
            None
        }
    }

    fn unit_exit_status(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<std::process::ExitStatus>> {
        #[cfg(any(target_os = "linux", test))]
        if let Some(unit) = process.and_then(|process| process.unit) {
            return crate::systemd::exit_status(&self.unit_name(config, unit), |args| {
                systemctl(config, args)
            })
            .map(Some);
        }
        let _ = (config, process);
        Ok(None)
    }

    fn collect_changes(
        &self,
        config: &Config,
        trees: &Trees,
        job: &Job,
    ) -> io::Result<Option<Vec<String>>> {
        if job.base {
            return Ok(None);
        }
        trees
            .changes(
                &paths::job_workspace(config, job),
                &job.prepared_javascript_directories,
            )
            .map(Some)
    }

    fn reconcile(&self, config: &Config, interrupted: &[Job]) -> io::Result<Vec<Uuid>> {
        let mut recovered = Vec::new();
        for job in interrupted {
            if job.pending() && job.started_ms.is_none() && job.process.is_none() {
                continue;
            }
            let alive = self.unit_alive(config, job.process)?;
            if alive == Some(true) || job.durable_output {
                recovered.push(job.id);
            } else if job.process.is_some_and(|p| p.unit.is_none()) {
                // A process without a systemd unit cannot be reattached. Stop
                // it before releasing its workspace for another job; platform
                // signalling checks the recorded kernel start time for PID reuse.
                self.cancel(config, job, true)?;
            }
        }
        Ok(recovered)
    }
}

impl Execution for platform::Launch {
    fn release(&mut self) -> io::Result<()> {
        self.release()
    }
    fn output(&self) -> io::Result<Output> {
        Ok(Output {
            stdout: Box::new(self.stdout.try_clone()?),
            stderr: Box::new(self.stderr.try_clone()?),
        })
    }
    fn abort(&self) -> io::Result<()> {
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            // The portable backend has no restart-safe PID signalling. Here
            // the unreaped execution still owns its child and process group.
            if unsafe { libc::kill(-self.identity.pid, libc::SIGKILL) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    fn wait(&mut self) -> io::Result<ExecutionOutcome> {
        self.wait().map(ExecutionOutcome::Unix)
    }
    fn reap(&self) {
        self.reap()
    }
}

#[cfg(unix)]
fn compile_cache_environment(
    config: &Config,
    setup: &Setup,
    prepared: &Prepared,
    env: &mut BTreeMap<String, String>,
) -> io::Result<()> {
    #[cfg(unix)]
    scrub_inherited_cache_environment(config, setup, prepared, env);
    if !config.shared_compile_cache() {
        env.retain(|key, _| key != "RUSTC_WRAPPER" && !key.starts_with("SCCACHE_"));
        // Without the shared cache an isolated Rust job still builds in the same fixed
        // folder as on the build server; the sandbox binds it to the job's own
        // target folder (`compile_cache_mounts`).
        #[cfg(target_os = "linux")]
        if config.isolate_jobs && rust_job(config, setup) {
            env.insert(
                "CARGO_TARGET_DIR".into(),
                crate::compile_cache::TARGET.into(),
            );
        }
        #[cfg(unix)]
        return remote_cache_environment(config, setup, prepared, env);
        #[cfg(not(unix))]
        return Ok(());
    }
    if let Some(wrapper) = &prepared.wrapper {
        env.insert(
            "RUSTC_WRAPPER".into(),
            wrapper.to_string_lossy().into_owned(),
        );
    } else {
        env.remove("RUSTC_WRAPPER");
    }
    #[cfg(target_os = "linux")]
    if config.isolate_jobs && rust_job(config, setup) {
        crate::compile_cache::reader_environment(env, config.sccache_size_gib);
        if setup.base {
            env.insert("SCCACHE_LOCAL_RW_MODE".into(), "READ_WRITE".into());
            env.insert("CARGO_INCREMENTAL".into(), "1".into());
            env.insert(
                "SCCACHE_SERVER_UDS".into(),
                config.writer_sccache_server_uds(setup.id),
            );
        }
        if prepared.wrapper.is_none()
            && (setup.base
                || !crate::compile_cache::configured_wrapper(
                    &paths::host_run(config, &setup.request).host_workspace,
                    &prepared.host_cwd,
                )?)
        {
            env.insert("RUSTC_WRAPPER".into(), "sccache".into());
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = setup;
    #[cfg(unix)]
    {
        remote_cache_environment(config, setup, prepared, env)
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

#[cfg(not(unix))]
fn compile_cache_environment(
    _config: &Config,
    _setup: &Setup,
    _prepared: &Prepared,
    _env: &mut BTreeMap<String, String>,
) -> io::Result<()> {
    Ok(())
}

/// A cache fill, or a job whose workspace root is a Cargo workspace.
#[cfg(target_os = "linux")]
fn rust_job(config: &Config, setup: &Setup) -> bool {
    setup.base
        || paths::host_run(config, &setup.request)
            .host_workspace
            .join("Cargo.toml")
            .is_file()
}

/// Remove inherited cache settings before adding runner-generated values.
/// Build-server isolation always sanitizes its local cache environment. Hosted
/// units sanitize only when prepared with remote sockets, preserving their
/// baseline otherwise. Using the prepared unit also avoids a tenant switch
/// change between preparation and launch changing which values are removed.
#[cfg(unix)]
pub(crate) fn scrub_inherited_cache_environment(
    config: &Config,
    setup: &Setup,
    prepared: &Prepared,
    env: &mut BTreeMap<String, String>,
) {
    // Keep removing inherited cache settings on the build server; the remote
    // cache switch does not control this cleanup.
    let local = config.isolate_jobs && !config.boat_profile && !config.tenant_vm;
    let remote = prepared
        .sandbox
        .as_ref()
        .is_some_and(|sandbox| sandbox.cache_generation().is_some());
    if !local && !remote {
        return;
    }
    // Declared C/C++ fills and jobs need not contain a Cargo manifest.
    env.retain(|key, _| !key.starts_with("CCACHE_"));
    #[cfg(target_os = "linux")]
    if !(setup.base || rust_job(config, setup)) {
        return;
    }
    #[cfg(not(target_os = "linux"))]
    if !setup.base {
        return;
    }
    env.retain(|key, _| {
        !crate::remote_cache::environment::INHERITED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
    });
}

/// The remote cache's job delta, when the integration is on for this unit.
///
/// It is on only when the runner switch, the cache tenant's `enabled` state
/// and job isolation all hold: without isolation a job runs as the runner's
/// own account and could open the keys, so the switch is ignored there. The
/// prepared sandbox is the authority on which sockets were successfully bound.
#[cfg(unix)]
fn remote_cache_environment(
    config: &Config,
    setup: &Setup,
    prepared: &Prepared,
    env: &mut BTreeMap<String, String>,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let remote = prepared
        .sandbox
        .as_ref()
        .and_then(|sandbox| sandbox.cache_generation());
    #[cfg(not(target_os = "linux"))]
    let remote: Option<(
        &crate::remote_cache::provisioning::ActiveGeneration,
        &[crate::remote_cache::transport::Forwarder],
    )> = {
        let _ = prepared;
        None
    };
    let Some((generation, forwarders)) = remote else {
        return Ok(());
    };
    remote_cache_job_environment(
        crate::remote_cache::provisioning::HostRole::of(config),
        forwarders,
        generation,
        setup.base,
        if setup.base {
            paths::run(&setup.request).caller_cwd.text()
        } else {
            paths::request(&setup.request).caller_workspace_key.text()
        },
        env,
    );
    Ok(())
}

/// Apply one host type's remote cache delta to `env`: exactly the difference
/// between its switch-off job environment and its switch-on one. `env` must
/// already hold the role's baseline.
#[cfg(unix)]
pub(crate) fn remote_cache_job_environment(
    role: crate::remote_cache::provisioning::HostRole,
    forwarders: &[crate::remote_cache::transport::Forwarder],
    generation: &crate::remote_cache::provisioning::ActiveGeneration,
    fill: bool,
    workspace: &str,
    env: &mut BTreeMap<String, String>,
) {
    let mut delta = crate::remote_cache::environment::delta(
        role,
        forwarders,
        &generation.bucket,
        &generation.region,
    );
    if fill
        && forwarders
            .iter()
            .any(|f| f.tool() == crate::remote_cache::gateway::Tool::Sccache)
    {
        delta.extend(crate::remote_cache::environment::fill_values());
        delta.insert("SCCACHE_RECACHE".into(), "1".into());
        delta.insert(
            "SCCACHE_S3_RW_MODE".into(),
            crate::remote_cache::environment::READ_WRITE_MODE.into(),
        );
    }
    if let Some(forwarder) = forwarders
        .iter()
        .find(|f| f.tool() == crate::remote_cache::gateway::Tool::Ccache)
        .filter(|_| crate::remote_cache::ccache::installed(role))
    {
        delta.extend(crate::remote_cache::ccache::environment(
            forwarder, fill, workspace,
        ));
    }
    env.extend(delta);
}

// Preserve only the daemon context toolchains need. Request values are filtered
// separately, and isolated backends supply their own base environment.
const JOB_BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "TZ",
    "RUSTUP_HOME",
    "CARGO_HOME",
];

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
