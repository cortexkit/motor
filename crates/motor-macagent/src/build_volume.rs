//! The job's build disk, mounted at the checkout's `target/` while the command
//! runs, so builds never list or delete files on the shared folder.
//!
//! Root-only, and it runs before the job's command and again after every job
//! process is gone, so it trusts nothing the job could have written: it never
//! follows a link, opens every path from a directory it already holds, and
//! mounts the volume `nosuid,nodev`. The host only creates, clones and renames
//! the disk's file; formatting and mounting happen here.
//!
//! Before the command, `attach` finds the disk by its fixed size, formats it as
//! APFS only while it is still blank (all zero, as the host creates it), mounts
//! its volume over the empty shared `target/` directory and gives the volume's
//! root to the job account. A volume that is missing, ambiguous or refuses to
//! mount is reformatted, and the job's output says its build starts cold.
//!
//! After the command, `detach` copies the requested returned files under
//! `target/` into a private local folder, unmounts the volume (forcing it if a
//! killed process still holds it busy), and then writes them into the now
//! uncovered shared `target/`, where the host's usual collection reads them.
//! The agent reports the command's exit only after this, so the host publishes
//! a disk only when its volume was unmounted cleanly.
use crate::Mount;
use motor_macvm::BuildConfig;
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
};

/// The name `diskutil` gives the volume when it formats a build disk.
pub const VOLUME_NAME: &str = "MotorBuild";
/// How much of the disk must be zero for it to count as never formatted. The
/// formatter writes its partition map and container in the first sectors.
pub const BLANK_PROBE_BYTES: usize = 1 << 20;

/// The build volume as `serve` uses it; tests stand in for the real disk.
pub trait BuildVolume: Send {
    /// Mounts the build volume at `target/`. Returns a line for the job's
    /// standard error when the volume had to be reformatted, so the build
    /// starts cold.
    fn attach(&mut self) -> io::Result<Option<String>>;
    /// Call only once every job process is gone. Copies the returned files
    /// out, unmounts the volume and puts the copies in the shared `target/`.
    fn detach(&mut self) -> io::Result<()>;
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}

/// The checkout's `target/` in the guest: the workspace share's destination,
/// which the boot map binds to the host-staged checkout.
pub fn target_of(mounts: &[Mount]) -> io::Result<PathBuf> {
    let [workspace] = mounts
        .iter()
        .filter(|mount| mount.tag == "workspace")
        .collect::<Vec<_>>()[..]
    else {
        return Err(invalid("boot map has no single workspace share"));
    };
    Ok(workspace.destination.join("target"))
}

/// The one whole disk of exactly the build-disk size. More than one, or none,
/// refuses the job: no other disk the VM sees has that size.
pub fn select_build_disk(disks: &[(String, u64)]) -> io::Result<String> {
    let matches: Vec<_> = disks
        .iter()
        .filter(|(name, size)| {
            *size == motor_macvm::BUILD_DISK_SIZE && motor_macvm::apfs::whole_disk(name)
        })
        .collect();
    match matches[..] {
        [(name, _)] => Ok(name.clone()),
        [] => Err(invalid("build disk not found")),
        _ => Err(invalid("more than one disk has the build disk's size")),
    }
}

/// A disk the host created and no guest has formatted is all zero.
pub fn is_blank(start: &[u8]) -> bool {
    start.iter().all(|byte| *byte == 0)
}

fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(invalid)
}
fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
fn open_at(directory: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
    let name = c_name(name)?;
    let fd = check(unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644 as libc::c_uint,
        )
    })?;
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn open_directory_at(directory: &File, name: &str) -> io::Result<File> {
    open_at(directory, name, libc::O_RDONLY | libc::O_DIRECTORY)
}

/// The normal components of an absolute path, refusing `.`, `..` and empty ones.
pub fn components(path: &Path) -> io::Result<Vec<&str>> {
    let text = path
        .to_str()
        .and_then(|text| text.strip_prefix('/'))
        .ok_or_else(|| invalid(format!("not an absolute path: {}", path.display())))?;
    let parts: Vec<&str> = text.split('/').collect();
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return Err(invalid(format!("not a normal path: {}", path.display())));
    }
    Ok(parts)
}

/// Whether the job account could rename or replace entries in a directory:
/// it owns it, or the directory is writable by its group or by everyone.
fn job_writable(metadata: &std::fs::Metadata, uid: libc::uid_t, gid: libc::gid_t) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == uid
        || metadata.mode() & 0o002 != 0
        || (metadata.gid() == gid && metadata.mode() & 0o020 != 0)
}

/// Opens a directory by walking from `root` one component at a time, never
/// following a link. With `job`, every directory above the last one, `root`
/// included, must be out of the job account's reach (see `job_writable`).
fn walk(root: &File, parts: &[&str], job: Option<(libc::uid_t, libc::gid_t)>) -> io::Result<File> {
    let mut current = root.try_clone()?;
    for part in parts {
        if let Some((uid, gid)) = job
            && job_writable(&current.metadata()?, uid, gid)
        {
            return Err(invalid(format!(
                "a directory above the workspace is writable by the job account, at {part}"
            )));
        }
        current = open_directory_at(&current, part)
            .map_err(|e| invalid(format!("{part}: not a plain directory: {e}")))?;
    }
    Ok(current)
}

