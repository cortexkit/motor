//! Each job executes in a disposable Windows VM, not in a Linux host shell.
//! The motor-prep account prepares its files, and motor-qemu runs the VM. The
//! host saves the Started job record on disk before asking the guest to run the
//! command, so a failed record write or a crash before that request starts nothing.
use crate::{backend::*, engine::Config, platform, storage::*, workspace::Trees};
#[cfg(any(windows, test))]
use motor_protocol::RefusalReason;
use motor_protocol::{Uuid, WindowsGuestAgentReadiness};
use serde::Deserialize;
#[cfg(any(unix, test))]
use std::process::Output;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};
#[cfg(unix)]
use std::{os::unix::fs::PermissionsExt, path::Component};

mod agent;
mod caches;
#[cfg(unix)]
mod host;
#[cfg(windows)]
#[path = "windows/hyperv_host.rs"]
mod host;
#[cfg(windows)]
mod hvsock;
pub(crate) mod prep;
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) mod prep_error;
#[cfg(test)]
mod staging;
#[cfg(windows)]
mod trusted_prep;
#[cfg(unix)]
pub(crate) mod vm;
#[cfg(windows)]
#[path = "windows/hyperv.rs"]
pub(crate) mod vm;
pub(crate) use agent::Runtime;
pub(crate) use caches::CacheDisks;
#[cfg(test)]
pub(crate) use vm::VmHook;

#[cfg(windows)]
static SERVICE_STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
pub(crate) fn request_service_stop() {
    SERVICE_STOP_REQUESTED.store(true, Ordering::Release);
}
#[cfg(windows)]
pub(crate) fn service_stop_requested() -> bool {
    SERVICE_STOP_REQUESTED.load(Ordering::Acquire)
}

#[cfg(unix)]
/// Prepare a QEMU job's resources for `id` and drop them at once, as a setup
/// attempt that fails right after `Resources::prepare` would.
#[cfg(all(test, unix))]
pub(crate) fn prepare_and_drop_resources(
    config: &crate::engine::Config,
    id: uuid::Uuid,
    workspace: &str,
) -> std::io::Result<()> {
    let staging = config
        .windows
        .as_ref()
        .expect("validated Windows config")
        .layout_root
        .join("staging")
        .join(id.to_string());
    vm::Resources::prepare(config, staging, id, workspace).map(drop)
}

#[cfg(unix)]
pub(crate) fn prepare_entry(path: &Path) -> std::io::Result<()> {
    prep::execute(path).inspect_err(|error| eprintln!("{}", prep_error::error_line(error)))
}
#[cfg(windows)]
pub(crate) fn probe_peers() -> io::Result<()> {
    hvsock::probe_peers()
}
#[cfg(windows)]
pub(crate) fn qualify_prep(config: &Config) -> io::Result<()> {
    trusted_prep::qualify_prep_system(config)
}
#[cfg(windows)]
pub(crate) fn qualify_prep_offline_cargo(config: Config) -> io::Result<()> {
    trusted_prep::offline_cargo_entry(config)
}
#[cfg(windows)]
pub(crate) fn qualify_prep_local_registry(config: Config) -> io::Result<()> {
    trusted_prep::local_registry_entry(config)
}
#[cfg(windows)]
pub(crate) fn qualify_disconnect(config: &Config) -> io::Result<()> {
    if let Some((_, detail)) =
        guest_agent_refusal(config.windows_runtime.guest_agent_readiness().as_ref())
    {
        return Err(io::Error::other(detail));
    }
    vm::qualify_disconnect(config)
}
#[cfg(windows)]
pub(crate) fn ensure_parent_guard(config: &Config) -> io::Result<()> {
    host::ensure_parent_guard(config)
}

