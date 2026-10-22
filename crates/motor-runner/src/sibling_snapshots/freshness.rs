//! Cargo freshness of sibling path dependencies across snapshot switches (the
//! switch touch).
//!
//! Cargo calls a path dependency fresh when none of its source files has an
//! mtime newer than the build that produced its artifact; it compares no
//! contents. A workspace W that built against one snapshot of a sibling and
//! then mounts another snapshot that already existed would see that tree's
//! differing files with mtimes older than its artifacts, call them fresh, and
//! link artifacts built from other code. Before such a setup's job starts, the
//! switch touch raises the mtime of those files in the requested snapshot.
//!
//! **Freshness record.** W's catalog entry keeps one record per sibling path
//! (`Workspace::sibling_freshness`). Each entry names a tree whose artifacts
//! may still be in W's `target/`: a verified `tree_hash`, or `unknown`, which
//! differs at every path. A closed entry carries its close time: the start of
//! the first completed setup or Prepare in W at a different tree. The current
//! tree's entry is open. A W with a catalog entry but no record for a sibling
//! (it was built with in-place siblings, or by an older runner that dropped
//! the field) starts from one open `unknown` entry. Entries leave the record
//! only with W's catalog entry, which goes when W and its `target/` are
//! deleted, except that a record over `Config::sibling_freshness_entries`
//! folds its oldest closed entries into one `unknown` entry carrying their
//! latest close time. Close times and touch timestamps come from the wall
//! clock, never from `Config::clock`, because they are compared with file
//! mtimes.
//!
//! **Touch rule.** A setup at snapshot S of tree T first closes the open entry
//! of any other tree at its own start time. It then touches a path p of S
//! only when some entry differs from T at p and that entry's close time is
//! not earlier than p's mtime. Every retained entry counts, not only the
//! previous one, because W's `target/` may hold artifacts of any of them. An
//! entry whose tree is missing from the mirror differs at every path. The
//! timestamp written is strictly greater than every close time compared, and
//! since only a path no newer than a close time is touched, no mtime is ever
//! lowered. Touched paths therefore end up newer than every close time, so a
//! repeated setup at S touches nothing. The rule depends only on mtimes, not
//! on how S was made. Inspection and writing for one snapshot happen under its
//! key mutex, so touches from different workspaces never interleave.
//!
//! The touch stays inside the snapshot: paths come from NUL-framed Git output,
//! paths absent from S (deleted from T) are skipped, ancestors are checked
//! without following symbolic links, a link is touched itself, and a link's
//! referent is touched only when it resolves inside S.
//!
//! **Contract for setup call sites.** A setup or Prepare of W:
//! 1. takes `Prior::of(&catalog, W)` under the state lock when it reserves W,
//!    and records `SystemTime::now()` as its start;
//! 2. resolves each sibling's snapshot with `SnapshotStore::resolve` while it
//!    holds W's reservation and no lock;
//! 3. calls `switch` with that snapshot directory and its verified tree, which
//!    touches the snapshot and returns a `Pending` record update;
//! 4. applies each `Pending` to the catalog under the state lock, after
//!    `note_workspace` has recorded W, only when the whole setup completed.
//!    A setup that fails or is refused before that point drops them and
//!    leaves the record unchanged. Once applied, a record stays correct even
//!    if saving the catalog then fails, because the touch it describes has
//!    already happened.
//!
//! `prepare_siblings` does steps 2 and 3 for every sibling of a request.

use super::{store::SnapshotStore, JavascriptEnvironment, SnapshotKey};
use crate::{
    engine::Config,
    storage::Catalog,
    workspace::{self, Trees},
};
use motor_protocol::SiblingSnapshot;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The default of `Config::sibling_freshness_entries`.
pub(crate) const DEFAULT_RECORD_ENTRIES: usize = 16;

/// One tree whose artifacts may still be in a workspace's `target/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// The verified tree hash; `None` is `unknown`, which differs from every
    /// tree at every path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tree: Option<String>,
    /// Wall-clock close time in nanoseconds since the Unix epoch; `None`
    /// while this is the workspace's current tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    closed_ns: Option<u64>,
}

/// One workspace's freshness record for one sibling.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Record {
    entries: Vec<Entry>,
}

/// A closed entry, as the touch compares it.
#[derive(Clone, Debug)]
struct Closed {
    tree: Option<String>,
    closed_ns: u64,
}