/// Opens the workspace share's directory before any job code runs. Refuses a
/// workspace below a directory the job account could change: during its run
/// the job could rename such a directory and leave a link in its place, so
/// that the path the agent unmounts afterwards names another mount.
pub fn open_workspace(
    root: &File,
    workspace: &Path,
    uid: libc::uid_t,
    gid: libc::gid_t,
) -> io::Result<File> {
    walk(root, &components(workspace)?, Some((uid, gid)))
}

/// Reopens a directory by its absolute path from `root` without following
/// any link, so a renamed ancestor cannot redirect the agent.
pub fn reopen(root: &File, path: &Path) -> io::Result<File> {
    walk(root, &components(path)?, None)
}

/// Gives the volume's root to the job account. An earlier job of the
/// checkout owned that root and may have set user flags such as `uchg` on it,
/// which would make even root's chmod fail; root clears them first.
pub fn take_ownership(root: &File, uid: libc::uid_t, gid: libc::gid_t) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        let flags = root.metadata()?.st_flags();
        if flags & libc::UF_SETTABLE != 0 {
            check(unsafe { libc::fchflags(root.as_raw_fd(), flags & !libc::UF_SETTABLE) })?;
        }
    }
    check(unsafe { libc::fchown(root.as_raw_fd(), uid, gid) })?;
    check(unsafe { libc::fchmod(root.as_raw_fd(), 0o755) })?;
    Ok(())
}

/// The disk operations `attach` chooses between, so tests can check its
/// decisions without a disk.
pub trait DiskSteps {
    /// Whether the build disk is all zero, as the host creates it.
    fn blank(&mut self) -> io::Result<bool>;
    /// Finds the disk's existing volume and mounts it at `target/`.
    fn mount_existing(&mut self) -> io::Result<()>;
    /// Unmounts anything from the disk, formats it and mounts the new volume.
    fn format_and_mount(&mut self) -> io::Result<()>;
    /// Gives the mounted volume's root to the job account.
    fn take_ownership(&mut self) -> io::Result<()>;
}

/// A blank disk is formatted. An existing volume that cannot be found,
/// mounted or handed to the job account is reformatted, and the returned
/// notice tells the job its build starts cold. A freshly formatted volume
/// that still fails refuses the job: the command never runs with the shared
/// folder in the volume's place.
pub fn attach_with(steps: &mut impl DiskSteps) -> io::Result<Option<String>> {
    if steps.blank()? {
        steps.format_and_mount()?;
        steps.take_ownership()?;
        return Ok(None);
    }
    match steps.mount_existing().and_then(|()| steps.take_ownership()) {
        Ok(()) => Ok(None),
        Err(error) => {
            eprintln!("motor-macagent: reformatting the build disk: {error}");
            steps.format_and_mount()?;
            steps.take_ownership()?;
            Ok(Some(format!(
                "motor: the build disk could not be used ({error}); it was reformatted, so this build starts cold\n"
            )))
        }
    }
}

/// Opens the empty directory the volume is mounted over, creating it if the
/// checkout has none. Anything else at `target` (a file, a link, a directory
/// with entries) refuses the job: mounting over it would hide or follow it.
pub fn mount_point(workspace: &File) -> io::Result<File> {
    let directory = match open_directory_at(workspace, "target") {
        Ok(directory) => directory,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let name = c_name("target")?;
            check(unsafe { libc::mkdirat(workspace.as_raw_fd(), name.as_ptr(), 0o755) })?;
            open_directory_at(workspace, "target")?
        }
        Err(e) => return Err(invalid(format!("target is not a plain directory: {e}"))),
    };
    if !directory.metadata()?.is_dir() {
        return Err(invalid("target is not a directory"));
    }
    if entries(&directory)? != 0 {
        return Err(invalid("target is not empty"));
    }
    Ok(directory)
}

/// Counts a directory's entries other than `.` and `..`.
fn entries(directory: &File) -> io::Result<usize> {
    let fd = check(unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }
    let mut count = 0;
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            count += 1;
        }
    }
    unsafe { libc::closedir(stream) };
    Ok(count)
}

/// A requested returned file found on the build volume.
#[derive(Debug, PartialEq, Eq)]
pub enum Staged {
    /// Copied to the staging folder under this index.
    Copied { path: String, index: usize },
    /// Larger than what was left of the cap; none of its bytes are copied.
    /// The host's placeholder is a sparse file of `size` bytes, which is
    /// always more than the whole cap. The host budgets every returned path
    /// while the agent budgets only those under `target/`, so a placeholder of
    /// the file's real size could still fit the host's remaining budget and be
    /// returned as zeros. One larger than the cap never fits, so the host
    /// always refuses it as over the cap and never reads it.
    OverCap { path: String, size: u64 },
}

/// Opens a regular file by a relative path below `root` without following
/// any link, and only while every opened component is on `device`, the build
/// volume's: a link cannot lead off the volume, and neither can another file
/// system mounted below it. `None` if any component is missing, a link, not a
/// directory or on another device, or if the file is not regular.
fn open_below(root: &File, device: u64, path: &str) -> Option<File> {
    use std::os::unix::fs::MetadataExt;
    let mut components = path.split('/').peekable();
    let mut directory: Option<File> = None;
    while let Some(component) = components.next() {
        if matches!(component, "" | "." | "..") {
            return None;
        }
        let parent = directory.as_ref().unwrap_or(root);
        if components.peek().is_none() {
            let file = open_at(parent, component, libc::O_RDONLY | libc::O_NONBLOCK).ok()?;
            let metadata = file.metadata().ok()?;
            return (metadata.is_file() && metadata.dev() == device).then_some(file);
        }
        let next = open_directory_at(parent, component).ok()?;
        if next.metadata().ok()?.dev() != device {
            return None;
        }
        directory = Some(next);
    }
    None
}

