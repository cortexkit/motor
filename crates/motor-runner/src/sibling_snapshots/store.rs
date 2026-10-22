//! Creation and publication of sibling snapshot directories.
//!
//! A snapshot of one key is built at most once: setups that need it take the
//! key's mutex, and the first one builds it while the others wait and then
//! find it ready. Setups that need different keys, including two snapshots of
//! one repository, build in parallel.
//!
//! A snapshot is built under a temporary name, from a copy of the most
//! recently used ready snapshot of the same repository (its predecessor) or
//! from a fresh clone of the mirror. Its tree is verified against the
//! requested `tree_hash`, its JavaScript dependencies are installed from its
//! lockfiles, group and other write access is removed, and the
//! job account gets read-only access. Then its metadata, which records the
//! verified tree, is written and fsynced beside the directory, and a rename
//! publishes the directory under its final name. A final directory therefore
//! always has metadata unless a crash interrupted its removal, and a final
//! directory without readable metadata is removed and built again.
//!
//! Lock order: a setup takes a key mutex while holding no state lock, job log
//! lock or other key mutex. The registry mutex below is only ever taken last
//! and held briefly.

use super::SnapshotKey;
use crate::{
    engine::Config,
    storage::{atomic_json, invalid, read_json},
    workspace::Trees,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Instant, SystemTime},
};
use uuid::Uuid;

/// Replaces the job-account ACL step of creation in tests. It receives the
/// snapshot's temporary directory before publication.
#[cfg(test)]
pub(crate) type AccessHook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

/// Replaces the reflink copy of a predecessor in tests, as `(source, target)`.
#[cfg(test)]
pub(crate) type CloneHook = Arc<dyn Fn(&Path, &Path) -> io::Result<()> + Send + Sync>;

/// Called in tests with a snapshot directory; see `touch_inspected`.
#[cfg(test)]
pub(crate) type TouchHook = Arc<dyn Fn(&Path) + Send + Sync>;

const METADATA_VERSION: u32 = 1;

/// Marks a directory under construction or being removed:
/// `<name>.tmp-<uuid>`. Final names contain no `.`.
const TEMPORARY_MARKER: &str = ".tmp-";

/// What the runner knows about a published snapshot. It lives in
/// `<name>.json` beside the snapshot directory, never inside the tree that
/// jobs see at the sibling's path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Metadata {
    version: u32,
    repository: String,
    commit: String,
    caller: String,
    javascript_environment: String,
    /// The tree the snapshot was verified to hold when it was created.
    pub(crate) tree_hash: String,
}

impl Metadata {
    fn new(key: &SnapshotKey, tree_hash: &str) -> Self {
        Self {
            version: METADATA_VERSION,
            repository: key.repository.clone(),
            commit: key.commit.clone(),
            caller: key.caller.clone(),
            javascript_environment: key.javascript_environment.as_str().to_string(),
            tree_hash: tree_hash.to_string(),
        }
    }

    /// Metadata of an unknown version, or one naming another key, cannot
    /// vouch for the directory beside it.
    fn describes(&self, key: &SnapshotKey) -> bool {
        self.version == METADATA_VERSION
            && self.repository == key.repository
            && self.commit == key.commit
            && self.caller == key.caller
            && self.javascript_environment == key.javascript_environment.as_str()
    }
}

/// `<name>.json` beside the snapshot directory `<name>`.
pub(crate) fn metadata_path(directory: &Path) -> PathBuf {
    let mut name = directory.file_name().unwrap_or_default().to_os_string();
    name.push(".json");
    directory.with_file_name(name)
}

/// Reads the metadata beside `directory`; any failure, including a missing
/// file, means the directory is not a usable snapshot.
pub(crate) fn read_metadata(directory: &Path) -> io::Result<Metadata> {
    read_json(&metadata_path(directory))
}

/// The snapshots that snapshot creations are reading, and the snapshots that
/// deletions have claimed. One mutex guards both, so a deletion can never
/// claim a snapshot that a creation has started to copy, and a creation never
/// starts copying a claimed one.
#[derive(Default)]
struct Registry {
    /// Predecessors being copied, with the number of creations copying each.
    predecessors: BTreeMap<PathBuf, usize>,
    /// Snapshots claimed by a deletion that has not finished.
    deleting: BTreeSet<PathBuf>,
}

