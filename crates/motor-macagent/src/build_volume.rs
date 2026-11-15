//! The job's build disk, mounted at `BUILD_MOUNT` on the guest's own disk while
//! the command runs, so builds never list or delete files on the shared folder.
//! Jobs reach it as `CARGO_TARGET_DIR`, and through the checkout's `target`,
//! which the runner makes a link to it on the workspace share.
//!
//! Root-only, and it runs before the job's command and again after every job
//! process is gone, so it trusts nothing the job could have written: it never
//! follows a link, opens every path from a directory it already holds, and
//! mounts the volume `nosuid,nodev`. The host only creates, clones and renames
//! the disk's file; formatting and mounting happen here.
//!
//! The volume is not mounted over a directory of the workspace share: in the
//! guest, a path through a virtiofs directory stopped crossing a mount on it a
//! few seconds after mounting, and builds then wrote to the shared folder.
//!
//! Before the command, `attach` refuses a workspace below any directory the job
//! account could change and a checkout whose `target` is not the runner's link
//! to the mount. It prepares the mount point (every directory on its path
//! root's and out of the job's reach), finds the disk by its fixed size,
//! formats it as APFS only while it is still blank (all zero, as the host
//! creates it), mounts its volume and gives the volume's root to the job
//! account. A volume that is missing, ambiguous, refuses to mount or cannot be
//! handed over is reformatted, and the job's output says its build starts cold.
//! It records the volume root's file system ID and inode.
//!
//! After the command, `detach` reaches the mount again from `/` without
//! following links and requires that recorded root. It copies the requested
//! returned files under `target/` into a private local folder, unmounts the
//! volume (forcing it if a killed process still holds it busy), and then
//! creates `TARGET_RETURNS` in the cargo-home share, which must not exist yet,
//! and writes the copies there, where the host reads them.
//! The agent reports the command's exit after this. When any of it fails, its
//! last standard-error line says so, and the host then publishes no disk and
//! reads no returned file under `target/`.
//!
//! While the volume is mounted, a `Watcher` looks at the mount and at `target`
//! every `WATCH_INTERVAL` and reports, in one line before release, whether and
//! when either stopped naming the volume's root.
use crate::Mount;
use motor_macvm::BuildConfig;
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::File,
    io::{self, Read},
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
    /// Mounts the build volume at `BUILD_MOUNT`. Returns a line for the job's
    /// standard error when the volume had to be reformatted, so the build
    /// starts cold.
    fn attach(&mut self) -> io::Result<Option<String>>;
    /// Call only once every job process is gone. Copies the returned files
    /// out, unmounts the volume and puts the copies in the cargo-home share.
    /// `no_job_processes` fails if a job process is alive; it is called
    /// after the unmount and again after the copies are placed.
    fn detach(&mut self, no_job_processes: &dyn Fn() -> io::Result<()>) -> io::Result<()>;
    /// Metadata only, sent on the existing stderr channel at lifecycle boundaries.
    fn diagnostics(&self, _phase: &str) -> Option<String> {
        None
    }
    /// Tells the volume when the job's command started, for its watch report.
    fn command_started(&mut self) {}
    /// Stops watching `target/` and returns the one line saying whether it
    /// kept naming the volume's root while the job ran (see `Watcher`).
    fn watch_report(&mut self) -> Option<String> {
        None
    }
}

/// How often `Watcher` looks at the build volume while the job runs.
pub const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// One look at `target/`: whether it still named the build volume's root,
/// and the metadata that says so.
#[cfg(any(target_os = "macos", test))]
pub struct Look {
    pub on_volume: bool,
    pub detail: String,
}

/// What `Watcher` keeps: counts and at most two looks.
#[cfg(any(target_os = "macos", test))]
struct Watch {
    mounted: std::time::Instant,
    command: Option<std::time::Instant>,
    looks: u64,
    /// Milliseconds after mounting, and the look's metadata.
    last_good: Option<(u128, String)>,
    /// Milliseconds after mounting and after the command started (if it had),
    /// and the look's metadata.
    first_bad: Option<(u128, Option<u128>, String)>,
}

