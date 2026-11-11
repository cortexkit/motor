pub mod cache;
pub mod cgroup;
pub mod fs;
pub mod images;
pub mod job;
pub mod process;
pub mod server;
pub mod startup_check;
pub mod tenant;
mod tenant_network;

use crate::{config::Config, Error, Kind, Lease, Machine, Result};
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::OwnedFd,
        unix::{fs::OpenOptionsExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const BUILD_ROOT: &str = "/var/lib/motor-vm";
pub const SOCKET: &str = "/run/motor-vm-helper.sock";
pub struct LinuxMachine {
    config: Config,
    base: File,
    base_path: PathBuf,
    cgroups: cgroup::Cgroups,
    tenants: crate::tenant::Controller<tenant::LinuxTenants>,
    jobs: job::LinuxJobs,
}
impl LinuxMachine {
    pub fn new(config: Config) -> Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(Error("motor-vm-helper must run as root".into()));
        }
        let base = startup_check::check(&config)?;
        // Filesystem-credential guards intentionally use only the peer gid.
        // Do not let the root service's supplementary groups grant the runner
        // access it would not have on its own. Do this before starting threads.
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let jail = Path::new(BUILD_ROOT).join("jail");
        if !jail.try_exists()? {
            std::fs::create_dir(&jail)?;
        }
        fs::trusted(&jail, &std::fs::symlink_metadata(&jail)?)?;
        let cgroups = cgroup::Cgroups::discover()?;
        let tenants =
            crate::tenant::Controller(tenant::LinuxTenants::new(&config, cgroups.root.clone()));
        let jobs = job::LinuxJobs::new(&config);
        Ok(Self {
            config,
            base,
            base_path: PathBuf::from(BUILD_ROOT),
            cgroups,
            tenants,
            jobs,
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
}
impl Machine for LinuxMachine {
    fn cache_write_authority(&self) -> Option<crate::cache_write::Authority> {
        Some(self.jobs.cache_write_grants.clone())
    }
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
        // Job evidence and leftover TAPs must be reconciled before the tenant
        // policy renderer can discard any no-longer-allocated counter.
        self.jobs.reconcile()?;
        self.tenants.0.reconcile()
    }
    fn start(&mut self, _slot: usize, _lease: &Lease, _kind: Kind) -> Result<Vec<OwnedFd>> {
        Err(Error(
            "legacy slot starts are retired; use start_job or tenant operations".into(),
        ))
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
    fn alive(&mut self, _slot: usize) -> bool {
        false
    }
    fn tenant(&mut self, tenant: &str, op: crate::tenant::Operation) -> Result<crate::Response> {
        self.tenants.handle(tenant, op)
    }
    fn job(&mut self, op: crate::job::Operation, peer: crate::Peer) -> Result<crate::Response> {
        self.jobs.handle(op, peer)
    }
    fn images(&mut self, op: crate::images::Operation) -> Result<crate::Response> {
        self.jobs.images(op)
    }
    fn job_vms_configured(&self) -> bool {
        self.config.job_vms.is_some()
    }
    fn workspace_disk_max_bytes(&self) -> Option<u64> {
        self.config
            .job_vms
            .as_ref()
            .and_then(|settings| settings.workspace_disk_max_bytes)
    }
    fn maintain_jobs(&mut self) -> Result<()> {
        self.jobs.maintain()
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
fn outputs(kind: Kind) -> &'static [(&'static str, &'static str)] {
    match kind {
        Kind::Job => &[("tree", "tree.out"), ("target", "target.out")],
        Kind::Warm => &[("target", "target.out")],
        Kind::Fetch => &[("cargo", "cargo.out")],
        Kind::Scan => &[],
    }
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

#[cfg(all(test, feature = "linux-root-tests"))]
pub(crate) mod root_test_support {
    use super::fs;
    use std::{
        fs::File,
        path::{Path, PathBuf},
        process::Command,
    };

    /// A disposable reflink-capable store even when the shared scratch is ext4.
    pub(crate) struct ReflinkStore(pub PathBuf);
    impl ReflinkStore {
        pub(crate) fn new(scratch: &Path) -> Self {
            let image = scratch.join("image-store.xfs");
            File::create(&image)
                .unwrap()
                .set_len(512 * 1024 * 1024)
                .unwrap();
            assert!(Command::new("/usr/sbin/mkfs.xfs")
                .args(["-q", "-f", "-m", "reflink=1"])
                .arg(&image)
                .status()
                .unwrap()
                .success());
            let mount = scratch.join("image-store");
            std::fs::create_dir(&mount).unwrap();
            assert!(Command::new("/usr/bin/mount")
                .args(["-o", "loop,nodev,nosuid,noexec"])
                .arg(&image)
                .arg(&mount)
                .status()
                .unwrap()
                .success());
            Self(mount)
        }
    }
    impl Drop for ReflinkStore {
        fn drop(&mut self) {
            fs::unmount(&self.0).expect("unmount disposable XFS store");
        }
    }
}