/// Creates sibling snapshots on demand. Holds no snapshot state of its own:
/// the snapshot directories and their metadata files are the record.
#[derive(Default)]
pub(crate) struct SnapshotStore {
    /// One mutex per snapshot directory. Entries are removed once no setup
    /// holds or waits for them.
    key_locks: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
    registry: Mutex<Registry>,
    #[cfg(test)]
    pub(crate) clone_override: Option<CloneHook>,
    /// Called by the switch touch once it has decided which paths of a
    /// snapshot to touch and before it writes any, with the key mutex held.
    #[cfg(test)]
    pub(crate) touch_inspected: Option<TouchHook>,
}

/// A predecessor registered by a creation, released when the creation ends.
struct Predecessor<'a> {
    store: &'a SnapshotStore,
    directory: PathBuf,
}

impl Drop for Predecessor<'_> {
    fn drop(&mut self) {
        let mut registry = lock(&self.store.registry);
        if let Some(count) = registry.predecessors.get_mut(&self.directory) {
            *count -= 1;
            if *count == 0 {
                registry.predecessors.remove(&self.directory);
            }
        }
    }
}

/// The protected data are filesystem facts that every holder checks again,
/// so a panic in another holder leaves nothing inconsistent behind.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum Existing {
    Absent,
    Ready(Metadata),
    /// A directory without metadata that describes it, or a non-directory at
    /// the snapshot's name.
    Unusable,
}

