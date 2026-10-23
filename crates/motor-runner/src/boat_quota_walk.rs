//! Bounded, descriptor-relative traversal of job-controlled quota trees.
use std::{
    collections::BTreeSet,
    ffi::{CString, OsStr},
    fs::{self, File, Metadata},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub entries: usize,
    pub depth: usize,
    pub elapsed: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 262_144,
            depth: 128,
            elapsed: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
struct WalkLimit(&'static str);
impl std::fmt::Display for WalkLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Boat quota traversal {} budget exhausted", self.0)
    }
}
impl std::error::Error for WalkLimit {}

pub(super) fn is_limit(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|error| error.is::<WalkLimit>())
}

pub(super) struct Budget {
    limits: Limits,
    started: Instant,
    entries: usize,
    seen: BTreeSet<(u64, u64)>,
}
impl Default for Budget {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}
impl Budget {
    pub(super) fn new(limits: Limits) -> Self {
        Self {
            limits,
            started: Instant::now(),
            entries: 0,
            seen: BTreeSet::new(),
        }
    }
    fn check_time(&self) -> io::Result<()> {
        if self.started.elapsed() >= self.limits.elapsed {
            return Err(io::Error::other(WalkLimit("elapsed-time")));
        }
        Ok(())
    }
    fn entry(&mut self) -> io::Result<()> {
        self.check_time()?;
        self.entries += 1;
        if self.entries > self.limits.entries {
            return Err(io::Error::other(WalkLimit("entry")));
        }
        Ok(())
    }
}

fn open_at(parent: RawFd, name: &OsStr, directory: bool) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    let flags = libc::O_PATH
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: name is NUL-terminated and a successful fd is transferred to File.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn physical_root(path: &Path, budget: &Budget) -> io::Result<File> {
    if !path.is_absolute() || path.parent().is_none() {
        return Err(io::Error::other(
            "quota root must be absolute and not filesystem root",
        ));
    }
    let mut names = path
        .components()
        .filter(|component| !matches!(component, Component::RootDir))
        .peekable();
    let mut current = open_at(libc::AT_FDCWD, OsStr::new("/"), true)?;
    while let Some(component) = names.next() {
        budget.check_time()?;
        let Component::Normal(name) = component else {
            return Err(io::Error::other("quota root has a nonphysical component"));
        };
        current = open_at(current.as_raw_fd(), name, names.peek().is_some())?;
    }
    let metadata = current.metadata()?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(io::Error::other(
            "quota root must be a physical directory or regular file",
        ));
    }
    Ok(current)
}

pub(super) fn pin_roots(
    paths: &[PathBuf],
    missing_ok: bool,
    budget: &Budget,
) -> io::Result<Vec<File>> {
    let mut roots = Vec::with_capacity(paths.len());
    // Validate every root before any inode can be relabelled.
    for path in paths {
        match physical_root(path, budget) {
            Ok(root) => roots.push(root),
            Err(error) if missing_ok && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(roots)
}

struct Frame {
    pin: File,
    entries: fs::ReadDir,
}

fn node(
    pin: File,
    device: u64,
    budget: &mut Budget,
    update: &mut impl FnMut(&File, &Metadata) -> io::Result<bool>,
) -> io::Result<Option<Frame>> {
    budget.check_time()?;
    let metadata = pin.metadata()?;
    if metadata.dev() != device {
        return Err(io::Error::other("Boat quota tree crosses a filesystem"));
    }
    if !metadata.is_dir() && !metadata.is_file() {
        return Ok(None);
    }
    // Count every directory entry against the work budget, but perform inode
    // operations only once even if a job created many hardlink aliases.
    if !budget.seen.insert((metadata.dev(), metadata.ino())) {
        return Ok(None);
    }
    let link = format!("/proc/self/fd/{}", pin.as_raw_fd());
    // The magic link names the already pinned, non-symlink inode. O_PATH fds
    // cannot issue these ioctls, so reopen only this kernel-owned reference.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&link)?;
    if !update(&file, &metadata)? {
        return Ok(None);
    }
    if metadata.is_dir() {
        return Ok(Some(Frame {
            entries: fs::read_dir(&link)?,
            pin,
        }));
    }
    Ok(None)
}

pub(super) fn visit(
    root: File,
    device: u64,
    budget: &mut Budget,
    update: &mut impl FnMut(&File, &Metadata) -> io::Result<bool>,
) -> io::Result<()> {
    let mut stack = Vec::new();
    budget.entry()?;
    if let Some(frame) = node(root, device, budget, update)? {
        stack.push(frame);
    }
    while let Some(parent) = stack.last_mut() {
        budget.check_time()?;
        let Some(entry) = parent.entries.next() else {
            stack.pop();
            continue;
        };
        let name = entry?.file_name();
        budget.entry()?;
        let child = match open_at(parent.pin.as_raw_fd(), &name, false) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if let Some(frame) = node(child, device, budget, update)? {
            if stack.len() >= budget.limits.depth {
                return Err(io::Error::other(WalkLimit("depth")));
            }
            stack.push(frame);
        }
    }
    Ok(())
}
