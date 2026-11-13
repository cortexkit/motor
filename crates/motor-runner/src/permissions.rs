//! Physical permission repair and staging cleanup, independent of coreutils.
//! Only the trusted daemon invokes the root CLI; jobs receive no sudo grant.
use sha2::Digest;
use std::{
    collections::{HashMap, HashSet},
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
    let mut hash = sha2::Sha256::new();
    hash.update(include_bytes!("permissions.rs"));
    hash.update(include_bytes!("source_seal.rs"));
    hash.update(include_bytes!("source_seal/authority.rs"));
    hash.update(include_bytes!("main.rs"));
    hash.update(include_bytes!("lib.rs"));
    hash.update(include_bytes!("isolation.rs"));
    format!("{hash:x}", hash = hash.finalize())
}

pub(crate) fn check_install() -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        crate::source_seal::authority::Authority::load()?;
    }
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
    check_install_commands()
}

fn check_install_commands() -> io::Result<()> {
    let output = crate::systemd::output(Command::new(HELPER).arg("permission-helper-digest"))?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != digest() {
        return Err(io::Error::other("installed permissions helper differs from candidate; administrator must install the matching helper before deploy"));
    }
    for policy in [Policy::OwnerWrite, Policy::Private] {
        let output = crate::systemd::output(Command::new("/usr/bin/sudo").args([
            "-n",
            "-l",
            HELPER,
            "physical-chmod",
            policy.argument(),
            "--",
            "/srv/build/tmp",
        ]))?;
        if !output.status.success() {
            return Err(io::Error::other("operator sudo policy must allow the fixed physical-chmod command forms before deploy"));
        }
    }
    for action in ["seal", "unseal"] {
        let output = crate::systemd::output(Command::new("/usr/bin/sudo").args([
            "-n",
            "-l",
            HELPER,
            action,
            "00000000-0000-0000-0000-000000000000",
            "/srv/build/tmp",
        ]))?;
        if !output.status.success() {
            return Err(io::Error::other(
                "operator sudo policy must allow the fixed seal and unseal forms before deploy",
            ));
        }
    }
    for action in ["check-staging-links", "remove-staging"] {
        let output = crate::systemd::output(Command::new("/usr/bin/sudo").args([
            "-n",
            "-l",
            HELPER,
            action,
            "0000000000000000000000000000000000000000000000000000000000000000",
            "00000000-0000-0000-0000-000000000000",
            "/srv/build/tmp",
        ]))?;
        if !output.status.success() {
            return Err(io::Error::other(
                "operator sudo policy must allow the fixed check-staging-links and remove-staging forms before deploy",
            ));
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
pub(crate) fn physical_root(path: &Path) -> io::Result<OwnedFd> {
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

pub(crate) fn seal_command(
    action: &str,
    state: &Path,
    id: motor_protocol::Uuid,
) -> io::Result<Command> {
    if !matches!(action, "seal" | "unseal") || !state.is_absolute() {
        return Err(io::Error::other("invalid source seal helper form"));
    }
    let mut command = Command::new("/usr/bin/sudo");
    command
        .args(["-n", HELPER, action, &id.to_string()])
        .arg(state);
    Ok(command)
}

fn physical_state_form(state: &Path) -> bool {
    state.is_absolute()
        && state.parent().is_some()
        && !state
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'/')
            .any(|part| matches!(part, b"." | b".."))
}

/// Recognize helper modes even when classic sudo's joined-argument wildcard
/// matched a single argument containing spaces. Malformed forms must not fall
/// through to daemon configuration, logging, or runtime initialization as root.
pub(crate) fn helper_dispatch(args: &[String]) -> Option<io::Result<()>> {
    let first = args.first()?;
    let action = first.split_ascii_whitespace().next()?;
    if !matches!(
        action,
        "physical-chmod"
            | "seal"
            | "unseal"
            | "check-staging-links"
            | "remove-staging"
            | "permission-helper-digest"
            | "check-permissions-helper"
    ) {
        return None;
    }
    let valid = first == action
        && match action {
            "physical-chmod" => {
                args.len() >= 4
                    && matches!(args[1].as_str(), "owner-rwX" | "private-rwX")
                    && args[2] == "--"
                    && args[3..]
                        .iter()
                        .all(|path| physical_state_form(Path::new(path)))
            }
            "seal" | "unseal" => {
                args.len() == 3
                    && args[1].parse::<motor_protocol::Uuid>().is_ok()
                    && physical_state_form(Path::new(&args[2]))
            }
            "check-staging-links" | "remove-staging" => {
                args.len() == 4
                    && args[2].parse::<motor_protocol::Uuid>().is_ok()
                    && validate_staging_form(action, Path::new(&args[3]), &args[1]).is_ok()
            }
            _ => args.len() == 1,
        };
    if !valid {
        return Some(Err(io::Error::other(
            "invalid fixed permissions helper invocation",
        )));
    }
    Some(match action {
        "physical-chmod" => run(&args[1..]),
        "seal" | "unseal" => crate::source_seal::helper(action, &args[1..]),
        "check-staging-links" | "remove-staging" => staging_helper(action, &args[1..]),
        "permission-helper-digest" => {
            println!("{}", digest());
            Ok(())
        }
        "check-permissions-helper" => check_install(),
        _ => unreachable!(),
    })
}

fn validate_staging_form(action: &str, state: &Path, repository: &str) -> io::Result<()> {
    if !matches!(action, "check-staging-links" | "remove-staging")
        || !physical_state_form(state)
        || repository.len() != 64
        || !repository
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::other("invalid staging generation helper form"));
    }
    Ok(())
}

fn staging_command(
    action: &str,
    state: &Path,
    repository: &str,
    fill: motor_protocol::Uuid,
) -> io::Result<Command> {
    validate_staging_form(action, state, repository)?;
    let mut command = Command::new("/usr/bin/sudo");
    command
        .args(["-n", HELPER, action, repository, &fill.to_string()])
        .arg(state);
    Ok(command)
}

#[derive(Clone, Copy)]
struct StagingOwners {
    runner: u32,
    writer: u32,
}

fn delegated_runner_uid() -> io::Result<u32> {
    let uid = std::env::var("SUDO_UID")
        .map_err(io::Error::other)?
        .parse::<u32>()
        .map_err(io::Error::other)?;
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0u8; 64 * 1024];
    let result = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    if uid == 0 || result != 0 || found.is_null() || entry.pw_uid != uid {
        return Err(io::Error::other(
            "staging helper requires a real non-root SUDO_UID",
        ));
    }
    Ok(uid)
}

