//! Content-derived source times for a build folder seeded during this setup.
use crate::{build_base::Record, build_base::Timestamp, workspace};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Eq, PartialEq)]
struct Entry {
    mode: String,
    oid: String,
}

fn entries(mirror: &Path, tree: &str) -> io::Result<BTreeMap<PathBuf, Entry>> {
    if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other("invalid seed tree id"));
    }
    let bytes = workspace::git_bytes(mirror, &["ls-tree", "-r", "-t", "-z", tree])?;
    let mut entries = BTreeMap::new();
    entries.insert(
        PathBuf::new(),
        Entry {
            mode: "040000".into(),
            oid: tree.into(),
        },
    );
    for entry in bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| io::Error::other("invalid seed tree entry"))?;
        let fields: Vec<_> = std::str::from_utf8(&entry[..tab])
            .map_err(io::Error::other)?
            .split_whitespace()
            .collect();
        if fields.len() != 3 || !matches!(fields[0], "040000" | "100644" | "100755" | "120000") {
            return Err(io::Error::other("unsupported seed tree entry"));
        }
        let path = PathBuf::from(OsStr::from_bytes(&entry[tab + 1..]));
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || matches!(path.components().next(), Some(Component::Normal(name)) if name == ".git" || name == "target")
        {
            return Err(io::Error::other("invalid tracked seed path"));
        }
        if entries
            .insert(
                path,
                Entry {
                    mode: fields[0].into(),
                    oid: fields[2].into(),
                },
            )
            .is_some()
        {
            return Err(io::Error::other("duplicate seed tree entry"));
        }
    }
    Ok(entries)
}

fn now() -> io::Result<Timestamp> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok(Timestamp {
        seconds: elapsed.as_secs().try_into().map_err(io::Error::other)?,
        nanos: elapsed.subsec_nanos().into(),
    })
}

