use super::*;
use crate::{BackendKind, Config};
use std::{fs, io, path::PathBuf};

/// An operator-supplied mapping. Neither side is normalized on input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallerRoot {
    pub caller_prefix: String,
    pub host_root: PathBuf,
}

impl CallerRoot {
    pub fn parse(value: &str) -> io::Result<Self> {
        let (caller, host) = value.split_once('=').ok_or_else(|| {
            io::Error::other(format!("caller root {value:?}: expected PREFIX=HOST"))
        })?;
        if !canonical_spelling(caller) || caller == "/" || !canonical_spelling(host) {
            return Err(refusal(
                caller,
                host,
                "expected canonical absolute paths and a non-root caller prefix",
            ));
        }
        Ok(Self {
            caller_prefix: caller.into(),
            host_root: host.into(),
        })
    }
}

fn canonical_spelling(value: &str) -> bool {
    value.starts_with('/')
        && (value == "/"
            || (!value.ends_with('/')
                && value[1..]
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | ".."))))
}

fn refusal(a: impl std::fmt::Display, b: impl std::fmt::Display, why: &str) -> io::Error {
    io::Error::other(format!("caller root {a} conflicts with {b}: {why}"))
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Checks only configuration. It opens no state and is shared by the CLI preflight.
pub(crate) fn options(config: &Config) -> io::Result<()> {
    for (i, root) in config.caller_roots.iter().enumerate() {
        CallerRoot::parse(&format!(
            "{}={}",
            root.caller_prefix,
            root.host_root.display()
        ))?;
        let caller = Path::new(&root.caller_prefix);
        let host = &root.host_root;
        if config.backend != BackendKind::Host || (!config.isolate_jobs && !test_unisolated(config))
        {
            return Err(refusal(
                caller.display(),
                host.display(),
                "caller roots require isolated Linux host jobs",
            ));
        }
        if !cfg!(target_os = "linux") {
            return Err(refusal(
                caller.display(),
                host.display(),
                "caller roots require the Linux host backend",
            ));
        }
        if config.tenant_vm && !config.boat_profile {
            return Err(refusal(
                caller.display(),
                host.display(),
                "tenant runners serve caller paths by identity",
            ));
        }
        for other in &config.caller_roots[..i] {
            if overlaps(caller, Path::new(&other.caller_prefix)) {
                return Err(refusal(
                    caller.display(),
                    &other.caller_prefix,
                    "caller prefixes must not nest or repeat",
                ));
            }
            if overlaps(host, &other.host_root) {
                return Err(refusal(
                    host.display(),
                    other.host_root.display(),
                    "host roots must not overlap",
                ));
            }
        }
        for identity in config
            .owned_workspace_roots
            .iter()
            .chain(&config.workspace_roots)
        {
            if overlaps(caller, identity) {
                return Err(refusal(
                    caller.display(),
                    identity.display(),
                    "caller prefix overlaps an identity root",
                ));
            }
            if overlaps(host, identity) {
                return Err(refusal(
                    host.display(),
                    identity.display(),
                    "host root overlaps an identity root",
                ));
            }
        }
        if overlaps(host, &config.state_dir()) {
            return Err(refusal(
                host.display(),
                config.state_dir().display(),
                "host root overlaps the instance state directory",
            ));
        }
    }
    Ok(())
}

fn test_unisolated(config: &Config) -> bool {
    #[cfg(test)]
    {
        config.test_allow_unisolated_caller_roots
    }
    #[cfg(not(test))]
    {
        let _ = config;
        false
    }
}

/// Resolve existing ancestors, retaining a nonexistent suffix without creating it.
fn canonical_future(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            Ok(canonical_future(parent)?.join(
                path.file_name()
                    .ok_or_else(|| io::Error::other("missing path name"))?,
            ))
        }
        Err(error) => Err(error),
    }
}