impl StagingOwners {
    fn current() -> io::Result<Self> {
        Self::for_runner(unsafe { libc::geteuid() })
    }

    fn for_runner(runner: u32) -> io::Result<Self> {
        let writer = crate::isolation::validate_writer_account()?.uid;
        if writer == runner {
            return Err(io::Error::other("fill writer must differ from the runner"));
        }
        Ok(Self { runner, writer })
    }

    fn store_directory(self, metadata: &fs::Metadata) -> io::Result<()> {
        if !metadata.is_dir()
            || ![0, self.runner].contains(&metadata.uid())
            || metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::other("build-base store directories must be runner/root-owned and not group/other-writable"));
        }
        Ok(())
    }
}

/// Inspect or discard only a fill's staging generation. Root runners walk in
/// process; unprivileged runners use the same fixed operation through sudo.
/// Every caller must stop the fill unit and prevent the fill account from
/// writing the tree before either operation. The link check holds no lock and
/// retains no directory descriptor for the caller: publication must use the same
/// tree immediately, without allowing the fill account to write again. The
/// writer is motor-cache, validated as distinct from the runner, root and
/// motor-job; there is no
/// runner-as-writer fallback. Only root or the runner may place staging roots
/// inside the store's protected build-bases and repository directories.
pub(crate) fn staging_operation(
    action: &str,
    config: &crate::Config,
    repository: &str,
    fill: motor_protocol::Uuid,
) -> io::Result<()> {
    let state = config.state_dir();
    validate_staging_form(action, &state, repository)?;
    let authority = crate::source_seal::authority::Authority::for_staging(config)?;
    staging_authorized(&authority, action, &state, repository, fill)
}

/// Use an already trusted policy, still enforcing its state allowlist on every
/// operation. Root runners derive it from their launch configuration; delegated
/// helpers and non-root runners require the installed root-owned policy.
pub(crate) fn staging_authorized(
    authority: &crate::source_seal::authority::Authority,
    action: &str,
    state: &Path,
    repository: &str,
    fill: motor_protocol::Uuid,
) -> io::Result<()> {
    staging_authorized_as(authority, action, state, repository, fill, None)
}

