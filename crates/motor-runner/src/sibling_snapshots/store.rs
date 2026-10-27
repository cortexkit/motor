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
//! requested `tree_hash`, its Git directory is made independent of the
//! mirror, its JavaScript dependencies are installed from its
//! lockfiles, group and other write access is removed, and the
//! job account gets read-only access. Then its metadata, which records the
//! verified tree, is written and fsynced beside the directory, and a rename
//! publishes the directory under its final name. A final directory therefore
//! always has metadata unless a crash interrupted its removal, and a final
//! directory without readable metadata is removed and built again.
//!
//! Lock order: a setup takes a key mutex while holding no state lock, job log
//! lock or other key mutex. A deletion takes the runner's state lock inside
//! the victim's key mutex, after the disk lock when pressure eviction
//! deletes: disk, then snapshot key, then state. The registry mutex below is
//! only ever taken last and held briefly.

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
    /// The boot of the host in which the snapshot's files were last known
    /// to be whole: the boot that published it, or a later one that checked
    /// it again. Publication does not flush the snapshot's files to disk, so
    /// after a host crash a snapshot from an earlier boot may have lost file
    /// contents and is checked before it is used. `None` in metadata written
    /// before this field existed, which counts as an earlier boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) boot_id: Option<String>,
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
            boot_id: None,
        }
    }

    /// Whether the snapshot's files may have been lost in a host crash since
    /// they were last known whole: the metadata names a boot other than
    /// `boot`, the current one. When the current boot is unknown nothing can
    /// be told, and the snapshot is trusted as before.
    fn predates_boot(&self, boot: Option<&str>) -> bool {
        boot.is_some_and(|boot| self.boot_id.as_deref() != Some(boot))
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
    /// Called by `delete` once it has claimed a snapshot and before it moves
    /// the directory away, with the key mutex held.
    #[cfg(test)]
    pub(crate) deletion_claimed: Option<TouchHook>,
}

/// How one `SnapshotStore::delete` ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deletion {
    /// The directory now lives at the caller's `aside` path, and its
    /// metadata is gone.
    Removed,
    /// Nothing was at the snapshot's name; any metadata left there is gone.
    Absent,
    /// Something references the snapshot; nothing changed.
    Referenced,
    /// A creation is copying the snapshot as its predecessor; nothing
    /// changed.
    Copying,
}