impl Record {
    /// Every entry other than `tree`'s, with an open one closed at
    /// `started_ns`. The requested tree's own entry is left out: its
    /// artifacts match the requested sources.
    fn closed_against(&self, tree: &str, started_ns: u64) -> Vec<Closed> {
        self.entries
            .iter()
            .filter(|entry| entry.tree.as_deref() != Some(tree))
            .map(|entry| Closed {
                tree: entry.tree.clone(),
                closed_ns: entry.closed_ns.unwrap_or(started_ns),
            })
            .collect()
    }

    /// The record after a completed setup at `tree`: the closed entries, with
    /// the oldest folded into one `unknown` entry beyond `limit` entries, and
    /// `tree` open.
    fn after(closed: Vec<Closed>, tree: &str, limit: usize) -> Self {
        let mut entries: Vec<Entry> = fold(closed, limit)
            .into_iter()
            .map(|closed| Entry {
                tree: closed.tree,
                closed_ns: Some(closed.closed_ns),
            })
            .collect();
        entries.push(Entry {
            tree: Some(tree.to_string()),
            closed_ns: None,
        });
        Self { entries }
    }
}

/// Keeps at most `limit - 1` closed entries beside the open one by merging
/// the oldest into one `unknown` entry closed at the latest of their close
/// times. That entry differs everywhere and is closed no earlier than any it
/// replaces, so it touches at least what they would have.
fn fold(mut closed: Vec<Closed>, limit: usize) -> Vec<Closed> {
    let keep = limit.max(2) - 1;
    if closed.len() <= keep {
        return closed;
    }
    closed.sort_by_key(|entry| entry.closed_ns);
    let folded = closed.len() - keep + 1;
    let latest = closed[..folded]
        .iter()
        .map(|entry| entry.closed_ns)
        .max()
        .unwrap_or_default();
    let mut kept = vec![Closed {
        tree: None,
        closed_ns: latest,
    }];
    kept.extend(closed.drain(folded..));
    kept
}

/// What a workspace's catalog entry said when its setup reserved it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Prior {
    /// Whether the workspace had a catalog entry, and so may have a
    /// `target/` built before any record was kept.
    catalogued: bool,
    records: BTreeMap<String, Record>,
}

impl Prior {
    pub(crate) fn of(catalog: &Catalog, workspace: &str) -> Self {
        match catalog.workspaces.get(workspace) {
            Some(entry) => Self {
                catalogued: true,
                records: entry.sibling_freshness.clone(),
            },
            None => Self::default(),
        }
    }

    fn record(&self, sibling: &str) -> Record {
        match self.records.get(sibling) {
            Some(record) => record.clone(),
            None if self.catalogued => Record {
                entries: vec![Entry {
                    tree: None,
                    closed_ns: None,
                }],
            },
            None => Record::default(),
        }
    }
}

/// A sibling's freshness record after a setup's touch, to be applied only if
/// that setup completes.
#[must_use = "a completed setup must apply its freshness update"]
#[derive(Debug)]
pub(crate) struct Pending {
    sibling: String,
    record: Record,
    touched: usize,
}

impl Pending {
    /// How many paths the touch raised the mtime of.
    pub(crate) fn touched(&self) -> usize {
        self.touched
    }

    /// Stores the record in the workspace's catalog entry. Returns false,
    /// changing nothing, when the workspace has no entry.
    pub(crate) fn apply(self, catalog: &mut Catalog, workspace: &str) -> bool {
        match catalog.workspaces.get_mut(workspace) {
            Some(entry) => {
                entry.sibling_freshness.insert(self.sibling, self.record);
                true
            }
            None => false,
        }
    }
}

/// Resolves or creates each sibling's snapshot and touches it for the
/// workspace whose catalog entry said `prior` when the setup started at
/// `started`. The caller holds the workspace's reservation and no lock, and
/// applies the returned updates only if the setup completes.
pub(crate) fn prepare_siblings(
    config: &Config,
    store: &SnapshotStore,
    trees: &Trees,
    prior: &Prior,
    started: SystemTime,
    siblings: &[SiblingSnapshot],
    environment: &JavascriptEnvironment,
) -> io::Result<Vec<Pending>> {
    siblings
        .iter()
        .map(|sibling| {
            let caller = sibling.base_commit.clone().unwrap_or_else(|| {
                workspace::resolve_caller_commit(
                    &trees.mirror(&sibling.canonical_path),
                    &sibling.commit,
                    &sibling.tree_hash,
                )
            });
            let key = SnapshotKey::new(
                &sibling.canonical_path,
                &sibling.commit,
                &caller,
                environment,
            )?;
            let snapshot = store.resolve(config, trees, &key, &sibling.tree_hash)?;
            switch(
                config,
                store,
                trees,
                prior,
                started,
                &sibling.canonical_path,
                &snapshot,
                &sibling.tree_hash,
            )
        })
        .collect()
}