fn no_symlinks(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(refusal(
                    path.display(),
                    ancestor.display(),
                    "host root has a symlink component",
                ))
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(refusal(
                    path.display(),
                    ancestor.display(),
                    "host root component is not a directory",
                ))
            }
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => {
                return Err(refusal(
                    path.display(),
                    ancestor.display(),
                    &error.to_string(),
                ))
            }
        }
    }
    Ok(())
}

fn same_or_nested(a: &Path, b: &Path) -> io::Result<bool> {
    if overlaps(&canonical_future(a)?, &canonical_future(b)?) {
        return Ok(true);
    }
    // A bind mount can hide lexical ancestry. Compare directory identities in
    // both directions so an alias of either input still counts as overlapping.
    for (root, descendant) in [(a, b), (b, a)] {
        if let Ok(id) = crate::filesystem::identity(root) {
            for ancestor in descendant.ancestors() {
                if crate::filesystem::identity(ancestor)
                    .is_ok_and(|other| (id.0, id.1) == (other.0, other.1))
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[derive(Debug)]
pub(super) struct Mount {
    device: String,
    root: PathBuf,
    pub(super) mountpoint: PathBuf,
}

fn unescape(value: &str) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let digits: Vec<_> = input.by_ref().take(3).collect();
        if digits.len() != 3 || !digits.iter().all(|b| matches!(b, b'0'..=b'7')) {
            return Err(io::Error::other("invalid mountinfo escape"));
        }
        let value = u16::from(digits[0] - b'0') * 64
            + u16::from(digits[1] - b'0') * 8
            + u16::from(digits[2] - b'0');
        bytes.push(u8::try_from(value).map_err(io::Error::other)?);
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

pub(super) fn mounts(table: &str) -> io::Result<Vec<Mount>> {
    table
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 10 || !fields.contains(&"-") {
                return Err(io::Error::other(format!("invalid mountinfo line {line:?}")));
            }
            Ok(Mount {
                device: fields[2].into(),
                root: unescape(fields[3])?.into(),
                mountpoint: unescape(fields[4])?.into(),
            })
        })
        .collect()
}

fn bind_sources(root: &Path, table: &[Mount]) -> Vec<PathBuf> {
    let join = |base: &Path, suffix: &Path| {
        // Joining an empty suffix adds a slash, which makes a mounted file
        // such as ld.so.cache look like a directory to canonicalize().
        if suffix.as_os_str().is_empty() {
            base.to_path_buf()
        } else {
            base.join(suffix)
        }
    };
    let Some(mount) = table
        .iter()
        .filter(|m| root.starts_with(&m.mountpoint))
        .max_by_key(|m| m.mountpoint.components().count())
    else {
        return Vec::new();
    };
    let offset = join(&mount.root, root.strip_prefix(&mount.mountpoint).unwrap());
    table
        .iter()
        .filter(|m| m.device == mount.device && offset.starts_with(&m.root))
        .map(|m| join(&m.mountpoint, offset.strip_prefix(&m.root).unwrap()))
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
mod mounted_file_tests {
    use super::*;

    #[test]
    fn mounted_file_bind_sources_remain_files_and_still_protect_their_parent() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-mounted-file-source");
        let mounted = scratch.join("cache");
        let backing = scratch.join("backing-cache");
        let host = scratch.join("callers/home");
        fs::write(&mounted, "cache").unwrap();
        fs::write(&backing, "cache").unwrap();
        fs::create_dir_all(&host).unwrap();
        let encode = |path: &Path| {
            path.to_str()
                .unwrap()
                .replace('\\', "\\134")
                .replace(' ', "\\040")
                .replace('\t', "\\011")
                .replace('\n', "\\012")
        };
        let table = mounts(&format!(
            "1 0 8:1 / / rw - xfs /dev/root rw\n2 1 8:1 {} {} rw - xfs /dev/root rw\n",
            encode(&backing),
            encode(&mounted)
        ))
        .unwrap();
        let sources = bind_sources(&mounted, &table);
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|p| p.as_os_str() == mounted.as_os_str()));
        assert!(sources.iter().any(|p| p.as_os_str() == backing.as_os_str()));
        for source in sources {
            assert!(!same_or_nested(&host, &source).unwrap());
            assert!(same_or_nested(&scratch, &source).unwrap());
        }
    }
}

