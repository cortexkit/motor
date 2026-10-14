use super::*;
use std::{
    os::unix::net::UnixListener,
    process::{Child, ExitStatus, Stdio},
};

pub(crate) trait UnitChild: Send {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn stop(&mut self) -> io::Result<()>;
    #[cfg(test)]
    fn agent_stream(&mut self) -> Option<std::os::unix::net::UnixStream> {
        None
    }
}
struct SystemdChild {
    child: Child,
}
impl UnitChild for SystemdChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
    fn stop(&mut self) -> io::Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child.kill()?;
        }
        let _ = self.child.wait()?;
        Ok(())
    }
}
struct NotBooted;
impl UnitChild for NotBooted {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(None)
    }
    fn stop(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[cfg(test)]
pub(crate) type VmHook =
    std::sync::Arc<dyn Fn(&[String], &str, &Path) -> io::Result<Box<dyn UnitChild>> + Send + Sync>;

pub(super) struct Resources {
    config: Config,
    pub(super) id: Uuid,
    workspace: String,
    pub staging: PathBuf,
    overlay: PathBuf,
    cache: PathBuf,
    socket_dir: PathBuf,
    booted: bool,
    attached: bool,
    cleaned: bool,
    cleanup_attempted: bool,
    cleanup_on_drop: bool,
    pub child: Box<dyn UnitChild>,
}
#[derive(Debug)]
struct PathIoError {
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
}
impl std::fmt::Display for PathIoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} (Windows {} {})",
            self.source,
            self.operation,
            self.path.display()
        )
    }
}
impl std::error::Error for PathIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}
pub(super) fn path_error(operation: &'static str, path: &Path, source: io::Error) -> io::Error {
    io::Error::new(
        source.kind(),
        PathIoError {
            operation,
            path: path.into(),
            source,
        },
    )
}

// The preparation executable runs under a different OS account, motor-prep.
// Its directories need a recursive permission grant so the runner can traverse
// them and delete their contents after preparation or VM exit. A windows_command
// hook can still execute the configured windows_prep_executable and must not
// suppress that grant. Only in-process mocks without that binary create files
// as the runner and can avoid the sudo permission update.
#[cfg(any(target_os = "linux", test))]
fn restore_staging_access_with(
    config: &Config,
    staging: &Path,
    restore: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    #[cfg(test)]
    if config.windows_command.is_some() && config.windows_prep_executable.is_none() {
        return Ok(());
    }
    #[cfg(not(test))]
    let _ = config;
    restore(staging).map_err(|error| path_error("restore runner access to", staging, error))
}
pub(super) fn restore_staging_access(config: &Config, staging: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        restore_staging_access_with(config, staging, crate::isolation::restore_runner_access)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (config, staging);
        Ok(())
    }
}

fn checked(config: &Config, program: &str, args: &[String]) -> io::Result<()> {
    let output = command(config, program, args)
        .map_err(|error| path_error("run command", Path::new(program), error))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}
