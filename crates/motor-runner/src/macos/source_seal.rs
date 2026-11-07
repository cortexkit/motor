//! Host-only immutable-file obligations. Paths are used only before VM exposure;
//! recovery finds inodes again after a guest has renamed their parent folders.
use super::{fs::Dir, invalid};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

const IMMUTABLE: u32 = 2; // Darwin UF_IMMUTABLE, also used by portable journal tests.
const MAX_JOURNAL: u64 = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
    birth_seconds: i64,
    birth_nanos: i64,
}

fn identity(stat: &libc::stat) -> Identity {
    Identity {
        #[cfg(target_os = "macos")]
        device: stat.st_dev as u64,
        #[cfg(not(target_os = "macos"))]
        device: stat.st_dev,
        inode: stat.st_ino,
        #[cfg(target_os = "macos")]
        birth_seconds: stat.st_birthtime,
        #[cfg(target_os = "macos")]
        birth_nanos: stat.st_birthtime_nsec,
        #[cfg(not(target_os = "macos"))]
        birth_seconds: 0,
        #[cfg(not(target_os = "macos"))]
        birth_nanos: 0,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    identity: Identity,
    original_flags: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    entries: Vec<Entry>,
    complete: bool,
    cleared: bool,
}

pub(super) fn path(root: &Path, id: Uuid) -> PathBuf {
    root.join("seals").join(format!("{id}.json"))
}

pub(super) fn regular_files(tree: &[u8]) -> io::Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    let mut files = Vec::new();
    for entry in tree.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| invalid("invalid source snapshot entry"))?;
        if entry[..tab].starts_with(b"100644 blob ") || entry[..tab].starts_with(b"100755 blob ") {
            files.push(PathBuf::from(std::ffi::OsStr::from_bytes(
                &entry[tab + 1..],
            )));
        }
    }
    Ok(files)
}

pub(super) fn returns(request: &motor_protocol::Run) -> io::Result<BTreeSet<PathBuf>> {
    crate::caller_path::run(request)
        .caller_return_paths
        .iter()
        .map(|path| {
            crate::return_paths::relative(request, path)
                .map(Path::to_path_buf)
                .map_err(|_| invalid("source seal return path escapes workspace"))
        })
        .collect()
}