fn set_time(file: &File, time: Timestamp) -> io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: time.seconds,
            tv_nsec: time.nanos,
        },
    ];
    // AT_EMPTY_PATH stamps the pinned inode, including a link itself. A rename
    // after the walk cannot redirect this operation to a replacement pathname.
    if unsafe {
        libc::utimensat(
            file.as_raw_fd(),
            c"".as_ptr(),
            times.as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Build-base seeding calls this after cloning and ownership repair, before
/// Cargo preparation and sealing, only for a folder seeded in this setup.
/// `tree` is the verified snapshot tree, never its commit or author date.
#[allow(dead_code)]
pub(crate) fn apply(
    root: &Path,
    mirror: &Path,
    tree: &str,
    base: &Record,
) -> io::Result<Timestamp> {
    let new = now()?;
    apply_at(root, mirror, tree, base, new)?;
    Ok(new)
}

fn apply_at(
    root: &Path,
    mirror: &Path,
    tree: &str,
    base: &Record,
    new: Timestamp,
) -> io::Result<()> {
    if [new, base.fill_start, base.newest_mtime]
        .iter()
        .any(|time| !(0..1_000_000_000).contains(&time.nanos))
        || new <= base.newest_mtime
        || base.fill_start > new
    {
        return Err(io::Error::other("seed time is not later than base times"));
    }
    let old_entries = entries(mirror, &base.tree)?;
    let new_entries = entries(mirror, tree)?;
    let root = File::from(crate::permissions::physical_root(root)?);
    if !root.metadata()?.is_dir() {
        return Err(io::Error::other("seed workspace is not a directory"));
    }
    let device = root.metadata()?.dev();
    let mut children: BTreeMap<PathBuf, Vec<&Path>> = BTreeMap::new();
    for path in new_entries
        .keys()
        .filter(|path| !path.as_os_str().is_empty())
    {
        children
            .entry(path.parent().unwrap().into())
            .or_default()
            .push(path);
    }
    Walk {
        old_entries: &old_entries,
        new_entries: &new_entries,
        children,
        device,
        old: base.fill_start,
        new,
    }
    .visit(&root, Path::new(""))
}

struct Walk<'a> {
    old_entries: &'a BTreeMap<PathBuf, Entry>,
    new_entries: &'a BTreeMap<PathBuf, Entry>,
    children: BTreeMap<PathBuf, Vec<&'a Path>>,
    device: u64,
    old: Timestamp,
    new: Timestamp,
}

impl Walk<'_> {
    fn visit(&self, pin: &File, path: &Path) -> io::Result<()> {
        let entry = &self.new_entries[path];
        let meta = pin.metadata()?;
        let kind_matches = match entry.mode.as_str() {
            "040000" => meta.is_dir(),
            "120000" => meta.is_symlink(),
            _ => meta.is_file() && (meta.mode() & 0o100 != 0) == (entry.mode == "100755"),
        };
        if !kind_matches || meta.dev() != self.device || (!meta.is_dir() && meta.nlink() != 1) {
            return Err(io::Error::other(
                "tracked seed inode type, mode or links changed",
            ));
        }
        // Reject ignored leftovers as well as unexpected entries. Only the two
        // root `.git` and `target` entries are exempt; neither is traversed or time-set.
        if meta.is_dir() {
            for child in fs::read_dir(format!("/proc/self/fd/{}", pin.as_raw_fd()))? {
                let name = child?.file_name();
                if path.as_os_str().is_empty() && (name == ".git" || name == "target") {
                    continue;
                }
                if !self.new_entries.contains_key(&path.join(name)) {
                    return Err(io::Error::other("untracked entry in seed workspace"));
                }
            }
            for child in self.children.get(path).into_iter().flatten() {
                let name = CString::new(child.file_name().unwrap().as_bytes())?;
                let fd = unsafe {
                    libc::openat(
                        pin.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                self.visit(&unsafe { File::from_raw_fd(fd) }, child)?;
            }
        }
        let time = if self.old_entries.get(path) == Some(entry) {
            self.old
        } else {
            self.new
        };
        set_time(pin, time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_base::{CompatibleInputs, ToolchainIdentity};
    use std::os::unix::fs::{symlink, PermissionsExt};

    const OLD: Timestamp = Timestamp {
        seconds: 1_000_000_000,
        nanos: 123_456_789,
    };
    const NEW: Timestamp = Timestamp {
        seconds: 1_700_000_000,
        nanos: 987_654_321,
    };

    struct Fixture {
        scratch: cortexkit_test_support::ScratchDir,
        root: PathBuf,
        base: Record,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = cortexkit_test_support::ScratchDir::new("seed-times");
            let root = scratch.join("repo");
            fs::create_dir(&root).unwrap();
            workspace::git(&root, &["init", "-q"]).unwrap();
            for (path, contents) in [
                ("changed/file", "before"),
                ("mode/file", "mode only"),
                ("deleted/gone", "gone"),
                ("deleted/keep", "keep"),
                ("renamed/from", "rename contents"),
                ("renamed/keep", "keep"),
                ("stable/file", "unchanged"),
            ] {
                fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
                fs::write(root.join(path), contents).unwrap();
            }
            fs::write(scratch.join("outside"), "outside").unwrap();
            symlink("../../outside", root.join("stable/link")).unwrap();
            workspace::git(&root, &["add", "."]).unwrap();
            let tree = workspace::git(&root, &["write-tree"])
                .unwrap()
                .trim()
                .to_owned();
            let base = Record {
                format: 1,
                // Only the recorded tree participates: there need not even be
                // a commit object from which a date could be read.
                commit: "not-a-commit".into(),
                tree,
                inputs: CompatibleInputs {
                    toolchain: ToolchainIdentity {
                        rustc_verbose: "test".into(),
                        rust_toolchain: None,
                        rust_toolchain_toml: None,
                        wrapper: None,
                    },
                    environment: "test".into(),
                },
                fixed_path: "/w/account/repo/name".into(),
                fill_start: OLD,
                newest_mtime: Timestamp {
                    seconds: OLD.seconds + 1,
                    nanos: 0,
                },
            };
            Self {
                scratch,
                root,
                base,
            }
        }

        fn snapshot(&self) -> String {
            workspace::git(&self.root, &["add", "-A"]).unwrap();
            workspace::git(&self.root, &["write-tree"])
                .unwrap()
                .trim()
                .into()
        }

        fn assert_time(&self, path: &str, expected: Timestamp) {
            assert_eq!(mtime(&self.root.join(path)), expected, "{path}");
        }

        fn after_verification(&self, tree: &str, hook: impl FnOnce()) -> io::Result<()> {
            workspace::git(&self.root, &["diff-files", "--quiet"])?;
            hook();
            apply_at(&self.root, &self.root, tree, &self.base, NEW)
        }
    }

    fn mtime(path: &Path) -> Timestamp {
        let meta = fs::symlink_metadata(path).unwrap();
        Timestamp {
            seconds: meta.mtime(),
            nanos: meta.mtime_nsec(),
        }
    }

    #[test]
    fn changed_mode_added_deleted_and_renamed_paths_stamp_their_trees_not_dates() {
        let f = Fixture::new();
        fs::write(f.root.join("changed/file"), "after").unwrap();
        fs::set_permissions(f.root.join("mode/file"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(f.root.join("added")).unwrap();
        fs::write(f.root.join("added/file"), "new").unwrap();
        fs::remove_file(f.root.join("deleted/gone")).unwrap();
        fs::rename(f.root.join("renamed/from"), f.root.join("renamed/to")).unwrap();
        let tree = f.snapshot();
        let diff = workspace::git(
            &f.root,
            &["diff", "--name-status", "-M", &f.base.tree, &tree],
        )
        .unwrap();
        for expected in [
            "M\tchanged/file",
            "M\tmode/file",
            "A\tadded/file",
            "D\tdeleted/gone",
            "R100\trenamed/from\trenamed/to",
        ] {
            assert!(diff.contains(expected), "missing {expected}: {diff}");
        }
        // A checkout with a changed source dated before the fill must still
        // rebuild, even though neither tree has a commit/author date at all.
        File::open(f.root.join("changed/file"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
        apply_at(&f.root, &f.root, &tree, &f.base, NEW).unwrap();
        for path in [
            "",
            "changed",
            "changed/file",
            "mode",
            "mode/file",
            "added",
            "added/file",
            "deleted",
            "renamed",
            "renamed/to",
        ] {
            f.assert_time(path, NEW);
        }
        for path in [
            "stable",
            "stable/file",
            "stable/link",
            "deleted/keep",
            "renamed/keep",
        ] {
            f.assert_time(path, OLD);
        }
        assert!(!f.root.join("deleted/gone").exists());
    }

    #[test]
    fn unchanged_root_and_tracked_symlink_receive_old_time_without_touching_target() {
        let f = Fixture::new();
        let outside = mtime(&f.scratch.join("outside"));
        apply_at(&f.root, &f.root, &f.base.tree, &f.base, NEW).unwrap();
        f.assert_time("", OLD);
        f.assert_time("stable", OLD);
        f.assert_time("stable/link", OLD);
        assert_eq!(mtime(&f.scratch.join("outside")), outside);
    }

    #[test]
    fn time_not_strictly_after_newest_base_mtime_is_refused_before_any_stamp() {
        let mut f = Fixture::new();
        let before = mtime(&f.root.join("stable/file"));
        for newest in [
            NEW,
            Timestamp {
                seconds: NEW.seconds + 1,
                nanos: 0,
            },
        ] {
            f.base.newest_mtime = newest;
            assert!(apply_at(&f.root, &f.root, &f.base.tree, &f.base, NEW).is_err());
            assert_eq!(mtime(&f.root.join("stable/file")), before);
        }
        f.base.newest_mtime = Timestamp {
            seconds: i64::MAX,
            nanos: 0,
        };
        assert!(apply(&f.root, &f.root, &f.base.tree, &f.base).is_err());
    }

    #[test]
    fn file_symlink_planted_after_verification_is_refused_not_followed() {
        let f = Fixture::new();
        let before = mtime(&f.scratch.join("outside"));
        assert!(f
            .after_verification(&f.base.tree, || {
                fs::remove_file(f.root.join("stable/file")).unwrap();
                symlink("../../outside", f.root.join("stable/file")).unwrap();
            })
            .is_err());
        assert_eq!(mtime(&f.scratch.join("outside")), before);
    }

    #[test]
    fn parent_symlink_planted_after_verification_is_refused_not_followed() {
        let f = Fixture::new();
        let outside = f.scratch.join("external");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), "outside").unwrap();
        let before = mtime(&outside.join("file"));
        let parent_before = mtime(&outside);
        assert!(f
            .after_verification(&f.base.tree, || {
                fs::remove_dir_all(f.root.join("stable")).unwrap();
                symlink(&outside, f.root.join("stable")).unwrap();
            })
            .is_err());
        assert_eq!(mtime(&outside.join("file")), before);
        assert_eq!(mtime(&outside), parent_before);
    }

    #[test]
    fn untracked_entries_are_refused_and_not_time_set() {
        let f = Fixture::new();
        fs::write(f.root.join("stable/ignored"), "leftover").unwrap();
        let before = mtime(&f.root.join("stable/ignored"));
        assert!(apply_at(&f.root, &f.root, &f.base.tree, &f.base, NEW).is_err());
        assert_eq!(mtime(&f.root.join("stable/ignored")), before);
    }
}
