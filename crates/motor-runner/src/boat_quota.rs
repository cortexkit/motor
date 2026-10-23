//! Project quotas for Boat's sparse XFS state disk. Never used by other profiles.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    fs::File,
    io,
    os::fd::AsRawFd,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MOUNT: &str = "/var/lib/motor-boat";
// Provisional: the existing admission floor, not a measured Boat snapshot bound.
// The write-to-quota + stop/resume probe records the headroom needed to tune it.
const PROVISIONAL_RESERVE: u64 = 24 * 1024 * 1024 * 1024;
const BLOCK_CAP: u64 = 32 * 1024 * 1024 * 1024;
const INODE_CAP: u64 = 65_536;
const INODE_RESERVE: u64 = 8192;

pub(crate) fn job_roots(
    workspace: &Path,
    target: &Path,
    runtime: &Path,
    spool: &Path,
) -> Vec<PathBuf> {
    // Relabel existing inodes too: targets and workspaces survive earlier jobs.
    // The complete runtime includes HOME, /tmp, and root-created output files.
    [workspace, target, runtime, spool]
        .into_iter()
        .map(Path::to_path_buf)
        .collect()
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct DiskQuota {
    version: i8,
    flags: i8,
    fieldmask: u16,
    id: u32,
    block_hard: u64,
    block_soft: u64,
    inode_hard: u64,
    inode_soft: u64,
    blocks: u64,
    inodes: u64,
    inode_timer: i32,
    block_timer: i32,
    inode_warns: u16,
    block_warns: u16,
    padding: i32,
    rt_hard: u64,
    rt_soft: u64,
    rt_blocks: u64,
    rt_timer: i32,
    rt_warns: u16,
    padding2: i16,
    padding3: [u8; 8],
}

fn quota(id: u32, limits: Option<(u64, u64)>) -> io::Result<DiskQuota> {
    let file = File::open(MOUNT)?;
    let mut data = DiskQuota {
        version: 1,
        flags: 2,
        id,
        ..Default::default()
    };
    let operation = if let Some((blocks, inodes)) = limits {
        data.fieldmask = (1 << 1) | (1 << 3); // FS_DQ_IHARD | FS_DQ_BHARD
        data.block_hard = blocks;
        data.inode_hard = inodes;
        4 // Q_XSETQLIM
    } else {
        3 // Q_XGETQUOTA
    };
    // QCMD combines the XFS operation with PRJQUOTA (2).
    let command = (((b'X' as u32) << 8 | operation) << 8) | 2;
    // SAFETY: the open mount fd and the Linux fs_disk_quota buffer remain live.
    let result = unsafe {
        libc::syscall(
            libc::SYS_quotactl_fd,
            file.as_raw_fd(),
            command,
            id,
            &mut data,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(data)
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct FsXattr {
    flags: u32,
    extent_size: u32,
    extents: u32,
    project: u32,
    cow_extent_size: u32,
    padding: [u8; 8],
}

// Keep the lease state machine independent of privileged kernel operations so
// crash windows and delayed cleanup can be exercised without mounting a disk.
trait QuotaSystem {
    fn mount(&self) -> &Path;
    fn quota(&self, id: u32, limits: Option<(u64, u64)>) -> io::Result<DiskQuota>;
    fn attributes(&self, file: &File) -> io::Result<FsXattr>;
    fn set_attributes(&self, file: &File, attr: &FsXattr) -> io::Result<()>;
    fn available(&self, path: &Path) -> io::Result<(u64, u64)>;
    fn monitor(&self, manifest: &Path, id: u32, device: u64) -> io::Result<()>;
    fn finish_monitor(&self, manifest: &Path) -> bool;
    #[cfg(test)]
    fn before_release_lock(&self) {}
}

struct System;
impl QuotaSystem for System {
    fn mount(&self) -> &Path {
        Path::new(MOUNT)
    }
    fn quota(&self, id: u32, limits: Option<(u64, u64)>) -> io::Result<DiskQuota> {
        quota(id, limits)
    }
    fn attributes(&self, file: &File) -> io::Result<FsXattr> {
        let mut attr = FsXattr::default();
        // SAFETY: FS_IOC_FSGETXATTR writes a correctly sized live fsxattr.
        if unsafe { libc::ioctl(file.as_raw_fd(), 0x801c581f_u64, &mut attr) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(attr)
    }
    fn set_attributes(&self, file: &File, attr: &FsXattr) -> io::Result<()> {
        // SAFETY: FS_IOC_FSSETXATTR reads a correctly sized live fsxattr.
        if unsafe { libc::ioctl(file.as_raw_fd(), 0x401c5820_u64, attr) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn available(&self, path: &Path) -> io::Result<(u64, u64)> {
        available(path)
    }
    fn monitor(&self, manifest: &Path, id: u32, device: u64) -> io::Result<()> {
        crate::boat_quota_events::start(manifest, id, device)
    }
    fn finish_monitor(&self, manifest: &Path) -> bool {
        crate::boat_quota_events::finish(manifest)
    }
}

fn assignment(mut attr: FsXattr, project: u32, directory: bool) -> FsXattr {
    attr.project = project;
    if directory {
        if project == 0 {
            attr.flags &= !0x200;
        } else {
            attr.flags |= 0x200;
        }
    }
    attr
}

fn assign(system: &impl QuotaSystem, path: &Path, project: u32, device: u64) -> io::Result<()> {
    visit(path, project, device, &mut |path, project, directory| {
        set_project(system, path, project, directory)
    })
}

fn visit(
    path: &Path,
    project: u32,
    device: u64,
    update: &mut impl FnMut(&Path, u32, bool) -> io::Result<()>,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.dev() != device {
        return Err(io::Error::other("Boat quota tree crosses a filesystem"));
    }
    // Never follow links into shared trees; special inodes have no writable data.
    if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
        return Ok(());
    }
    update(path, project, metadata.is_dir())?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            visit(&entry?.path(), project, device, update)?;
        }
    }
    Ok(())
}

fn set_project(
    system: &impl QuotaSystem,
    path: &Path,
    project: u32,
    directory: bool,
) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let mut attr = system.attributes(&file)?;
    attr = assignment(attr, project, directory);
    system.set_attributes(&file, &attr)
}

fn assign_owned(
    system: &impl QuotaSystem,
    path: &Path,
    project: u32,
    device: u64,
) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // A replaced root may no longer belong to this lease. Never follow a
    // symlink or special file during cleanup.
    if !metadata.is_dir() && !metadata.is_file() {
        return Ok(());
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !owns_project(system.attributes(&file)?.project, project) {
        return Ok(());
    }
    visit(path, 0, device, &mut |path, _, directory| {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let attr = system.attributes(&file)?;
        if owns_project(attr.project, project) {
            system.set_attributes(&file, &assignment(attr, 0, directory))?;
        }
        Ok(())
    })
}

fn owns_project(current: u32, lease: u32) -> bool {
    current == lease
}

fn available(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: statvfs initializes value on success; name is NUL terminated.
    if unsafe { libc::statvfs(name.as_ptr(), value.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let value = unsafe { value.assume_init() };
    Ok((
        value.f_bavail.saturating_mul(value.f_frsize),
        value.f_favail,
    ))
}

fn budget(xfs: u64, root: u64, reserved: u64, cap: u64, floor: u64) -> io::Result<u64> {
    let result = xfs
        .min(root)
        .saturating_sub(floor)
        .saturating_sub(reserved)
        .min(cap);
    if result == 0 {
        Err(io::Error::other("no unreserved Boat job quota capacity"))
    } else {
        Ok(result)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Legacy,
    Pending,
    Active,
    Releasing,
}

#[derive(Serialize, Deserialize)]
struct Lease {
    project: u32,
    roots: Vec<PathBuf>,
    blocks: u64,
    inodes: u64,
    growth_bytes: u64,
    #[serde(default)]
    phase: Phase,
}

impl Lease {
    fn active(&self) -> bool {
        self.phase == Phase::Active || (self.phase == Phase::Legacy && self.blocks != 0)
    }
}

fn usage(system: &impl QuotaSystem, project: u32) -> io::Result<Option<DiskQuota>> {
    match system.quota(project, None) {
        Ok(usage) => Ok(Some(usage)),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(error) => Err(error),
    }
}

fn lock(state: &Path) -> io::Result<(File, PathBuf)> {
    let directory = state.join("boat-quotas");
    fs::create_dir_all(&directory)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("lock"))?;
    // SAFETY: flock holds this open file until the returned File is dropped.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((file, directory))
}

pub(crate) fn prepare(state: &Path, id: Uuid, roots: Vec<PathBuf>) -> io::Result<()> {
    prepare_with(&System, state, id, roots)
}

fn prepare_with(
    system: &impl QuotaSystem,
    state: &Path,
    id: Uuid,
    roots: Vec<PathBuf>,
) -> io::Result<()> {
    let (_lock, directory) = lock(state)?;
    let manifest = directory.join(format!("{id}.json"));
    if manifest.exists() {
        return Err(io::Error::other("Boat job already has a quota lease"));
    }
    let mut used_ids = std::collections::BTreeSet::new();
    let (mut reserved_blocks, mut reserved_inodes) = (0u64, 0u64);
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let lease: Lease = serde_json::from_slice(&fs::read(path)?)?;
            used_ids.insert(lease.project);
            // Pending or releasing leases cannot launch a job. Missing quota records
            // are also normal before assignment and after limits were cleared.
            if !lease.active() {
                continue;
            }
            let Some(usage) = usage(system, lease.project)? else {
                continue;
            };
            reserved_blocks =
                reserved_blocks.saturating_add(lease.blocks.saturating_sub(usage.blocks));
            reserved_inodes =
                reserved_inodes.saturating_add(lease.inodes.saturating_sub(usage.inodes));
        }
    }
    let project = (1..u32::MAX)
        .find(|id| !used_ids.contains(id))
        .ok_or_else(|| io::Error::other("Boat project IDs exhausted"))?;
    let (xfs, inodes) = system.available(system.mount())?;
    let (root, _) = system.available(Path::new("/"))?;
    // The replay tail is written by the runner, outside the job project. Keep
    // its maximum size in addition to the provisional snapshot reserve.
    let bytes = budget(
        xfs,
        root,
        reserved_blocks.saturating_mul(512),
        BLOCK_CAP,
        PROVISIONAL_RESERVE + crate::storage::REPLAY_BYTES,
    )? / 512
        * 512;
    let inode_budget = budget(inodes, inodes, reserved_inodes, INODE_CAP, INODE_RESERVE)?;
    if bytes == 0 {
        return Err(io::Error::other("Boat quota smaller than one block"));
    }
    let device = fs::metadata(system.mount())?.dev();
    // Persist before touching inodes: runtime cleanup can undo partial assignment.
    let mut lease = Lease {
        project,
        roots,
        blocks: 0,
        inodes: 0,
        growth_bytes: bytes,
        phase: Phase::Pending,
    };
    crate::storage::atomic_json_private(&manifest, &lease)?;
    system.monitor(&manifest, project, device)?;
    for path in &lease.roots {
        assign(system, path, project, device)?;
    }
    let initial = system.quota(project, None)?;
    lease.blocks = initial.blocks + bytes / 512;
    lease.inodes = initial.inodes + inode_budget;
    crate::storage::atomic_json_private(&manifest, &lease)?;
    system.quota(project, Some((lease.blocks, lease.inodes)))?;
    lease.phase = Phase::Active;
    crate::storage::atomic_json_private(&manifest, &lease)?;
    tracing::info!(job_id = %id, project, writable_bytes = bytes, inode_budget, block_limit = lease.blocks, inode_limit = lease.inodes, xfs_free = xfs, root_free = root, provisional_reserve = PROVISIONAL_RESERVE, "Boat job disk quota installed");
    Ok(())
}

// Called only after the cgroup is gone, including recovered-runtime cleanup.
pub(crate) fn release_runtime(runtime: &Path) -> io::Result<()> {
    release_runtime_with(&System, runtime)
}

fn release_runtime_with(system: &impl QuotaSystem, runtime: &Path) -> io::Result<()> {
    let Some(state) = runtime.parent().and_then(Path::parent) else {
        return Ok(());
    };
    let Some(id) = runtime.file_name() else {
        return Ok(());
    };
    let manifest = state.join("boat-quotas").join(id).with_extension("json");
    // Non-Boat profiles never create this directory. The manifest itself must
    // be read only after taking the lock: two cleanup owners may race.
    if !state.join("boat-quotas").exists() {
        return Ok(());
    }
    #[cfg(test)]
    system.before_release_lock();
    let (_lock, _) = lock(state)?;
    let mut lease: Lease = match fs::read(&manifest) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    lease.phase = Phase::Releasing;
    crate::storage::atomic_json_private(&manifest, &lease)?;
    let usage = usage(system, lease.project)?.unwrap_or_default();
    let hit_observed = system.finish_monitor(&manifest);
    tracing::info!(job_id = ?id, project = lease.project, blocks = usage.blocks, block_limit = lease.blocks, inodes = usage.inodes, inode_limit = lease.inodes,
        hit_observed, "Boat job disk quota cleanup");
    let device = fs::metadata(system.mount())?.dev();
    for path in &lease.roots {
        assign_owned(system, path, lease.project, device)?;
    }
    if let Err(error) = system.quota(lease.project, Some((0, 0))) {
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error);
        }
    }
    if hit_observed {
        fs::remove_file(manifest.with_extension("hit"))?;
    }
    fs::remove_file(manifest)
}

#[cfg(test)]
#[path = "boat_quota_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn job_roots_include_reused_trees_and_all_private_writable_storage() {
        assert_eq!(
            job_roots(
                Path::new("/workspace"),
                Path::new("/target"),
                Path::new("/state/sandboxes/job"),
                Path::new("/state/spools/job")
            ),
            [
                PathBuf::from("/workspace"),
                PathBuf::from("/target"),
                PathBuf::from("/state/sandboxes/job"),
                PathBuf::from("/state/spools/job")
            ]
        );
    }
    #[test]
    fn budget_preserves_reserve_on_both_disks_and_caps_growth() {
        assert_eq!(budget(100, 80, 10, 90, 24).unwrap(), 46);
        assert_eq!(budget(80, 100, 10, 90, 24).unwrap(), 46);
        assert_eq!(budget(100, 100, 0, 32, 24).unwrap(), 32);
        assert!(budget(23, 100, 0, 32, 24).is_err());
        assert!(budget(100, 24, 0, 32, 24).is_err());
        assert!(budget(100, 100, u64::MAX, 32, 24).is_err());
    }
    #[test]
    fn project_assignment_preserves_flags_and_inherits_only_on_directories() {
        let directory = assignment(
            FsXattr {
                flags: 0x80,
                ..Default::default()
            },
            42,
            true,
        );
        assert_eq!((directory.project, directory.flags), (42, 0x280));
        let cleared = assignment(directory, 0, true);
        assert_eq!((cleared.project, cleared.flags), (0, 0x80));
        let file = assignment(FsXattr::default(), 42, false);
        assert_eq!((file.project, file.flags), (42, 0));
        assert_eq!(std::mem::size_of::<FsXattr>(), 28);
        assert_eq!(std::mem::size_of::<DiskQuota>(), 112);
    }

    #[test]
    fn assignment_covers_entire_tree_with_one_id_without_following_links() {
        let scratch = cortexkit_test_support::ScratchDir::new("boat-project-walk");
        let root = scratch.path();
        fs::create_dir(root.join("child")).unwrap();
        fs::write(root.join("child/file"), b"bytes").unwrap();
        std::os::unix::fs::symlink("/", root.join("outside")).unwrap();
        let device = fs::metadata(root).unwrap().dev();
        let mut assigned = Vec::new();
        visit(root, 42, device, &mut |path, id, directory| {
            assigned.push((
                path.strip_prefix(root).unwrap().to_path_buf(),
                id,
                directory,
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            assigned,
            [
                (PathBuf::new(), 42, true),
                (PathBuf::from("child"), 42, true),
                (PathBuf::from("child/file"), 42, false)
            ]
        );
        assert!(visit(root, 42, device + 1, &mut |_, _, _| panic!(
            "cross-device assignment"
        ))
        .is_err());
    }
}