impl Journal {
    fn load(path: &Path) -> io::Result<Option<Self>> {
        let file = match Dir::open(path.parent().ok_or_else(|| invalid("seal parent"))?, false)
            .and_then(|dir| dir.file(path.file_name().unwrap(), false))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_JOURNAL {
            return Err(invalid("source seal journal too large"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_JOURNAL + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(invalid("source seal journal too large"));
        }
        let journal: Self = serde_json::from_slice(&bytes).map_err(invalid)?;
        if journal.version != 1 || journal.entries.len() > MAX_ENTRIES {
            return Err(invalid("unsupported source seal journal"));
        }
        let unique: BTreeSet<_> = journal.entries.iter().map(|e| &e.identity).collect();
        if unique.len() != journal.entries.len() {
            return Err(invalid("duplicate source seal identity"));
        }
        Ok(Some(journal))
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(invalid)?;
        if bytes.len() as u64 > MAX_JOURNAL || self.entries.len() > MAX_ENTRIES {
            return Err(invalid("source seal journal too large"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| invalid("seal journal parent"))?;
        Dir::open(parent, true)?;
        // Persist a newly created seals directory itself as well as the journal
        // rename inside it; otherwise a crash could orphan flagged job inodes.
        Dir::open(
            parent
                .parent()
                .ok_or_else(|| invalid("seal journal root"))?,
            false,
        )?
        .sync_directory()?;
        crate::storage::atomic_bytes(path, &bytes, true)
    }
}

pub(super) struct Plan {
    pub(super) files: Vec<PathBuf>,
    unprotected: Vec<PathBuf>,
}
impl Plan {
    pub(super) fn source_read_only(&self) -> bool {
        self.unprotected.is_empty()
    }

    pub(super) fn unprotected(&self) -> &[PathBuf] {
        &self.unprotected
    }
}

/// Paths in `returns` are relative to the workspace, as in `return_paths::relative`.
/// Excluding a directory never exempts its children.
pub(super) fn plan(
    files: impl IntoIterator<Item = PathBuf>,
    returns: &BTreeSet<PathBuf>,
) -> io::Result<Plan> {
    let mut selected = BTreeSet::new();
    let mut unprotected = BTreeSet::new();
    for path in files {
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(invalid("non-normal source seal path"));
        }
        if returns.contains(&path) {
            continue;
        }
        if path.components().any(|c| {
            c == Component::Normal("node_modules".as_ref())
                || c == Component::Normal("target".as_ref())
        }) {
            unprotected.insert(path);
            continue;
        }
        selected.insert(path);
    }
    Ok(Plan {
        files: selected.into_iter().collect(),
        unprotected: unprotected.into_iter().collect(),
    })
}

fn open(root: &Path, relative: &Path) -> io::Result<File> {
    let mut dir = Dir::open(root, false)?;
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Err(invalid("non-normal source seal path"));
        };
        if parts.peek().is_none() {
            return dir.file(name, false);
        }
        dir = dir.child(name, false)?;
    }
    Err(invalid("empty source seal path"))
}

/// Plan against the trusted snapshots, not Git configuration in the staged tree.
pub(super) fn prepare_times(
    trees: &[crate::source_mtimes::Tree],
    staging: &Path,
    store: &Path,
    record: &Path,
    workspace: &str,
    selected: &Plan,
    now: std::time::SystemTime,
) -> io::Result<(crate::source_mtimes::Record, crate::source_mtimes::Times)> {
    let mut exported = BTreeSet::new();
    let mut sealed = BTreeSet::new();
    for tree in trees {
        for path in regular_files(&tree.entries)? {
            let key = tree.prefix.join(&path).to_string_lossy().into_owned();
            exported.insert(key.clone());
            // Sibling shares are read-only; workspace files need the inode seal.
            if tree.destination != workspace || selected.files.binary_search(&path).is_ok() {
                sealed.insert(key);
            }
        }
    }
    crate::source_mtimes::prepare(
        trees,
        staging,
        crate::source_mtimes::Store::Directory(store),
        record,
        crate::source_mtimes::Policy {
            exported: &exported,
            sealed: &sealed,
            // The workspace lock is held from staging through VM stop and
            // copy-back. Serializing jobs of a checkout makes time reuse safe.
            concurrent_jobs: 1,
            validate_path: |path| {
                if path.is_empty() || path.split('/').any(|part| matches!(part, "" | "." | "..")) {
                    return Err(invalid("non-normal source timestamp path"));
                }
                Ok(())
            },
        },
        now,
    )
}

/// Apply before setting uchg: immutable inodes cannot be retimed. Every path
/// component is opened relative to a directory descriptor without following links.
pub(super) fn apply_times(root: &Path, times: &crate::source_mtimes::Times) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    for (path, modified_ms) in times {
        let file = open(root, Path::new(path))?;
        if file.metadata()?.nlink() != 1 {
            return Err(invalid("source timestamp file has aliases"));
        }
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: (modified_ms / 1000).try_into().map_err(invalid)?,
                tv_nsec: ((modified_ms % 1000) * 1_000_000)
                    .try_into()
                    .map_err(invalid)?,
            },
        ];
        if unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

