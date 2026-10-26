//! Ordinary-file Firecracker snapshots, independent of job UFFD/hugepage VMs.
mod activation;
mod capacity;
use super::{fs, process, tenant_network};
use crate::{
    config::Config,
    tenant::{
        Backend, Registry, ResizeActivation, SnapshotKey, StartRefusal, StartRefusalCase,
        StartResult, REGISTRY,
    },
    Error, Result,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufReader, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Running {
    child: Child,
    disk: File,
    root: PathBuf,
    group: PathBuf,
    paused: bool,
    attached: bool,
    restored: bool,
    snapshot_uncertain: bool,
}
pub struct LinuxTenants {
    config: Config,
    groups: PathBuf,
    running: BTreeMap<String, Running>,
}
impl LinuxTenants {
    pub fn new(config: &Config, groups: PathBuf) -> Self {
        Self {
            config: config.clone(),
            groups: groups.join("tenants"),
            running: BTreeMap::new(),
        }
    }
    fn uid(&self, r: &Registry) -> Result<u32> {
        let uid = 400000 + u32::from(r.tap_index);
        if (self.config.uid_base..self.config.uid_base + self.config.slots.len() as u32)
            .contains(&uid)
            || (self.config.job_account_range.base
                ..self.config.job_account_range.base + self.config.job_account_range.count)
                .contains(&uid)
            || self.config.allowed_uids.contains(&uid)
        {
            return Err(Error("tenant uid overlaps another account range".into()));
        }
        process::refuse_passwd_entries(uid, 1)?;
        Ok(uid)
    }
    pub fn reconcile(&mut self) -> Result<()> {
        // A helper restart is a loss boundary, not adoption of an unverified pid.
        // Kill old VMMs before unmounting any resource, but never remove disk data.
        if self.groups.exists() {
            for entry in std::fs::read_dir(&self.groups)? {
                let path = entry?.path();
                if path.is_dir() {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| Error("invalid tenant cgroup name".into()))?;
                    let registry = self.registry(name)?;
                    self.mark_unclean(&registry, "helper_restart")?;
                    std::fs::write(path.join("cgroup.kill"), "1")?;
                    wait_empty(&path)?;
                    std::fs::remove_dir(path)?;
                }
            }
        }
        let jail = Path::new(super::BUILD_ROOT).join("jail/tenants");
        if jail.exists() {
            for entry in std::fs::read_dir(&jail)? {
                cleanup_boundary(&entry?.path())?;
            }
        }
        // A disk may still have its jail uid after a helper crash. All registry
        // ancestors remain root-controlled; restore ownership only after kill.
        let registries = self.registries()?;
        tenant_network::install(&registries, &self.config)?;
        for r in registries {
            for ancestor in r.disk.parent().unwrap().ancestors() {
                fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
            }
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&r.disk)?;
            if !file.metadata()?.is_file() {
                return Err(Error("tenant disk is not a regular file".into()));
            }
            fs::chown(&file, 0)?;
        }
        Ok(())
    }
    fn registries(&mut self) -> Result<Vec<Registry>> {
        let mut values = Vec::new();
        let entries = match std::fs::read_dir(REGISTRY) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(values),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            let r = self.registry(name)?;
            if values
                .iter()
                .any(|v: &Registry| v.tap_index == r.tap_index || v.disk == r.disk)
            {
                return Err(Error("duplicate tenant tap or disk".into()));
            }
            values.push(r);
        }
        Ok(values)
    }
    fn vm(&mut self, tenant: &str) -> Result<&mut Running> {
        if !self.running(tenant) {
            return Err(Error("tenant is not running".into()));
        }
        Ok(self.running.get_mut(tenant).unwrap())
    }
    fn agent(&mut self, tenant: &str, body: Value) -> Result<()> {
        let fd = self.relay(tenant, 54)?;
        exchange_agent(UnixStream::from(fd), &body, Duration::from_secs(30))
    }
}

