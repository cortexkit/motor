//! Root-only guest bootstrap. No command is received until every share is verified.
use crate::{CARGO_HOME, Mount};
use std::{collections::BTreeSet, io, path::Path};

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}
fn entry_error(entry: &Mount, reason: impl std::fmt::Display) -> io::Error {
    invalid(format!(
        "virtiofs tag {} destination {}: {reason}",
        entry.tag,
        entry.destination.display()
    ))
}
/// Validate the complete map before creating directories or invoking mount tools.
pub fn validate(entries: &[Mount]) -> io::Result<()> {
    if entries.len() < 2 || entries.len() > 64 {
        return Err(invalid(
            "mount map requires workspace and cargo-home, at most 64 entries",
        ));
    }
    let mut tags = BTreeSet::new();
    for (i, entry) in entries.iter().enumerate() {
        let text = entry
            .destination
            .to_str()
            .ok_or_else(|| entry_error(entry, "non-UTF8 destination"))?;
        if !text.starts_with('/')
            || text[1..].split('/').any(|p| matches!(p, "" | "." | ".."))
            || text.bytes().any(|b| b == 0)
        {
            return Err(entry_error(
                entry,
                "destination must be absolute and normalized",
            ));
        }
        let role = if entry.tag == "workspace" {
            0
        } else if entry.tag == "cargo-home" {
            1
        } else if entry
            .tag
            .strip_prefix("sibling-")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            2
        } else {
            return Err(entry_error(entry, "unknown share role"));
        };
        if !tags.insert(entry.tag.clone()) {
            return Err(entry_error(entry, "duplicate tag"));
        }
        if entry.tag.len() > 36 {
            return Err(entry_error(entry, "tag exceeds 36 bytes"));
        }
        if role == 1 {
            if entry.destination != Path::new(CARGO_HOME) || entry.read_only {
                return Err(entry_error(
                    entry,
                    "cargo-home must be read-write at its pinned destination",
                ));
            }
        } else {
            if !text.starts_with("/Users/")
                || !text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
            {
                return Err(entry_error(
                    entry,
                    "workspace and siblings must be beneath /Users with allowed path characters",
                ));
            }
            if entry.read_only != (role == 2) {
                return Err(entry_error(
                    entry,
                    "workspace must be read-write and siblings read-only",
                ));
            }
        }
        if entries[..i].iter().any(|other| {
            let left = entry.destination.to_string_lossy().to_ascii_lowercase();
            let right = other.destination.to_string_lossy().to_ascii_lowercase();
            Path::new(&left).starts_with(&right) || Path::new(&right).starts_with(&left)
        }) {
            return Err(entry_error(entry, "overlapping destinations"));
        }
    }
    if !tags.contains("workspace") || !tags.contains("cargo-home") {
        return Err(invalid("mount map lacks workspace or cargo-home"));
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub tag: String,
    pub destination: std::path::PathBuf,
    pub read_only: bool,
    pub nosuid: bool,
    pub nodev: bool,
    pub filesystem: String,
}
pub trait System {
    fn prepare(&mut self, destination: &Path) -> io::Result<()>;
    fn mount(&mut self, entry: &Mount) -> io::Result<()>;
    fn observations(&mut self) -> io::Result<Vec<Observation>>;
}
pub fn mount_all(entries: &[Mount], system: &mut impl System) -> io::Result<()> {
    validate(entries)?;
    for entry in entries {
        system
            .prepare(&entry.destination)
            .map_err(|e| entry_error(entry, e))?;
    }
    for entry in entries {
        system.mount(entry).map_err(|e| entry_error(entry, e))?;
    }
    let observed = system.observations()?;
    for entry in entries {
        let matches: Vec<_> = observed
            .iter()
            .filter(|o| o.destination == entry.destination)
            .collect();
        if matches.len() != 1
            || matches[0].tag != VIRTIOFS_SOURCE
            || matches[0].filesystem != VIRTIOFS_TYPE
            || matches[0].read_only != entry.read_only
            || !matches[0].nosuid
            || !matches[0].nodev
        {
            return Err(entry_error(
                entry,
                format!(
                    "mounted filesystem or access mode does not match metadata; observed at this destination: {matches:?}"
                ),
            ));
        }
    }
    if observed
        .iter()
        .filter(|o| o.filesystem == VIRTIOFS_TYPE)
        .count()
        != entries.len()
    {
        return Err(invalid("unexpected virtiofs mount outside boot map"));
    }
    Ok(())
}