pub(crate) fn filesystem(config: &Config) -> io::Result<()> {
    if config.caller_roots.is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    let table = fs::read_to_string("/proc/self/mountinfo")?;
    #[cfg(not(target_os = "linux"))]
    let table = String::new();
    filesystem_with_mountinfo(config, &table)
}

pub(super) fn filesystem_with_mountinfo(config: &Config, table: &str) -> io::Result<()> {
    filesystem_with_probe(config, table, probe)
}

pub(super) fn filesystem_with_probe(
    config: &Config,
    table: &str,
    probe_root: impl Fn(&Config, &CallerRoot) -> io::Result<()>,
) -> io::Result<()> {
    options(config)?;
    let table = mounts(table)?;
    let mut reserved = vec![config.state_dir(), config.build_slot_root.clone()];
    #[cfg(target_os = "linux")]
    reserved.extend(crate::isolation::reserved_bind_sources(config));
    for root in config
        .owned_workspace_roots
        .iter()
        .chain(&config.workspace_roots)
    {
        reserved.push(root.clone());
        reserved.extend(bind_sources(root, &table));
    }
    for source in reserved.clone() {
        reserved.extend(bind_sources(&source, &table));
    }
    for root in &config.caller_roots {
        no_symlinks(&root.host_root)?;
        for other in config
            .caller_roots
            .iter()
            .map(|r| &r.host_root)
            .filter(|p| *p != &root.host_root)
            .chain(reserved.iter())
        {
            if same_or_nested(&root.host_root, other).map_err(|error| {
                refusal(
                    root.host_root.display(),
                    other.display(),
                    &error.to_string(),
                )
            })? {
                return Err(refusal(
                    root.host_root.display(),
                    other.display(),
                    "host roots and reserved bind sources must not overlap",
                ));
            }
        }
    }
    for root in &config.caller_roots {
        probe_root(config, root).map_err(|error| {
            refusal(
                root.host_root.display(),
                config.state_dir().display(),
                &format!("host filesystem probe failed: {error}"),
            )
        })?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn probe(config: &Config, root: &CallerRoot) -> io::Result<()> {
    use std::{
        ffi::CString,
        io::{Read, Seek, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::MetadataExt,
        },
    };
    // Each open is relative to the previous directory descriptor; no component
    // can redirect the probe through a symlink between checking and opening it.
    let mut directory = fs::File::open("/")?;
    for component in root.host_root.components().skip(1) {
        use std::os::unix::ffi::OsStrExt;
        let name = CString::new(component.as_os_str().as_bytes())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        directory = unsafe { fs::File::from_raw_fd(fd) };
    }
    let state = config.state_dir();
    let existing = state
        .ancestors()
        .find(|p| p.exists())
        .ok_or_else(|| io::Error::other("state has no existing ancestor"))?;
    if directory.metadata()?.dev() != fs::metadata(existing)?.dev() {
        return Err(refusal(
            root.host_root.display(),
            existing.display(),
            "host root and state must share a filesystem",
        ));
    }
    let temporary = || -> io::Result<fs::File> {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c".".as_ptr(),
                libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    };
    let mut source = temporary()?;
    let mut target = temporary()?;
    source.write_all(b"motor caller-root reflink probe")?;
    source.sync_all()?;
    if unsafe {
        libc::ioctl(
            target.as_raw_fd(),
            // FICLONE. libc's ioctl request type differs between glibc
            // (c_ulong) and musl (c_int); `libc::Ioctl` names the right one.
            0x40049409_u32 as libc::Ioctl,
            source.as_raw_fd(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    target.rewind()?;
    let mut contents = Vec::new();
    target.read_to_end(&mut contents)?;
    if contents != b"motor caller-root reflink probe" {
        return Err(io::Error::other("reflink probe contents differ"));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn probe(_: &Config, _: &CallerRoot) -> io::Result<()> {
    Err(io::Error::other("caller roots require Linux"))
}
