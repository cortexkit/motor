pub mod cgroup;
pub mod fs;
pub mod process;
pub mod server;
pub mod tenant;
mod tenant_network;

use crate::{
    config::{Config, SnapshotRecord},
    Error, Kind, Lease, Machine, Result,
};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub const BUILD_ROOT: &str = "/var/lib/motor-vm";
pub const SOCKET: &str = "/run/motor-vm-helper.sock";
struct Running {
    loader: Child,
    jailer: Child,
}
pub struct LinuxMachine {
    config: Config,
    base: File,
    base_path: PathBuf,
    cgroups: cgroup::Cgroups,
    running: BTreeMap<usize, Running>,
    tenants: crate::tenant::Controller<tenant::LinuxTenants>,
}
impl LinuxMachine {
    pub fn new(config: Config) -> Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(Error("motor-vm-helper must run as root".into()));
        }
        // Filesystem-credential guards intentionally use only the peer gid.
        // Do not let the root service's supplementary groups grant the runner
        // access it would not have on its own. Do this before starting threads.
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        config.validate()?;
        process::refuse_passwd_entries(config.uid_base, config.slots.len())?;
        for path in [&config.jailer, &config.firecracker, &config.loader] {
            fs::trusted_file(path)?;
        }
        if config.firecracker.file_name().and_then(|n| n.to_str()) != Some("firecracker") {
            return Err(Error(
                "configured Firecracker executable must be named firecracker".into(),
            ));
        }
        let base = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(BUILD_ROOT)?;
        fs::trusted(Path::new(BUILD_ROOT), &base.metadata()?)?;
        for ancestor in Path::new(BUILD_ROOT).ancestors() {
            fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
        }
        let jail = Path::new(BUILD_ROOT).join("jail");
        if !jail.try_exists()? {
            std::fs::create_dir(&jail)?;
        }
        fs::trusted(&jail, &std::fs::symlink_metadata(&jail)?)?;
        let cgroups = cgroup::Cgroups::discover()?;
        let tenants =
            crate::tenant::Controller(tenant::LinuxTenants::new(&config, cgroups.root.clone()));
        Ok(Self {
            config,
            base,
            base_path: PathBuf::from(BUILD_ROOT),
            cgroups,
            running: BTreeMap::new(),
            tenants,
        })
    }
    fn boundary(&self, slot: usize) -> PathBuf {
        self.base_path.join(format!("jail/slot-{slot}"))
    }
    /// The official jailer appends <binary>/<id>/root to chroot-base-dir. The
    /// root is nested, but the whole root-owned slot directory is reconciled.
    fn root(&self, slot: usize) -> PathBuf {
        self.boundary(slot)
            .join(format!("firecracker/slot-{slot}/root"))
    }
    fn kill(&mut self, slot: usize) -> Result<()> {
        self.cgroups.kill(slot)?;
        if let Some(mut running) = self.running.remove(&slot) {
            // Reap direct children so no zombie keeps a slot uid visible.
            let _ = running.loader.kill();
            let _ = running.jailer.kill();
            running.loader.wait()?;
            running.jailer.wait()?;
        }
        process::kill_uid(self.config.uid_base + slot as u32)?;
        Ok(())
    }
    fn remove_paths(&self, slot: usize) -> Result<()> {
        let boundary = self.boundary(slot);
        if boundary.try_exists()? {
            fs::trusted(&boundary, &std::fs::symlink_metadata(&boundary)?)?;
            // Unmount all descendants, not just today's two bind mounts: an
            // interrupted upgrade must not leave a mount referencing job data.
            let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
            let mut paths: Vec<PathBuf> = mounts
                .lines()
                .filter_map(|l| l.split_whitespace().nth(4))
                .map(unescape_mount)
                .filter(|p| p.starts_with(&boundary))
                .collect();
            paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for path in paths {
                fs::unmount(&path)?;
            }
            std::fs::remove_dir_all(boundary)?;
        }
        self.cgroups.remove(slot)
    }
    /// Capacity admission owns the whole preparation continuation, so a
    /// shortage or an unreadable pool cannot create a jail or launch a fallback.
    /// The production reader uses /proc/meminfo; tests need no reserved pages
    /// or privileges to exercise the same gate and its side-effect ordering.
    fn prepare<T>(
        &self,
        free_pages: impl FnOnce() -> Result<u64>,
        create_vm: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let free = free_pages()?;
        if free < self.config.hugepages_per_slot {
            return Err(Error("vm_capacity: not enough HugePages_Free".into()));
        }
        create_vm()
    }
    fn create_vm(
        &self,
        slot: usize,
        job: &fs::JobDir<'_>,
        kind: Kind,
    ) -> Result<(File, UnixListener)> {
        let uid = self.config.uid_base + slot as u32;
        // Opening the job directory precedes any side effects and never uses
        // root credentials. A pre-existing chroot is not a reusable VM.
        let snapshot = if kind == Kind::Scan {
            &self.config.scan_snapshot
        } else {
            &self.config.job_snapshot
        };
        let record: SnapshotRecord = serde_json::from_reader(fs::trusted_file(&snapshot.record)?)
            .map_err(|e| Error(e.to_string()))?;
        let mut sources = Vec::new();
        for &(input, drive) in inputs(kind) {
            let size = *record
                .image_sizes
                .get(drive)
                .ok_or_else(|| Error(format!("snapshot record missing {drive} size")))?;
            sources.push((drive, job.source(input, size)?, size));
        }
        create_boundary(&self.boundary(slot))?;
        std::fs::create_dir_all(self.root(slot))?;
        self.cgroups.create(slot, &self.config.slots[slot])?;
        let root = self.root(slot);
        for (name, path) in [("vmstate", &snapshot.vmstate), ("rootfs", &snapshot.rootfs)] {
            let source = fs::trusted_file(path)?;
            use std::os::unix::fs::MetadataExt;
            if source.metadata()?.mode() & 0o004 == 0 {
                return Err(Error(
                    "shared read-only snapshot files must be readable by slot uids".into(),
                ));
            }
            let target = root.join(name);
            create(&target)?;
            fs::bind_readonly(&source, &target)?;
        }
        for (drive, source, size) in sources {
            let dest = create(&root.join(drive))?;
            job.clone_into(&source, &dest)?;
            if dest.metadata()?.len() != size {
                return Err(Error(format!(
                    "vm_image_size: {drive} changed during clone"
                )));
            }
            fs::chown(&dest, uid)?;
            if kind == Kind::Scan {
                std::fs::set_permissions(root.join(drive), std::fs::Permissions::from_mode(0o400))?;
            }
        }
        // /dev/userfaultfd is a device-node capability restricted to this uid.
        std::fs::create_dir(root.join("dev"))?;
        let device = CString::new(root.join("dev/userfaultfd").as_os_str().as_encoded_bytes())
            .map_err(|_| Error("invalid device path".into()))?;
        let host = std::fs::metadata("/dev/userfaultfd")?;
        use std::os::unix::fs::MetadataExt;
        if host.mode() & libc::S_IFMT != libc::S_IFCHR {
            return Err(Error("/dev/userfaultfd is not a character device".into()));
        }
        if unsafe { libc::mknod(device.as_ptr(), libc::S_IFCHR | 0o600, host.rdev()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { libc::chown(device.as_ptr(), uid, uid) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        std::fs::create_dir(root.join("loader-root"))?;
        // The loader receives already-open descriptors and never sees the
        // memory pathname or a guest image in its empty root.
        let memory = fs::trusted_file(&snapshot.memory)?;
        let listener = UnixListener::bind(root.join("uffd.sock"))?;
        let sock = CString::new(root.join("uffd.sock").as_os_str().as_encoded_bytes()).unwrap();
        if unsafe { libc::chown(sock.as_ptr(), uid, uid) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        std::fs::set_permissions(
            root.join("uffd.sock"),
            std::fs::Permissions::from_mode(0o600),
        )?;
        job.verify()?;
        Ok((memory, listener))
    }
    fn launch(&mut self, slot: usize, memory: File, listener: UnixListener) -> Result<()> {
        let uid = self.config.uid_base + slot as u32;
        let procs = OpenOptions::new()
            .write(true)
            .open(self.cgroups.slot(slot).join("cgroup.procs"))?;
        let executable = fs::trusted_file(&self.config.loader)?;
        let exec_fd = executable.as_raw_fd();
        let memory_fd = memory.as_raw_fd();
        let listener_fd = listener.as_raw_fd();
        let root = CString::new(
            self.root(slot)
                .join("loader-root")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        let argv = [
            CString::new("motor-vm-loader").unwrap(),
            CString::new("--memory-fd").unwrap(),
            CString::new(memory_fd.to_string()).unwrap(),
            CString::new("--listen-fd").unwrap(),
            CString::new(listener_fd.to_string()).unwrap(),
        ];
        let argv_ptrs: Vec<_> = argv
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        // Store pointer addresses as integers so the pre_exec closure is Send.
        let argv_address = argv_ptrs.as_ptr() as usize;
        let filter = process::loader_program(exec_fd);
        let jailer_procs = OpenOptions::new()
            .write(true)
            .open(self.cgroups.slot(slot).join("cgroup.procs"))?;
        let mut loader = Command::new(&self.config.loader);
        loader
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .env_clear();
        unsafe {
            loader.pre_exec(move || {
                raw_place(procs.as_raw_fd())?;
                if libc::chroot(root.as_ptr()) != 0
                    || libc::chdir(c"/".as_ptr()) != 0
                    || libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setgid(uid) != 0
                    || libc::setuid(uid) != 0
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in [memory_fd, listener_fd] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                process::install_loader_filter(&filter)?;
                let env = [std::ptr::null::<libc::c_char>()];
                libc::syscall(
                    libc::SYS_execveat,
                    exec_fd,
                    c"".as_ptr(),
                    argv_address as *const *const libc::c_char,
                    env.as_ptr(),
                    libc::AT_EMPTY_PATH,
                );
                Err(std::io::Error::last_os_error())
            });
        }
        let mut loader = loader.spawn()?;
        // Keep all fd/argv storage alive until spawn's exec handshake completes.
        drop((argv, argv_ptrs, executable, memory, listener));
        let mut jailer = Command::new(&self.config.jailer);
        jailer
            .args(["--id", &format!("slot-{slot}"), "--exec-file"])
            .arg(&self.config.firecracker)
            .args([
                "--uid",
                &uid.to_string(),
                "--gid",
                &uid.to_string(),
                "--chroot-base-dir",
            ])
            .arg(self.boundary(slot))
            .args(["--", "--api-sock", "/api.sock"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .env_clear();
        unsafe {
            jailer.pre_exec(move || raw_place(jailer_procs.as_raw_fd()));
        }
        let jailer = match jailer.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = loader.kill();
                let _ = loader.wait();
                return Err(e.into());
            }
        };
        self.running.insert(slot, Running { loader, jailer });
        Ok(())
    }
}
impl Machine for LinuxMachine {
    fn reconcile(&mut self) -> Result<()> {
        // Check no-internal-process constraints even before cleanup. Never fix
        // incorrect placement by moving an unrecognised root service process.
        for path in [&self.cgroups.root, &self.cgroups.root.join("slots")] {
            if path.try_exists()?
                && !std::fs::read_to_string(path.join("cgroup.procs"))?
                    .trim()
                    .is_empty()
            {
                return Err(Error(format!(
                    "delegation root {} holds a process",
                    path.display()
                )));
            }
        }
        for slot in 0..self.config.slots.len() {
            self.cleanup(slot)?;
        }
        // Reconcile old slot directories and cgroups after a configured shrink.
        for parent in [self.base_path.join("jail"), self.cgroups.root.join("slots")] {
            if !parent.try_exists()? {
                continue;
            }
            for entry in std::fs::read_dir(parent)? {
                let entry = entry?;
                let name = entry.file_name();
                if let Some(n) = name
                    .to_str()
                    .and_then(|s| s.strip_prefix("slot-"))
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    if n >= self.config.slots.len() {
                        // Old processes in an old slot cgroup are killed before
                        // removing its mounts, even if its uid range changed.
                        self.cgroups.kill(n)?;
                        self.remove_paths(n)?;
                    }
                }
            }
        }
        self.cgroups.enable()?;
        self.tenants.0.reconcile()
    }
    fn start(&mut self, slot: usize, lease: &Lease, kind: Kind) -> Result<Vec<OwnedFd>> {
        let base = self.base.try_clone()?;
        let job = fs::JobDir::open(&base, &lease.identity, lease.peer)?;
        let (memory, listener) =
            self.prepare(hugepages_free, || self.create_vm(slot, &job, kind))?;
        self.launch(slot, memory, listener)?;
        let root = self.root(slot);
        let api = wait_socket(&root.join("api.sock"), Duration::from_secs(10))?;
        let body = serde_json::json!({"snapshot_path":"/vmstate", "mem_backend":{"backend_path":"/uffd.sock","backend_type":"Uffd"}, "enable_diff_snapshots":false, "resume_vm":true});
        api_request(api, "/snapshot/load", &body.to_string())?;
        let uds = root.join("vsock.sock");
        let proxy = if kind == Kind::Fetch || (kind == Kind::Job && self.config.proxy_for_jobs) {
            let path = root.join(format!("vsock.sock_{}", self.config.proxy_port));
            let listener = UnixListener::bind(&path)?;
            let path_c = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            let uid = self.config.uid_base + slot as u32;
            if unsafe { libc::chown(path_c.as_ptr(), uid, uid) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            Some(listener)
        } else {
            None
        };
        let mut agent = wait_socket(&uds, Duration::from_secs(10))?;
        agent.set_read_timeout(Some(Duration::from_secs(10)))?;
        writeln!(agent, "CONNECT {}", self.config.agent_port)?;
        // Do not buffer beyond the one-line host-vsock handshake: bytes after it
        // are guest agent frames and belong to the runner, not the helper.
        let mut answer = Vec::new();
        while answer.len() < 128 {
            let mut byte = [0];
            agent.read_exact(&mut byte)?;
            answer.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        if !answer.starts_with(b"OK ") || !answer.ends_with(b"\n") {
            return Err(Error("vsock agent connection refused".into()));
        }
        agent.set_read_timeout(None)?;
        job.verify()?;
        let mut fds = vec![agent.into()];
        if let Some(proxy) = proxy {
            fds.push(proxy.into());
        }
        Ok(fds)
    }
    fn stop(&mut self, slot: usize, lease: &Lease, export: bool) -> Result<()> {
        let base = self.base.try_clone()?;
        let job = fs::JobDir::open(&base, &lease.identity, lease.peer)?;
        self.kill(slot)?;
        job.verify()?;
        if export {
            let mut images = Vec::new();
            for &(drive, output) in
                outputs(lease.running.ok_or_else(|| Error("no running VM".into()))?)
            {
                let source = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                    .open(self.root(slot).join(drive))?;
                if !source.metadata()?.is_file() {
                    return Err(Error("output image is not a regular file".into()));
                }
                images.push((output, source));
            }
            job.export(&images)?;
        }
        self.remove_paths(slot)
    }
    fn cleanup(&mut self, slot: usize) -> Result<()> {
        self.kill(slot)?;
        self.remove_paths(slot)
    }
    fn alive(&mut self, slot: usize) -> bool {
        self.running.get_mut(&slot).is_some_and(|r| {
            matches!(r.loader.try_wait(), Ok(None)) && matches!(r.jailer.try_wait(), Ok(None))
        })
    }
    fn tenant(&mut self, tenant: &str, op: crate::tenant::Operation) -> Result<crate::Response> {
        self.tenants.handle(tenant, op)
    }
    fn shutdown(&mut self) -> Result<()> {
        // stop_all logs each tenant's outcome; this only summarises.
        let failures = self.tenants.stop_all();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error(format!(
                "{} tenant(s) recorded as unclean",
                failures.len()
            )))
        }
    }
}
fn inputs(kind: Kind) -> &'static [(&'static str, &'static str)] {
    if kind == Kind::Scan {
        &[("tree.out", "tree")]
    } else {
        &[
            ("tree.in", "tree"),
            ("target.in", "target"),
            ("cargo.in", "cargo"),
        ]
    }
}
fn outputs(kind: Kind) -> &'static [(&'static str, &'static str)] {
    match kind {
        Kind::Job => &[("tree", "tree.out"), ("target", "target.out")],
        Kind::Warm => &[("target", "target.out")],
        Kind::Fetch => &[("cargo", "cargo.out")],
        Kind::Scan => &[],
    }
}
fn create(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?)
}
fn create_boundary(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // The runner must not reach the API/UFFD sockets by pathname, even if the
    // jailer makes its nested root traversable. Both children chroot while
    // still root and need no access through this parent after dropping uid.
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}
fn raw_place(fd: i32) -> std::io::Result<()> {
    if unsafe { libc::write(fd, b"0".as_ptr().cast(), 1) } != 1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn hugepages_free() -> Result<u64> {
    std::fs::read_to_string("/proc/meminfo")?
        .lines()
        .find_map(|l| l.strip_prefix("HugePages_Free:"))
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| Error("HugePages_Free missing".into()))
}
fn wait_socket(path: &Path, timeout: Duration) -> Result<UnixStream> {
    let until = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path) {
            Ok(s) => {
                s.set_read_timeout(Some(timeout))?;
                s.set_write_timeout(Some(timeout))?;
                return Ok(s);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) && Instant::now() < until =>
            {
                std::thread::park_timeout(Duration::from_millis(10))
            }
            Err(e) => return Err(e.into()),
        }
    }
}
fn api_request(mut stream: UnixStream, path: &str, body: &str) -> Result<()> {
    write!(stream, "PUT {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())?;
    let mut reader = BufReader::new(stream).take(8192);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.starts_with("HTTP/1.1 204") {
        return Err(Error(format!("snapshot load failed: {}", status.trim())));
    }
    Ok(())
}
fn unescape_mount(s: &str) -> PathBuf {
    PathBuf::from(
        s.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\"),
    )
}

#[cfg(test)]
mod tests;
