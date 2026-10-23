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
    path_limit: Option<usize>,
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
            path_limit: None,
        }
    }
    pub(super) fn before_path_helpers(path: &Path) -> Self {
        // Checkout and access-control-list helpers use whole pathnames, which
        // can exceed PATH_MAX. Evict an overlong warm tree before invoking them.
        Self {
            path_limit: Some((libc::PATH_MAX as usize).saturating_sub(path.as_os_str().len() + 1)),
            ..Self::default()
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
    path_bytes: usize,
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
            path_bytes: 0,
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
        let path_bytes = parent.path_bytes + 1 + name.len();
        if budget.path_limit.is_some_and(|limit| path_bytes > limit) {
            return Err(io::Error::other(WalkLimit("pathname-length")));
        }
        let child = match open_at(parent.pin.as_raw_fd(), &name, false) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if let Some(mut frame) = node(child, device, budget, update)? {
            frame.path_bytes = path_bytes;
            if stack.len() >= budget.limits.depth {
                return Err(io::Error::other(WalkLimit("depth")));
            }
            stack.push(frame);
        }
    }
    Ok(())
}

fn unlink_at(parent: RawFd, name: &OsStr, directory: bool) -> io::Result<()> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: the parent is pinned and the name is one NUL-terminated component.
    let result = unsafe {
        libc::unlinkat(
            parent,
            name.as_ptr(),
            if directory { libc::AT_REMOVEDIR } else { 0 },
        )
    };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

struct RemovalFrame {
    pin: File,
    entries: fs::ReadDir,
    name: std::ffi::OsString,
}

// Only private retired trees are accepted. Partial progress is safe to retry;
// deep subtrees are lifted into the same private container instead of requiring
// unbounded descriptors, recursion, or reconstructed descendant pathnames.
pub(super) fn remove_private(path: &Path, container: &Path) -> io::Result<()> {
    if path.parent() != Some(container) {
        return Err(io::Error::other(
            "retired tree is not a direct child of its private container",
        ));
    }
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("missing retired tree name"))?;
    let mut budget = Budget::default();
    let parent = physical_root(container, &budget)?;
    let root = match open_at(parent.as_raw_fd(), name, false) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let device = parent.metadata()?.dev();
    let metadata = root.metadata()?;
    if metadata.dev() != device {
        return Err(io::Error::other("retired tree crosses a filesystem"));
    }
    if !metadata.is_dir() {
        return unlink_at(parent.as_raw_fd(), name, false);
    }
    let entries = fs::read_dir(format!("/proc/self/fd/{}", root.as_raw_fd()))?;
    let mut stack = vec![RemovalFrame {
        pin: root,
        entries,
        name: name.to_owned(),
    }];
    while let Some(frame) = stack.last_mut() {
        budget.check_time()?;
        let Some(entry) = frame.entries.next() else {
            let finished = stack.pop().unwrap();
            let owner = stack
                .last()
                .map_or(parent.as_raw_fd(), |frame| frame.pin.as_raw_fd());
            unlink_at(owner, &finished.name, true)?;
            continue;
        };
        budget.entry()?;
        let name = entry?.file_name();
        let child = match open_at(frame.pin.as_raw_fd(), &name, false) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata = child.metadata()?;
        if metadata.dev() != device {
            return Err(io::Error::other("retired tree crosses a filesystem"));
        }
        if !metadata.is_dir() {
            unlink_at(frame.pin.as_raw_fd(), &name, false)?;
            continue;
        }
        if stack.len() >= budget.limits.depth {
            let target = CString::new(format!("boat-deep-{}", uuid::Uuid::now_v7())).unwrap();
            let source = CString::new(name.as_bytes())?;
            // SAFETY: both parents are pinned directories on the same filesystem.
            if unsafe {
                libc::renameat(
                    stack.last().unwrap().pin.as_raw_fd(),
                    source.as_ptr(),
                    parent.as_raw_fd(),
                    target.as_ptr(),
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
            continue;
        }
        let entries = fs::read_dir(format!("/proc/self/fd/{}", child.as_raw_fd()))?;
        stack.push(RemovalFrame {
            pin: child,
            entries,
            name,
        });
    }
    Ok(())
}