pub(super) fn acl(config: &Config, grant: &str, path: &Path) -> io::Result<()> {
    checked(
        config,
        "/usr/bin/sudo",
        &[
            "-n".into(),
            "/usr/bin/setfacl".into(),
            "-m".into(),
            grant.into(),
            path.to_string_lossy().into_owned(),
        ],
    )
    .map_err(|error| path_error("grant ACL on", path, error))
}
impl Resources {
    pub(super) fn prepare(
        config: &Config,
        staging: PathBuf,
        id: Uuid,
        workspace: &str,
    ) -> io::Result<Self> {
        let vm = config.windows.as_ref().expect("validated Windows config");
        let mut resources = Self {
            config: config.clone(),
            id,
            workspace: workspace.into(),
            staging,
            overlay: vm
                .layout_root
                .join("vm/overlays")
                .join(format!("{id}.qcow2")),
            cache: vm.caches().disk(workspace),
            socket_dir: vm.layout_root.join("vm/sockets").join(id.to_string()),
            booted: false,
            attached: false,
            cleaned: false,
            cleanup_attempted: false,
            cleanup_on_drop: true,
            child: Box::new(NotBooted),
        };
        fs::create_dir(&resources.staging)
            .map_err(|error| path_error("create staging directory", &resources.staging, error))?;
        fs::set_permissions(&resources.staging, fs::Permissions::from_mode(0o700))
            .map_err(|error| path_error("chmod staging directory", &resources.staging, error))?;
        acl(config, "u:motor-prep:rwx", &resources.staging)?;
        fs::create_dir(&resources.socket_dir)
            .map_err(|error| path_error("create socket directory", &resources.socket_dir, error))?;
        fs::set_permissions(&resources.socket_dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| path_error("chmod socket directory", &resources.socket_dir, error))?;
        acl(config, "u:motor-qemu:x", &resources.socket_dir)?;
        checked(
            config,
            "/usr/bin/qemu-img",
            &[
                "create".into(),
                "-f".into(),
                "qcow2".into(),
                "-F".into(),
                "qcow2".into(),
                "-b".into(),
                vm.baseline_path().to_string_lossy().into_owned(),
                resources.overlay.to_string_lossy().into_owned(),
            ],
        )?;
        acl(config, "u:motor-qemu:rw", &resources.overlay)?;
        if !resources.cache.exists() {
            checked(
                config,
                "/usr/bin/qemu-img",
                &[
                    "create".into(),
                    "-f".into(),
                    "qcow2".into(),
                    resources.cache.to_string_lossy().into_owned(),
                    "40G".into(),
                ],
            )?;
        }
        acl(config, "u:motor-qemu:rw", &resources.cache)?;
        vm.caches()
            .attach(workspace, (config.clock)())
            .map_err(|error| path_error("mark cache attached", &resources.cache, error))?;
        resources.attached = true;
        Ok(resources)
    }
    pub(super) fn agent_path(&self) -> PathBuf {
        self.socket_dir.join("agent.sock")
    }
    pub(super) fn listen(&self, config: &Config) -> io::Result<Option<UnixListener>> {
        #[cfg(test)]
        if config.windows_socket_pair {
            return Ok(None);
        }
        let path = self.agent_path();
        let listener = UnixListener::bind(&path)
            .map_err(|error| path_error("bind agent socket", &path, error))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| path_error("chmod agent socket", &path, error))?;
        acl(config, "u:motor-qemu:rw", &path)?;
        listener
            .set_nonblocking(true)
            .map_err(|error| path_error("set agent socket nonblocking", &path, error))?;
        Ok(Some(listener))
    }
    pub(super) fn boot(&mut self, config: &Config, nonce: &str) -> io::Result<()> {
        let args = qemu_args(
            config,
            self.id,
            &self.overlay,
            &self.cache,
            &self.socket_dir,
            nonce,
        );
        self.booted = true;
        #[cfg(test)]
        if let Some(hook) = &config.windows_vm {
            self.child = hook(&args, nonce, &self.socket_dir.join("agent.sock"))?;
            return Ok(());
        }
        if !cfg!(target_os = "linux") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows VM host requires Linux",
            ));
        }
        self.child = Box::new(SystemdChild {
            child: Command::new("/usr/bin/sudo")
                .args(args)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|error| path_error("boot VM for overlay", &self.overlay, error))?,
        });
        Ok(())
    }
    pub(super) fn complete(&mut self, known: bool) -> io::Result<()> {
        self.config
            .windows
            .as_ref()
            .unwrap()
            .caches()
            .complete(&self.workspace, (self.config.clock)(), known, true, true)
            .map_err(|error| path_error("complete cache disk", &self.cache, error))?;
        self.attached = false;
        Ok(())
    }
}
impl Resources {
    #[cfg(test)]
    pub(super) fn record_cache_timings(&self, timings: motor_winagent::CacheTimings) {
        self.config
            .windows_runtime
            .cache_timings
            .lock()
            .unwrap()
            .insert(self.id, timings);
    }
    fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.cleanup_attempted = true;
        if self.booted {
            let exited = self.child.try_wait()?.is_some();
            if !exited {
                #[cfg(any(target_os = "linux", test))]
                {
                    let unit = WindowsBackend.unit_name(&self.config, self.id);
                    let killed = systemctl(
                        &self.config,
                        &["kill", "--kill-whom=all", "--signal=KILL", &unit],
                    )?;
                    let stopped = systemctl(&self.config, &["stop", &unit])?;
                    if (!killed.status.success() || !stopped.status.success())
                        && crate::systemd::alive(&unit, |args| systemctl(&self.config, args))?
                    {
                        return Err(io::Error::other(format!(
                            "Windows VM cleanup failed for live unit {unit}: kill {}: {}; stop {}: {}",
                            killed.status,
                            String::from_utf8_lossy(&killed.stderr),
                            stopped.status,
                            String::from_utf8_lossy(&stopped.stderr),
                        )));
                    }
                    // --collect can unload the unit between kill and stop. An
                    // unsuccessful command is harmless only after systemd has
                    // confirmed the unit is no longer alive; otherwise retain
                    // every attached file for a maintenance retry.
                }
            }
            if let Err(error) = self.child.stop() {
                // A stop can fail after the child has exited. Remember that
                // verified exit, but do not treat a live child as stopped.
                if exited || self.child.try_wait().ok().flatten().is_some() {
                    self.booted = false;
                }
                return Err(error);
            }
            self.booted = false;
        }
        if self.attached {
            self.config
                .windows
                .as_ref()
                .unwrap()
                .caches()
                .complete(&self.workspace, (self.config.clock)(), false, false, false)
                .map_err(|error| path_error("discard attached cache disk", &self.cache, error))?;
            self.attached = false;
        }
        {
            let path = &self.overlay;
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(path_error("remove overlay", path, e)),
            }
        }
        match fs::remove_dir_all(&self.socket_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(path_error("remove socket directory", &self.socket_dir, e)),
        }
        if self.staging.exists() {
            restore_staging_access(&self.config, &self.staging)?;
        }
        match fs::remove_dir_all(&self.staging) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(path_error("remove staging directory", &self.staging, e)),
        }
        self.cleaned = true;
        Ok(())
    }
    pub(super) fn finalize(&mut self) -> io::Result<()> {
        self.cleanup()
    }
    pub(super) fn finalize_known(&mut self) -> io::Result<()> {
        // Keep the on-disk attachment marker until all cleanup succeeds. A
        // cleanup failure makes the cache suspect even when the command's exit
        // is known. Do not expose a reusable disk before finalization can fail.
        self.attached = false;
        let result = self.cleanup().and_then(|()| self.complete(true));
        if result.is_err() {
            self.cleaned = false;
            // A failed stop can leave QEMU using the cache. Retain the attachment
            // so cleanup retries discard it only after the unit is gone.
            self.attached = true;
            if !self.booted || self.child.try_wait()?.is_some() {
                self.complete(false)?;
            }
        }
        result
    }
}
impl Drop for Resources {
    fn drop(&mut self) {
        if !self.cleanup_on_drop || self.cleaned {
            return;
        }
        if !self.cleanup_attempted {
            match self.cleanup() {
                Ok(()) => return,
                Err(error) => {
                    tracing::warn!(job_id = %self.id, %error, "Windows VM cleanup deferred to maintenance");
                }
            }
        }
        let runtime = self.config.windows_runtime.clone();
        let mut config = self.config.clone();
        // Pending resources belong to the runtime. Their cleanup configuration
        // must not keep that same runtime alive through an ownership cycle.
        config.windows_runtime = Default::default();
        let pending = Self {
            config,
            id: self.id,
            workspace: std::mem::take(&mut self.workspace),
            staging: std::mem::take(&mut self.staging),
            overlay: std::mem::take(&mut self.overlay),
            cache: std::mem::take(&mut self.cache),
            socket_dir: std::mem::take(&mut self.socket_dir),
            booted: self.booted,
            attached: self.attached,
            cleaned: false,
            cleanup_attempted: true,
            cleanup_on_drop: false,
            child: std::mem::replace(&mut self.child, Box::new(NotBooted)),
        };
        runtime.defer_cleanup(pending);
    }
}

