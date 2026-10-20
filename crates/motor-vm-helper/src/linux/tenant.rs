//! Ordinary-file Firecracker snapshots, independent of job UFFD/hugepage VMs.
use super::{fs, process, tenant_network};
use crate::{
    config::Config,
    tenant::{Backend, Registry, SnapshotKey, REGISTRY},
    Error, Result,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
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
        tenant_network::install(&registries)?;
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
    fn was_restored(&self, tenant: &str) -> bool {
        self.running.get(tenant).is_some_and(|vm| vm.restored)
    }
    fn load(&mut self, tenant: &str, r: &Registry, saved: bool) -> Result<()> {
        if self.running(tenant) {
            return Err(Error("tenant already running".into()));
        }
        if self.running.contains_key(tenant) {
            self.mark_unclean(r, "vm_process_lost")?;
        }
        self.stop(tenant)?;
        let uid = self.uid(r)?;
        let disk = fs::trusted_file(&r.disk)?;
        if disk.metadata()?.len() != r.disk_quota_bytes {
            return Err(Error("tenant disk size mismatch".into()));
        }
        if unsafe { libc::flock(disk.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error("tenant disk already attached".into()));
        }
        let boundary = Path::new(super::BUILD_ROOT).join(format!("jail/tenants/{tenant}"));
        let id = format!("tenant-{}", r.tap_index);
        let root = boundary.join(format!("firecracker/{id}/root"));
        let group = self.groups.join(&id);
        let state = if saved { &r.suspend } else { &r.base() };
        let prefix = if saved { "suspend" } else { "base" };
        let mut boundary_created = false;
        let mut group_created = false;
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
                for cleanup in [ownership, mounts, cgroup] {
                    if let Err(error) = cleanup {
                        return Err(Error(format!("{e}; tenant cleanup failed: {error}")));
                    }
                }
                return Err(e);
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
            },
        );
        // A successful connection is required, not merely a leftover socket inode.
        let root_fd = File::open(&root)?;
        let socket_root = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
        let socket = super::wait_socket(&socket_root.join("api.sock"), Duration::from_secs(10))?;
        drop(socket);
        api(&root, "PUT", "/snapshot/load", &load_body(prefix, &r.tap()))
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
        api(&vm.root, "PATCH", "/vm", &json!({"state":"Paused"}))?;
        vm.paused = true;
        let high = vm.group.join("memory.high");
        let previous = std::fs::read_to_string(&high)?;
        std::fs::write(
            &high,
            (r.snapshot_memory_high_mib.min(r.guest_ram_mib) * 1048576).to_string(),
        )?;
        let result = (|| {
            api(&vm.root, "PUT", "/snapshot/create", &snapshot_body())?;
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
        std::fs::write(high, previous)?;
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
        tenant_network::install(&registries)?;
        tenant_network::ensure(r, uid)
    }
    fn relay(&mut self, tenant: &str, port: u32) -> Result<OwnedFd> {
        crate::tenant::validate_port(port)?;
        let root = self.vm(tenant)?.root.clone();
        connect_relay(&root, port)
    }
}

fn connect_relay(root: &Path, port: u32) -> Result<OwnedFd> {
    crate::tenant::validate_port(port)?;
    let root_fd = File::open(root)?;
    let socket_root = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
    let mut stream = super::wait_socket(&socket_root.join("vsock.sock"), Duration::from_secs(10))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    writeln!(stream, "CONNECT {port}")?;
    let mut answer = Vec::new();
    while answer.len() < 128 {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        answer.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    if !answer.starts_with(b"OK ") || answer.last() != Some(&b'\n') {
        return Err(Error("tenant vsock connection refused".into()));
    }
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(stream.into())
}

pub fn api(root: &Path, method: &str, path: &str, body: &Value) -> Result<()> {
    let root_fd = File::open(root)?;
    let mut socket =
        UnixStream::connect(format!("/proc/self/fd/{}/api.sock", root_fd.as_raw_fd()))?;
    socket.set_read_timeout(Some(Duration::from_secs(300)))?;
    socket.set_write_timeout(Some(Duration::from_secs(30)))?;
    let data = body.to_string();
    write!(socket, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len())?;
    let mut reader = BufReader::new(socket);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if status.split_whitespace().nth(1) != Some("204") {
        return Err(Error(format!(
            "Firecracker {method} {path}: {}",
            status.trim()
        )));
    }
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