fn staging_authorized_as(
    authority: &crate::source_seal::authority::Authority,
    action: &str,
    state: &Path,
    repository: &str,
    fill: motor_protocol::Uuid,
    runner: Option<u32>,
) -> io::Result<()> {
    validate_staging_form(action, state, repository)?;
    authority.roots(state)?;
    let owners = runner.map_or_else(StagingOwners::current, StagingOwners::for_runner)?;
    staging_execute(action, state, repository, fill, owners)
}

#[cfg(test)]
pub(crate) fn fixture_staging_operation(
    authority: &crate::source_seal::authority::Authority,
    action: &str,
    state: &Path,
    repository: &str,
    fill: motor_protocol::Uuid,
) -> io::Result<()> {
    validate_staging_form(action, state, repository)?;
    authority.roots(state)?;
    // Portable fixtures use one real UID for both roles. Keep the physical
    // tree, ownership, mode, mount and link checks; distinct system accounts
    // and the privileged ownership transfer are covered by the root tests.
    let uid = unsafe { libc::geteuid() };
    staging_apply(
        action,
        state,
        repository,
        fill,
        StagingOwners {
            runner: uid,
            writer: uid,
        },
    )
}

#[cfg(test)]
pub(crate) fn fixture_reclaim_staging(path: &Path) -> io::Result<()> {
    apply(Policy::OwnerWrite, &[path])
}

fn staging_execute(
    action: &str,
    state: &Path,
    repository: &str,
    fill: motor_protocol::Uuid,
    owners: StagingOwners,
) -> io::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return staging_apply(action, state, repository, fill, owners);
    }
    let mut command = staging_command(action, state, repository, fill)?;
    let output = crate::systemd::output(&mut command)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

pub(crate) fn staging_helper(action: &str, args: &[String]) -> io::Result<()> {
    staging_helper_with_authority(action, args, crate::source_seal::authority::Authority::load)
}

fn staging_helper_with_authority(
    action: &str,
    args: &[String],
    load: impl FnOnce() -> io::Result<crate::source_seal::authority::Authority>,
) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 || args.len() != 3 {
        return Err(io::Error::other(
            "staging helper requires root, repository key, fill UUID and state root",
        ));
    }
    let repository = &args[0];
    let fill = args[1]
        .parse::<motor_protocol::Uuid>()
        .map_err(io::Error::other)?;
    let state = Path::new(&args[2]);
    validate_staging_form(action, state, repository)?;
    let runner = delegated_runner_uid()?;
    // Keep short-lived helper walks expendable without changing a root
    // daemon's protected score on the in-process path.
    fs::write("/proc/self/oom_score_adj", "900")?;
    // Only this explicit sudo-helper entry point delegates ownership. A root
    // daemon's in-process path must not interpret inherited SUDO_UID as identity.
    let authority = load()?;
    staging_authorized_as(&authority, action, state, repository, fill, Some(runner))
}

