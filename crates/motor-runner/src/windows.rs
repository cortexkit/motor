//! Each job executes in a disposable Windows VM, not in a Linux host shell.
//! The motor-prep account prepares its files, and motor-qemu runs the VM. The
//! host saves the Started job record on disk before asking the guest to run the
//! command, so a failed record write or a crash before that request starts nothing.
use crate::{backend::*, engine::Config, platform, storage::*, workspace::Trees};
use motor_protocol::Uuid;
use serde::Deserialize;
#[cfg(any(unix, test))]
use std::process::Output;
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
#[cfg(windows)]
mod trusted_prep;
#[cfg(unix)]
pub(crate) mod vm;
#[cfg(windows)]
#[path = "windows/hyperv.rs"]
pub(crate) mod vm;
pub(crate) use agent::{PreparedVm, Runtime};
pub(crate) use caches::CacheDisks;
#[cfg(test)]
pub(crate) use vm::VmHook;

pub(crate) fn prepare_entry(path: &Path) -> std::io::Result<()> {
    prep::execute(path)
}
#[cfg(windows)]
pub(crate) fn probe_peers() -> io::Result<()> {
    hvsock::probe_peers()
}
#[cfg(windows)]
pub(crate) fn qualify_prep(config: &Config) -> io::Result<()> {
    trusted_prep::qualify(config)
}
#[cfg(windows)]
pub(crate) fn prep_probe(args: &[String]) -> io::Result<()> {
    trusted_prep::probe(args)
}
#[cfg(windows)]
pub(crate) fn qualify_disconnect(config: &Config) -> io::Result<()> {
    vm::qualify_disconnect(config)
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
    #[serde(default = "one_slot")]
    pub concurrent_jobs: u32,
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
            .windows
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
            execution: Box::new(execution),
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
    fn collect_changes(&self, trees: &Trees, job: &Job) -> io::Result<Option<Vec<String>>> {
        let path = trees
            .state
            .join("jobs")
            .join(job.id.to_string())
            .join("windows-changes.json");
        if path.exists() {
            read_json(&path).map(Some)
        } else {
            Ok(None)
        }
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
