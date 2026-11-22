//! Descriptor-relative inspection of a job-written tree without following symlinked ancestors.
use super::*;
use std::{
    ffi::{CStr, CString, OsString},
    fs::File,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{ffi::OsStringExt, fs::PermissionsExt},
    },
};

// Snapshot comparisons must not read a guest-selected disk image or an
// indefinitely growing file. Larger inputs are conservatively reported changed.
const MAX_HASH_BYTES: u64 = 64 * 1024 * 1024;

pub(super) enum Lookup {
    Missing,
    Blocked,
    Entry(Entry),
}

pub(super) struct Entry {
    parent: File,
    leaf: CString,
    stat: libc::stat,
}

fn open_at(parent: RawFd, name: &CStr, flags: i32) -> io::Result<File> {
    // Below the trusted worktree root, names are single validated components.
    // O_NOFOLLOW therefore protects every guest-controlled path component.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn unavailable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
    )
}

pub(super) fn lookup(root: &Path, name: &Path) -> io::Result<Lookup> {
    let mut components = Vec::new();
    for component in name.components() {
        let Component::Normal(name) = component else {
            return Err(invalid("invalid change-scan path component"));
        };
        components.push(CString::new(name.as_encoded_bytes()).map_err(invalid)?);
    }
    let leaf = components
        .pop()
        .ok_or_else(|| invalid("empty change-scan path"))?;
    let root = CString::new(root.as_os_str().as_encoded_bytes()).map_err(invalid)?;
    let mut parent = open_at(libc::AT_FDCWD, &root, libc::O_RDONLY | libc::O_DIRECTORY)?;
    for component in components {
        parent = match open_at(
            parent.as_raw_fd(),
            &component,
            libc::O_RDONLY | libc::O_DIRECTORY,
        ) {
            Ok(directory) => directory,
            // A missing ancestor means the path doesn't exist either, as when
            // the caller deleted a whole folder. A symlinked or non-directory
            // ancestor is never followed, so its path stays unknown (Blocked).
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(Lookup::Missing),
            Err(error) if unavailable(&error) => return Ok(Lookup::Blocked),
            Err(error) => return Err(error),
        };
    }
    let mut stat = std::mem::MaybeUninit::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(Lookup::Missing)
        } else {
            Err(error)
        };
    }
    Ok(Lookup::Entry(Entry {
        parent,
        leaf,
        stat: unsafe { stat.assume_init() },
    }))
}

impl Entry {
    fn kind(&self) -> libc::mode_t {
        self.stat.st_mode & libc::S_IFMT
    }

    fn read_link(&self) -> io::Result<OsString> {
        let mut bytes = vec![0; 256];
        loop {
            let size = unsafe {
                libc::readlinkat(
                    self.parent.as_raw_fd(),
                    self.leaf.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if size < 0 {
                return Err(io::Error::last_os_error());
            }
            if (size as usize) < bytes.len() {
                bytes.truncate(size as usize);
                return Ok(OsString::from_vec(bytes));
            }
            if bytes.len() >= 65536 {
                return Err(invalid("change-scan symlink is too long"));
            }
            bytes.resize(bytes.len() * 2, 0);
        }
    }
}

pub(super) fn matches_snapshot(
    scan: &ChangeScan<'_>,
    name: &Path,
    snapshot: &SnapshotEntry,
) -> io::Result<bool> {
    let entry = match lookup(scan.path, name)? {
        Lookup::Entry(entry) => entry,
        Lookup::Missing => return Ok(snapshot.mode == "000000"),
        Lookup::Blocked => return Ok(false),
    };
    let mut command = scan.command();
    command.args(["hash-object", "--stdin"]);
    let output = match snapshot.mode.as_str() {
        "120000" if entry.kind() == libc::S_IFLNK => {
            git_input(command, entry.read_link()?.as_encoded_bytes())?
        }
        "100644" | "100755" if entry.kind() == libc::S_IFREG => {
            // O_NONBLOCK prevents a replaced FIFO from blocking the scanner.
            let file = match open_at(
                entry.parent.as_raw_fd(),
                &entry.leaf,
                libc::O_RDONLY | libc::O_NONBLOCK,
            ) {
                Ok(file) => file,
                Err(error) if unavailable(&error) => return Ok(false),
                Err(error) => return Err(error),
            };
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.len() > MAX_HASH_BYTES
                || (metadata.permissions().mode() & 0o100 != 0) != (snapshot.mode == "100755")
            {
                return Ok(false);
            }
            // Bound the read as well as the initial size check: the descriptor
            // could grow, and virtual files can report a length of zero.
            let mut bytes = Vec::new();
            file.take(MAX_HASH_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_HASH_BYTES {
                return Ok(false);
            }
            // --stdin supplies only confined bytes; --path still selects Git's
            // clean filters and attributes for the original repository name.
            command.arg("--path").arg(name);
            git_input(command, &bytes)?
        }
        "160000" if entry.kind() == libc::S_IFDIR => {
            match lookup(scan.path, &name.join(".git"))? {
                Lookup::Missing => return Ok(true),
                Lookup::Blocked => return Ok(false),
                Lookup::Entry(_) => {}
            }
            if scan.untrusted() {
                return Ok(false);
            }
            let full = scan.path.join(name);
            if !change_scan_git(&full, &["status", "--porcelain", "--untracked-files=all"])?
                .is_empty()
            {
                return Ok(false);
            }
            return Ok(
                String::from_utf8_lossy(&change_scan_git(&full, &["rev-parse", "HEAD"])?)
                    .trim_end()
                    == snapshot.blob,
            );
        }
        _ => return Ok(false),
    };
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end() == snapshot.blob)
}
