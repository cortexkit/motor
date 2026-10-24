//! Physical recursive mode repair, independent of coreutils option versions.
//! Only the trusted daemon invokes the root CLI; jobs receive no sudo grant.
use sha2::Digest;
use std::{
    ffi::CString,
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
    process::Command,
};

#[derive(Clone, Copy)]
pub(crate) enum Policy {
    OwnerWrite,
    Private,
}
pub(crate) const HELPER: &str = "/usr/local/libexec/motor-runner-permissions";

pub(crate) fn digest() -> String {
    format!(
        "{:x}",
        sha2::Sha256::digest(include_bytes!("permissions.rs"))
    )
}

pub(crate) fn check_install() -> io::Result<()> {
    let helper = Path::new(HELPER);
    for path in helper.ancestors() {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot inspect permissions helper path {}: {error}",
                    path.display()
                ),
            )
        })?;
        let valid_type = if path == helper {
            metadata.is_file() && metadata.mode() & 0o7777 == 0o755
        } else {
            metadata.is_dir()
        };
        if !valid_type || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::other(format!("unsafe permissions helper path {}: uid={} gid={} mode={:04o} type={:?}; expected physical root-owned non-job-writable ancestry and root0755 helper", path.display(), metadata.uid(), metadata.gid(), metadata.mode() & 0o7777, metadata.file_type())));
        }
    }
    let output = Command::new(HELPER)
        .arg("permission-helper-digest")
        .output()?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != digest() {
        return Err(io::Error::other("installed permissions helper differs from candidate; administrator must install the matching helper before deploy"));
    }
    for policy in [Policy::OwnerWrite, Policy::Private] {
        let output = Command::new("/usr/bin/sudo")
            .args([
                "-n",
                "-l",
                HELPER,
                "physical-chmod",
                policy.argument(),
                "--",
                "/srv/build/tmp",
            ])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("operator sudo policy must allow the fixed physical-chmod command forms before deploy"));
        }
    }
    Ok(())
}
impl Policy {
    fn argument(self) -> &'static str {
        match self {
            Self::OwnerWrite => "owner-rwX",
            Self::Private => "private-rwX",
        }
    }
    fn mode(self, current: u32, directory: bool) -> u32 {
        let mut mode = (current & 0o7777) | 0o600;
        if directory || current & 0o111 != 0 {
            mode |= 0o100;
        }
        if matches!(self, Self::Private) {
            mode &= !0o077;
        }
        mode
    }
}

/// Repair modes on `paths` without following any symlink. A root process (the
/// runner inside a tenant VM, or tests run as root there) walks the trees
/// itself: going through sudo would add nothing, and the tenant image has no
/// installed helper copy. Everywhere else the fixed root-owned helper runs
/// through the operator's narrow sudo rule, the only way an unprivileged
/// runner can change modes on job-owned trees.
pub(crate) fn repair_tree(policy: Policy, paths: &[&Path]) -> io::Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
            return Err(io::Error::other(
                "permission repair requires absolute tree paths",
            ));
        }
        return apply(policy, paths);
    }
    let mut command = command(policy, paths)?;
    let output = crate::systemd::output(&mut command)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

pub(crate) fn command(policy: Policy, paths: &[&Path]) -> io::Result<Command> {
    if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
        return Err(io::Error::other(
            "permission repair requires absolute tree paths",
        ));
    }
    let mut command = Command::new("/usr/bin/sudo");
    command
        .arg("-n")
        .arg(HELPER)
        .args(["physical-chmod", policy.argument(), "--"])
        .args(paths);
    Ok(command)
}