#[cfg(test)]
pub(crate) type CommandHook =
    std::sync::Arc<dyn Fn(&str, &[String]) -> io::Result<Output> + Send + Sync>;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VmConfig {
    pub layout_root: PathBuf,
    pub baseline: String,
    #[serde(default = "default_quota")]
    pub overlay_growth_quota_bytes: u64,
    pub memory_mib: u64,
    pub cpu_set: Vec<u32>,
    pub toolchain_version: String,
    #[cfg(windows)]
    pub parent_sha256: String,
    #[cfg(windows)]
    pub prep_cargo_sha256: String,
    #[cfg(windows)]
    #[serde(default)]
    pub guest_agent_sha256: Option<String>,
    #[cfg(windows)]
    #[serde(default = "one_slot")]
    pub concurrent_jobs: u32,
}
/// The guest agent's readiness for the sealed parent is read by the Windows
/// service and its qualification commands; the Unix backend never starts a
/// Windows guest, so only the portable tests use it there.
#[cfg(any(windows, test))]
pub(crate) fn guest_agent_readiness(
    parent_agent_sha256: Option<&str>,
    release_agent_sha256: &str,
) -> WindowsGuestAgentReadiness {
    let valid_hash =
        |hash: &str| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit());
    let release = release_agent_sha256.to_ascii_uppercase();
    let parent = parent_agent_sha256
        .filter(|hash| valid_hash(hash))
        .map(str::to_ascii_uppercase);
    let detail = if !valid_hash(&release) {
        Some(format!(
            "windows_guest_agent_release_hash_invalid release_agent_sha256={release}"
        ))
    } else if parent.is_none() {
        Some(format!("windows_guest_agent_identity_missing parent_agent_sha256=missing release_agent_sha256={release}"))
    } else if parent.as_deref() != Some(release.as_str()) {
        Some(format!(
            "windows_guest_agent_mismatch parent_agent_sha256={} release_agent_sha256={release}",
            parent.as_deref().unwrap()
        ))
    } else {
        None
    };
    WindowsGuestAgentReadiness {
        ready: detail.is_none(),
        parent_agent_sha256: parent,
        release_agent_sha256: release,
        detail,
    }
}

#[cfg(windows)]
fn one_slot() -> u32 {
    1
}
fn default_quota() -> u64 {
    40 << 30
}

impl VmConfig {
    #[cfg(unix)]
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        let vm: Self = toml::from_str(&fs::read_to_string(path)?)
            .map_err(|e| invalid(format!("--vm-config: {e}")))?;
        if !vm.layout_root.is_absolute()
            || !vm.layout_root.to_str().is_some_and(|s| {
                s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
            })
            || !vm
                .baseline
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || vm
                .layout_root
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            || vm.baseline.is_empty()
            || Path::new(&vm.baseline).components().count() != 1
            || !matches!(
                Path::new(&vm.baseline).components().next(),
                Some(Component::Normal(_))
            )
            || vm.overlay_growth_quota_bytes == 0
            || vm.memory_mib == 0
            || vm.memory_mib.checked_add(2048).is_none()
            || vm.cpu_set != [12, 13, 14, 15]
            || vm.toolchain_version.trim().is_empty()
        {
            return Err(invalid("invalid Windows --vm-config: absolute layout_root, baseline file name, positive quota/memory, cpu_set [12,13,14,15] and toolchain_version required"));
        }
        for directory in [&vm.layout_root, &vm.layout_root.join("baselines")] {
            if !fs::symlink_metadata(directory)?.is_dir() {
                return Err(invalid(format!(
                    "Windows baseline directory is not a real directory: {}",
                    directory.display()
                )));
            }
        }
        let baseline = vm.baseline_path();
        let metadata = fs::symlink_metadata(&baseline).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Windows baseline {}: {e}", baseline.display()),
            )
        })?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o7777 != 0o440 {
            return Err(invalid(format!(
                "Windows baseline {} must be a regular file with mode 0440",
                baseline.display()
            )));
        }
        Ok(vm)
    }
    #[cfg(windows)]
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        host::load(path)
    }
    pub(crate) fn baseline_path(&self) -> PathBuf {
        self.layout_root.join("baselines").join(&self.baseline)
    }
    pub(crate) fn caches(&self) -> CacheDisks {
        CacheDisks::new(self)
    }
}