pub(crate) fn exchange_agent(
    mut stream: UnixStream,
    body: &Value,
    timeout: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let bytes = serde_json::to_vec(body).map_err(|e| Error(e.to_string()))?;
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    let mut header = [0; 4];
    read_deadline(&mut stream, &mut header, deadline)?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > 1048576 {
        return Err(Error("invalid tenant agent frame length".into()));
    }
    let mut bytes = vec![0; len];
    read_deadline(&mut stream, &mut bytes, deadline)?;
    let reply: Value = serde_json::from_slice(&bytes).map_err(|e| Error(e.to_string()))?;
    if reply["ok"] != true {
        return Err(Error(format!("tenant agent refused: {}", reply["error"])));
    }
    Ok(())
}
fn read_deadline(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: std::time::Instant,
) -> Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| Error("tenant agent deadline elapsed".into()))?;
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(bytes)?;
        if n == 0 {
            return Err(Error("tenant agent closed before reply".into()));
        }
        bytes = &mut bytes[n..];
    }
    Ok(())
}
impl Backend for LinuxTenants {
    fn check_start(&mut self, tenant: &str) -> std::result::Result<(), StartRefusal> {
        capacity::check_journal(&Path::new(crate::tenant::TENANTS).join(tenant))?;
        let capacity = capacity::read_capacity(Path::new(capacity::CONFIG))?;
        let registries = self
            .registries()
            .map_err(|error| crate::tenant::start_refused(StartRefusalCase::Fit, error))?;
        let meminfo = std::fs::read_to_string("/proc/meminfo")
            .map_err(|error| crate::tenant::start_refused(StartRefusalCase::Fit, error))?;
        capacity.check_fit(&registries, &meminfo)?;
        let output = Command::new("systemctl")
            .args(["show", "-p", "LoadState,MemoryMax", "motor-jobs.slice"])
            .output()
            .map_err(|error| crate::tenant::start_refused(StartRefusalCase::LiveCap, error))?;
        let properties = String::from_utf8_lossy(&output.stdout);
        // systemctl may exit nonzero for an absent unit. Only its explicit
        // not-found state can use the exact-drop-in alternative below.
        if !output.status.success() && !properties.lines().any(|line| line == "LoadState=not-found")
        {
            return Err(crate::tenant::start_refused(
                StartRefusalCase::LiveCap,
                format!(
                    "systemctl show failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            ));
        }
        capacity.check_live_cap(&properties, || {
            std::fs::read(capacity::DROP_IN).map_err(Error::from)
        })
    }
    fn unclean(&mut self, registry: &Registry) -> Result<bool> {
        let path = registry.generation.with_file_name("unclean-stop.json");
        match fs::trusted_file(&path) {
            Ok(file) => {
                let _: Value = serde_json::from_reader(file)
                    .map_err(|e| Error(format!("invalid unclean-stop record: {e}")))?;
                Ok(true)
            }
            Err(_) if !path.exists() => Ok(false),
            Err(error) => Err(error),
        }
    }
    fn mark_unclean(&mut self, registry: &Registry, reason: &str) -> Result<()> {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        durable_write(
            &registry.generation.with_file_name("unclean-stop.json"),
            &serde_json::to_vec(
                &json!({"at_ms":at_ms,"reason":reason.chars().take(1024).collect::<String>()}),
            )
            .map_err(|e| Error(e.to_string()))?,
        )
    }
    fn prepare_stop(&mut self, tenant: &str) -> Result<()> {
        let fd = self.relay(tenant, 54)?;
        exchange_agent(
            UnixStream::from(fd),
            &json!({"op":"prepare_snapshot","kind":"suspend"}),
            Duration::from_secs(15),
        )
    }
    fn recovery_complete(&mut self, tenant: &str, registry: &Registry) -> Result<()> {
        if !self.unclean(registry)? {
            return Ok(());
        }
        let generation = self.key(registry)?.generation;
        self.agent(
            tenant,
            json!({"op":"recovery_status","generation":generation}),
        )?;
        let path = registry.generation.with_file_name("unclean-stop.json");
        std::fs::remove_file(&path)?;
        File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    }
    fn was_running(&mut self, registry: &Registry) -> Result<bool> {
        running_marker_present(&running_marker(registry))
    }
    fn clear_running(&mut self, registry: &Registry) -> Result<()> {
        remove_running_marker(&running_marker(registry))
    }
    fn running_tenants(&mut self) -> Vec<String> {
        self.running.keys().cloned().collect()
    }
    fn metrics(&mut self, tenant: &str, registry: &Registry) -> Result<(u64, u64)> {
        let memory = std::fs::read_to_string(self.vm(tenant)?.group.join("memory.current"))?
            .trim()
            .parse::<u64>()
            .map_err(|_| Error("invalid tenant memory sample".into()))?;
        Ok((memory, tenant_network::egress_bytes(registry)?))
    }
    fn registry(&mut self, tenant: &str) -> Result<Registry> {
        crate::tenant::validate_name(tenant)?;
        // Validate before even opening the registry directory. trusted_file
        // rejects symlinks in every component as well as writable ancestors.
        let r: Registry = serde_json::from_reader(fs::trusted_file(
            &Path::new(REGISTRY).join(format!("{tenant}.json")),
        )?)
        .map_err(|e| Error(e.to_string()))?;
        r.validate(tenant)?;
        Ok(r)
    }
    fn key(&mut self, r: &Registry) -> Result<SnapshotKey> {
        let mut key: SnapshotKey =
            serde_json::from_reader(fs::trusted_file(&r.image().join("manifest.json"))?)
                .map_err(|e| Error(e.to_string()))?;
        if key.version != r.image_version {
            return Err(Error("tenant image manifest version mismatch".into()));
        }
        for digest in [&key.kernel_sha256, &key.rootfs_sha256] {
            if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error("invalid image digest".into()));
            }
        }
        let mut generation = String::new();
        fs::trusted_file(&r.generation)?.read_to_string(&mut generation)?;
        key.generation = generation
            .trim()
            .parse()
            .map_err(|_| Error("invalid generation".into()))?;
        Ok(key)
    }
    fn saved_key(&mut self, r: &Registry) -> Result<Option<SnapshotKey>> {
        if !r.suspend.join("key.json").exists() {
            return Ok(None);
        }
        Ok(Some(
            serde_json::from_reader(fs::trusted_file(&r.suspend.join("key.json"))?)
                .map_err(|e| Error(e.to_string()))?,
        ))
    }
    fn running(&mut self, tenant: &str) -> bool {
        self.running
            .get_mut(tenant)
            .is_some_and(|v| matches!(v.child.try_wait(), Ok(None)))
    }
    fn snapshot_uncertain(&self, tenant: &str) -> bool {
        self.running
            .get(tenant)
            .is_some_and(|vm| vm.snapshot_uncertain)
    }
    fn was_restored(&self, tenant: &str) -> bool {
        self.running.get(tenant).is_some_and(|vm| vm.restored)
    }
    fn load(&mut self, tenant: &str, r: &Registry, saved: bool) -> StartResult<()> {
        if self.running(tenant) {
            return Err(Error("tenant already running".into()).into());
        }
        if self.running.contains_key(tenant) {
            self.mark_unclean(r, "vm_process_lost")?;
        }
        self.stop(tenant)?;
        let uid = self.uid(r)?;
        let disk = fs::trusted_file(&r.disk)?;
        if disk.metadata()?.len() != r.disk_quota_bytes {
            return Err(Error("tenant disk size mismatch".into()).into());
        }
        if unsafe { libc::flock(disk.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error("tenant disk already attached".into()).into());
        }
        let boundary = Path::new(super::BUILD_ROOT).join(format!("jail/tenants/{tenant}"));
        let id = format!("tenant-{}", r.tap_index);
        let root = boundary.join(format!("firecracker/{id}/root"));
        let group = self.groups.join(&id);
        let state = if saved { &r.suspend } else { &r.base() };
        let prefix = if saved { "suspend" } else { "base" };
        let mut boundary_created = false;
        let mut group_created = false;
        let marker = running_marker(r);
        let mut marker_written = false;
        let setup = (|| {
            std::fs::create_dir_all(boundary.parent().unwrap())?;
            super::create_boundary(&boundary)?;
            boundary_created = true;
            std::fs::create_dir_all(&root)?;
            std::fs::set_permissions(&boundary, std::fs::Permissions::from_mode(0o700))?;
            fs::trusted(&boundary, &std::fs::symlink_metadata(&boundary)?)?;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
            std::fs::create_dir(root.join("state"))?;
            std::fs::set_permissions(root.join("state"), std::fs::Permissions::from_mode(0o700))?;
            chown_path(&root, uid)?;
            chown_path(&root.join("state"), uid)?;
            // Only the VMM leaf uses pids. Job slots retain their existing controllers.
            std::fs::write(
                self.groups.parent().unwrap().join("cgroup.subtree_control"),
                "+cpu +cpuset +memory +io +pids",
            )?;
            std::fs::create_dir_all(&self.groups)?;
            std::fs::write(
                self.groups.join("cgroup.subtree_control"),
                "+cpu +cpuset +memory +io +pids",
            )?;
            std::fs::create_dir(&group)?;
            group_created = true;
            let limits = cgroup_limits(
                r,
                std::fs::read_to_string(self.groups.join("cpuset.mems.effective"))?,
            )?;
            for (name, value) in limits {
                std::fs::write(group.join(name), value)?;
            }
            bind(
                &fs::trusted_file(&r.image().join("rootfs"))?,
                &root.join("rootfs.ext4"),
                true,
            )?;
            for (source, dest) in [
                (
                    format!("{prefix}.vmstate"),
                    format!("state/{prefix}.vmstate"),
                ),
                (format!("{prefix}.memory"), format!("state/{prefix}.mem")),
            ] {
                let file = fs::trusted_file(&state.join(source))?;
                bind(&file, &root.join(dest), true)?;
            }
            fs::chown(&disk, uid)?;
            bind(&disk, &root.join("replacement.img"), false)?;
            if !saved {
                // Match the base's unmounted placeholder at load time. Only
                // the subsequent drive PATCH changes virtio-blk's capacity.
                let placeholder = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(root.join("tenant.img"))?;
                placeholder.set_len(512 * 1048576)?;
                fs::chown(&placeholder, uid)?;
            }
            let procs = OpenOptions::new()
                .write(true)
                .open(group.join("cgroup.procs"))?;
            let binary_root = Path::new("/usr/local/lib/firecracker/1.17.0");
            for name in ["jailer", "firecracker"] {
                fs::trusted_file(&binary_root.join(name))?;
                let output = Command::new(binary_root.join(name))
                    .arg("--version")
                    .output()?;
                if !output.status.success()
                    || !String::from_utf8_lossy(&output.stdout).contains("1.17.0")
                {
                    return Err(Error(
                        "tenant VMs require Firecracker and jailer 1.17.0".into(),
                    ));
                }
            }
            let mut cmd = Command::new(binary_root.join("jailer"));
            // The root-managed registry can opt into a fixed console file, never
            // a caller-selected output path. Keep it across suspend and restore.
            let console = if r.capture_console {
                Stdio::from(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(r.disk.with_file_name("console.log"))?,
                )
            } else {
                Stdio::null()
            };
            cmd.args(["--id", &id, "--exec-file"])
                .arg(binary_root.join("firecracker"))
                .args([
                    "--uid",
                    &uid.to_string(),
                    "--gid",
                    &uid.to_string(),
                    "--chroot-base-dir",
                ])
                .arg(&boundary)
                .args(["--", "--api-sock", "/api.sock"])
                .stdin(Stdio::null())
                .stdout(console)
                .stderr(Stdio::inherit())
                .env_clear();
            unsafe {
                cmd.pre_exec(move || super::raw_place(procs.as_raw_fd()));
            }
            // Durable before the VMM can touch the disk, so a helper killed
            // at any later point leaves evidence of an unclean stop.
            write_running_marker(&marker)?;
            marker_written = true;
            Ok(cmd.spawn()?)
        })();
        let child = match setup {
            Ok(child) => child,
            Err(e) => {
                let ownership = fs::chown(&disk, 0);
                let mounts = if boundary_created {
                    cleanup_boundary(&boundary)
                } else {
                    Ok(())
                };
                let cgroup = if group_created {
                    std::fs::remove_dir(&group).map_err(Error::from)
                } else {
                    Ok(())
                };
                // Only a failed spawn follows the marker write: no VM ran.
                let marker = if marker_written {
                    remove_running_marker(&marker)
                } else {
                    Ok(())
                };
                for cleanup in [ownership, mounts, cgroup, marker] {
                    if let Err(error) = cleanup {
                        return Err(Error(format!("{e}; tenant cleanup failed: {error}")).into());
                    }
                }
                return Err(e.into());
            }
        };
        self.running.insert(
            tenant.into(),
            Running {
                child,
                disk,
                root: root.clone(),
                group,
                paused: true,
                attached: false,
                restored: saved,
                snapshot_uncertain: false,
            },
        );
        // A successful connection is required, not merely a leftover socket inode.
        let root_fd = File::open(&root)?;
        let socket_root = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
        let socket = super::wait_socket(&socket_root.join("api.sock"), Duration::from_secs(10))?;
        drop(socket);
        let pid = self.vm(tenant)?.child.id();
        guarded_snapshot_load(&root, prefix, &r.tap(), || protect_vmm(pid))
    }
    fn drive(&mut self, tenant: &str, _: &Registry) -> Result<()> {
        let vm = self.vm(tenant)?;
        if vm.attached {
            return Ok(());
        }
        if !vm.paused {
            return Err(Error("drive replacement requires paused VM".into()));
        }
        api(&vm.root, "PATCH", "/drives/tenant", &drive_body())?;
        vm.attached = true;
        Ok(())
    }
    fn consume(&mut self, r: &Registry) -> Result<()> {
        for name in ["key.json", "suspend.vmstate", "suspend.memory"] {
            match std::fs::remove_file(r.suspend.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        File::open(&r.suspend)?.sync_all()?;
        Ok(())
    }
    fn advance(&mut self, r: &Registry) -> Result<u64> {
        let generation = self
            .key(r)?
            .generation
            .checked_add(1)
            .ok_or_else(|| Error("generation exhausted".into()))?;
        durable_write(&r.generation, format!("{generation}\n").as_bytes())?;
        Ok(generation)
    }
    fn resume(&mut self, tenant: &str) -> Result<()> {
        let vm = self.vm(tenant)?;
        api(&vm.root, "PATCH", "/vm", &json!({"state":"Resumed"}))?;
        vm.paused = false;
        Ok(())
    }
    fn staged_resize(&mut self, r: &Registry) -> Result<Option<ResizeActivation>> {
        activation::read_staged(&r.disk.with_file_name("runner-args.staged"))
    }
    fn apply_resize(&mut self, tenant: &str, activation: &ResizeActivation) -> Result<()> {
        self.agent(
            tenant,
            json!({"op":"apply_resize", "runner_args":activation.runner_args}),
        )
    }
    fn sync_disk(&mut self, tenant: &str) -> Result<()> {
        self.vm(tenant)?.disk.sync_all()?;
        Ok(())
    }
    fn post_grow_check(&mut self, r: &Registry) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let device = std::fs::symlink_metadata(&r.disk)?.dev();
        let registries = self
            .registries()?
            .into_iter()
            .map(|registry| {
                let dev = std::fs::symlink_metadata(&registry.disk)?.dev();
                Ok((registry, dev))
            })
            .collect::<Result<Vec<_>>>()?;
        let on_device = registries
            .into_iter()
            .filter_map(|(r, dev)| (dev == device).then_some(r))
            .collect::<Vec<_>>();
        let capacity =
            capacity::read_capacity(Path::new(capacity::CONFIG)).map_err(|e| Error(e.message))?;
        capacity.check_post_grow(&on_device, activation::available(&r.disk)?)
    }
    fn complete_resize(&mut self, r: &Registry, activation: &ResizeActivation) -> Result<()> {
        activation::complete(&r.disk.with_file_name("runner-args.staged"), activation)
    }
    fn prepare(&mut self, tenant: &str) -> Result<()> {
        self.agent(tenant, json!({"op":"prepare_snapshot","kind":"suspend"}))
    }
    fn thaw(&mut self, tenant: &str) -> Result<()> {
        if self.vm(tenant)?.paused {
            self.resume(tenant)?;
        }
        self.agent(tenant, json!({"op":"thaw"}))
    }
    fn snapshot(&mut self, tenant: &str, r: &Registry, key: &SnapshotKey) -> Result<()> {
        let vm = self.vm(tenant)?;
        // A lost pause reply may still have paused the VM. Compensation must
        // issue Resume even when the API call itself fails.
        vm.paused = true;
        api_with_timeout(
            &vm.root,
            "PATCH",
            "/vm",
            &json!({"state":"Paused"}),
            Duration::from_secs(10),
        )
        .map_err(|e| Error(e.to_string()))?;
        let high = vm.group.join("memory.high");
        let previous = std::fs::read_to_string(&high)?;
        std::fs::write(
            &high,
            (r.snapshot_memory_high_mib.min(r.guest_ram_mib) * 1048576).to_string(),
        )?;
        let result = (|| {
            let timeout = snapshot_create_timeout(r.guest_ram_mib);
            eprintln!(
                "tenant {tenant}: suspend_snapshot_create ram_mib={} timeout_ms={}",
                r.guest_ram_mib,
                timeout.as_millis()
            );
            create_snapshot(&vm.root, timeout, &mut vm.snapshot_uncertain)?;
            for (src, dst) in [
                ("new.vmstate", "suspend.vmstate"),
                ("new.memory", "suspend.memory"),
            ] {
                let file = File::open(vm.root.join("state").join(src))?;
                fs::chown(&file, 0)?;
                file.set_permissions(std::fs::Permissions::from_mode(0o444))?;
                file.sync_all()?;
                std::fs::rename(vm.root.join("state").join(src), r.suspend.join(dst))?;
            }
            durable_write(
                &r.suspend.join("key.json"),
                &serde_json::to_vec(key).map_err(|e| Error(e.to_string()))?,
            )
        })();
        let restored = std::fs::write(high, previous);
        if result.is_err() {
            if let Err(error) = restored {
                eprintln!("tenant {tenant}: suspend_restore_memory_high_failed: {error}");
            }
            return result;
        }
        restored?;
        result
    }
    fn stop(&mut self, tenant: &str) -> Result<()> {
        if let Some(vm) = self.running.get_mut(tenant) {
            std::fs::write(vm.group.join("cgroup.kill"), "1")?;
            let _ = vm.child.kill();
            vm.child.wait()?;
            wait_empty(&vm.group)?;
            fs::chown(&vm.disk, 0)?;
            let boundary = Path::new(super::BUILD_ROOT).join(format!("jail/tenants/{tenant}"));
            cleanup_boundary(&boundary)?;
            std::fs::remove_dir(&vm.group)?;
        }
        self.running.remove(tenant);
        Ok(())
    }
    fn available(&mut self, r: &Registry) -> Result<u64> {
        let path = std::ffi::CString::new(r.suspend.as_os_str().as_encoded_bytes())
            .map_err(|e| Error(e.to_string()))?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
    fn cpu(&mut self, tenant: &str, value: &str) -> Result<()> {
        std::fs::write(self.vm(tenant)?.group.join("cpu.max"), value)?;
        Ok(())
    }
    fn egress(&mut self, _: &str, r: &Registry) -> Result<()> {
        let uid = self.uid(r)?;
        let registries = self.registries()?;
        // Install the deny rules before bringing a tap up, not after it can send.
        tenant_network::install(&registries, &self.config)?;
        tenant_network::ensure(r, uid)
    }
    fn relay(&mut self, tenant: &str, port: u32) -> Result<OwnedFd> {
        crate::tenant::validate_port(port)?;
        let root = self.vm(tenant)?.root.clone();
        connect_relay(&root, port)
    }
    fn vm_present(&mut self, tenant: &str) -> Result<bool> {
        crate::tenant::validate_name(tenant)?;
        let jail = Path::new(super::BUILD_ROOT).join(format!("jail/tenants/{tenant}"));
        Ok(self.groups.join(tenant).exists() || jail.exists())
    }
    fn remove(&mut self, tenant: &str, _r: &Registry) -> Result<PathBuf> {
        let paths = crate::removal::Paths::live();
        let _lock = crate::removal::lock(&paths)?;
        let unix_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let quarantine = crate::removal::begin(&paths, tenant, unix_s)?;
        crate::removal::finish_with_network(&paths, tenant, &quarantine, tenant_network::remove)?;
        eprintln!(
            "tenant {tenant}: removed records and network; data quarantined at {} until purge",
            quarantine.display()
        );
        Ok(quarantine)
    }
    fn resume_removal(&mut self, tenant: &str) -> Result<Option<PathBuf>> {
        let paths = crate::removal::Paths::live();
        let _lock = crate::removal::lock(&paths)?;
        let Some(quarantine) = crate::removal::pending(&paths, tenant)? else {
            return Ok(None);
        };
        crate::removal::finish_with_network(&paths, tenant, &quarantine, |registry| {
            crate::removal::refuse_reassigned_tap(&paths, tenant, registry, false, |path| {
                serde_json::from_reader(fs::trusted_file(path)?).map_err(|e| Error(e.to_string()))
            })?;
            tenant_network::remove(registry)
        })?;
        eprintln!(
            "tenant {tenant}: completed an interrupted removal; data quarantined at {}",
            quarantine.display()
        );
        Ok(Some(quarantine))
    }
}

pub(crate) fn guarded_snapshot_load(
    root: &Path,
    prefix: &str,
    tap: &str,
    protect: impl FnOnce() -> std::result::Result<(), StartRefusal>,
) -> StartResult<()> {
    protect()?;
    api(root, "PUT", "/snapshot/load", &load_body(prefix, tap)).map_err(Into::into)
}

fn protect_vmm(pid: u32) -> std::result::Result<(), StartRefusal> {
    protect_vmm_at(&PathBuf::from(format!("/proc/{pid}")))
}

fn protect_vmm_at(process: &Path) -> std::result::Result<(), StartRefusal> {
    protect_vmm_with(
        |name| std::fs::read_to_string(process.join(name)),
        |value| std::fs::write(process.join("oom_score_adj"), value),
    )
}

pub(crate) fn protect_vmm_with(
    mut read: impl FnMut(&str) -> std::io::Result<String>,
    write: impl FnOnce(&str) -> std::io::Result<()>,
) -> std::result::Result<(), StartRefusal> {
    let check = || -> Result<()> {
        let comm = read("comm")?;
        if comm.trim_end() != "firecracker" {
            return Err(Error(format!("expected firecracker, comm={comm:?}")));
        }
        write("-500").map_err(|error| Error(format!("oom write: {error}")))?;
        let actual =
            read("oom_score_adj").map_err(|error| Error(format!("oom readback: {error}")))?;
        if actual.trim() != "-500" {
            return Err(Error(format!(
                "oom readback: expected -500, got {actual:?}"
            )));
        }
        Ok(())
    };
    check().map_err(|error| crate::tenant::start_refused(StartRefusalCase::Oom, error))
}

fn connect_relay(root: &Path, port: u32) -> Result<OwnedFd> {
    crate::tenant::validate_port(port)?;
    let context = |step: &str, error: &dyn std::fmt::Display| {
        Error(format!(
            "Firecracker vsock peer={}/vsock.sock guest_port={port} step={step}: {error}",
            root.display()
        ))
    };
    let root_fd = File::open(root).map_err(|e| context("open_jail_root", &e))?;
    let socket_root = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
    let mut stream = super::wait_socket(&socket_root.join("vsock.sock"), Duration::from_secs(10))
        .map_err(|e| context("connect_uds", &e))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| context("set_read_timeout", &e))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| context("set_write_timeout", &e))?;
    relay_handshake(&mut stream, port).map_err(|e| context("handshake", &e))?;
    stream
        .set_read_timeout(None)
        .map_err(|e| context("clear_read_timeout", &e))?;
    stream
        .set_write_timeout(None)
        .map_err(|e| context("clear_write_timeout", &e))?;
    Ok(stream.into())
}

fn relay_handshake(stream: &mut (impl Read + Write), port: u32) -> Result<()> {
    // Firecracker 1.17 parses CONNECT on the first readable event, without
    // buffering fragments. Formatting directly into the socket can send the
    // prefix before the port and newline, so prepare the entire line first.
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| Error(format!("write_CONNECT: {e}")))?;
    let mut answer = Vec::new();
    while answer.len() < 128 {
        let mut byte = [0];
        stream
            .read_exact(&mut byte)
            .map_err(|e| Error(format!("read_CONNECT_ack: {e}")))?;
        answer.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    if !answer.starts_with(b"OK ") || answer.last() != Some(&b'\n') {
        return Err(Error("read_CONNECT_ack: invalid acknowledgement".into()));
    }
    Ok(())
}

/// Budget for 64 MiB/s plus 30 s of overhead. Reserve two minutes of the host's
/// 600 s helper deadline for guest preparation, pause and failure compensation.
fn snapshot_create_timeout(ram_mib: u64) -> Duration {
    Duration::from_secs(ram_mib.div_ceil(64).saturating_add(30).clamp(60, 480))
}

pub(crate) fn create_snapshot(root: &Path, timeout: Duration, uncertain: &mut bool) -> Result<()> {
    *uncertain = true;
    let result = api_with_timeout(root, "PUT", "/snapshot/create", &snapshot_body(), timeout)
        .map_err(|error| {
            let prefix = if error.request_started {
                crate::tenant::SNAPSHOT_CREATE_UNCERTAIN
            } else {
                "suspend_create_not_sent"
            };
            Error(format!("{prefix}: {error}"))
        });
    if !result.as_ref().is_err_and(|error| {
        error
            .0
            .starts_with(crate::tenant::SNAPSHOT_CREATE_UNCERTAIN)
    }) {
        *uncertain = false;
    }
    result
}

struct ApiFailure {
    method: String,
    path: String,
    step: &'static str,
    request_started: bool,
    elapsed_ms: u128,
    timeout_ms: u128,
    detail: String,
}
impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Firecracker {} {} step={} request_started={} elapsed_ms={} timeout_ms={}: {}",
            self.method,
            self.path,
            self.step,
            self.request_started,
            self.elapsed_ms,
            self.timeout_ms,
            self.detail
        )
    }
}