/// Looks at `target/` every interval from the moment the build volume is
/// mounted until it is released, and keeps the first look that found it no
/// longer naming the volume's root, together with the last look before that
/// which did. It stops looking after the first such look. It writes nothing
/// per look: `finish` returns one diagnostic line, and the agent's own log
/// gets one line when the change is seen.
#[cfg(any(target_os = "macos", test))]
pub struct Watcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    state: std::sync::Arc<std::sync::Mutex<Watch>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(any(target_os = "macos", test))]
impl Watcher {
    pub fn start(
        interval: std::time::Duration,
        mut look: impl FnMut() -> Look + Send + 'static,
    ) -> Self {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Instant;
        let mounted = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(Watch {
            mounted,
            command: None,
            looks: 0,
            last_good: None,
            first_bad: None,
        }));
        let (thread_stop, thread_state) = (stop.clone(), state.clone());
        let thread = std::thread::Builder::new()
            .name("build-volume-watch".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    let sample = look();
                    let now = Instant::now();
                    let since_mount = now.duration_since(mounted).as_millis();
                    {
                        let mut watch = thread_state.lock().unwrap();
                        watch.looks += 1;
                        if sample.on_volume {
                            watch.last_good = Some((since_mount, sample.detail));
                        } else {
                            let since_command = watch
                                .command
                                .map(|command| now.saturating_duration_since(command).as_millis());
                            eprintln!(
                                "motor-macagent: target stopped naming the build volume root {since_mount} ms after mounting: {}",
                                sample.detail
                            );
                            watch.first_bad = Some((since_mount, since_command, sample.detail));
                            return;
                        }
                    }
                    let next = now + interval;
                    loop {
                        let left = next.saturating_duration_since(Instant::now());
                        if left.is_zero() || thread_stop.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::park_timeout(left);
                    }
                }
            })
            .ok();
        Self {
            stop,
            state,
            thread,
        }
    }

    /// Records that the job's command has started.
    pub fn command_started(&self) {
        self.state.lock().unwrap().command = Some(std::time::Instant::now());
    }

    fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }

    /// Stops looking and returns the one diagnostic line.
    pub fn finish(&mut self) -> String {
        let running = self.thread.is_some();
        self.stop();
        let watch = self.state.lock().unwrap();
        let elapsed = watch.mounted.elapsed().as_millis();
        let last_good = match &watch.last_good {
            Some((at, detail)) => format!("{at} ms after mounting: {detail}"),
            None => "none".into(),
        };
        let detail = match &watch.first_bad {
            Some((at, since_command, detail)) => format!(
                "target stopped naming the build volume root at look {} (one every {} ms), {at} ms after mounting, {}: {detail}; last good look {last_good}",
                watch.looks,
                WATCH_INTERVAL.as_millis(),
                match since_command {
                    Some(ms) => format!("{ms} ms after the command started"),
                    None => "before the command started".into(),
                }
            ),
            None if !running && watch.looks == 0 => {
                "target was not watched: the watch thread could not start".into()
            }
            None => format!(
                "target named the build volume root in all {} looks over {elapsed} ms; last good look {last_good}",
                watch.looks
            ),
        };
        diagnostic_line("watch", &detail)
    }
}

#[cfg(any(target_os = "macos", test))]
impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(any(target_os = "macos", test))]
const DIAGNOSTIC_MAX_BYTES: usize = 16 * 1024;

#[cfg(any(target_os = "macos", test))]
fn diagnostic_line(phase: &str, detail: &str) -> String {
    const TRUNCATED: &str = " [truncated]";
    let mut line =
        format!("motor: build-volume diagnostic {phase}: {detail}").replace(['\n', '\r'], " ");
    if line.len() >= DIAGNOSTIC_MAX_BYTES {
        let mut end = DIAGNOSTIC_MAX_BYTES - TRUNCATED.len() - 1;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str(TRUNCATED);
    }
    line.push('\n');
    line
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}

/// The checkout's `target` in the guest, the runner's link to the build mount:
/// in the workspace share's destination, which the boot map binds to the
/// host-staged checkout.
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

/// The cargo-home share's destination in the guest, from the boot map.
pub fn cargo_home_of(mounts: &[Mount]) -> io::Result<PathBuf> {
    let [cargo_home] = mounts
        .iter()
        .filter(|mount| mount.tag == "cargo-home")
        .collect::<Vec<_>>()[..]
    else {
        return Err(invalid("boot map has no single cargo-home share"));
    };
    Ok(cargo_home.destination.clone())
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

/// What names a mounted file system's directory: the file system ID from
/// `fstatfs` and the directory's inode number.
pub type Identity = ([i32; 2], u64);

/// The identity of an open directory (see `Identity`).
pub fn identity(directory: &File) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstatfs(directory.as_raw_fd(), &mut stat) })?;
    Ok((fsid(stat.f_fsid), directory.metadata()?.ino()))
}

/// The two words of a file system ID.
pub(crate) fn fsid(fsid: libc::fsid_t) -> [i32; 2] {
    // libc keeps fsid_t's two words private; it is exactly that array.
    unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(fsid) }
}

/// The checkout's `target` as the agent sees it, read without following it:
/// whether it is the runner's link to the build mount, another link, or
/// something else. Never resolves the link, so the answer does not depend on
/// where a link the job made points.
pub fn target_state(target: &Path) -> &'static str {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_symlink() => match std::fs::read_link(target) {
            Ok(link) if link == Path::new(motor_macvm::BUILD_MOUNT) => "link-to-build-mount",
            Ok(_) => "link-elsewhere",
            Err(_) => "unreadable-link",
        },
        Ok(metadata) if metadata.is_dir() => "directory",
        Ok(_) => "other",
        Err(error) if error.kind() == io::ErrorKind::NotFound => "missing",
        Err(_) => "unreadable",
    }
}

/// Opens a share's directory before any job code runs, refusing one below a
/// directory the job account could change (as `open_workspace` does), and
/// records its identity, which `reopen_recorded` requires later.
pub fn open_share(
    root: &File,
    path: &Path,
    uid: libc::uid_t,
    gid: libc::gid_t,
) -> io::Result<(File, Identity)> {
    let share = walk(root, &components(path)?, Some((uid, gid)))?;
    let recorded = identity(&share)?;
    Ok((share, recorded))
}

/// Reopens a share's directory from `root` without following links and
/// requires the identity recorded before the job: the same file system and
/// the same directory, not one the job put in its place.
pub fn reopen_recorded(root: &File, path: &Path, recorded: Identity) -> io::Result<File> {
    let share = reopen(root, path)?;
    if identity(&share)? != recorded {
        return Err(invalid(format!(
            "{} is no longer the share recorded before the job",
            path.display()
        )));
    }
    Ok(share)
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
    /// Finds the disk's existing volume and mounts it at `BUILD_MOUNT`.
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

/// Opens `name` in `parent` as a directory without following a link,
/// creating it with mode 0755 when it is missing (the agent's umask would
/// otherwise leave it 0700).
fn directory_at(parent: &File, name: &str) -> io::Result<File> {
    match open_directory_at(parent, name) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let c = c_name(name)?;
            check(unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o755) })?;
            let directory = open_directory_at(parent, name)?;
            check(unsafe { libc::fchmod(directory.as_raw_fd(), 0o755) })?;
            Ok(directory)
        }
        result => result,
    }
}