impl SnapshotStore {
    /// Returns the published snapshot directory of `key` holding `tree_hash`,
    /// creating it if it does not exist. Refuses when sibling snapshot mode is
    /// off, when the tree a new snapshot checks out is not `tree_hash`, and
    /// when the existing snapshot was verified to hold a different tree; that
    /// snapshot is then left exactly as it was.
    ///
    /// The caller must hold no state lock, job log lock or snapshot key
    /// mutex.
    pub(crate) fn resolve(
        &self,
        config: &Config,
        trees: &Trees,
        key: &SnapshotKey,
        tree_hash: &str,
    ) -> io::Result<PathBuf> {
        if !config.sibling_snapshot_mode() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sibling snapshot mode is off: siblings are checked out in place",
            ));
        }
        if !super::full_object_id(tree_hash) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "sibling {} tree hash {tree_hash:?} is not a full lowercase \
                     hexadecimal object ID",
                    key.repository
                ),
            ));
        }
        let directory = key.directory(config);
        self.with_key_lock(&directory, || {
            match existing(key, &directory)? {
                Existing::Ready(metadata) if metadata.tree_hash == tree_hash => {
                    // The directory's mtime is its last-use time.
                    if let Err(error) = stamp(&directory) {
                        tracing::warn!(snapshot = %directory.display(), %error, "cannot stamp sibling snapshot use");
                    }
                    return Ok(());
                }
                Existing::Ready(metadata) => {
                    return Err(invalid(format!(
                        "sibling {} snapshot {} was verified to hold tree {}, not the \
                         requested tree {tree_hash}; refusing it",
                        key.repository,
                        directory.display(),
                        metadata.tree_hash
                    )));
                }
                Existing::Unusable => {
                    tracing::warn!(snapshot = %directory.display(), "sibling snapshot without readable metadata; recreating it");
                    discard(&directory)?;
                }
                Existing::Absent => {}
            }
            self.create(config, trees, key, tree_hash, &directory)
        })?;
        Ok(directory)
    }

    /// Whether a creation in progress is copying `directory` as its
    /// predecessor. Such a snapshot must not be deleted.
    pub(crate) fn is_creation_source(&self, directory: &Path) -> bool {
        lock(&self.registry).predecessors.contains_key(directory)
    }

    /// Marks `directory` as being deleted, so no creation picks it as a
    /// predecessor from now on. Refuses, returning false, while a creation is
    /// copying it. A deletion calls this while holding the snapshot's key
    /// mutex and, once deleting is done, calls `release_deletion`.
    pub(crate) fn claim_for_deletion(&self, directory: &Path) -> bool {
        let mut registry = lock(&self.registry);
        if registry.predecessors.contains_key(directory) {
            return false;
        }
        registry.deleting.insert(directory.to_path_buf());
        true
    }

    pub(crate) fn release_deletion(&self, directory: &Path) {
        lock(&self.registry).deleting.remove(directory);
    }

    /// Runs `work` while holding the mutex of the snapshot at `directory`.
    pub(crate) fn with_key_lock<T>(&self, directory: &Path, work: impl FnOnce() -> T) -> T {
        let mutex = lock(&self.key_locks)
            .entry(directory.to_path_buf())
            .or_default()
            .clone();
        let result = {
            let _held = lock(&mutex);
            work()
        };
        let mut locks = lock(&self.key_locks);
        // Only the map and this call hold the mutex: nobody else holds or
        // waits for it, and nobody can obtain it without the map's lock.
        if Arc::strong_count(&mutex) == 2 {
            locks.remove(directory);
        }
        result
    }

    /// Builds the snapshot of `key` into a temporary directory and publishes
    /// it at `directory`. On any failure the temporary directory is removed
    /// and nothing is published.
    fn create(
        &self,
        config: &Config,
        trees: &Trees,
        key: &SnapshotKey,
        tree_hash: &str,
        directory: &Path,
    ) -> io::Result<()> {
        let started = Instant::now();
        let parent = directory
            .parent()
            .ok_or_else(|| invalid("sibling snapshot has no parent directory"))?;
        fs::create_dir_all(parent)?;
        let temporary = temporary_path(directory);
        // The predecessor stays registered until publication, so a deletion
        // cannot remove it while its files are being copied.
        let predecessor = self.register_predecessor(key, parent);
        let source = predecessor.as_ref().map(|p| p.directory.clone());
        let result = self
            .build(config, trees, key, tree_hash, source.as_deref(), &temporary)
            .and_then(|cloned| publish(key, tree_hash, &temporary, directory).map(|()| cloned));
        drop(predecessor);
        match result {
            Ok(cloned) => {
                tracing::info!(
                    snapshot = %directory.display(),
                    predecessor = source.filter(|_| cloned).map(|p| p.display().to_string()),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "sibling snapshot created"
                );
                Ok(())
            }
            Err(error) => {
                if let Err(cleanup) = remove_tree(&temporary) {
                    tracing::warn!(path = %temporary.display(), error = %cleanup, "cannot remove failed sibling snapshot");
                }
                Err(error)
            }
        }
    }

    /// Fills `temporary` with the verified tree and restricts access to it.
    /// Returns whether it started from a copy of `predecessor`.
    fn build(
        &self,
        config: &Config,
        trees: &Trees,
        key: &SnapshotKey,
        tree_hash: &str,
        predecessor: Option<&Path>,
        temporary: &Path,
    ) -> io::Result<bool> {
        let mut cloned = false;
        if let Some(source) = predecessor {
            match self.clone_tree(source, temporary) {
                Ok(()) => cloned = true,
                Err(error) => {
                    // A copy is only a shortcut: a fresh clone of the mirror
                    // gives the same verified tree.
                    tracing::warn!(predecessor = %source.display(), %error, "cannot copy sibling snapshot predecessor; cloning the mirror instead");
                    remove_tree(temporary)?;
                }
            }
        }
        // On a copy this checks out only the paths that differ, so unchanged
        // files keep the predecessor's mtimes; otherwise it clones the mirror.
        trees.sync_workspace(
            &key.repository,
            temporary,
            &key.commit,
            tree_hash,
            &key.caller,
        )?;
        let computed = trees.tree_hash(temporary)?;
        if computed != tree_hash {
            let mismatch = Trees::tree_hash_mismatch(temporary, tree_hash, &computed);
            return Err(invalid(format!("sibling {}: {mismatch}", key.repository)));
        }
        install_javascript(config, key, tree_hash, temporary)?;
        remove_group_and_other_write(temporary)?;
        restrict_job_access(config, temporary)?;
        Ok(cloned)
    }

    /// Picks the most recently used ready snapshot of the repository that no
    /// deletion has claimed and registers it as a predecessor.
    fn register_predecessor(&self, key: &SnapshotKey, parent: &Path) -> Option<Predecessor<'_>> {
        let candidates = ready_snapshots(key, parent);
        let mut registry = lock(&self.registry);
        let directory = candidates
            .into_iter()
            .find(|candidate| !registry.deleting.contains(candidate))?;
        *registry.predecessors.entry(directory.clone()).or_default() += 1;
        Some(Predecessor {
            store: self,
            directory,
        })
    }

    fn clone_tree(&self, source: &Path, target: &Path) -> io::Result<()> {
        #[cfg(test)]
        if let Some(clone) = &self.clone_override {
            return clone(source, target);
        }
        #[cfg(target_os = "linux")]
        {
            crate::platform::clone_target(source, target)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (source, target);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "copying a sibling snapshot needs Linux reflinks",
            ))
        }
    }
}

fn existing(key: &SnapshotKey, directory: &Path) -> io::Result<Existing> {
    match fs::symlink_metadata(directory) {
        Ok(found) if found.is_dir() => Ok(match read_metadata(directory) {
            Ok(metadata) if metadata.describes(key) => Existing::Ready(metadata),
            _ => Existing::Unusable,
        }),
        Ok(_) => Ok(Existing::Unusable),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Existing::Absent),
        Err(error) => Err(error),
    }
}