pub(super) fn qemu_args(
    config: &Config,
    id: Uuid,
    overlay: &Path,
    cache: &Path,
    socket_dir: &Path,
    nonce: &str,
) -> Vec<String> {
    let vm = config.windows.as_ref().unwrap();
    let unit = WindowsBackend.unit_name(config, id);
    let slot = vm.layout_root.parent().unwrap().join("slot.lock");
    vec![
        "-n".into(),
        "/usr/bin/systemd-run".into(),
        "--wait".into(),
        "--collect".into(),
        format!("--unit={unit}"),
        "--property=User=motor-qemu".into(),
        "--property=SupplementaryGroups=kvm".into(),
        "--property=ProtectSystem=strict".into(),
        "--property=ProtectHome=yes".into(),
        "--property=NoNewPrivileges=yes".into(),
        "--property=PrivateTmp=yes".into(),
        "--property=PrivateNetwork=yes".into(),
        "--property=KillMode=control-group".into(),
        "--property=TimeoutStopSec=5s".into(),
        "--property=SendSIGKILL=yes".into(),
        "--property=DevicePolicy=closed".into(),
        "--property=DeviceAllow=/dev/kvm rw".into(),
        "--property=AllowedCPUs=12-15".into(),
        "--property=CPUQuota=400%".into(),
        format!("--property=MemoryMax={}M", vm.memory_mib + 2048),
        "--property=TasksMax=256".into(),
        format!("--property=LimitFSIZE={}", vm.overlay_growth_quota_bytes),
        format!(
            "--property=ReadWritePaths={} {} {}",
            overlay.display(),
            cache.display(),
            socket_dir.display()
        ),
        "--".into(),
        "/usr/bin/flock".into(),
        "-n".into(),
        "-E".into(),
        "75".into(),
        slot.to_string_lossy().into_owned(),
        "/usr/bin/qemu-system-x86_64".into(),
        "-enable-kvm".into(),
        "-nodefaults".into(),
        "-nic".into(),
        "none".into(),
        "-sandbox".into(),
        "on".into(),
        "-display".into(),
        "none".into(),
        "-cpu".into(),
        "host".into(),
        "-smp".into(),
        "4".into(),
        "-m".into(),
        vm.memory_mib.to_string(),
        "-drive".into(),
        format!(
            "file={},format=qcow2,if=none,id=motor-os",
            overlay.display()
        ),
        // Create both disks explicitly in OS/cache order. Mixing an implicit
        // OS device with an explicit cache can make firmware boot the cache.
        "-device".into(),
        "virtio-blk-pci,drive=motor-os,bootindex=0".into(),
        "-drive".into(),
        format!(
            "file={},format=qcow2,if=none,id=motor-cache",
            cache.display()
        ),
        "-device".into(),
        format!(
            "virtio-blk-pci,drive=motor-cache,serial={}",
            motor_winagent::cache::CACHE_DISK_SERIAL
        ),
        "-chardev".into(),
        format!(
            "socket,id=agent,path={}",
            socket_dir.join("agent.sock").display()
        ),
        "-device".into(),
        "virtio-serial-pci".into(),
        "-device".into(),
        "virtserialport,chardev=agent,name=org.cortexkit.motor.winagent.0".into(),
        "-smbios".into(),
        format!("type=11,value=motor.boot_nonce={nonce}"),
    ]
}