/// Applies the touch rule to `snapshot`, the published snapshot of `sibling`
/// holding the verified `tree`, for a setup of the workspace whose catalog
/// entry said `prior` and that started at `started` (wall clock). Returns the
/// sibling's record as it stands once the setup completes. Takes the
/// snapshot's key mutex, so the caller must hold no state lock, job log lock
/// or snapshot key mutex.
#[allow(clippy::too_many_arguments)]
pub(crate) fn switch(
    config: &Config,
    store: &SnapshotStore,
    trees: &Trees,
    prior: &Prior,
    started: SystemTime,
    sibling: &str,
    snapshot: &Path,
    tree: &str,
) -> io::Result<Pending> {
    if !config.sibling_snapshot_mode() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "sibling snapshot mode is off: siblings are checked out in place",
        ));
    }
    let closed = prior.record(sibling).closed_against(tree, nanos(started));
    let touched = store.with_key_lock(snapshot, || {
        let plan = inspect(trees, sibling, snapshot, tree, &closed)?;
        #[cfg(test)]
        if let Some(inspected) = &store.touch_inspected {
            inspected(snapshot);
        }
        plan.write()
    })?;
    if touched > 0 {
        tracing::info!(sibling, snapshot = %snapshot.display(), touched, "touched sibling snapshot paths after a switch");
    }
    Ok(Pending {
        sibling: sibling.to_string(),
        record: Record::after(closed, tree, config.sibling_freshness_entries),
        touched,
    })
}

/// Nanoseconds since the Unix epoch; earlier times count as the epoch.
fn nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn time(nanos: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(nanos)
}

/// The paths a touch will raise, and the latest close time it compared.
struct Plan {
    paths: BTreeSet<PathBuf>,
    floor: SystemTime,
}

/// The paths of one closed entry that differ from the requested tree; `None`
/// is every path.
struct Differing {
    paths: Option<BTreeSet<Vec<u8>>>,
    closed: SystemTime,
}

impl Differing {
    fn at(&self, path: &[u8]) -> bool {
        self.paths.as_ref().is_none_or(|paths| paths.contains(path))
    }
}

fn inspect(
    trees: &Trees,
    sibling: &str,
    snapshot: &Path,
    tree: &str,
    closed: &[Closed],
) -> io::Result<Plan> {
    let Some(floor) = closed.iter().map(|entry| entry.closed_ns).max() else {
        return Ok(Plan {
            paths: BTreeSet::new(),
            floor: UNIX_EPOCH,
        });
    };
    let mirror = trees.mirror(sibling);
    let requested_known = has_tree(&mirror, tree);
    let mut differing = Vec::with_capacity(closed.len());
    for entry in closed {
        let paths = match &entry.tree {
            Some(old) if requested_known && has_tree(&mirror, old) => {
                Some(split(&workspace::git_bytes(
                    &mirror,
                    &[
                        "diff-tree",
                        "-r",
                        "-z",
                        "--no-renames",
                        "--name-only",
                        // Deleted paths are absent from the snapshot.
                        "--diff-filter=d",
                        old,
                        tree,
                    ],
                )?))
            }
            _ => None,
        };
        differing.push(Differing {
            paths,
            closed: time(entry.closed_ns),
        });
    }
    let candidates: BTreeSet<Vec<u8>> = if differing.iter().any(|d| d.paths.is_none()) {
        every_path(&mirror, requested_known, snapshot, tree)?
    } else {
        differing
            .iter()
            .flat_map(|d| d.paths.iter().flatten().cloned())
            .collect()
    };
    let root = fs::canonicalize(snapshot)?;
    let mut paths = BTreeSet::new();
    for candidate in candidates {
        let Some((path, found)) = inside(snapshot, &candidate)? else {
            continue;
        };
        let compared: Vec<SystemTime> = differing
            .iter()
            .filter(|d| d.at(&candidate))
            .map(|d| d.closed)
            .collect();
        let eligible = |mtime: SystemTime| compared.iter().any(|closed| *closed >= mtime);
        if eligible(found.modified()?) {
            paths.insert(path.clone());
        }
        if found.file_type().is_symlink() {
            // A changed link can make sources that did not change part of
            // the build, so its referent counts as changed too, but only a
            // referent inside the snapshot belongs to the runner.
            if let Ok(referent) = fs::canonicalize(&path) {
                if referent != root && referent.starts_with(&root) {
                    let found = fs::symlink_metadata(&referent)?;
                    if eligible(found.modified()?) {
                        paths.insert(referent);
                    }
                }
            }
        }
    }
    Ok(Plan {
        paths,
        floor: time(floor),
    })
}