trait Flags {
    fn inspect(&self, file: &File) -> io::Result<(Identity, u32)>;
    fn set(&self, file: &File, flags: u32) -> io::Result<()>;
    fn clear_barrier(&self, roots: &[&Path]) -> io::Result<()> {
        for root in roots {
            match Dir::open(root, false) {
                Ok(dir) => return dir.sync_directory(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

struct Native;
impl Flags for Native {
    fn inspect(&self, file: &File) -> io::Result<(Identity, u32)> {
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let stat = unsafe { stat.assume_init() };
            Ok((identity(&stat), stat.st_flags))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = file;
            Err(invalid("host source flags require macOS"))
        }
    }

    fn set(&self, file: &File, flags: u32) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::fchflags(file.as_raw_fd(), flags) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (file, flags);
            Err(invalid("host source flags require macOS"))
        }
    }
}

pub(super) fn seal(journal: &Path, root: &Path, selected: &[PathBuf]) -> io::Result<()> {
    seal_with(journal, root, selected, &Native).inspect_err(|error| {
        tracing::warn!(?journal, ?root, %error, errno = ?error.raw_os_error(), "macOS source seal failed");
    })
}

fn seal_with(
    journal: &Path,
    root: &Path,
    selected: &[PathBuf],
    flags: &impl Flags,
) -> io::Result<()> {
    let started = std::time::Instant::now();
    if Journal::load(journal)?.is_some() {
        return Err(invalid("source seal intent already exists"));
    }
    let mut intent = Journal {
        version: 1,
        entries: Vec::new(),
        complete: false,
        cleared: false,
    };
    let mut identities = BTreeSet::new();
    for path in selected {
        let file = open(root, path)?;
        if file.metadata()?.nlink() != 1 {
            return Err(invalid("source seal file has aliases"));
        }
        let (identity, original_flags) = flags.inspect(&file)?;
        if !identities.insert(identity.clone()) {
            return Err(invalid("source seal files alias each other"));
        }
        intent.entries.push(Entry {
            identity,
            original_flags,
        });
    }
    // All planned identities are durable before the first inode is changed.
    intent.save(journal)?;
    for (path, entry) in selected.iter().zip(&intent.entries) {
        let file = open(root, path)?;
        let (identity, old) = flags.inspect(&file)?;
        if identity != entry.identity
            || old != entry.original_flags
            || file.metadata()?.nlink() != 1
        {
            return Err(invalid("source seal identity changed before exposure"));
        }
        flags.set(&file, old | IMMUTABLE)?;
        if flags.inspect(&file)?.1 & IMMUTABLE == 0 {
            return Err(invalid("source seal flag not applied"));
        }
    }
    // Flag durability is unnecessary for completion: a host crash loses the VM
    // and command anyway. The identity journal was durable before the first flag,
    // so daemon/host recovery can still reconcile every inode we may have sealed.
    intent.complete = true;
    intent.save(journal)?;
    tracing::info!(
        ?journal,
        files = selected.len(),
        elapsed_us = started.elapsed().as_micros(),
        "macOS source sealed"
    );
    Ok(())
}

/// Call only after lifecycle has saved `stop_proven` with its current proof
/// version: the helper has exited and VM disk holders have been ruled out.
/// Every owned root is walked without following links; detached targets count.
pub(super) fn unseal(journal: &Path, roots: &[&Path]) -> io::Result<()> {
    unseal_with(journal, roots, &Native).inspect_err(|error| {
        tracing::warn!(?journal, ?roots, %error, errno = ?error.raw_os_error(), "macOS source unseal failed; obligation retained");
    })
}

fn unseal_with(journal: &Path, roots: &[&Path], flags: &impl Flags) -> io::Result<()> {
    let started = std::time::Instant::now();
    let Some(mut intent) = Journal::load(journal)? else {
        return Ok(());
    };
    if intent.cleared {
        return Ok(());
    }
    let mut pending: BTreeMap<_, _> = intent
        .entries
        .iter()
        .filter(|e| e.original_flags & IMMUTABLE == 0)
        .map(|e| (e.identity.clone(), e))
        .collect();
    for root in roots {
        if pending.is_empty() {
            break;
        }
        Dir::visit_owned_files(root, |dir, name, stat| {
            let key = identity(stat);
            if pending.contains_key(&key) {
                let file = dir.file(name, false)?;
                let (opened, current) = flags.inspect(&file)?;
                if opened != key {
                    return Err(invalid("source inode changed during unseal"));
                }
                flags.set(&file, current & !IMMUTABLE)?;
                if flags.inspect(&file)?.1 & IMMUTABLE != 0 {
                    return Err(invalid("source seal flag not cleared"));
                }
                pending.remove(&key);
            }
            Ok(())
        })?;
    }
    if !pending.is_empty() {
        return Err(invalid(format!(
            "source seal identities missing: {}",
            pending.len()
        )));
    }
    // Use one bulk directory flush, not F_FULLFSYNC for every file. A crash may
    // still leave an old flag beside a cleared marker: stopped-job deletion must
    // retry EPERM after clearing the immutable flag; target clones are scrubbed
    // before publication as well.
    flags.clear_barrier(roots)?;
    intent.cleared = true;
    intent.save(journal)?;
    tracing::info!(
        ?journal,
        files = intent.entries.len(),
        elapsed_us = started.elapsed().as_micros(),
        "macOS source unsealed"
    );
    Ok(())
}

/// The clear marker is retained until the owned trees have all been deleted.
pub(super) fn forget(journal: &Path) -> io::Result<()> {
    if let Some(intent) = Journal::load(journal)? {
        if !intent.cleared {
            return Err(invalid("source seal has not been cleared"));
        }
        let parent = Dir::open(journal.parent().unwrap(), false)?;
        parent.remove(journal.file_name().unwrap())?;
        parent.sync_directory()?;
    }
    Ok(())
}

pub(super) fn require_cleared(journal: &Path) -> io::Result<()> {
    if Journal::load(journal)?.is_some_and(|j| !j.cleared) {
        return Err(invalid("source seal has not been cleared"));
    }
    Ok(())
}

pub(super) fn require_complete(journal: &Path) -> io::Result<()> {
    if !Journal::load(journal)?.is_some_and(|j| j.complete) {
        return Err(invalid("complete source seal journal missing"));
    }
    Ok(())
}

pub(super) fn before_exposure(journal: &Path) -> io::Result<()> {
    if !Journal::load(journal)?.is_some_and(|j| j.complete && !j.cleared) {
        return Err(invalid("active complete source seal journal missing"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "source_seal_tests.rs"]
mod tests;