/// Ready snapshots of the repository in `parent`, most recently used first.
/// Temporary directories and metadata files have a `.` in their names; final
/// snapshot names never do.
fn ready_snapshots(key: &SnapshotKey, parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut ready: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().contains('.'))
        .filter_map(|entry| {
            let path = entry.path();
            let found = fs::symlink_metadata(&path).ok()?;
            let metadata = read_metadata(&path).ok()?;
            (found.is_dir()
                && metadata.version == METADATA_VERSION
                && metadata.repository == key.repository)
                .then_some((found.modified().ok()?, path))
        })
        .collect();
    ready.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ready.into_iter().map(|(_, path)| path).collect()
}

fn temporary_path(directory: &Path) -> PathBuf {
    let mut name = directory.file_name().unwrap_or_default().to_os_string();
    name.push(format!("{TEMPORARY_MARKER}{}", Uuid::now_v7()));
    directory.with_file_name(name)
}

/// Writes and fsyncs the metadata beside the final name first, then renames
/// the temporary directory to it. A crash between the two leaves metadata
/// without a directory, which reads as absent; a published directory never
/// lacks metadata.
fn publish(
    key: &SnapshotKey,
    tree_hash: &str,
    temporary: &Path,
    directory: &Path,
) -> io::Result<()> {
    let parent = directory
        .parent()
        .ok_or_else(|| invalid("sibling snapshot has no parent directory"))?;
    atomic_json(&metadata_path(directory), &Metadata::new(key, tree_hash))?;
    crate::filesystem::rename(temporary, directory)?;
    crate::filesystem::sync_directory(parent)?;
    if let Err(error) = stamp(directory) {
        tracing::warn!(snapshot = %directory.display(), %error, "cannot stamp sibling snapshot use");
    }
    Ok(())
}

/// Removes an unusable entry at a snapshot's final name. It is renamed away
/// first, so a reader never sees a partly removed directory there.
fn discard(directory: &Path) -> io::Result<()> {
    let doomed = temporary_path(directory);
    crate::filesystem::rename(directory, &doomed)?;
    if let Err(error) = remove_tree(&doomed) {
        tracing::warn!(path = %doomed.display(), %error, "cannot remove unusable sibling snapshot");
    }
    Ok(())
}

fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(found) if found.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Sets the directory's mtime, its last-use time, from the wall clock.
fn stamp(directory: &Path) -> io::Result<()> {
    fs::File::open(directory)?.set_times(fs::FileTimes::new().set_modified(SystemTime::now()))
}

/// Clears the group and other write bits of every entry in the tree, without
/// following symbolic links. The read-only mount already stops jobs writing,
/// but the tree must not be writable on the host either, for example through
/// a 0666 file that a copy or a checkout produced.
#[cfg(unix)]
pub(crate) fn remove_group_and_other_write(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let found = fs::symlink_metadata(&path)?;
        if found.file_type().is_symlink() {
            continue;
        }
        let mode = found.permissions().mode() & 0o7777;
        if mode & 0o022 != 0 {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode & !0o022))?;
        }
        if found.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn remove_group_and_other_write(_root: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sibling snapshots need a Unix runner",
    ))
}

/// Installs the snapshot's JavaScript dependencies from its lockfiles, once,
/// as jobs would otherwise do for an in-place sibling on every setup. The
/// package managers run in helper units that show the snapshot at the
/// sibling's path, so the install sees exactly the tree jobs will see there.
fn install_javascript(
    config: &Config,
    key: &SnapshotKey,
    tree_hash: &str,
    temporary: &Path,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        crate::setup_log::phase("prepare sibling snapshot javascript dependencies", || {
            crate::isolation::prepare_snapshot_javascript(
                config,
                Path::new(&key.repository),
                temporary,
                tree_hash,
            )
        })
        .map_err(|error| {
            io::Error::new(error.kind(), format!("sibling {}: {error}", key.repository))
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Snapshot mode is never on outside isolated Linux runners, and
        // portable fixtures on other systems use no JavaScript.
        let _ = (config, key, tree_hash, temporary);
        Ok(())
    }
}

/// Gives the job account read-only access through ACLs and removes any ACL
/// entry that would let it write.
fn restrict_job_access(config: &Config, path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(access) = &config.sibling_snapshot_access {
        return access(path);
    }
    let _ = config;
    #[cfg(target_os = "linux")]
    {
        crate::isolation::restrict_sibling_snapshot_access(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "sibling snapshots need an isolated Linux runner",
        ))
    }
}

#[cfg(all(test, unix))]
#[path = "store_tests.rs"]
mod tests;