pub fn api(root: &Path, method: &str, path: &str, body: &Value) -> Result<()> {
    api_with_timeout(root, method, path, body, Duration::from_secs(300))
        .map_err(|error| Error(error.to_string()))
}

fn api_with_timeout(
    root: &Path,
    method: &str,
    path: &str,
    body: &Value,
    timeout: Duration,
) -> std::result::Result<(), Box<ApiFailure>> {
    let started = std::time::Instant::now();
    let failure = |step, request_started, error: std::io::Error| {
        Box::new(ApiFailure {
            method: method.into(),
            path: path.into(),
            step,
            request_started,
            elapsed_ms: started.elapsed().as_millis(),
            timeout_ms: timeout.as_millis(),
            detail: if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                format!("api_timeout: {error}")
            } else {
                format!("{:?}: {error}", error.kind())
            },
        })
    };
    let root_fd = File::open(root).map_err(|e| failure("open_jail_root", false, e))?;
    let mut socket = UnixStream::connect(format!("/proc/self/fd/{}/api.sock", root_fd.as_raw_fd()))
        .map_err(|e| failure("connect", false, e))?;
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|e| failure("set_read_timeout", false, e))?;
    socket
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| failure("set_write_timeout", false, e))?;
    let data = body.to_string();
    write!(socket, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len())
        .map_err(|e| failure("write_request", true, e))?;
    let mut reader = BufReader::new(socket);
    let mut status = String::new();
    // Read one byte at a time with an absolute deadline; a trickled HTTP status
    // must not extend the helper's reply budget indefinitely.
    let end = started + timeout;
    while !status.ends_with('\n') {
        let remaining = end.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(failure(
                "read_status",
                true,
                std::io::ErrorKind::TimedOut.into(),
            ));
        }
        reader
            .get_ref()
            .set_read_timeout(Some(remaining))
            .map_err(|e| failure("set_read_timeout", true, e))?;
        let mut byte = [0];
        reader
            .read_exact(&mut byte)
            .map_err(|e| failure("read_status", true, e))?;
        status.push(byte[0] as char);
        if status.len() > 1024 {
            return Err(failure(
                "read_status",
                true,
                std::io::Error::other("status line too long"),
            ));
        }
    }
    if status.split_whitespace().nth(1) != Some("204") {
        return Err(failure(
            "response_status",
            true,
            std::io::Error::other(status.trim()),
        ));
    }
    eprintln!(
        "Firecracker {method} {path} completed elapsed_ms={} timeout_ms={}",
        started.elapsed().as_millis(),
        timeout.as_millis()
    );
    Ok(())
}
fn load_body(prefix: &str, tap: &str) -> Value {
    json!({"snapshot_path":format!("/state/{prefix}.vmstate"),
        "mem_backend":{"backend_type":"File","backend_path":format!("/state/{prefix}.mem")},
        "enable_diff_snapshots":false,"resume_vm":false,
        "network_overrides":[{"iface_id":"eth0","host_dev_name":tap}]})
}
fn drive_body() -> Value {
    json!({"drive_id":"tenant","path_on_host":"/replacement.img"})
}
fn snapshot_body() -> Value {
    json!({"snapshot_type":"Full","snapshot_path":"/state/new.vmstate","mem_file_path":"/state/new.memory","sync_snapshot_files":false})
}
fn cgroup_limits(r: &Registry, mems: String) -> Result<[(&'static str, String); 6]> {
    Ok([
        ("cpuset.cpus", r.cpuset.clone()),
        ("cpuset.mems", mems),
        ("cpu.max", r.cpu_max(1)?),
        ("memory.max", r.memory_max()?.to_string()),
        ("pids.max", (r.vcpu_ceiling + 64).to_string()),
        ("io.weight", format!("default {}", r.io_weight)),
    ])
}
fn bind(file: &File, target: &Path, readonly: bool) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)?;
    if readonly {
        return fs::bind_readonly(file, target);
    }
    let source = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
    let dest = std::ffi::CString::new(target.as_os_str().as_encoded_bytes())
        .map_err(|e| Error(e.to_string()))?;
    if unsafe {
        libc::mount(
            source.as_ptr(),
            dest.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn chown_path(path: &Path, uid: u32) -> Result<()> {
    fs::chown(&File::open(path)?, uid)
}
fn cleanup_boundary(boundary: &Path) -> Result<()> {
    if !boundary.exists() {
        return Ok(());
    }
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
    let mut paths: Vec<PathBuf> = mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(4))
        .map(super::unescape_mount)
        .filter(|p| p.starts_with(boundary))
        .collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for path in paths {
        fs::unmount(&path)?;
    }
    std::fs::remove_dir_all(boundary)?;
    Ok(())
}
fn wait_empty(group: &Path) -> Result<()> {
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(group.join("cgroup.procs"))?
        .trim()
        .is_empty()
    {
        if std::time::Instant::now() >= end {
            return Err(Error("tenant cgroup did not exit".into()));
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }
    Ok(())
}
/// Present from just before a tenant's VMM starts until a clean stop or a
/// completed suspend, so it survives SIGKILL and power loss. It sits beside
/// `unclean-stop.json` in the root-owned tenant home.
fn running_marker(r: &Registry) -> PathBuf {
    r.generation.with_file_name("running.json")
}
fn write_running_marker(path: &Path) -> Result<()> {
    let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    durable_write(path, json!({ "at_ms": at_ms }).to_string().as_bytes())
}
/// Any trusted regular file counts, whatever it holds: a damaged marker is
/// still evidence. An operator can create one by hand (root-owned) to make the
/// next start treat the tenant as stopped uncleanly.
fn running_marker_present(path: &Path) -> Result<bool> {
    match fs::trusted_file(path) {
        Ok(_) => Ok(true),
        Err(_) if !path.exists() => Ok(false),
        Err(error) => Err(error),
    }
}
fn remove_running_marker(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn durable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("pending");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests;