impl Plan {
    /// Sets each path's mtime to just after the latest close time compared,
    /// without following links. Returns how many paths it changed.
    fn write(&self) -> io::Result<usize> {
        let stamp = self.floor + Duration::from_nanos(1);
        let mut touched = 0;
        for path in &self.paths {
            if fs::symlink_metadata(path)?.modified()? > self.floor {
                continue;
            }
            set_mtime(path, stamp)?;
            if fs::symlink_metadata(path)?.modified()? <= self.floor {
                // A filesystem with coarser timestamps rounded the stamp
                // down; two whole seconds later is above the floor on any of
                // them.
                let seconds = self
                    .floor
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                set_mtime(path, UNIX_EPOCH + Duration::from_secs(seconds + 2))?;
                if fs::symlink_metadata(path)?.modified()? <= self.floor {
                    return Err(io::Error::other(format!(
                        "cannot raise the mtime of sibling snapshot path {} above {:?}",
                        path.display(),
                        self.floor
                    )));
                }
            }
            touched += 1;
        }
        Ok(touched)
    }
}

fn has_tree(mirror: &Path, tree: &str) -> bool {
    super::full_object_id(tree)
        && workspace::git(mirror, &["cat-file", "-e", &format!("{tree}^{{tree}}")]).is_ok()
}

fn split(output: &[u8]) -> BTreeSet<Vec<u8>> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Every path of the requested tree, listed from the mirror, or, when the
/// mirror no longer has it, every entry of the snapshot outside its `.git`.
fn every_path(
    mirror: &Path,
    requested_known: bool,
    snapshot: &Path,
    tree: &str,
) -> io::Result<BTreeSet<Vec<u8>>> {
    if requested_known {
        return Ok(split(&workspace::git_bytes(
            mirror,
            &["ls-tree", "-r", "-z", "--name-only", "--full-tree", tree],
        )?));
    }
    let mut paths = BTreeSet::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(snapshot.join(&relative))? {
            let entry = entry?;
            if relative.as_os_str().is_empty() && entry.file_name() == ".git" {
                continue;
            }
            let path = relative.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                pending.push(path.clone());
            }
            paths.insert(path.into_os_string().into_encoded_bytes());
        }
    }
    Ok(paths)
}

/// The entry at `relative` inside `root` and its metadata, found without
/// following symbolic links; `None` when it does not exist, when any
/// ancestor is not a real directory, or when `relative` is not a plain
/// relative path.
fn inside(root: &Path, relative: &[u8]) -> io::Result<Option<(PathBuf, fs::Metadata)>> {
    let Some(relative) = os_path(relative) else {
        return Ok(None);
    };
    let mut path = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Ok(None);
        };
        path.push(name);
        let found = match fs::symlink_metadata(&path) {
            Ok(found) => found,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if components.peek().is_none() {
            return Ok(Some((path, found)));
        }
        if !found.is_dir() {
            return Ok(None);
        }
    }
    Ok(None)
}

/// A path from Git output or a snapshot walk. On Unix every byte string is a
/// path; elsewhere only UTF-8 is accepted.
#[cfg(unix)]
fn os_path(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn os_path(bytes: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Sets `path`'s mtime to `time` and leaves its access time, without
/// following a symbolic link at `path`.
#[cfg(unix)]
fn set_mtime(path: &Path, time: SystemTime) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let since = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| io::Error::other("touch time before the Unix epoch"))?;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("path contains a NUL byte"))?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: since.as_secs() as libc::time_t,
            tv_nsec: since.subsec_nanos() as _,
        },
    ];
    // SAFETY: `path` is a NUL-terminated string and `times` holds the two
    // entries utimensat reads; both outlive the call.
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn set_mtime(_path: &Path, _time: SystemTime) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sibling snapshots need a Unix runner",
    ))
}

#[cfg(all(test, unix))]
#[path = "freshness_tests.rs"]
mod tests;