/// Copies the requested files from the mounted volume into `staging`, a
/// private folder on the guest's own disk. Files are admitted in request order
/// while their sizes fit under the cap, the rule the host applies when it
/// collects them. A file the host would refuse is not copied, and a path that
/// is missing, a link or not a regular file is left out, so the host reports
/// it missing.
pub fn stage_returns(
    volume: &File,
    build: &BuildConfig,
    staging: &Path,
) -> io::Result<Vec<Staged>> {
    use std::os::unix::fs::MetadataExt;
    let device = volume.metadata()?.dev();
    let mut staged = Vec::new();
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for path in &build.returns {
        let Some(mut file) = open_below(volume, device, path) else {
            continue;
        };
        let size = file.metadata()?.len();
        if total.saturating_add(size) > build.return_max_bytes {
            if seen.insert(path.clone()) {
                staged.push(Staged::OverCap {
                    path: path.clone(),
                    size: size.max(build.return_max_bytes.saturating_add(1)),
                });
            }
            continue;
        }
        total += size;
        // A repeated path counts against the cap again, as on the host, but
        // its bytes are copied once.
        if !seen.insert(path.clone()) {
            continue;
        }
        let index = staged.len();
        let mut copy = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staging.join(index.to_string()))?;
        let copied = io::copy(&mut (&mut file).take(size), &mut copy)?;
        if copied != size || file.metadata()?.len() != size {
            // The file changed size; leave it out rather than return a torn copy.
            std::fs::remove_file(staging.join(index.to_string()))?;
            continue;
        }
        staged.push(Staged::Copied {
            path: path.clone(),
            index,
        });
    }
    Ok(staged)
}