#[cfg(unix)]
pub(crate) fn command(config: &Config, program: &str, args: &[String]) -> io::Result<Output> {
    #[cfg(test)]
    if let Some(run) = &config.windows_command {
        return run(program, args);
    }
    let _ = config;
    Command::new(program).args(args).output()
}

/// Converts one member of the stored Windows collection to the form the Linux
/// backend reports in `workspace_changes`: a '/'-separated path relative to the
/// job's checkout root (the workspace key).
///
/// The guest sees host path P at `C:\m` + P, so a collection member is
/// `m/` followed by the host path without its leading '/'. The guest collects
/// everything under `C:\m`, which also holds sibling checkouts. The Linux
/// backend runs `git status` in the checkout root, and sibling checkouts sit
/// beside the checkout rather than inside it, so Linux never reports them.
/// Members outside the checkout root are therefore left out here too.
/// The stored collection keeps the `m/` form because other readers of the
/// job directory depend on it; only the reported list is converted.
fn checkout_relative(key: &str, member: &str) -> Option<String> {
    let host = member.strip_prefix("m/")?;
    let root = key.trim_matches('/');
    let relative = if root.is_empty() {
        host
    } else {
        host.strip_prefix(root)?.strip_prefix('/')?
    };
    (!relative.is_empty()).then(|| relative.to_owned())
}

pub(crate) struct WindowsBackend;
impl Backend for WindowsBackend {
    fn validate(&self, config: &Config) -> io::Result<()> {
        host::validate(config)
    }
    fn initialize(&self, config: &Config) -> io::Result<()> {
        host::grant_mirrors(config)
    }
    fn materialize(&self, config: &Config, trees: &Trees, setup: &Setup) -> io::Result<Prepared> {
        agent::materialize(config, trees, setup)
    }
    fn launch(
        &self,
        _config: &Config,
        _trees: &Trees,
        setup: &Setup,
        prepared: &Prepared,
        _weight: u32,
    ) -> io::Result<Launch> {
        let execution = prepared
            .vm
            .as_ref()
            .ok_or_else(|| invalid("Windows VM was not prepared"))?
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| invalid("Windows VM ownership already transferred"))?;
        Ok(Launch {
            process: Some(platform::ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(setup.id),
            }),
            durable_output: false,
            execution,
            // A VM job never mounts a fill's write sockets.
            #[cfg(unix)]
            cache_ledgers: Vec::new(),
        })
    }
    fn cancel(&self, config: &Config, job: &Job, hard: bool) -> io::Result<()> {
        if job.pending() {
            config.windows_runtime.abort_setup(job.id);
            return Ok(());
        }
        if config.windows_runtime.cancel(job.id, hard) {
            return Ok(());
        }
        let _ = (config, job, hard);
        Ok(())
    }
    fn unit_alive(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        if let Some(unit) = process.and_then(|p| p.unit) {
            if let Some(alive) = config.windows_runtime.alive(unit) {
                return Ok(Some(alive));
            }
        }
        #[cfg(any(target_os = "linux", test))]
        if let Some(unit) = process.and_then(|p| p.unit) {
            return crate::systemd::alive(&self.unit_name(config, unit), |args| {
                systemctl(config, args)
            })
            .map(Some);
        }
        let _ = (config, process);
        Ok(None)
    }
    fn collect_changes(
        &self,
        _config: &Config,
        trees: &Trees,
        job: &Job,
    ) -> io::Result<Option<Vec<String>>> {
        let path = trees
            .state
            .join("jobs")
            .join(job.id.to_string())
            .join("windows-changes.json");
        if !path.exists() {
            return Ok(None);
        }
        let collected: Vec<String> = read_json(&path)?;
        Ok(Some(
            collected
                .iter()
                .filter_map(|member| checkout_relative(job.key(), member))
                .collect(),
        ))
    }
    fn reconcile(&self, config: &Config, _interrupted: &[Job]) -> io::Result<Vec<Uuid>> {
        #[cfg(any(target_os = "linux", test))]
        {
            // Every VM unit this instance starts is named
            // motor-windows-<instance>-<job>.service. A unit can be booting before
            // its job record names it, so after a restart, stop every unit with
            // this instance's prefix instead of only the units the records name.
            let pattern = self.unit_pattern(config);
            let output = systemctl(
                config,
                &[
                    "list-units",
                    "--all",
                    "--type=service",
                    "--no-legend",
                    "--plain",
                    "--no-pager",
                    &pattern,
                ],
            )?;
            if !output.status.success() {
                return Err(io::Error::other(
                    "cannot inventory Windows units during reconcile",
                ));
            }
            let prefix = pattern.trim_end_matches('*');
            for unit in std::str::from_utf8(&output.stdout)
                .map_err(invalid)?
                .lines()
                .filter_map(|line| line.split_whitespace().next())
            {
                if unit.starts_with(prefix) && unit.ends_with(".service") {
                    let stopped = systemctl(config, &["stop", unit])?;
                    if !stopped.status.success() {
                        return Err(io::Error::other(format!(
                            "cannot stop Windows unit {unit} during reconcile"
                        )));
                    }
                }
            }
        }
        #[cfg(windows)]
        vm::cleanup_orphans(config)?;
        config
            .windows
            .as_ref()
            .expect("validated Windows config")
            .caches()
            .reconcile()?;
        #[cfg(unix)]
        vm::cleanup_orphans(config)?;
        Ok(Vec::new())
    }
}