/// Prepares the directory the build volume is mounted on, `mount`, before any
/// job code runs, and returns the directory that holds it. Every directory
/// from `root` down to the mount point is opened without following a link and
/// must be out of the job account's reach (see `job_writable`), so the job can
/// never rename one and redirect the agent's later unmount; a missing one is
/// created with mode 0755, owned by the agent (root). The mount point must be
/// an empty directory that nothing is mounted on yet.
pub fn mount_point(
    root: &File,
    mount: &Path,
    uid: libc::uid_t,
    gid: libc::gid_t,
) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;
    let mut parent = None;
    let mut current = root.try_clone()?;
    for part in components(mount)? {
        if job_writable(&current.metadata()?, uid, gid) {
            return Err(invalid(format!(
                "a directory above the build mount is writable by the job account, at {part}"
            )));
        }
        let next = directory_at(&current, part)
            .map_err(|e| invalid(format!("{part}: not a plain directory: {e}")))?;
        parent = Some(std::mem::replace(&mut current, next));
    }
    let parent = parent.ok_or_else(|| invalid("the build mount has no name"))?;
    let metadata = current.metadata()?;
    if job_writable(&metadata, uid, gid) {
        return Err(invalid(
            "the build mount point is writable by the job account",
        ));
    }
    if metadata.dev() != parent.metadata()?.dev() {
        return Err(invalid(
            "something is already mounted on the build mount point",
        ));
    }
    if entries(&current)? != 0 {
        return Err(invalid("the build mount point is not empty"));
    }
    Ok(parent)
}

/// Reads the link `name` in `directory` without following it.
fn read_link_at(directory: &File, name: &str) -> io::Result<Vec<u8>> {
    let c = c_name(name)?;
    let mut buffer = vec![0u8; 4097];
    let length = unsafe {
        libc::readlinkat(
            directory.as_raw_fd(),
            c.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize == buffer.len() {
        return Err(invalid("link target too long"));
    }
    buffer.truncate(length as usize);
    Ok(buffer)
}

/// Requires the checkout's `target` on the workspace share to be the link the
/// runner makes to `BUILD_MOUNT`, read without following it, so that paths
/// written `target/...` reach the build volume. Anything else there (a
/// directory, a file, another link) refuses the job.
pub fn check_target_link(workspace: &File) -> io::Result<()> {
    let link = read_link_at(workspace, "target").map_err(|e| {
        invalid(format!(
            "target is not the runner's link to the build volume: {e}"
        ))
    })?;
    if link != motor_macvm::BUILD_MOUNT.as_bytes() {
        return Err(invalid(format!(
            "target links to {:?}, not to the build volume at {}",
            String::from_utf8_lossy(&link),
            motor_macvm::BUILD_MOUNT
        )));
    }
    Ok(())
}

/// Creates the directory, in the job's cargo-home share, that receives the
/// returned files copied off the build volume, and opens it without following
/// a link. Call only once every job process is gone. It must not exist yet:
/// the job can write the share, so anything already there is the job's, and
/// the agent places nothing beside it. The host reads returned files under
/// `target/` only from this directory.
pub fn returns_directory(share: &File) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;
    let name = motor_macvm::TARGET_RETURNS;
    let c = c_name(name)?;
    if let Err(error) = check(unsafe { libc::mkdirat(share.as_raw_fd(), c.as_ptr(), 0o755) }) {
        return Err(if error.kind() == io::ErrorKind::AlreadyExists {
            invalid(format!(
                "{name} already exists in the cargo-home share: the job created it"
            ))
        } else {
            error
        });
    }
    let directory = open_directory_at(share, name)?;
    check(unsafe { libc::fchmod(directory.as_raw_fd(), 0o755) })?;
    if directory.metadata()?.dev() != share.metadata()?.dev() || entries(&directory)? != 0 {
        return Err(invalid(format!("{name} is not a new, empty directory")));
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

/// How `place_returns` fills one placed file; tests inject failures.
trait PlaceWrites {
    fn copy(&mut self, from: &mut File, to: &mut File) -> io::Result<()>;
    fn set_len(&mut self, file: &File, size: u64) -> io::Result<()>;
}
struct RealWrites;
impl PlaceWrites for RealWrites {
    fn copy(&mut self, from: &mut File, to: &mut File) -> io::Result<()> {
        io::copy(from, to).map(drop)
    }
    fn set_len(&mut self, file: &File, size: u64) -> io::Result<()> {
        file.set_len(size)
    }
}

/// Flushes a file or directory to its file system. A file system that does
/// not support it (EINVAL, ENOTSUP) is accepted: there is nothing more to do.
fn sync(file: &File) -> io::Result<()> {
    if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EINVAL) | Some(libc::ENOTSUP) => Ok(()),
        _ => Err(error),
    }
}

/// Writes the staged files into `target`, the returns directory, at their
/// requested paths, creating directories as needed and never following or
/// replacing anything already there. Each file is flushed, then the
/// directory holding it, before the next. A file that cannot be written whole
/// is removed through its directory, so the host reports it missing rather
/// than returning part of it; if it cannot be removed, placing fails, and so
/// does the release.
pub fn place_returns(target: &File, staged: &[Staged], staging: &Path) -> io::Result<()> {
    place_returns_with(target, staged, staging, &mut RealWrites)
}

fn place_returns_with(
    target: &File,
    staged: &[Staged],
    staging: &Path,
    writes: &mut impl PlaceWrites,
) -> io::Result<()> {
    for entry in staged {
        let path = match entry {
            Staged::Copied { path, .. } | Staged::OverCap { path, .. } => path,
        };
        let mut components: Vec<&str> = path.split('/').collect();
        let Some(name) = components.pop() else {
            continue;
        };
        let opened = (|| -> io::Result<(Option<File>, File)> {
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
            let output = open_at(parent, name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
            Ok((directory, output))
        })();
        // Nothing was created under the file's name when this fails.
        let (directory, mut output) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                eprintln!("motor-macagent: returned file {path} not placed: {error}");
                continue;
            }
        };
        let parent = directory.as_ref().unwrap_or(target);
        let written = (|| -> io::Result<()> {
            match entry {
                Staged::Copied { index, .. } => writes.copy(
                    &mut File::open(staging.join(index.to_string()))?,
                    &mut output,
                )?,
                Staged::OverCap { size, .. } => writes.set_len(&output, *size)?,
            }
            sync(&output)?;
            sync(parent)
        })();
        drop(output);
        if let Err(error) = written {
            let c = c_name(name)?;
            if unsafe { libc::unlinkat(parent.as_raw_fd(), c.as_ptr(), 0) } != 0 {
                return Err(invalid(format!(
                    "returned file {path} was not written whole ({error}) and could not be removed: {}",
                    io::Error::last_os_error()
                )));
            }
            eprintln!("motor-macagent: returned file {path} not placed: {error}");
        }
    }
    sync(target)
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
    fn detach(&mut self, _: &dyn Fn() -> io::Result<()>) -> io::Result<()> {
        Err(invalid("the build volume requires a macOS guest"))
    }
}