/// Writes the staged files into the shared `target/` directory at their
/// requested paths, creating directories as needed and never following or
/// replacing anything already there. A file that cannot be placed is left out
/// and reported missing by the host; the job itself is not failed for it.
pub fn place_returns(target: &File, staged: &[Staged], staging: &Path) -> io::Result<()> {
    for entry in staged {
        let path = match entry {
            Staged::Copied { path, .. } | Staged::OverCap { path, .. } => path,
        };
        let result = (|| -> io::Result<()> {
            let mut components: Vec<&str> = path.split('/').collect();
            let name = components.pop().ok_or_else(|| invalid("empty path"))?;
            let mut directory = None;
            for component in components {
                let parent: &File = directory.as_ref().unwrap_or(target);
                let opened = match open_directory_at(parent, component) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        let c = c_name(component)?;
                        check(unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o755) })?;
                        open_directory_at(parent, component)?
                    }
                    result => result?,
                };
                directory = Some(opened);
            }
            let parent = directory.as_ref().unwrap_or(target);
            let mut output = open_at(parent, name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
            match entry {
                Staged::Copied { index, .. } => {
                    io::copy(
                        &mut File::open(staging.join(index.to_string()))?,
                        &mut output,
                    )?;
                }
                Staged::OverCap { size, .. } => output.set_len(*size)?,
            }
            output.flush()
        })();
        if let Err(error) = result {
            eprintln!("motor-macagent: returned file {path} not placed: {error}");
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub use native::Native;

/// Stands in for the build volume where there is no macOS disk to mount; a job
/// never runs without its build volume.
#[cfg(not(target_os = "macos"))]
pub struct Unsupported;
#[cfg(not(target_os = "macos"))]
impl BuildVolume for Unsupported {
    fn attach(&mut self) -> io::Result<Option<String>> {
        Err(invalid("the build volume requires a macOS guest"))
    }
    fn detach(&mut self) -> io::Result<()> {
        Err(invalid("the build volume requires a macOS guest"))
    }
}

/// The real disk. Every tool runs from its absolute system path with an empty
/// environment: `diskutil eraseDisk` and `diskutil apfs list`, `plutil` to
/// turn that list into JSON, and `mount_apfs`. Everything else is a system call.
#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    };
    use std::time::{Duration, Instant};

    const DISKUTIL: &str = "/usr/sbin/diskutil";
    const PLUTIL: &str = "/usr/bin/plutil";
    const MOUNT_APFS: &str = "/sbin/mount_apfs";
    // _IOR('d', 24, uint32_t) and _IOR('d', 25, uint64_t) from <sys/disk.h>.
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;
    /// The system attaches an existing container shortly after boot.
    const ATTACH_WAIT: Duration = Duration::from_secs(30);

    pub struct Native {
        target: PathBuf,
        build: BuildConfig,
        uid: libc::uid_t,
        gid: libc::gid_t,
        /// The workspace share's directory, opened before the job ran.
        workspace: Option<File>,
        /// The mounted volume's file system ID and its root's inode, recorded
        /// before the job ran.
        root: Option<Identity>,
    }

    /// What names the mounted build volume's root: the file system ID and the
    /// root directory's inode number.
    type Identity = ([i32; 2], u64);

    fn identity(directory: &File) -> io::Result<Identity> {
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatfs(directory.as_raw_fd(), &mut stat) })?;
        Ok((fsid(stat.f_fsid), directory.metadata()?.ino()))
    }
    fn fsid(fsid: libc::fsid_t) -> [i32; 2] {
        // libc keeps fsid_t's two words private; it is exactly that array.
        unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(fsid) }
    }
    fn open_root() -> io::Result<File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open("/")
    }

    impl Native {
        pub fn new(
            mounts: &[Mount],
            build: BuildConfig,
            uid: libc::uid_t,
            gid: libc::gid_t,
        ) -> io::Result<Self> {
            if uid == 0 || gid == 0 {
                return Err(invalid("build volume owner must not be root"));
            }
            build.validate()?;
            if build.return_max_bytes > motor_protocol::RETURN_LARGE_MAX_BYTES_CAP {
                return Err(invalid("returned-file cap exceeds the protocol's ceiling"));
            }
            Ok(Self {
                target: target_of(mounts)?,
                build,
                uid,
                gid,
                workspace: None,
                root: None,
            })
        }
    }

    /// The real disk operations behind `attach_with`, before any job code runs.
    struct Steps<'a> {
        native: &'a Native,
        workspace: &'a File,
        disk: String,
        raw: Option<File>,
        /// The volume currently mounted at `target/`, such as `disk5s1`.
        volume: Option<String>,
    }

    impl Steps<'_> {
        fn mount(&mut self, volume: String) -> io::Result<()> {
            let device = format!("/dev/{volume}");
            let target = &self.native.target;
            // The system may already have mounted the volume, under /Volumes.
            for mount in mounted()? {
                if mount.from == device {
                    unmount(&mount.on, mount.fsid)?;
                }
            }
            run(
                MOUNT_APFS,
                &[
                    "-o".as_ref(),
                    "nosuid,nodev".as_ref(),
                    device.as_ref(),
                    target.as_os_str(),
                ],
                None,
            )?;
            self.volume = Some(volume);
            let at_target: Vec<_> = mounted()?
                .into_iter()
                .filter(|mount| mount.on == *target)
                .collect();
            match &at_target[..] {
                [mount]
                    if mount.from == device
                        && mount.filesystem == "apfs"
                        && mount.flags & libc::MNT_NOSUID as u32 != 0
                        && mount.flags & libc::MNT_NODEV as u32 != 0
                        && mount.flags & libc::MNT_RDONLY as u32 == 0 => {}
                _ => {
                    return Err(invalid(format!(
                        "build volume mount does not match: {at_target:?}"
                    )));
                }
            }
            Ok(())
        }

        /// The mounted volume's root, reached through the workspace share and
        /// checked to be the root of the volume mounted at `target/`.
        fn root(&self) -> io::Result<File> {
            let volume = self
                .volume
                .as_deref()
                .ok_or_else(|| invalid("no build volume is mounted"))?;
            let root = open_directory_at(self.workspace, "target")?;
            let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
            check(unsafe { libc::fstatfs(root.as_raw_fd(), &mut stat) })?;
            let text = |bytes: &[libc::c_char]| {
                unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            };
            if text(&stat.f_mntfromname) != format!("/dev/{volume}")
                || Path::new(&text(&stat.f_mntonname)) != self.native.target
            {
                return Err(invalid("target is not the build volume's root"));
            }
            Ok(root)
        }
    }

    impl DiskSteps for Steps<'_> {
        fn blank(&mut self) -> io::Result<bool> {
            let raw = self
                .raw
                .take()
                .ok_or_else(|| invalid("build disk already read"))?;
            Ok(is_blank(&read_start(raw)?))
        }
        fn mount_existing(&mut self) -> io::Result<()> {
            let volume = wait_volume(&self.disk, ATTACH_WAIT)?
                .ok_or_else(|| invalid("the disk holds no APFS volume"))?;
            self.mount(volume)
        }
        fn format_and_mount(&mut self) -> io::Result<()> {
            if let Some(volume) = self.volume.take() {
                let device = format!("/dev/{volume}");
                for mount in mounted()? {
                    if mount.from == device {
                        unmount(&mount.on, mount.fsid)?;
                    }
                }
            }
            run(
                DISKUTIL,
                &[
                    "eraseDisk".as_ref(),
                    "APFS".as_ref(),
                    VOLUME_NAME.as_ref(),
                    "GPT".as_ref(),
                    format!("/dev/{}", self.disk).as_ref(),
                ],
                None,
            )?;
            let volume = wait_volume(&self.disk, ATTACH_WAIT)?
                .ok_or_else(|| invalid("formatted build disk shows no APFS volume"))?;
            self.mount(volume)
        }
        fn take_ownership(&mut self) -> io::Result<()> {
            super::take_ownership(&self.root()?, self.native.uid, self.native.gid)
        }
    }

    impl BuildVolume for Native {
        fn attach(&mut self) -> io::Result<Option<String>> {
            let workspace = open_workspace(
                &open_root()?,
                self.target
                    .parent()
                    .ok_or_else(|| invalid("target parent"))?,
                self.uid,
                self.gid,
            )?;
            drop(mount_point(&workspace)?);
            let (disk, raw) = find_disk(self.gid)?;
            let mut steps = Steps {
                native: self,
                workspace: &workspace,
                disk,
                raw: Some(raw),
                volume: None,
            };
            let notice = attach_with(&mut steps)?;
            let root = identity(&steps.root()?)?;
            self.root = Some(root);
            self.workspace = Some(workspace);
            Ok(notice)
        }

        fn detach(&mut self) -> io::Result<()> {
            let (Some(workspace), Some(recorded)) = (self.workspace.take(), self.root.take())
            else {
                return Err(invalid("build volume is not attached"));
            };
            // Job code has run: reach target/ again from / without following
            // links, and require the volume root recorded before the job.
            let root = reopen(&open_root()?, &self.target)?;
            if identity(&root)? != recorded {
                return Err(invalid("target no longer names the build volume's root"));
            }
            let staging = tempfile::Builder::new()
                .prefix("motor-returns-")
                .tempdir_in("/var/tmp")?;
            let staged = if self.build.returns.is_empty() {
                Vec::new()
            } else {
                stage_returns(&root, &self.build, staging.path())?
            };
            // The root's descriptor must close before the unmount.
            drop(root);
            let entries: Vec<_> = mounted()?
                .into_iter()
                .filter(|mount| mount.fsid == recorded.0)
                .collect();
            if !matches!(&entries[..], [mount] if mount.on == self.target) {
                return Err(invalid(format!(
                    "build volume is not mounted exactly once at target: {entries:?}"
                )));
            }
            unmount(&self.target, recorded.0)?;
            let shared = open_directory_at(&workspace, "target")?;
            if shared.metadata()?.dev() != workspace.metadata()?.dev() {
                return Err(invalid("target is still a mount point after unmounting"));
            }
            place_returns(&shared, &staged, staging.path())
        }
    }

    #[derive(Debug)]
    struct Mounted {
        from: String,
        on: PathBuf,
        filesystem: String,
        flags: u32,
        fsid: [i32; 2],
    }

    fn mounted() -> io::Result<Vec<Mounted>> {
        let mut mounts = std::ptr::null_mut();
        let count = unsafe { libc::getmntinfo(&mut mounts, libc::MNT_NOWAIT) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        let text = |bytes: &[libc::c_char]| {
            unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        Ok(
            unsafe { std::slice::from_raw_parts(mounts, count as usize) }
                .iter()
                .map(|mount| Mounted {
                    from: text(&mount.f_mntfromname),
                    on: text(&mount.f_mntonname).into(),
                    filesystem: text(&mount.f_fstypename),
                    flags: mount.f_flags,
                    fsid: fsid(mount.f_fsid),
                })
                .collect(),
        )
    }

    /// Unmounts the file system `fsid` mounted at `path`. Call only once the
    /// path has been checked to reach it. A process killed with the job can
    /// briefly keep the volume busy, and the job's processes are all gone, so
    /// a busy unmount is forced.
    fn unmount(path: &Path, fsid: [i32; 2]) -> io::Result<()> {
        let path_c = CString::new(path.as_os_str().as_bytes()).map_err(invalid)?;
        if unsafe { libc::unmount(path_c.as_ptr(), 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBUSY) {
                return Err(error);
            }
            eprintln!("motor-macagent: forcing busy unmount of {}", path.display());
            check(unsafe { libc::unmount(path_c.as_ptr(), libc::MNT_FORCE) })?;
        }
        if mounted()?.iter().any(|mount| mount.fsid == fsid) {
            return Err(invalid(format!("{} is still mounted", path.display())));
        }
        Ok(())
    }

    fn run(tool: &str, args: &[&std::ffi::OsStr], input: Option<&[u8]>) -> io::Result<Vec<u8>> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(tool)
            .args(args)
            .env_clear()
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = input {
            // The tools here read all input before writing output.
            child.stdin.take().unwrap().write_all(input)?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            return Err(invalid(format!(
                "{tool} failed ({}): {}",
                output.status,
                detail.chars().take(2000).collect::<String>()
            )));
        }
        Ok(output.stdout)
    }

    fn wait_volume(disk: &str, wait: Duration) -> io::Result<Option<String>> {
        let deadline = Instant::now() + wait;
        loop {
            let plist = run(
                DISKUTIL,
                &["apfs".as_ref(), "list".as_ref(), "-plist".as_ref()],
                None,
            )?;
            let json = run(
                PLUTIL,
                &[
                    "-convert".as_ref(),
                    "json".as_ref(),
                    "-o".as_ref(),
                    "-".as_ref(),
                    "-".as_ref(),
                ],
                Some(&plist),
            )?;
            if let Some(volume) = motor_macvm::apfs::volume_on_disk(&json, disk)? {
                return Ok(Some(volume));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Opens a disk's raw device after checking the job account cannot write
    /// it: the job must not be able to change the volume below the file system.
    fn open_raw(name: &str, job_gid: libc::gid_t) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(Path::new("/dev").join(format!("r{name}")))?;
        let writable = |metadata: &std::fs::Metadata| {
            metadata.mode() & 0o002 != 0
                || (metadata.gid() == job_gid && metadata.mode() & 0o020 != 0)
        };
        let raw = file.metadata()?;
        use std::os::unix::fs::FileTypeExt;
        let block = std::fs::symlink_metadata(Path::new("/dev").join(name))?;
        if raw.uid() != 0
            || !raw.file_type().is_char_device()
            || writable(&raw)
            || block.uid() != 0
            || !block.file_type().is_block_device()
            || writable(&block)
        {
            return Err(invalid(format!("{name} is not protected from motorjob")));
        }
        Ok(file)
    }

    fn size(file: &File) -> io::Result<u64> {
        let mut block: u32 = 0;
        let mut count: u64 = 0;
        check(unsafe { libc::ioctl(file.as_raw_fd(), DKIOCGETBLOCKSIZE, &mut block) })?;
        check(unsafe { libc::ioctl(file.as_raw_fd(), DKIOCGETBLOCKCOUNT, &mut count) })?;
        Ok(u64::from(block).saturating_mul(count))
    }

    /// The build disk's name, such as `disk4`, and its opened raw device.
    fn find_disk(job_gid: libc::gid_t) -> io::Result<(String, File)> {
        let mut disks = Vec::new();
        for entry in std::fs::read_dir("/dev")? {
            let entry = entry?;
            let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix('r'))
                .filter(|n| motor_macvm::apfs::whole_disk(n))
                .map(str::to_owned)
            else {
                continue;
            };
            let Ok(file) = std::fs::File::open(entry.path()) else {
                continue;
            };
            disks.push((name, size(&file)?));
        }
        let disk = select_build_disk(&disks)?;
        let raw = open_raw(&disk, job_gid)?;
        if size(&raw)? != motor_macvm::BUILD_DISK_SIZE {
            return Err(invalid("build disk changed while it was being opened"));
        }
        Ok((disk, raw))
    }

    /// Reads the disk's first bytes with one aligned read, as raw devices need.
    fn read_start(mut file: File) -> io::Result<Vec<u8>> {
        let mut buffer = vec![0u8; BLANK_PROBE_BYTES + 4096];
        let offset = buffer.as_ptr().align_offset(4096);
        let start = &mut buffer[offset..offset + BLANK_PROBE_BYTES];
        file.read_exact(start)?;
        Ok(start.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn mounts(workspace: &str) -> Vec<Mount> {
        vec![
            Mount {
                tag: "workspace".into(),
                destination: workspace.into(),
                read_only: false,
            },
            Mount {
                tag: "cargo-home".into(),
                destination: crate::CARGO_HOME.into(),
                read_only: false,
            },
        ]
    }
    #[test]
    fn target_is_the_workspace_shares_target_directory() {
        assert_eq!(
            target_of(&mounts("/Users/test/repo")).unwrap(),
            Path::new("/Users/test/repo/target")
        );
        assert!(target_of(&mounts("/Users/test/repo")[1..]).is_err());
        let mut twice = mounts("/Users/test/repo");
        twice.push(twice[0].clone());
        assert!(target_of(&twice).is_err());
    }
    #[test]
    fn build_disk_is_the_one_whole_disk_of_the_fixed_size() {
        let size = motor_macvm::BUILD_DISK_SIZE;
        let boot = ("disk0".to_owned(), 96 << 30);
        let nonce = ("disk1".to_owned(), motor_macvm::NONCE_IMAGE_SIZE as u64);
        let build = ("disk2".to_owned(), size);
        assert_eq!(
            select_build_disk(&[boot.clone(), nonce.clone(), build.clone()]).unwrap(),
            "disk2"
        );
        // A synthesized or partition identifier of that size is not the disk.
        let partition = ("disk2s2".to_owned(), size);
        assert_eq!(
            select_build_disk(&[partition, build.clone()]).unwrap(),
            "disk2"
        );
        assert!(select_build_disk(&[boot.clone(), nonce]).is_err());
        assert!(select_build_disk(&[build.clone(), ("disk3".into(), size)]).is_err());
        assert!(select_build_disk(&[("disk2".into(), size - 1)]).is_err());
        assert!(select_build_disk(&[("disk2".into(), size + 512)]).is_err());
    }
    #[test]
    fn only_an_all_zero_start_counts_as_blank() {
        assert!(is_blank(&vec![0; BLANK_PROBE_BYTES]));
        let mut formatted = vec![0; BLANK_PROBE_BYTES];
        formatted[512..520].copy_from_slice(b"EFI PART");
        assert!(!is_blank(&formatted));
        let mut last = vec![0; BLANK_PROBE_BYTES];
        *last.last_mut().unwrap() = 1;
        assert!(!is_blank(&last));
    }
    #[test]
    fn mount_point_is_created_or_must_be_an_empty_real_directory() {
        let scratch = cortexkit_test_support::ScratchDir::new("build-mount-point");
        let workspace = scratch.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let open = || File::open(&workspace).unwrap();
        mount_point(&open()).unwrap();
        assert!(workspace.join("target").is_dir());
        mount_point(&open()).unwrap();
        std::fs::write(workspace.join("target/stale"), "x").unwrap();
        assert!(mount_point(&open()).is_err());
        std::fs::remove_dir_all(workspace.join("target")).unwrap();
        std::fs::write(workspace.join("target"), "a file").unwrap();
        assert!(mount_point(&open()).is_err());
        std::fs::remove_file(workspace.join("target")).unwrap();
        let elsewhere = scratch.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, workspace.join("target")).unwrap();
        assert!(mount_point(&open()).is_err());
        symlink("missing", workspace.join("dangling")).unwrap();
        std::fs::remove_file(workspace.join("target")).unwrap();
        symlink(workspace.join("dangling"), workspace.join("target")).unwrap();
        assert!(mount_point(&open()).is_err());
        assert!(!scratch.join("missing").exists());
    }

    struct Volume {
        _scratch: cortexkit_test_support::ScratchDir,
        volume: PathBuf,
        staging: PathBuf,
        target: PathBuf,
        outside: PathBuf,
    }
    fn volume() -> Volume {
        let scratch = cortexkit_test_support::ScratchDir::new("build-returns");
        let [volume, staging, target, outside] =
            ["volume", "staging", "target", "outside"].map(|name| scratch.join(name));
        for path in [&volume, &staging, &target, &outside] {
            std::fs::create_dir(path).unwrap();
        }
        Volume {
            _scratch: scratch,
            volume,
            staging,
            target,
            outside,
        }
    }
    fn copy(v: &Volume, returns: &[&str], cap: u64) -> Vec<Staged> {
        let build = BuildConfig {
            returns: returns.iter().map(|p| p.to_string()).collect(),
            return_max_bytes: cap,
        };
        let staged = stage_returns(&File::open(&v.volume).unwrap(), &build, &v.staging).unwrap();
        place_returns(&File::open(&v.target).unwrap(), &staged, &v.staging).unwrap();
        staged
    }
    #[test]
    fn returned_files_are_copied_to_their_paths_in_request_order_under_the_cap() {
        let v = volume();
        std::fs::create_dir_all(v.volume.join("debug/deps")).unwrap();
        std::fs::write(v.volume.join("debug/app"), vec![1; 10]).unwrap();
        std::fs::write(v.volume.join("debug/deps/lib.rlib"), vec![2; 20]).unwrap();
        std::fs::write(v.volume.join("small"), vec![3; 5]).unwrap();
        let staged = copy(
            &v,
            &["debug/app", "debug/deps/lib.rlib", "small", "debug/app"],
            26,
        );
        assert_eq!(
            staged,
            vec![
                Staged::Copied {
                    path: "debug/app".into(),
                    index: 0
                },
                Staged::OverCap {
                    path: "debug/deps/lib.rlib".into(),
                    size: 27
                },
                Staged::Copied {
                    path: "small".into(),
                    index: 2
                },
            ]
        );
        assert_eq!(
            std::fs::read(v.target.join("debug/app")).unwrap(),
            vec![1; 10]
        );
        assert_eq!(std::fs::read(v.target.join("small")).unwrap(), vec![3; 5]);
        // The over-cap placeholder is larger than the whole cap, so the host
        // refuses it whatever else it admitted, and none of the file's bytes
        // left the volume.
        let placeholder = v.target.join("debug/deps/lib.rlib");
        assert_eq!(std::fs::metadata(&placeholder).unwrap().len(), 27);
        assert_eq!(std::fs::read(&placeholder).unwrap(), vec![0; 27]);
    }
    #[test]
    fn links_and_non_regular_files_on_the_volume_are_never_followed_or_copied() {
        let v = volume();
        std::fs::write(v.outside.join("secret"), "outside the volume").unwrap();
        std::fs::create_dir(v.volume.join("dir")).unwrap();
        symlink(v.outside.join("secret"), v.volume.join("leaf")).unwrap();
        symlink(&v.outside, v.volume.join("parent")).unwrap();
        symlink("../outside/secret", v.volume.join("relative")).unwrap();
        let staged = copy(
            &v,
            &[
                "leaf",
                "parent/secret",
                "relative",
                "dir",
                "missing",
                "dir/missing",
            ],
            1 << 20,
        );
        assert!(staged.is_empty());
        assert_eq!(std::fs::read_dir(&v.target).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&v.staging).unwrap().count(), 0);
    }
    #[test]
    fn placing_never_follows_or_replaces_what_is_already_in_the_shared_target() {
        let v = volume();
        std::fs::write(v.volume.join("app"), "built").unwrap();
        std::fs::create_dir(v.volume.join("debug")).unwrap();
        std::fs::write(v.volume.join("debug/app"), "built").unwrap();
        symlink(v.outside.join("written"), v.target.join("app")).unwrap();
        symlink(&v.outside, v.target.join("debug")).unwrap();
        copy(&v, &["app", "debug/app"], 1 << 20);
        assert!(!v.outside.join("written").exists());
        assert!(!v.outside.join("app").exists());
    }

    #[test]
    fn workspace_below_a_directory_the_job_can_change_is_refused() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let scratch = cortexkit_test_support::ScratchDir::new("build-workspace-walk");
        let root = scratch.join("root");
        let workspace = root.join("Users/p/repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        for path in [&root, &root.join("Users"), &root.join("Users/p")] {
            mode(path, 0o755);
        }
        // The workspace itself is the share; the job writes in it.
        mode(&workspace, 0o777);
        let open = |uid, gid| {
            open_workspace(
                &File::open(&root).unwrap(),
                Path::new("/Users/p/repo"),
                uid,
                gid,
            )
        };
        let me = std::fs::metadata(&root).unwrap();
        let (other_uid, other_gid) = (me.uid() + 1, me.gid() + 1);
        let opened = open(other_uid, other_gid).unwrap();
        assert_eq!(
            opened.metadata().unwrap().ino(),
            std::fs::metadata(&workspace).unwrap().ino()
        );
        // Owned by the job account, as a home directory would be.
        assert!(open(me.uid(), other_gid).is_err());
        mode(&root.join("Users/p"), 0o775);
        assert!(open(other_uid, me.gid()).is_err());
        assert!(open(other_uid, other_gid).is_ok());
        mode(&root.join("Users/p"), 0o757);
        assert!(open(other_uid, other_gid).is_err());
        mode(&root.join("Users/p"), 0o755);
        mode(&root, 0o777);
        assert!(open(other_uid, other_gid).is_err());
        mode(&root, 0o755);
        // A link in place of an ancestor is never followed.
        std::fs::rename(root.join("Users/p"), root.join("Users/moved")).unwrap();
        std::os::unix::fs::symlink(root.join("Users/moved"), root.join("Users/p")).unwrap();
        assert!(open(other_uid, other_gid).is_err());
        assert!(
            open_workspace(
                &File::open(&root).unwrap(),
                Path::new("/Users/../p"),
                other_uid,
                other_gid
            )
            .is_err()
        );
    }

    #[test]
    fn target_is_reopened_without_following_a_renamed_ancestor() {
        use std::os::unix::fs::MetadataExt;
        let scratch = cortexkit_test_support::ScratchDir::new("build-target-reopen");
        let root = scratch.join("root");
        std::fs::create_dir_all(root.join("Users/p/repo/target")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere/repo/target")).unwrap();
        let inode = |path: &Path| std::fs::metadata(path).unwrap().ino();
        let target = Path::new("/Users/p/repo/target");
        let reopened = reopen(&File::open(&root).unwrap(), target).unwrap();
        assert_eq!(
            reopened.metadata().unwrap().ino(),
            inode(&root.join("Users/p/repo/target"))
        );
        std::fs::rename(root.join("Users/p"), root.join("Users/moved")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("Users/p")).unwrap();
        assert!(reopen(&File::open(&root).unwrap(), target).is_err());
    }

    #[derive(Default)]
    struct FakeSteps {
        blank: bool,
        mount_fails: bool,
        /// Ownership fails while this many volumes are left to format.
        own_fails_until_formats: usize,
        own_always_fails: bool,
        log: Vec<&'static str>,
    }
    impl DiskSteps for FakeSteps {
        fn blank(&mut self) -> io::Result<bool> {
            Ok(self.blank)
        }
        fn mount_existing(&mut self) -> io::Result<()> {
            self.log.push("mount");
            if self.mount_fails {
                return Err(invalid("will not mount"));
            }
            Ok(())
        }
        fn format_and_mount(&mut self) -> io::Result<()> {
            self.log.push("format");
            self.own_fails_until_formats = self.own_fails_until_formats.saturating_sub(1);
            Ok(())
        }
        fn take_ownership(&mut self) -> io::Result<()> {
            self.log.push("own");
            if self.own_always_fails || self.own_fails_until_formats > 0 {
                return Err(invalid("operation not permitted"));
            }
            Ok(())
        }
    }
    #[test]
    fn attach_formats_blank_disks_and_reformats_any_volume_it_cannot_use() {
        let mut blank = FakeSteps {
            blank: true,
            ..FakeSteps::default()
        };
        assert_eq!(attach_with(&mut blank).unwrap(), None);
        assert_eq!(blank.log, ["format", "own"]);
        let mut warm = FakeSteps::default();
        assert_eq!(attach_with(&mut warm).unwrap(), None);
        assert_eq!(warm.log, ["mount", "own"]);
        let mut unmountable = FakeSteps {
            mount_fails: true,
            ..FakeSteps::default()
        };
        assert!(
            attach_with(&mut unmountable)
                .unwrap()
                .unwrap()
                .contains("starts cold")
        );
        assert_eq!(unmountable.log, ["mount", "format", "own"]);
        // An earlier job made the volume's root impossible to hand over: the
        // checkout heals by reformatting instead of failing every later job.
        let mut locked = FakeSteps {
            own_fails_until_formats: 1,
            ..FakeSteps::default()
        };
        let notice = attach_with(&mut locked).unwrap().unwrap();
        assert!(notice.contains("operation not permitted") && notice.contains("starts cold"));
        assert_eq!(locked.log, ["mount", "own", "format", "own"]);
        // A fresh volume that still fails refuses the job.
        let mut broken = FakeSteps {
            own_always_fails: true,
            ..FakeSteps::default()
        };
        assert!(attach_with(&mut broken).is_err());
        let mut broken_blank = FakeSteps {
            blank: true,
            own_always_fails: true,
            ..FakeSteps::default()
        };
        assert!(attach_with(&mut broken_blank).is_err());
    }

    #[test]
    fn returned_files_are_read_only_from_the_build_volumes_device() {
        use std::os::unix::fs::MetadataExt;
        let v = volume();
        std::fs::create_dir(v.volume.join("debug")).unwrap();
        std::fs::write(v.volume.join("debug/app"), "built").unwrap();
        let root = File::open(&v.volume).unwrap();
        let device = root.metadata().unwrap().dev();
        assert!(open_below(&root, device, "debug/app").is_some());
        // Anything on another device, such as a file system mounted below
        // target/, is not the build volume's.
        assert!(open_below(&root, device.wrapping_add(1), "debug/app").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ownership_is_taken_even_from_a_root_the_job_made_immutable() {
        use std::os::macos::fs::MetadataExt as _;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let scratch = cortexkit_test_support::ScratchDir::new("build-root-uchg");
        let root = scratch.join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = File::open(&root).unwrap();
        check(unsafe { libc::fchflags(file.as_raw_fd(), libc::UF_IMMUTABLE) }).unwrap();
        let me = file.metadata().unwrap();
        let result = take_ownership(&file, me.uid(), me.gid());
        // Leave the scratch directory removable whatever happened.
        unsafe { libc::fchflags(file.as_raw_fd(), 0) };
        result.unwrap();
        let after = std::fs::metadata(&root).unwrap();
        assert_eq!(after.mode() & 0o777, 0o755);
        assert_eq!(after.st_flags() & libc::UF_SETTABLE, 0);
    }
}