#[cfg(any(windows, test))]
pub(crate) fn guest_agent_refusal(
    readiness: Option<&WindowsGuestAgentReadiness>,
) -> Option<(RefusalReason, String)> {
    match readiness {
        Some(readiness) if !readiness.ready => Some((
            RefusalReason::RequestRejected,
            readiness
                .detail
                .clone()
                .unwrap_or_else(|| "windows guest agent is not ready".into()),
        )),
        None => Some((
            RefusalReason::RequestRejected,
            "windows_guest_agent_not_ready identity_check_missing".into(),
        )),
        Some(_) => None,
    }
}

#[cfg(test)]
mod guest_agent_readiness_tests {
    use super::*;

    #[test]
    fn guest_agent_readiness_covers_match_mismatch_and_missing_record() {
        let release = "A".repeat(64);
        let matched = guest_agent_readiness(Some(&release), &release);
        assert!(matched.ready);
        assert_eq!(
            matched.parent_agent_sha256.as_deref(),
            Some(release.as_str())
        );
        assert_eq!(matched.release_agent_sha256, release);
        assert!(matched.detail.is_none());

        let parent = "B".repeat(64);
        let mismatch = guest_agent_readiness(Some(&parent), &"A".repeat(64));
        assert!(!mismatch.ready);
        let detail = mismatch.detail.unwrap();
        assert!(detail.contains(&format!("parent_agent_sha256={parent}")));
        assert!(detail.contains(&format!("release_agent_sha256={}", "A".repeat(64))));

        let missing = guest_agent_readiness(None, &"A".repeat(64));
        assert!(!missing.ready);
        let detail = missing.detail.unwrap();
        assert!(detail.contains("parent_agent_sha256=missing"));
        assert!(detail.contains(&format!("release_agent_sha256={}", "A".repeat(64))));
    }

    #[test]
    fn guest_agent_mismatch_refuses_before_job_start_with_both_hashes() {
        let parent = "B".repeat(64);
        let release = "A".repeat(64);
        let readiness = guest_agent_readiness(Some(&parent), &release);
        let refusal = guest_agent_refusal(Some(&readiness)).unwrap();
        assert_eq!(refusal.0, RefusalReason::RequestRejected);
        assert!(refusal.1.contains(&parent));
        assert!(refusal.1.contains(&release));
        assert!(
            guest_agent_refusal(Some(&guest_agent_readiness(Some(&release), &release))).is_none()
        );
        assert!(guest_agent_refusal(None).is_some());
    }
}