fn open_at(parent: RawFd, name: &std::ffi::OsStr, directory: bool) -> io::Result<OwnedFd> {
    let name =
        CString::new(name.as_bytes()).map_err(|_| io::Error::other("NUL in permission path"))?;
    let flags = libc::O_PATH
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | if directory { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// Pin a repair root without following any symlink. Every ancestor must be a
/// physical directory. The root itself may be a directory (repaired recursively)
/// or a regular file (only that inode is repaired), as `chmod -R` accepted both.
/// A symlink or special-inode root is refused before any mode changes.
fn physical_root(path: &Path) -> io::Result<OwnedFd> {
    if !path.is_absolute() || path.parent().is_none() {
        return Err(io::Error::other(
            "permission tree must be absolute and not filesystem root",
        ));
    }
    let names = path
        .components()
        .filter_map(|component| match component {
            Component::RootDir => None,
            Component::Normal(name) => Some(Ok(name)),
            _ => Some(Err(io::Error::other(
                "permission tree contains a nonphysical component",
            ))),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let Some((last, ancestors)) = names.split_last() else {
        return Err(io::Error::other(
            "refusing permission repair of filesystem root",
        ));
    };
    let mut fd = open_at(libc::AT_FDCWD, std::ffi::OsStr::new("/"), true)?;
    for name in ancestors {
        fd = open_at(fd.as_raw_fd(), name, true)?;
    }
    // Without O_DIRECTORY, O_PATH|O_NOFOLLOW pins a symlink itself rather than
    // failing, so the root's type is checked on the pinned inode.
    let root = open_at(fd.as_raw_fd(), last, false)?;
    let kind = fs::File::from(root.try_clone()?).metadata()?.file_type();
    if !kind.is_dir() && !kind.is_file() {
        return Err(io::Error::other(format!(
            "permission root must be a physical directory or regular file, not a symlink or special inode: {}",
            path.display()
        )));
    }
    Ok(root)
}

struct Frame {
    fd: OwnedFd,
    entries: fs::ReadDir,
}

fn repair_node(fd: OwnedFd, policy: Policy) -> io::Result<Option<Frame>> {
    let link = format!("/proc/self/fd/{}", fd.as_raw_fd());
    let metadata = fs::File::from(fd.try_clone()?).metadata()?;
    // An O_PATH|O_NOFOLLOW descriptor may name the symlink itself. Never chmod it.
    if metadata.file_type().is_symlink() {
        return Ok(None);
    }
    let mode = policy.mode(metadata.mode(), metadata.is_dir());
    let link_c = CString::new(link.as_bytes()).unwrap();
    // O_PATH avoids opening devices/FIFOs. This kernel-owned magic link addresses
    // the pinned, non-symlink inode, not a job-controlled pathname or replacement.
    if unsafe { libc::fchmodat(libc::AT_FDCWD, link_c.as_ptr(), mode, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if metadata.is_dir() {
        let entries = fs::read_dir(&link)?;
        Ok(Some(Frame { fd, entries }))
    } else {
        Ok(None)
    }
}

fn repair(root: OwnedFd, policy: Policy) -> io::Result<()> {
    let mut stack = Vec::new();
    if let Some(frame) = repair_node(root, policy)? {
        stack.push(frame);
    }
    while let Some(parent) = stack.last_mut() {
        let Some(entry) = parent.entries.next() else {
            stack.pop();
            continue;
        };
        let name = entry?.file_name();
        let child = match open_at(parent.fd.as_raw_fd(), &name, false) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if let Some(frame) = repair_node(child, policy)? {
            stack.push(frame);
        }
    }
    Ok(())
}

pub(crate) fn apply(policy: Policy, paths: &[&Path]) -> io::Result<()> {
    if paths.is_empty() {
        return Err(io::Error::other("permission repair requires a tree"));
    }
    // Validate/pin all roots before changing any mode, including every ancestor.
    let roots = paths
        .iter()
        .map(|path| physical_root(path))
        .collect::<io::Result<Vec<_>>>()?;
    for root in roots {
        repair(root, policy)?;
    }
    Ok(())
}

pub(crate) fn run(args: &[String]) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other(
            "physical-chmod requires the trusted root caller",
        ));
    }
    let policy = match args.first().map(String::as_str) {
        Some("owner-rwX") => Policy::OwnerWrite,
        Some("private-rwX") => Policy::Private,
        _ => return Err(io::Error::other("unknown physical-chmod policy")),
    };
    if args.get(1).map(String::as_str) != Some("--") {
        return Err(io::Error::other(
            "physical-chmod requires -- before tree paths",
        ));
    }
    // This short-lived root helper inherits the runner's protected score, but
    // permission walks are expendable setup work, not the daemon itself.
    fs::write("/proc/self/oom_score_adj", "900")?;
    apply(policy, &args[2..].iter().map(Path::new).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests;