// WindowsBackend::reconcile first stops systemd units named
// motor-windows-<instance>-*, where <instance> is this runner's configured name.
// Only then remove its job overlays, socket directories and staging trees, so no
// VM or preparation process still uses those files. Removing an unexpected
// symlink deletes the link itself, not the files at its destination.
pub(super) fn cleanup_orphans(config: &Config) -> io::Result<()> {
    let root = &config.windows.as_ref().unwrap().layout_root;
    for (name, disks) in [
        ("vm/overlays", true),
        ("vm/sockets", false),
        ("staging", false),
    ] {
        let directory = root.join(name);
        for entry in fs::read_dir(&directory)
            .map_err(|error| path_error("read orphan directory", &directory, error))?
        {
            let path = entry
                .map_err(|error| path_error("read orphan directory entry", &directory, error))?
                .path();
            let id = if disks {
                path.file_stem()
            } else {
                path.file_name()
            };
            if !id
                .and_then(|id| id.to_str())
                .is_some_and(|id| Uuid::parse_str(id).is_ok())
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| path_error("stat orphan", &path, error))?;
            if metadata.is_dir() {
                restore_staging_access(config, &path)?;
                fs::remove_dir_all(&path)
                    .map_err(|error| path_error("remove orphan directory", &path, error))?;
            } else {
                fs::remove_file(&path)
                    .map_err(|error| path_error("remove orphan file", &path, error))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed_config() -> Config {
        let mut config = Config::new(
            cortexkit_test_support::scratch_root(),
            "windows-access-test",
        );
        config.windows_command = Some(std::sync::Arc::new(|_, _| {
            unreachable!("observer should not run in access test")
        }));
        config
    }

    #[test]
    fn command_observer_with_operator_preparer_restores_staging_access() {
        let mut config = observed_config();
        config.windows_prep_executable = Some(config.state_root.join("operator-runner"));
        let path = config.state_root.join("staging");
        let mut called = false;
        restore_staging_access_with(&config, &path, |actual| {
            assert_eq!(actual, path);
            called = true;
            Ok(())
        })
        .unwrap();
        assert!(
            called,
            "a real preparer's files must be reclaimed despite a command observer"
        );
    }

    #[test]
    fn ordinary_mock_preparation_does_not_invoke_privileged_restore() {
        let config = observed_config();
        restore_staging_access_with(&config, &config.state_root, |_| {
            panic!("ordinary in-process preparation must not invoke sudo")
        })
        .unwrap();
    }

    #[test]
    fn staging_access_errors_name_path_and_preserve_kind_and_errno_source() {
        let mut config = observed_config();
        config.windows_prep_executable = Some(config.state_root.join("operator-runner"));
        let path = config.state_root.join("staging/denied");
        let error = restore_staging_access_with(&config, &path, |_| {
            Err(io::Error::from_raw_os_error(libc::EACCES))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
        assert!(
            error.to_string().contains("restore runner access"),
            "{error}"
        );
        let original = error
            .get_ref()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(original.raw_os_error(), Some(libc::EACCES));
    }
}