/// macOS reports every virtiofs mount with this file-system type and the same
/// generic source name, never the share's tag, so the tag cannot be read back.
/// The checks above still bind each tag to its destination: `mount_virtiofs`
/// mounted that tag there, exactly one virtio mount sits at each destination,
/// and no virtio mount exists outside the boot map.
const VIRTIOFS_TYPE: &str = "AppleVirtIOFS";
const VIRTIOFS_SOURCE: &str = "virtio-fs";

/// Walk from a retained directory descriptor, refusing links in every component.
/// Newly created directories are root-owned when used by the root guest daemon.
#[cfg(any(target_os = "macos", test))]
fn prepare_at(root: &Path, destination: &Path) -> io::Result<()> {
    use std::path::Component;
    use std::{
        ffi::CString,
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    let c = |name: &std::ffi::OsStr| CString::new(name.as_bytes()).map_err(invalid);
    let open = |parent, name: &std::ffi::CStr| unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    let fd = unsafe {
        libc::open(
            c(root.as_os_str())?.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut dir = unsafe { File::from_raw_fd(fd) };
    for part in destination.components() {
        let Component::Normal(name) = part else {
            if part == Component::RootDir {
                continue;
            }
            return Err(invalid("non-normal mount destination"));
        };
        let name = c(name)?;
        let mut next = open(dir.as_raw_fd(), &name);
        if next < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
                return Err(io::Error::last_os_error());
            }
            next = open(dir.as_raw_fd(), &name);
        }
        if next < 0 {
            return Err(io::Error::last_os_error());
        }
        dir = unsafe { File::from_raw_fd(next) };
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn mount_native(entries: &[Mount]) -> io::Result<()> {
    struct Native;
    impl System for Native {
        fn prepare(&mut self, destination: &Path) -> io::Result<()> {
            prepare_at(Path::new("/"), destination)
        }
        fn mount(&mut self, entry: &Mount) -> io::Result<()> {
            let mut command = std::process::Command::new("/sbin/mount_virtiofs");
            command.env_clear();
            command.args([
                "-o",
                if entry.read_only {
                    "ro,nosuid,nodev"
                } else {
                    "nosuid,nodev"
                },
            ]);
            let output = command.arg(&entry.tag).arg(&entry.destination).output()?;
            if !output.status.success() {
                return Err(invalid(format!(
                    "mount_virtiofs failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(())
        }
        fn observations(&mut self) -> io::Result<Vec<Observation>> {
            use std::ffi::CStr;
            let mut mounts = std::ptr::null_mut();
            let count = unsafe { libc::getmntinfo(&mut mounts, libc::MNT_NOWAIT) };
            if count <= 0 {
                return Err(io::Error::last_os_error());
            }
            let mut out = Vec::new();
            for mount in unsafe { std::slice::from_raw_parts(mounts, count as usize) } {
                let string = |bytes: &[libc::c_char]| -> io::Result<String> {
                    unsafe { CStr::from_ptr(bytes.as_ptr()) }
                        .to_str()
                        .map(str::to_string)
                        .map_err(invalid)
                };
                out.push(Observation {
                    tag: string(&mount.f_mntfromname)?,
                    destination: string(&mount.f_mntonname)?.into(),
                    read_only: mount.f_flags & libc::MNT_RDONLY as u32 != 0,
                    nosuid: mount.f_flags & libc::MNT_NOSUID as u32 != 0,
                    nodev: mount.f_flags & libc::MNT_NODEV as u32 != 0,
                    filesystem: string(&mount.f_fstypename)?,
                });
            }
            Ok(out)
        }
    }
    if unsafe { libc::geteuid() } != 0 {
        return Err(invalid("virtiofs bootstrap requires root"));
    }
    mount_all(entries, &mut Native)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn map() -> Vec<Mount> {
        vec![
            Mount {
                tag: "workspace".into(),
                destination: "/Users/test/repo".into(),
                read_only: false,
            },
            Mount {
                tag: "sibling-0".into(),
                destination: "/Users/test/sibling".into(),
                read_only: true,
            },
            Mount {
                tag: "cargo-home".into(),
                destination: CARGO_HOME.into(),
                read_only: false,
            },
        ]
    }
    #[test]
    fn destinations_refuse_relative_non_normal_disallowed_and_overlapping_paths() {
        for path in [
            "Users/a",
            "/Users/a/../b",
            "/Users/./b",
            "/Users//b",
            "/Users/a/",
            "/Users",
            "/etc/repo",
            "/Users/a b",
            "/Users/\0b",
        ] {
            let mut entries = map();
            entries[0].destination = path.into();
            assert!(validate(&entries).is_err(), "{path}");
        }
        for path in [
            "/Users/test/repo",
            "/Users/test/repo/sub",
            "/Users/test",
            "/Users/TEST/REPO/sub",
        ] {
            let mut entries = map();
            entries[1].destination = path.into();
            assert!(validate(&entries).is_err());
        }
    }
    #[test]
    fn map_refuses_wrong_roles_access_modes_counts_and_cargo_destination() {
        let valid = map();
        validate(&valid).unwrap();
        for index in 0..3 {
            let mut entries = valid.clone();
            entries[index].read_only = !entries[index].read_only;
            assert!(validate(&entries).is_err());
        }
        for tag in [
            "unknown",
            "workspace",
            "sibling-",
            "sibling-!",
            "sibling-111111111111111111111111111111111111111",
        ] {
            let mut entries = valid.clone();
            entries[1].tag = tag.into();
            assert!(validate(&entries).is_err());
        }
        let mut entries = valid.clone();
        entries[2].destination = "/opt/motor/elsewhere".into();
        assert!(validate(&entries).is_err());
        let mut entries = valid.clone();
        entries.remove(0);
        assert!(validate(&entries).is_err());
        assert!(validate(&[]).is_err());
        assert!(validate(&vec![valid[0].clone(); 65]).is_err());
    }
    #[test]
    fn descriptor_walk_rejects_symlink_ancestor_and_mountpoint() {
        use std::os::unix::fs::symlink;
        let scratch = cortexkit_test_support::ScratchDir::new("mount-walk");
        let outside = scratch.join("outside");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, scratch.join("linked")).unwrap();
        assert!(prepare_at(&scratch.join(""), Path::new("/linked/child")).is_err());
        assert!(prepare_at(&scratch.join(""), Path::new("/linked")).is_err());
        assert!(!outside.join("child").exists());
        std::fs::write(scratch.join("file"), "not a directory").unwrap();
        assert!(prepare_at(&scratch.join(""), Path::new("/file")).is_err());
        prepare_at(&scratch.join(""), Path::new("/real/child")).unwrap();
        assert!(scratch.join("real/child").is_dir());
    }
    #[derive(Default)]
    struct Fake {
        entries: Vec<Mount>,
        prepared: usize,
        corrupt: usize,
    }
    impl System for Fake {
        fn prepare(&mut self, _: &Path) -> io::Result<()> {
            self.prepared += 1;
            Ok(())
        }
        fn mount(&mut self, entry: &Mount) -> io::Result<()> {
            self.entries.push(entry.clone());
            Ok(())
        }
        fn observations(&mut self) -> io::Result<Vec<Observation>> {
            // Exactly what macOS reports for a virtiofs mount: a generic source
            // name and Apple's file-system type, never the share's tag.
            let mut out: Vec<_> = self
                .entries
                .iter()
                .map(|e| Observation {
                    tag: VIRTIOFS_SOURCE.into(),
                    destination: e.destination.clone(),
                    read_only: e.read_only,
                    nosuid: true,
                    nodev: true,
                    filesystem: VIRTIOFS_TYPE.into(),
                })
                .collect();
            match self.corrupt {
                1 => out[0].tag = "/dev/disk3s1".into(),
                2 => out[0].read_only = true,
                3 => out[0].filesystem = "apfs".into(),
                4 => {
                    out.pop();
                }
                5 => out.push(Observation {
                    tag: VIRTIOFS_SOURCE.into(),
                    destination: "/Users/other".into(),
                    read_only: false,
                    nosuid: true,
                    nodev: true,
                    filesystem: VIRTIOFS_TYPE.into(),
                }),
                6 => out[0].nosuid = false,
                7 => out[0].nodev = false,
                8 => out[0].destination = "/Users/test/other".into(),
                _ => {}
            }
            Ok(out)
        }
    }
    #[test]
    fn mounts_are_verified_before_bootstrap_can_connect() {
        let mut fake = Fake::default();
        mount_all(&map(), &mut fake).unwrap();
        assert_eq!(fake.prepared, 3);
        for corrupt in 1..=8 {
            let mut fake = Fake {
                corrupt,
                ..Fake::default()
            };
            assert!(
                mount_all(&map(), &mut fake).is_err(),
                "corruption {corrupt}"
            );
        }
        let mut bad = map();
        bad[0].destination = "relative".into();
        let mut fake = Fake::default();
        assert!(mount_all(&bad, &mut fake).is_err());
        assert_eq!(fake.prepared, 0);
    }
}