fn directory_entries(fd: &OwnedFd) -> io::Result<fs::ReadDir> {
    // The kernel-owned magic link names the pinned directory, even if its
    // former pathname has been replaced. No job-controlled link is followed.
    fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

const MAX_STAGING_DEPTH: usize = 128;

fn mount_id(fd: &OwnedFd) -> io::Result<u64> {
    // The libc crate has no statx binding for musl, and the static runner is a
    // musl build, so call the syscall directly. The buffer and offsets follow
    // the kernel ABI in include/uapi/linux/stat.h: a 256-byte `struct statx`
    // with `stx_mask` at offset 0 and `stx_mnt_id` at offset 144.
    const STATX_MNT_ID: u32 = 0x1000;
    const MASK_OFFSET: usize = 0;
    const MNT_ID_OFFSET: usize = 144;
    #[repr(C, align(8))]
    struct Statx([u8; 256]);
    let mut stat = Statx([0; 256]);
    if unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MNT_ID,
            stat.0.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let field = |offset: usize, len: usize| &stat.0[offset..offset + len];
    let mask = u32::from_ne_bytes(field(MASK_OFFSET, 4).try_into().expect("4 bytes"));
    if mask & STATX_MNT_ID == 0 {
        return Err(io::Error::other(
            "staging walk requires statx mount identity",
        ));
    }
    Ok(u64::from_ne_bytes(
        field(MNT_ID_OFFSET, 8).try_into().expect("8 bytes"),
    ))
}

struct StagingWalk {
    owners: StagingOwners,
    device: u64,
    mount: u64,
    directories: HashSet<(u64, u64)>,
    inodes: HashMap<(u64, u64), (u64, u64)>,
    track_links: bool,
    removing: bool,
}

impl StagingWalk {
    fn new(root: &OwnedFd, owners: StagingOwners) -> io::Result<Self> {
        Ok(Self {
            owners,
            device: fs::File::from(root.try_clone()?).metadata()?.dev(),
            mount: mount_id(root)?,
            directories: HashSet::new(),
            inodes: HashMap::new(),
            track_links: true,
            removing: false,
        })
    }

    fn visit(&mut self, fd: &OwnedFd, depth: usize) -> io::Result<bool> {
        let metadata = fs::File::from(fd.try_clone()?).metadata()?;
        if metadata.uid() != self.owners.writer
            && !(self.removing && metadata.uid() == self.owners.runner)
        {
            return Err(io::Error::other(
                "staging inode must be owned by the fill writer",
            ));
        }
        if metadata.dev() != self.device || mount_id(fd)? != self.mount {
            return Err(io::Error::other(
                "staging walk refuses device or mount crossings",
            ));
        }
        let key = (metadata.dev(), metadata.ino());
        if metadata.is_dir() {
            if !self.removing && metadata.mode() & 0o022 != 0 {
                return Err(io::Error::other(
                    "staging directories must not be group/other-writable",
                ));
            }
            if !self.removing && depth > MAX_STAGING_DEPTH {
                return Err(io::Error::other("staging walk exceeds maximum depth"));
            }
            if !self.directories.insert(key) {
                return Err(io::Error::other(
                    "staging directory inode visited more than once",
                ));
            }
        } else if self.track_links {
            let (entries, links) = self.inodes.entry(key).or_insert((0, metadata.nlink()));
            *entries += 1;
            if *links != metadata.nlink() || *entries > *links {
                return Err(io::Error::other(
                    "staging inode visit count exceeds or changes its link count",
                ));
            }
        }
        Ok(metadata.is_dir())
    }
}

fn inspect_staging(root: OwnedFd, owners: StagingOwners, check_links: bool) -> io::Result<()> {
    let mut walk = StagingWalk::new(&root, owners)?;
    // Publication may have crashed partway through chown. Removal can unlink
    // the runner's own entries as well, but link inspection still requires every
    // entry to belong to the fill writer.
    walk.removing = !check_links;
    if !walk.visit(&root, 0)? {
        return Err(io::Error::other("staging root must be a directory"));
    }
    let mut stack = vec![Frame {
        entries: directory_entries(&root)?,
        fd: root,
    }];
    while let Some(parent) = stack.last_mut() {
        let Some(entry) = parent.entries.next() else {
            stack.pop();
            continue;
        };
        let child = open_at(parent.fd.as_raw_fd(), &entry?.file_name(), false)?;
        if walk.visit(&child, stack.len())? {
            stack.push(Frame {
                entries: directory_entries(&child)?,
                fd: child,
            });
        }
    }
    // Directory link counts include child directories' '..' entries and are
    // not evidence of an outside hard link. Only non-directory inodes count.
    if check_links && walk.inodes.values().any(|(entries, links)| links > entries) {
        return Err(io::Error::other(
            "staging generation contains an inode linked from outside the generation",
        ));
    }
    Ok(())
}

fn unlink_at(parent: RawFd, name: &std::ffi::OsStr, directory: bool) -> io::Result<()> {
    let name =
        CString::new(name.as_bytes()).map_err(|_| io::Error::other("NUL in staging entry"))?;
    if unsafe {
        libc::unlinkat(
            parent,
            name.as_ptr(),
            if directory { libc::AT_REMOVEDIR } else { 0 },
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn remove_staging(
    root: OwnedFd,
    repository: OwnedFd,
    name: &std::ffi::OsStr,
    owners: StagingOwners,
) -> io::Result<()> {
    preflight_removal(root.try_clone()?, owners)?;
    let mut walk = StagingWalk::new(&root, owners)?;
    // Removal never changes non-directory metadata, including external hard
    // links. A stopped writer's hostile modes and depth must not block cleanup.
    walk.track_links = false;
    walk.removing = true;
    walk.visit(&root, 0)?;
    removal_directory(&root)?;
    let mut current = root;
    let mut ancestors = Vec::new();
    loop {
        // Reopen the pinned directory on each pass. Do not retain a ReadDir or
        // descriptor per ancestor: a fill can exceed both PATH_MAX and RLIMIT_NOFILE.
        let next = directory_entries(&current)?.next().transpose()?;
        if let Some(entry) = next {
            let name = entry.file_name();
            let child = open_at(current.as_raw_fd(), &name, false)?;
            if walk.visit(&child, ancestors.len() + 1)? {
                removal_directory(&child)?;
                ancestors.push((name, directory_identity(&current)?));
                current = child;
            } else {
                unlink_at(current.as_raw_fd(), &name, false)?;
            }
        } else {
            let Some((child_name, identity)) = ancestors.pop() else {
                confirm_directory_entry(&repository, name, &current)?;
                return unlink_at(repository.as_raw_fd(), name, true);
            };
            let parent = open_at(current.as_raw_fd(), std::ffi::OsStr::new(".."), true)?;
            if directory_identity(&parent)? != identity || mount_id(&parent)? != walk.mount {
                return Err(io::Error::other("staging parent replaced during removal"));
            }
            confirm_directory_entry(&parent, &child_name, &current)?;
            unlink_at(parent.as_raw_fd(), &child_name, true)?;
            current = parent;
        }
    }
}

fn preflight_removal(mut current: OwnedFd, owners: StagingOwners) -> io::Result<()> {
    let mut walk = StagingWalk::new(&current, owners)?;
    walk.removing = true;
    walk.track_links = false;
    walk.visit(&current, 0)?;
    removal_directory(&current)?;
    let entries = |fd: &OwnedFd| -> io::Result<_> {
        Ok(directory_entries(fd)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?
            .into_iter())
    };
    let mut pending = entries(&current)?;
    let mut ancestors = Vec::new();
    loop {
        if let Some(name) = pending.next() {
            let child = open_at(current.as_raw_fd(), &name, false)?;
            if walk.visit(&child, ancestors.len() + 1)? {
                removal_directory(&child)?;
                ancestors.push((pending, directory_identity(&current)?));
                current = child;
                pending = entries(&current)?;
            }
        } else if let Some((remaining, identity)) = ancestors.pop() {
            let parent = open_at(current.as_raw_fd(), std::ffi::OsStr::new(".."), true)?;
            if directory_identity(&parent)? != identity || mount_id(&parent)? != walk.mount {
                return Err(io::Error::other("staging parent replaced during preflight"));
            }
            current = parent;
            pending = remaining;
        } else {
            return Ok(());
        }
    }
}

fn directory_identity(fd: &OwnedFd) -> io::Result<(u64, u64)> {
    let metadata = fs::File::from(fd.try_clone()?).metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

fn confirm_directory_entry(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
    fd: &OwnedFd,
) -> io::Result<()> {
    let entry = open_at(parent.as_raw_fd(), name, true)?;
    if directory_identity(&entry)? != directory_identity(fd)? {
        return Err(io::Error::other(
            "staging directory replaced during removal",
        ));
    }
    Ok(())
}

fn removal_directory(fd: &OwnedFd) -> io::Result<()> {
    // Only directories are chmodded, through the pinned kernel handle. Never
    // repair a file that may also be hard-linked outside the generation.
    let directory = fs::File::open(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn staging_apply(
    action: &str,
    state: &Path,
    repository: &str,
    fill: motor_protocol::Uuid,
    owners: StagingOwners,
) -> io::Result<()> {
    validate_staging_form(action, state, repository)?;
    let state = physical_root(state)?;
    let bases = open_at(state.as_raw_fd(), std::ffi::OsStr::new("build-bases"), true)?;
    owners.store_directory(&fs::File::from(bases.try_clone()?).metadata()?)?;
    let repository = open_at(bases.as_raw_fd(), std::ffi::OsStr::new(repository), true)?;
    owners.store_directory(&fs::File::from(repository.try_clone()?).metadata()?)?;
    let name = format!("staging-{fill}");
    let root = open_at(repository.as_raw_fd(), std::ffi::OsStr::new(&name), true)?;
    if mount_id(&root)? != mount_id(&repository)? {
        return Err(io::Error::other(
            "staging walk refuses device or mount crossings",
        ));
    }
    match action {
        "check-staging-links" => inspect_staging(root, owners, true),
        "remove-staging" => remove_staging(root, repository, std::ffi::OsStr::new(&name), owners),
        _ => unreachable!(),
    }
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