/// The real disk. Every tool runs from its absolute system path with an empty
/// environment: `diskutil eraseDisk` and `diskutil apfs list`, `plutil` to
/// turn that list into JSON, and `mount_apfs`. Everything else is a system call.
#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::io::Write;
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
        /// Where the volume is mounted: `BUILD_MOUNT`.
        mount: PathBuf,
        /// The checkout's `target` on the workspace share: the runner's link
        /// to the mount.
        target: PathBuf,
        /// The cargo-home share's destination, which receives returned files.
        cargo_home: PathBuf,
        build: BuildConfig,
        uid: libc::uid_t,
        gid: libc::gid_t,
        /// The directory holding the mount point, opened before the job ran.
        parent: Option<File>,
        /// The mounted volume's file system ID and its root's inode, recorded
        /// before the job ran.
        root: Option<Identity>,
        /// Watches the mount and `target` from mounting until release.
        watcher: Option<Watcher>,
        /// The cargo-home share's identity, recorded before the job ran.
        cargo_home_identity: Option<Identity>,
    }

    fn open_root() -> io::Result<File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open("/")
    }
    /// The mount point's own name, the last component of the build mount.
    fn mount_name(mount: &Path) -> io::Result<&str> {
        components(mount)?
            .last()
            .copied()
            .ok_or_else(|| invalid("the build mount has no name"))
    }
    /// The mount a path is on, its file system ID and its inode, following
    /// links; for a watch look's detail.
    fn resolve(path: &Path) -> io::Result<(String, [i32; 2], u64)> {
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(invalid)?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        check(unsafe { libc::statfs(c_path.as_ptr(), &mut stat) })?;
        Ok((
            text(&stat.f_mntonname),
            fsid(stat.f_fsid),
            std::fs::metadata(path)?.ino(),
        ))
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
                mount: motor_macvm::BUILD_MOUNT.into(),
                target: target_of(mounts)?,
                cargo_home: cargo_home_of(mounts)?,
                build,
                uid,
                gid,
                parent: None,
                root: None,
                watcher: None,
                cargo_home_identity: None,
            })
        }

        /// One look for `Watcher`: on the volume while the mount resolves to
        /// the recorded file system and root inode and the checkout's `target`
        /// is still the runner's link. The agent is root, so it never resolves
        /// `target` itself: that would follow wherever the job pointed it.
        fn look(mount: &Path, target: &Path, recorded: Identity) -> Look {
            use std::fmt::Write;
            let state = target_state(target);
            let mut detail = format!("target={state}");
            let mut on_volume = state == "link-to-build-mount";
            match resolve(mount) {
                Ok((on, id, inode)) => {
                    on_volume &= id == recorded.0 && inode == recorded.1;
                    let _ = write!(
                        detail,
                        " mount(f_mntonname={on:?},f_fsid={id:?},inode={inode})"
                    );
                }
                Err(error) => {
                    on_volume = false;
                    let _ = write!(detail, " mount_error={error}");
                }
            }
            Look { on_volume, detail }
        }

        fn snapshot(&self, phase: &str) -> String {
            use std::fmt::Write;
            let mut detail = format!(
                "target={} mount={:?} recorded={:?}",
                target_state(&self.target),
                self.mount,
                self.root
            );
            match resolve(&self.mount) {
                Ok((on, id, inode)) => {
                    let _ = write!(
                        detail,
                        " statfs(f_mntonname={on:?},f_fsid={id:?},inode={inode})"
                    );
                }
                Err(error) => {
                    let _ = write!(detail, " statfs_error={error}");
                }
            }
            // A private fixed-capacity buffer avoids getmntinfo's shared static
            // storage and bounds both allocation and the logged mount table.
            const MOUNTS: usize = 128;
            let mut mounts: Vec<libc::statfs> =
                (0..MOUNTS).map(|_| unsafe { std::mem::zeroed() }).collect();
            let count = unsafe {
                libc::getfsstat(
                    mounts.as_mut_ptr(),
                    std::mem::size_of_val(mounts.as_slice()) as libc::c_int,
                    libc::MNT_NOWAIT,
                )
            };
            if count < 0 {
                let _ = write!(detail, " getfsstat_error={}", io::Error::last_os_error());
            } else {
                let _ = write!(detail, " getfsstat=[");
                for mount in mounts.iter().take((count as usize).min(MOUNTS)) {
                    let _ = write!(
                        detail,
                        " {{on={:?},type={:?},flags={:#x},fsid={:?}}}",
                        text(&mount.f_mntonname),
                        text(&mount.f_fstypename),
                        mount.f_flags,
                        fsid(mount.f_fsid)
                    );
                }
                let _ = write!(detail, " ]");
                if count as usize >= MOUNTS {
                    detail.push_str(" [mount table capacity reached]");
                }
            }
            diagnostic_line(phase, &detail)
        }
    }

    fn text(bytes: &[libc::c_char]) -> String {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        String::from_utf8_lossy(
            &bytes[..end]
                .iter()
                .map(|byte| *byte as u8)
                .collect::<Vec<_>>(),
        )
        .into_owned()
    }

    /// The real disk operations behind `attach_with`, before any job code runs.
    struct Steps<'a> {
        native: &'a Native,
        /// The directory holding the mount point, opened by `mount_point`.
        parent: &'a File,
        disk: String,
        raw: Option<File>,
        /// The volume currently mounted at the build mount, such as `disk5s1`.
        volume: Option<String>,
    }

    impl Steps<'_> {
        fn mount(&mut self, volume: String) -> io::Result<()> {
            let device = format!("/dev/{volume}");
            let mount_path = &self.native.mount;
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
                    mount_path.as_os_str(),
                ],
                None,
            )?;
            self.volume = Some(volume);
            let at_mount: Vec<_> = mounted()?
                .into_iter()
                .filter(|mount| mount.on == *mount_path)
                .collect();
            match &at_mount[..] {
                [mount]
                    if mount.from == device
                        && mount.filesystem == "apfs"
                        && mount.flags & libc::MNT_NOSUID as u32 != 0
                        && mount.flags & libc::MNT_NODEV as u32 != 0
                        && mount.flags & libc::MNT_RDONLY as u32 == 0 => {}
                _ => {
                    return Err(invalid(format!(
                        "build volume mount does not match: {at_mount:?}"
                    )));
                }
            }
            Ok(())
        }

        /// The mounted volume's root, reached from the mount point's parent and
        /// checked to be the root of the volume mounted at the build mount.
        fn root(&self) -> io::Result<File> {
            let volume = self
                .volume
                .as_deref()
                .ok_or_else(|| invalid("no build volume is mounted"))?;
            let root = open_directory_at(self.parent, mount_name(&self.native.mount)?)?;
            let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
            check(unsafe { libc::fstatfs(root.as_raw_fd(), &mut stat) })?;
            let text = |bytes: &[libc::c_char]| {
                unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            };
            if text(&stat.f_mntfromname) != format!("/dev/{volume}")
                || Path::new(&text(&stat.f_mntonname)) != self.native.mount
            {
                return Err(invalid("the build mount is not the build volume's root"));
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
            let root_directory = open_root()?;
            // The runner made the checkout's target a link to the mount;
            // anything else there is refused before the job runs.
            let workspace = open_workspace(
                &root_directory,
                self.target
                    .parent()
                    .ok_or_else(|| invalid("target parent"))?,
                self.uid,
                self.gid,
            )?;
            check_target_link(&workspace)?;
            let (_, cargo_home) =
                open_share(&root_directory, &self.cargo_home, self.uid, self.gid)?;
            let parent = mount_point(&root_directory, &self.mount, self.uid, self.gid)?;
            let (disk, raw) = find_disk(self.gid)?;
            let mut steps = Steps {
                native: self,
                parent: &parent,
                disk,
                raw: Some(raw),
                volume: None,
            };
            let notice = attach_with(&mut steps)?;
            let root = identity(&steps.root()?)?;
            self.root = Some(root);
            self.parent = Some(parent);
            self.cargo_home_identity = Some(cargo_home);
            let (mount, target) = (self.mount.clone(), self.target.clone());
            self.watcher = Some(Watcher::start(WATCH_INTERVAL, move || {
                Self::look(&mount, &target, root)
            }));
            Ok(notice)
        }

        fn command_started(&mut self) {
            if let Some(watcher) = &self.watcher {
                watcher.command_started();
            }
        }

        fn watch_report(&mut self) -> Option<String> {
            self.watcher.take().map(|mut watcher| watcher.finish())
        }

        fn detach(&mut self, no_job_processes: &dyn Fn() -> io::Result<()>) -> io::Result<()> {
            // Dropping the watcher stops it if no report was taken.
            self.watcher = None;
            let (Some(parent), Some(recorded), Some(cargo_home)) =
                (self.parent.take(), self.root, self.cargo_home_identity)
            else {
                return Err(invalid("build volume is not attached"));
            };
            // Job code has run: reach the mount again from / without following
            // links, and require the volume root recorded before the job.
            let root = reopen(&open_root()?, &self.mount)?;
            if identity(&root)? != recorded {
                return Err(invalid(
                    "the build mount no longer names the build volume's root",
                ));
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
            if !matches!(&entries[..], [mount] if mount.on == self.mount) {
                return Err(invalid(format!(
                    "build volume is not mounted exactly once at the build mount: {entries:?}"
                )));
            }
            unmount(&self.mount, recorded.0)?;
            let point = open_directory_at(&parent, mount_name(&self.mount)?)?;
            if point.metadata()?.dev() != parent.metadata()?.dev() {
                return Err(invalid("the build mount is still mounted after unmounting"));
            }
            no_job_processes()?;
            // The host reads returned files under target/ only from a
            // directory created here, now that no job process is left.
            if !self.build.returns.is_empty() {
                let share = reopen_recorded(&open_root()?, &self.cargo_home, cargo_home)?;
                place_returns(&returns_directory(&share)?, &staged, staging.path())?;
                no_job_processes()?;
            }
            self.root = None;
            Ok(())
        }

        fn diagnostics(&self, phase: &str) -> Option<String> {
            Some(self.snapshot(phase))
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn snapshot_reports_the_mount_and_targets_state_without_resolving_target() {
            let scratch = tempfile::Builder::new()
                .prefix("motor-volume-diagnostic-")
                .tempdir_in(cortexkit_test_support::scratch_root())
                .unwrap();
            let target = scratch.path().join("target");
            let mount = scratch.path().join("mount");
            std::fs::create_dir(&target).unwrap();
            std::fs::create_dir(&mount).unwrap();
            let inode = std::fs::metadata(&mount).unwrap().ino();
            let volume = Native {
                mount: mount.clone(),
                target: target.clone(),
                cargo_home: scratch.path().join("cargo-home"),
                build: BuildConfig {
                    returns: Vec::new(),
                    return_max_bytes: 0,
                },
                uid: 501,
                gid: 20,
                parent: None,
                root: None,
                watcher: None,
                cargo_home_identity: None,
            };
            let snapshot = volume.snapshot("before-detach");
            for field in [
                "target=directory",
                "statfs(f_mntonname=",
                "f_fsid=",
                "getfsstat=[",
                "type=",
                "flags=",
            ] {
                assert!(snapshot.contains(field), "{snapshot}");
            }
            assert!(snapshot.contains(&format!("inode={inode}")), "{snapshot}");
            assert!(snapshot.len() <= DIAGNOSTIC_MAX_BYTES);
            std::fs::remove_dir(&target).unwrap();
            std::os::unix::fs::symlink(&mount, &target).unwrap();
            let snapshot = volume.snapshot("detach-failed");
            assert!(snapshot.contains("target=link-elsewhere"), "{snapshot}");
            std::fs::remove_file(&target).unwrap();
            std::fs::remove_dir(&mount).unwrap();
            let snapshot = volume.snapshot("detach-failed");
            assert!(snapshot.contains("target=missing"), "{snapshot}");
            assert!(snapshot.contains("statfs_error="), "{snapshot}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    #[test]
    fn diagnostics_are_one_bounded_utf8_line_with_explicit_truncation() {
        let line = diagnostic_line("before-detach", &"é\n\r".repeat(DIAGNOSTIC_MAX_BYTES));
        assert!(line.len() <= DIAGNOSTIC_MAX_BYTES);
        assert!(line.starts_with("motor: build-volume diagnostic before-detach: "));
        assert!(line.ends_with(" [truncated]\n"));
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains('\r'));
        assert_eq!(
            diagnostic_line("after-attach", "inode=2"),
            "motor: build-volume diagnostic after-attach: inode=2\n"
        );
    }

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
    /// A scratch tree standing in for `/`, with the job account another user.
    struct MountRoot {
        _scratch: cortexkit_test_support::ScratchDir,
        root: PathBuf,
        uid: libc::uid_t,
        gid: libc::gid_t,
    }
    fn mount_root() -> MountRoot {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let scratch = cortexkit_test_support::ScratchDir::new("build-mount-point");
        let root = scratch.join("root");
        std::fs::create_dir_all(root.join("private/var")).unwrap();
        for path in [&root, &root.join("private"), &root.join("private/var")] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let me = std::fs::metadata(&root).unwrap();
        MountRoot {
            _scratch: scratch,
            root,
            uid: me.uid() + 1,
            gid: me.gid() + 1,
        }
    }
    fn prepare(m: &MountRoot, uid: libc::uid_t, gid: libc::gid_t) -> io::Result<File> {
        mount_point(
            &File::open(&m.root).unwrap(),
            Path::new(motor_macvm::BUILD_MOUNT),
            uid,
            gid,
        )
    }

    /// Missing directories on the mount's path are created 0755 even under
    /// the agent's 077 umask, and the parent of the mount point is returned.
    #[test]
    fn mount_point_is_created_out_of_the_jobs_reach() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = mount_root();
        let previous = unsafe { libc::umask(0o077) };
        let parent = prepare(&m, m.uid, m.gid);
        unsafe { libc::umask(previous) };
        let parent = parent.unwrap();
        let point = m.root.join("private/var/motor/build");
        for path in [m.root.join("private/var/motor"), point.clone()] {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o755, "{path:?}");
        }
        assert_eq!(
            parent.metadata().unwrap().ino(),
            std::fs::metadata(m.root.join("private/var/motor"))
                .unwrap()
                .ino()
        );
        // An existing empty mount point is used again.
        prepare(&m, m.uid, m.gid).unwrap();
    }

    #[test]
    fn mount_point_below_a_directory_the_job_can_change_is_refused() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = mount_root();
        let me = std::fs::metadata(&m.root).unwrap();
        // Owned by the job account.
        assert!(prepare(&m, me.uid(), m.gid).is_err());
        let var = m.root.join("private/var");
        for mode in [0o775, 0o757] {
            std::fs::set_permissions(&var, std::fs::Permissions::from_mode(mode)).unwrap();
            let gid = if mode == 0o775 { me.gid() } else { m.gid };
            let error = prepare(&m, m.uid, gid).unwrap_err();
            assert!(error.to_string().contains("writable by the job"), "{error}");
        }
        std::fs::set_permissions(&var, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The mount point itself must be out of reach too.
        prepare(&m, m.uid, m.gid).unwrap();
        let point = m.root.join("private/var/motor/build");
        std::fs::set_permissions(&point, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(prepare(&m, m.uid, m.gid).is_err());
    }

    #[test]
    fn mount_point_that_is_a_link_a_file_or_not_empty_is_refused() {
        let m = mount_root();
        let elsewhere = m.root.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        // A link on the path is never followed, at any depth.
        symlink(&elsewhere, m.root.join("private/var/motor")).unwrap();
        assert!(prepare(&m, m.uid, m.gid).is_err());
        assert!(!elsewhere.join("build").exists());
        std::fs::remove_file(m.root.join("private/var/motor")).unwrap();
        std::fs::create_dir(m.root.join("private/var/motor")).unwrap();
        let point = m.root.join("private/var/motor/build");
        symlink(&elsewhere, &point).unwrap();
        assert!(prepare(&m, m.uid, m.gid).is_err());
        std::fs::remove_file(&point).unwrap();
        std::fs::write(&point, "a file").unwrap();
        assert!(prepare(&m, m.uid, m.gid).is_err());
        std::fs::remove_file(&point).unwrap();
        std::fs::create_dir(&point).unwrap();
        std::fs::write(point.join("stale"), "x").unwrap();
        let error = prepare(&m, m.uid, m.gid).unwrap_err();
        assert!(error.to_string().contains("not empty"), "{error}");
    }

    /// The checkout's `target` must be exactly the runner's link to the mount.
    #[test]
    fn target_must_be_the_runners_link_to_the_build_mount() {
        let scratch = cortexkit_test_support::ScratchDir::new("build-target-link");
        let workspace = scratch.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let check = || check_target_link(&File::open(&workspace).unwrap());
        assert!(check().is_err());
        symlink(motor_macvm::BUILD_MOUNT, workspace.join("target")).unwrap();
        check().unwrap();
        std::fs::remove_file(workspace.join("target")).unwrap();
        symlink("/private/var/motor/other", workspace.join("target")).unwrap();
        assert!(check().unwrap_err().to_string().contains("links to"));
        std::fs::remove_file(workspace.join("target")).unwrap();
        std::fs::create_dir(workspace.join("target")).unwrap();
        assert!(check().is_err());
        std::fs::remove_dir(workspace.join("target")).unwrap();
        std::fs::write(workspace.join("target"), motor_macvm::BUILD_MOUNT).unwrap();
        assert!(check().is_err());
    }

    /// The returns directory is made once, after the job, and never reused:
    /// a directory, file or link the job left under that name refuses it.
    #[test]
    fn returns_directory_is_created_new_and_never_taken_over() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = cortexkit_test_support::ScratchDir::new("build-returns-directory");
        let share = scratch.join("cargo-home");
        std::fs::create_dir(&share).unwrap();
        let open = || File::open(&share).unwrap();
        let name = share.join(motor_macvm::TARGET_RETURNS);
        let previous = unsafe { libc::umask(0o077) };
        let created = returns_directory(&open());
        unsafe { libc::umask(previous) };
        created.unwrap();
        assert_eq!(
            std::fs::metadata(&name).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let error = returns_directory(&open()).unwrap_err();
        assert!(error.to_string().contains("the job created it"), "{error}");
        std::fs::remove_dir(&name).unwrap();
        let elsewhere = scratch.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, &name).unwrap();
        assert!(returns_directory(&open()).is_err());
        std::fs::remove_file(&name).unwrap();
        std::fs::write(&name, "x").unwrap();
        assert!(returns_directory(&open()).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// The watcher and diagnostics read `target` without following it, so
    /// their report is the same whether a job's link points at something
    /// that exists or not, even under a directory only root can read.
    #[test]
    fn target_state_never_depends_on_where_a_job_link_points() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = cortexkit_test_support::ScratchDir::new("build-target-state");
        let private = scratch.join("root-only");
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("exists"), "x").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = scratch.join("target");
        let mut states = Vec::new();
        for pointed in ["exists", "missing"] {
            symlink(private.join(pointed), &target).unwrap();
            states.push(target_state(&target));
            std::fs::remove_file(&target).unwrap();
        }
        assert_eq!(states, ["link-elsewhere", "link-elsewhere"]);
        symlink(motor_macvm::BUILD_MOUNT, &target).unwrap();
        assert_eq!(target_state(&target), "link-to-build-mount");
        std::fs::remove_file(&target).unwrap();
        assert_eq!(target_state(&target), "missing");
        std::fs::create_dir(&target).unwrap();
        assert_eq!(target_state(&target), "directory");
    }

    /// The cargo-home share is opened before the job only when no directory
    /// above it is the job's to change, and is required to be the same share
    /// before the returns directory is made in it.
    #[test]
    fn cargo_home_share_is_out_of_the_jobs_reach_and_must_stay_the_same() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let scratch = cortexkit_test_support::ScratchDir::new("build-cargo-home-share");
        let root = scratch.join("root");
        let share = root.join("opt/motor/job/cargo-home");
        std::fs::create_dir_all(&share).unwrap();
        for path in [
            root.clone(),
            root.join("opt"),
            root.join("opt/motor"),
            root.join("opt/motor/job"),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The share itself is the job's to write.
        std::fs::set_permissions(&share, std::fs::Permissions::from_mode(0o777)).unwrap();
        let me = std::fs::metadata(&root).unwrap();
        let (uid, gid) = (me.uid() + 1, me.gid() + 1);
        let path = Path::new("/opt/motor/job/cargo-home");
        let open = || File::open(&root).unwrap();
        let (_, recorded) = open_share(&open(), path, uid, gid).unwrap();
        reopen_recorded(&open(), path, recorded).unwrap();
        let job = root.join("opt/motor/job");
        std::fs::set_permissions(&job, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(open_share(&open(), path, uid, gid).is_err());
        std::fs::set_permissions(&job, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(open_share(&open(), path, me.uid(), gid).is_err());
        // Another directory put in the share's place is refused.
        std::fs::rename(&share, job.join("moved")).unwrap();
        std::fs::create_dir(&share).unwrap();
        let error = reopen_recorded(&open(), path, recorded).unwrap_err();
        assert!(error.to_string().contains("no longer the share"), "{error}");
        std::fs::remove_dir(&share).unwrap();
        symlink(job.join("moved"), &share).unwrap();
        assert!(reopen_recorded(&open(), path, recorded).is_err());
    }

    /// Fails one placed file's writing the way a full or failing disk does.
    struct FailingWrites {
        /// Bytes `copy` writes before failing with EIO.
        partial: usize,
        fail_set_len: bool,
    }
    impl PlaceWrites for FailingWrites {
        fn copy(&mut self, from: &mut File, to: &mut File) -> io::Result<()> {
            let mut bytes = vec![0; self.partial];
            from.read_exact(&mut bytes)?;
            to.write_all(&bytes)?;
            Err(io::Error::from_raw_os_error(libc::EIO))
        }
        fn set_len(&mut self, file: &File, size: u64) -> io::Result<()> {
            if self.fail_set_len {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            file.set_len(size)
        }
    }

    /// A copy that fails partway and a placeholder whose size cannot be set
    /// both leave nothing under the file's name, so the host reports them
    /// missing instead of returning a torn or empty file.
    #[test]
    fn a_returned_file_that_fails_to_be_written_is_removed() {
        let v = volume();
        std::fs::create_dir(v.volume.join("debug")).unwrap();
        std::fs::write(v.volume.join("debug/app"), vec![7; 64]).unwrap();
        std::fs::write(v.volume.join("large"), vec![8; 40]).unwrap();
        let build = BuildConfig {
            returns: vec!["debug/app".into(), "large".into()],
            return_max_bytes: 70,
        };
        let staged = stage_returns(&File::open(&v.volume).unwrap(), &build, &v.staging).unwrap();
        assert!(matches!(staged[1], Staged::OverCap { .. }));
        place_returns_with(
            &File::open(&v.target).unwrap(),
            &staged,
            &v.staging,
            &mut FailingWrites {
                partial: 10,
                fail_set_len: true,
            },
        )
        .unwrap();
        assert!(std::fs::symlink_metadata(v.target.join("debug/app")).is_err());
        assert!(std::fs::symlink_metadata(v.target.join("large")).is_err());
        // Written whole, both are placed.
        place_returns(&File::open(&v.target).unwrap(), &staged, &v.staging).unwrap();
        assert_eq!(
            std::fs::read(v.target.join("debug/app")).unwrap(),
            vec![7; 64]
        );
        assert_eq!(std::fs::metadata(v.target.join("large")).unwrap().len(), 71);
    }

    #[test]
    fn cargo_home_is_the_cargo_home_shares_destination() {
        assert_eq!(
            cargo_home_of(&mounts("/Users/test/repo")).unwrap(),
            Path::new(crate::CARGO_HOME)
        );
        assert!(cargo_home_of(&mounts("/Users/test/repo")[..1]).is_err());
    }

    fn look(on_volume: bool, detail: &str) -> Look {
        Look {
            on_volume,
            detail: detail.into(),
        }
    }

    /// The watcher keeps only the first look that left the volume and the
    /// last good one before it, timed from mounting and from the command.
    #[test]
    fn watcher_reports_the_first_look_off_the_volume_with_the_last_good_one() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let looks = count.clone();
        let mut watcher = Watcher::start(std::time::Duration::from_millis(1), move || match looks
            .fetch_add(1, Ordering::SeqCst)
        {
            0..=2 => look(true, "on the volume"),
            3 => look(false, "on the share"),
            _ => look(false, "looked again"),
        });
        watcher.command_started();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while count.load(Ordering::SeqCst) < 4 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let line = watcher.finish();
        assert!(line.starts_with("motor: build-volume diagnostic watch: target stopped naming the build volume root at look 4 "), "{line}");
        assert!(line.contains(": on the share; last good look "), "{line}");
        assert!(line.ends_with(": on the volume\n"), "{line}");
        assert!(!line.contains("looked again"), "{line}");
        // It stops looking once the change is seen.
        assert_eq!(count.load(Ordering::SeqCst), 4);
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn watcher_that_saw_no_change_says_so_in_one_line() {
        let mut watcher = Watcher::start(std::time::Duration::from_millis(1), || {
            look(true, "on the volume")
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        let line = watcher.finish();
        assert!(
            line.starts_with(
                "motor: build-volume diagnostic watch: target named the build volume root in all "
            ),
            "{line}"
        );
        assert_eq!(line.matches('\n').count(), 1);
        // A report taken before the command started says so.
        let mut early = Watcher::start(std::time::Duration::from_millis(1), || {
            look(false, "on the share")
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        let line = early.finish();
        assert!(line.contains("before the command started"), "{line}");
        assert!(line.contains("last good look none"), "{line}");
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
        // the build mount, is not the build volume's.
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