/// A deletion's claim on a snapshot, released when the deletion ends,
/// whether it succeeded or not.
struct Claim<'a> {
    store: &'a SnapshotStore,
    directory: &'a Path,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.store.release_deletion(self.directory);
    }
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
        let boot = current_boot(config);
        self.with_key_lock(&directory, || {
            match existing(key, &directory)? {
                Existing::Ready(metadata) if metadata.tree_hash == tree_hash => {
                    if metadata.predates_boot(boot.as_deref()) {
                        // The first use since the host restarted: the files
                        // may not have reached the disk before it went down.
                        match verify_after_reboot(trees, key, &metadata, &directory) {
                            Ok(()) => atomic_json(
                                &metadata_path(&directory),
                                &Metadata {
                                    boot_id: boot.clone(),
                                    ..metadata
                                },
                            )?,
                            Err(error) => {
                                tracing::warn!(snapshot = %directory.display(), %error, "sibling snapshot from before the host restarted is damaged; recreating it");
                                discard(&directory)?;
                                return self.create(
                                    config,
                                    trees,
                                    key,
                                    tree_hash,
                                    &directory,
                                    boot.as_deref(),
                                );
                            }
                        }
                    }
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
            self.create(config, trees, key, tree_hash, &directory, boot.as_deref())
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

    /// Deletes the snapshot at `directory` by the deletion protocol, while
    /// holding its key mutex:
    ///
    /// 1. `claim_unreferenced` takes the runner's state lock, checks that
    ///    nothing references the snapshot, and, still holding the state lock,
    ///    calls the claim it is given and returns that claim's result. It
    ///    returns `None`, claiming nothing, when the snapshot is referenced.
    ///    The claim fails while a creation is copying the snapshot, and once
    ///    it succeeds no creation picks the snapshot as its predecessor.
    /// 2. The directory is renamed to `aside`, which takes it away from its
    ///    final name in one step, and then its metadata is removed. The
    ///    caller deletes `aside` afterwards, without any lock.
    ///
    /// A setup that needs the snapshot meanwhile waits for the key mutex,
    /// then finds no snapshot and creates it again. The caller must hold no
    /// state lock, job log lock or snapshot key mutex.
    pub(crate) fn delete(
        &self,
        directory: &Path,
        aside: &Path,
        claim_unreferenced: impl FnOnce(&dyn Fn() -> bool) -> Option<bool>,
    ) -> io::Result<Deletion> {
        self.with_key_lock(directory, || {
            match claim_unreferenced(&|| self.claim_for_deletion(directory)) {
                None => return Ok(Deletion::Referenced),
                Some(false) => return Ok(Deletion::Copying),
                Some(true) => {}
            }
            let _claim = Claim {
                store: self,
                directory,
            };
            #[cfg(test)]
            if let Some(claimed) = &self.deletion_claimed {
                claimed(directory);
            }
            remove_published(directory, aside)
        })
    }

    /// Removes the metadata file `<name>.json` when no directory `<name>`
    /// exists beside it, as a deletion interrupted between its two steps
    /// leaves it. The check and the removal happen under the snapshot's key
    /// mutex, so a creation publishing that snapshot is never undone.
    pub(crate) fn remove_orphaned_metadata(&self, directory: &Path) -> io::Result<bool> {
        self.with_key_lock(directory, || {
            if fs::symlink_metadata(directory).is_ok() {
                return Ok(false);
            }
            match fs::remove_file(metadata_path(directory)) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        })
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
        boot: Option<&str>,
    ) -> io::Result<()> {
        let started = Instant::now();
        let parent = directory
            .parent()
            .ok_or_else(|| invalid("sibling snapshot has no parent directory"))?;
        fs::create_dir_all(parent)?;
        let temporary = temporary_path(directory);
        // The predecessor stays registered until publication, so a deletion
        // cannot remove it while its files are being copied.
        let predecessor = self.register_predecessor(key, parent, boot);
        let source = predecessor.as_ref().map(|p| p.directory.clone());
        let result = self
            .build(config, trees, key, tree_hash, source.as_deref(), &temporary)
            .and_then(|cloned| {
                publish(key, tree_hash, boot, &temporary, directory).map(|()| cloned)
            });
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
            crate::caller_path::host(temporary),
            &key.commit,
            tree_hash,
            &key.caller,
        )?;
        let computed = trees.tree_hash(temporary)?;
        if computed != tree_hash {
            let mismatch = Trees::tree_hash_mismatch(temporary, tree_hash, &computed);
            return Err(invalid(format!("sibling {}: {mismatch}", key.repository)));
        }
        dissociate(temporary, tree_hash)?;
        install_javascript(config, key, tree_hash, temporary)?;
        remove_group_and_other_write(temporary)?;
        restrict_job_access(config, temporary)?;
        Ok(cloned)
    }

    /// Picks the most recently used ready snapshot of the repository that no
    /// deletion has claimed and registers it as a predecessor. A snapshot
    /// from an earlier boot that no setup has checked since is not copied:
    /// its files may be damaged.
    fn register_predecessor(
        &self,
        key: &SnapshotKey,
        parent: &Path,
        boot: Option<&str>,
    ) -> Option<Predecessor<'_>> {
        let candidates = ready_snapshots(key, parent, boot);
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

/// Ready snapshots of the repository in `parent` known whole in the current
/// `boot`, most recently used first. Temporary directories and metadata files
/// have a `.` in their names; final snapshot names never do.
fn ready_snapshots(key: &SnapshotKey, parent: &Path, boot: Option<&str>) -> Vec<PathBuf> {
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
                && metadata.repository == key.repository
                && !metadata.predates_boot(boot))
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
    boot: Option<&str>,
    temporary: &Path,
    directory: &Path,
) -> io::Result<()> {
    let parent = directory
        .parent()
        .ok_or_else(|| invalid("sibling snapshot has no parent directory"))?;
    let metadata = Metadata {
        boot_id: boot.map(str::to_string),
        ..Metadata::new(key, tree_hash)
    };
    atomic_json(&metadata_path(directory), &metadata)?;
    crate::filesystem::rename(temporary, directory)?;
    crate::filesystem::sync_directory(parent)?;
    if let Err(error) = stamp(directory) {
        tracing::warn!(snapshot = %directory.display(), %error, "cannot stamp sibling snapshot use");
    }
    Ok(())
}

/// The current boot of the host, or `None` where it cannot be read. The
/// kernel picks a new boot ID at every boot, so a daemon restart keeps it and
/// a host restart, including after a crash, changes it.
fn current_boot(config: &Config) -> Option<String> {
    #[cfg(test)]
    if let Some(boot) = &config.boot_id_override {
        return Some(boot.clone());
    }
    let _ = config;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(boot.trim().to_string()).filter(|boot| !boot.is_empty())
}

/// Checks, before its first use since the host restarted, a snapshot whose
/// files may not have reached the disk before the restart: its HEAD is still
/// the caller commit, `git fsck` finds its Git directory whole (every object
/// and the index), and the checkout still hashes to the tree it was verified
/// to hold. Untracked contents such as `node_modules` are not checked.
fn verify_after_reboot(
    trees: &Trees,
    key: &SnapshotKey,
    metadata: &Metadata,
    directory: &Path,
) -> io::Result<()> {
    let head = crate::workspace::git(directory, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if head != key.caller {
        return Err(invalid(format!(
            "HEAD is {head}, not the caller commit {}",
            key.caller
        )));
    }
    crate::workspace::git(directory, &["fsck", "--no-progress", "--no-dangling"])?;
    let computed = trees.tree_hash(directory)?;
    if computed != metadata.tree_hash {
        return Err(Trees::tree_hash_mismatch(
            directory,
            &metadata.tree_hash,
            &computed,
        ));
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

/// The second step of `SnapshotStore::delete`: the directory leaves its
/// final name before its metadata goes, so a crash in between leaves
/// metadata without a directory, which reads as absent.
fn remove_published(directory: &Path, aside: &Path) -> io::Result<Deletion> {
    let removed = match fs::symlink_metadata(directory) {
        Ok(_) => {
            crate::filesystem::rename(directory, aside)?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    match fs::remove_file(metadata_path(directory)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(parent) = directory.parent() {
        if let Err(error) = crate::filesystem::sync_directory(parent) {
            tracing::warn!(path = %parent.display(), %error, "cannot sync sibling snapshot directory after a deletion");
        }
    }
    Ok(if removed {
        Deletion::Removed
    } else {
        Deletion::Absent
    })
}

/// One entry at a snapshot's final name, as listed for removal.
#[derive(Clone, Debug)]
pub(crate) struct Listed {
    pub(crate) directory: PathBuf,
    /// The directory's mtime: when a setup last used the snapshot.
    pub(crate) used: SystemTime,
}

/// The repository directories under the snapshot root `root`.
pub(crate) fn repositories(root: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut repositories = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            repositories.push(entry.path());
        }
    }
    repositories.sort();
    Ok(repositories)
}

/// Every entry at a final snapshot name in one repository's directory, most
/// recently used first. Metadata files and temporary directories have a `.`
/// in their names and are left out; a final name never has one. An entry
/// without usable metadata is listed too: it takes disk space all the same,
/// and the next setup that needs it would remove it anyway.
pub(crate) fn list(repository: &Path) -> io::Result<Vec<Listed>> {
    let entries = match fs::read_dir(repository) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut listed = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().contains('.') {
            continue;
        }
        let found = match fs::symlink_metadata(entry.path()) {
            Ok(found) => found,
            // Removed since the directory was read.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        listed.push(Listed {
            directory: entry.path(),
            used: found.modified()?,
        });
    }
    listed.sort_by(|a, b| {
        b.used
            .cmp(&a.used)
            .then_with(|| a.directory.cmp(&b.directory))
    });
    Ok(listed)
}

/// The snapshot directories in one repository's directory whose metadata file
/// is all that is left of them, as `<name>` paths.
pub(crate) fn orphaned_metadata(repository: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(repository) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut orphaned = Vec::new();
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let Some(snapshot) = name.strip_suffix(".json") else {
            continue;
        };
        let directory = repository.join(snapshot);
        if !snapshot.contains('.') && fs::symlink_metadata(&directory).is_err() {
            orphaned.push(directory);
        }
    }
    Ok(orphaned)
}

/// The temporary entries (`<name>.tmp-<uuid>`) in one repository's
/// directory: creations and removals that a crash interrupted. Only safe to
/// remove while no creation can be running, at startup.
pub(crate) fn temporaries(repository: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(repository) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut temporaries = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .contains(TEMPORARY_MARKER)
        {
            temporaries.push(entry.path());
        }
    }
    Ok(temporaries)
}

/// Publishes an empty snapshot directory of `key` with metadata for
/// `tree_hash`, last used at `used`, for tests of what happens to published
/// snapshots.
#[cfg(test)]
pub(crate) fn publish_for_test(
    config: &Config,
    key: &SnapshotKey,
    tree_hash: &str,
    used: SystemTime,
) -> PathBuf {
    let directory = key.directory(config);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("source"), "snapshot\n").unwrap();
    atomic_json(&metadata_path(&directory), &Metadata::new(key, tree_hash)).unwrap();
    fs::File::open(&directory)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(used))
        .unwrap();
    directory
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

/// The ref that keeps a snapshot's verified tree reachable while `dissociate`
/// repacks. It exists only during the repack.
const DISSOCIATE_PIN_REF: &str = "refs/motor/snapshot-tree";

/// Copies every object the snapshot's Git directory borrows from the mirror
/// into its own pack and drops the link to the mirror. Jobs cannot read the
/// mirror, so Git inside a job needs the snapshot to stand alone. Both a
/// fresh clone (`clone --shared`) and a copied predecessor (re-linked by the
/// sync) borrow from the mirror at this point.
///
/// `tree_hash`, the tree the snapshot was verified to hold, is copied too.
/// `repack -a` copies only objects reachable from the snapshot's refs,
/// reflogs and index. For a sibling with uncommitted changes, HEAD and the
/// index name the caller commit, and the snapshot commit that holds
/// `tree_hash` is named by nothing in a snapshot copied from a predecessor
/// (a fresh clone still names it in HEAD's reflog). Without the temporary
/// ref, that tree and its changed blobs stayed in the mirror only, and every
/// later read of the tree in the snapshot, such as the JavaScript install's
/// `git ls-tree`, failed with Git's bare "not a tree object".
pub(crate) fn dissociate(temporary: &Path, tree_hash: &str) -> io::Result<()> {
    let alternates = temporary.join(".git/objects/info/alternates");
    if !alternates.is_file() {
        return Ok(());
    }
    crate::setup_log::phase("dissociate sibling snapshot", || {
        let git = |args: &[&str]| {
            crate::workspace::git(temporary, args).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "dissociate snapshot: `git {}`: {}",
                        args.join(" "),
                        error.to_string().trim_end()
                    ),
                )
            })
        };
        git(&["update-ref", DISSOCIATE_PIN_REF, tree_hash])?;
        let packed = git(&["repack", "-a", "-d", "-q"]);
        let unpinned = git(&["update-ref", "-d", DISSOCIATE_PIN_REF]);
        packed?;
        unpinned?;
        fs::remove_file(&alternates)?;
        // Refuse here, naming the tree, rather than leave a snapshot that
        // cannot read its own tree to fail in a later step.
        let present = format!("{tree_hash}^{{tree}}");
        match crate::workspace::git(temporary, &["cat-file", "-e", &present]) {
            Ok(_) => Ok(()),
            Err(error) => Err(invalid(format!(
                "dissociate snapshot: tree {tree_hash} is missing after `git repack -a -d -q` \
                 (`git cat-file -e {present}` failed: {})",
                error.to_string().trim_end()
            ))),
        }
    })
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
            io::Error::new(
                error.kind(),
                format!(
                    "sibling {}: prepare sibling snapshot javascript dependencies: {error}",
                    key.repository
                ),
            )
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
