//! Deleting sibling snapshots, and the in-place sibling checkouts that
//! runners without snapshots left behind.
//!
//! **References.** A snapshot is referenced while something may mount it or
//! is about to resolve it: a job that has not ended (or is starting or
//! finalizing), a workspace reservation, an admission or a Prepare that
//! declares its sibling at its commit. References are read from the runner's
//! state under the state lock, the way disk eviction protects workspaces, so
//! no separate count can drift. Job records are saved with their requests, so
//! a job recovered at startup references its snapshots again. Every snapshot
//! name starts with its commit, so the check runs no process and reads no
//! metadata: each snapshot of a sibling path whose name starts with a
//! referenced commit counts as referenced, whatever its caller commit and
//! JavaScript environment. A creation in progress protects its own snapshot
//! by holding the key mutex, and its predecessor through the store's claim.
//!
//! **Deletion** follows `SnapshotStore::delete`: holding the victim's key
//! mutex, it rechecks under the state lock that nothing references the
//! victim and claims it before releasing the state lock, then moves the
//! directory to the trash and removes its metadata. Lock order: disk, then
//! snapshot key, then state. No key mutex is taken while the state lock, a
//! job log lock, `ref_mutation` or another key mutex is held.
//!
//! **When.** At most `Config::sibling_snapshot_cap` unreferenced snapshots of
//! each repository are kept, the most recently used. The cap is enforced
//! after each setup or Prepare that resolved snapshots, at startup and at
//! every maintenance, so a snapshot that becomes unreferenced with no
//! creation after it is trimmed within one maintenance interval. Maintenance
//! also deletes unreferenced snapshots unused for
//! `Config::sibling_snapshot_idle`. Pressure eviction deletes unreferenced
//! snapshots before any other candidate (`disk.rs`), and retirement deletes
//! a repository's whole snapshot directory with its mirror
//! (`retirement.rs`).
//!
//! **Legacy sibling checkouts.** A runner with in-place siblings checked each
//! sibling out at its host path P and recorded it in
//! `Catalog::sibling_commits`. In snapshot mode nothing uses such a checkout
//! again once the jobs started with P bound into their units have ended.
//! Maintenance then discards a P that is not a workspace and drops its entry;
//! a P that is also a workspace keeps its directory and loses only the entry.
use super::*;
use crate::sibling_snapshots::{
    store::{self, Deletion},
    SiblingFences,
};

/// The commits that snapshots are referenced for, by repository directory
/// name (the sibling's mirror name).
#[derive(Default)]
pub(super) struct SnapshotReferences(BTreeMap<String, BTreeSet<String>>);

impl SnapshotReferences {
    /// Every sibling commit the runner's state declares in use. Takes no lock
    /// and spawns no process: the caller holds the state lock.
    pub(super) fn of(inner: &Inner) -> Self {
        let mut commits = BTreeMap::<String, BTreeSet<String>>::new();
        let mut add = |siblings: &[SiblingSnapshot]| {
            for sibling in siblings {
                commits
                    .entry(workspace::name(&sibling.canonical_path))
                    .or_default()
                    .insert(sibling.commit.clone());
            }
        };
        for job in inner.jobs.values() {
            if Self::live(inner, job) {
                if let Some(request) = &job.request {
                    add(&request.siblings);
                }
            }
        }
        for reservation in inner.reserved.values() {
            add(&reservation.siblings);
        }
        for request in inner.admitting.values() {
            add(&request.siblings);
        }
        for request in inner.preparing.values() {
            add(&request.siblings);
        }
        Self(commits)
    }

    /// A job that may still run or start: not ended, or still being started
    /// or finalized.
    fn live(inner: &Inner, job: &Job) -> bool {
        job.terminal.is_none()
            || inner.starting.contains(&job.id)
            || inner.finalizing.contains(&job.id)
    }

    /// Whether the snapshot at `directory`
    /// (`<root>/<repository>/<commit>-...`) is referenced. A path not shaped
    /// like that counts as referenced, so it is never deleted.
    pub(super) fn covers(&self, directory: &Path) -> bool {
        let (Some(repository), Some(name)) = (
            directory.parent().and_then(Path::file_name),
            directory.file_name(),
        ) else {
            return true;
        };
        let name = name.to_string_lossy();
        self.0
            .get(repository.to_string_lossy().as_ref())
            .is_some_and(|commits| {
                commits.iter().any(|commit| {
                    name.strip_prefix(commit.as_str())
                        .is_some_and(|rest| rest.starts_with('-'))
                })
            })
    }
}

/// Whether one path contains the other.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl Runner {
    /// Removes what a crash left in the snapshot root: the temporary
    /// directories of interrupted creations and removals. Runs at startup
    /// only, before anything can create a snapshot.
    pub(super) fn clean_sibling_snapshot_leftovers(&self) {
        if self.core.config.windows_instance() {
            return;
        }
        let root = self.core.config.sibling_snapshots_dir();
        let repositories = match store::repositories(&root) {
            Ok(repositories) => repositories,
            Err(error) => {
                tracing::warn!(root = %root.display(), %error, "cannot list sibling snapshot repositories");
                return;
            }
        };
        for repository in repositories {
            let temporaries = store::temporaries(&repository).unwrap_or_else(|error| {
                tracing::warn!(repository = %repository.display(), %error, "cannot list interrupted sibling snapshot work");
                Vec::new()
            });
            for temporary in temporaries {
                match self.discard(&temporary) {
                    Ok(()) => {
                        tracing::info!(path = %temporary.display(), "removed interrupted sibling snapshot work")
                    }
                    Err(error) => {
                        tracing::warn!(path = %temporary.display(), %error, "cannot remove interrupted sibling snapshot work")
                    }
                }
            }
        }
    }

    /// The sibling snapshot part of maintenance: metadata left by an
    /// interrupted deletion, the cap and idle expiry for every repository,
    /// then the legacy sibling checkouts. Runs without the state lock, job
    /// log locks or `ref_mutation`, and never while job records are
    /// quarantined, since references are read from them.
    pub(super) fn maintain_sibling_snapshots(&self) {
        if self.core.config.windows_instance() || !self.core.quarantined.is_empty() {
            return;
        }
        let root = self.core.config.sibling_snapshots_dir();
        let repositories = store::repositories(&root).unwrap_or_else(|error| {
            tracing::warn!(root = %root.display(), %error, "cannot list sibling snapshot repositories");
            Vec::new()
        });
        let idle_before = SystemTime::now().checked_sub(self.core.config.sibling_snapshot_idle);
        for repository in repositories {
            let orphaned = store::orphaned_metadata(&repository).unwrap_or_else(|error| {
                tracing::warn!(repository = %repository.display(), %error, "cannot list sibling snapshot metadata");
                Vec::new()
            });
            for directory in orphaned {
                if let Err(error) = self.core.snapshots.remove_orphaned_metadata(&directory) {
                    tracing::warn!(snapshot = %directory.display(), %error, "cannot remove metadata of a deleted sibling snapshot");
                }
            }
            self.trim_sibling_snapshots(&repository, idle_before);
        }
        self.clean_legacy_sibling_checkouts();
    }

    /// Enforces the cap on the repositories of `siblings`, after a setup or
    /// Prepare resolved their snapshots and released every key mutex. The
    /// caller holds no lock.
    pub(super) fn cap_sibling_snapshots(&self, siblings: &[SiblingSnapshot]) {
        if !self.core.quarantined.is_empty() {
            return;
        }
        let root = self.core.config.sibling_snapshots_dir();
        let repositories: BTreeSet<_> = siblings
            .iter()
            .map(|sibling| root.join(workspace::name(&sibling.canonical_path)))
            .collect();
        for repository in repositories {
            self.trim_sibling_snapshots(&repository, None);
        }
    }

    /// Deletes the unreferenced snapshots of one repository beyond the cap,
    /// least recently used first, and, given `idle_before`, every
    /// unreferenced one last used before it. Each deletion rechecks the
    /// references, so a snapshot referenced since it was picked stays.
    fn trim_sibling_snapshots(&self, repository: &Path, idle_before: Option<SystemTime>) {
        let listed = match store::list(repository) {
            Ok(listed) => listed,
            Err(error) => {
                tracing::warn!(repository = %repository.display(), %error, "cannot list sibling snapshots");
                return;
            }
        };
        if listed.is_empty() {
            return;
        }
        let cap = self.core.config.sibling_snapshot_cap;
        let victims: Vec<_> = {
            let inner = self.core.inner.lock().unwrap();
            let references = SnapshotReferences::of(&inner);
            listed
                .into_iter()
                .filter(|snapshot| !references.covers(&snapshot.directory))
                .enumerate()
                .filter(|(rank, snapshot)| {
                    *rank >= cap || idle_before.is_some_and(|before| snapshot.used < before)
                })
                .map(|(_, snapshot)| snapshot)
                .collect()
        };
        for victim in victims {
            match self.delete_sibling_snapshot(&victim.directory) {
                Ok(Some(aside)) => {
                    tracing::info!(snapshot = %victim.directory.display(), used = ?victim.used, "deleted unreferenced sibling snapshot");
                    self.purge(aside);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(snapshot = %victim.directory.display(), %error, "cannot delete sibling snapshot; maintenance retries")
                }
            }
        }
    }

    /// Deletes one snapshot by the deletion protocol. Returns the trash path
    /// its directory was moved to, for the caller to delete, or `None` when
    /// it was referenced again, is being copied by a creation, or was
    /// already gone. The caller holds no state lock, job log lock or snapshot
    /// key mutex.
    pub(super) fn delete_sibling_snapshot(&self, directory: &Path) -> io::Result<Option<PathBuf>> {
        // Between picking the victim and the recheck below; tests pause here.
        self.long_work("sibling_snapshot_selected");
        fs::create_dir_all(self.trash())?;
        let aside = self
            .trash()
            .join(format!("sibling-snapshot-{}", Uuid::now_v7()));
        let deletion = self.core.snapshots.delete(directory, &aside, |claim| {
            let inner = self.core.inner.lock().unwrap();
            if SnapshotReferences::of(&inner).covers(directory) {
                None
            } else {
                Some(claim())
            }
        })?;
        Ok((deletion == Deletion::Removed).then_some(aside))
    }

    /// Discards the legacy in-place sibling checkouts that nothing uses any
    /// more (see the module documentation). Only in snapshot mode: with
    /// in-place siblings these checkouts are live, and the Windows instance
    /// has none.
    fn clean_legacy_sibling_checkouts(&self) {
        let mut inner = self.core.inner.lock().unwrap();
        if inner.sibling_fences != SiblingFences::Legacy || inner.catalog.sibling_commits.is_empty()
        {
            return;
        }
        let paths: Vec<String> = inner.catalog.sibling_commits.keys().cloned().collect();
        let mut changed = false;
        for path in paths {
            if Self::legacy_checkout_in_use(&inner, &path) {
                continue;
            }
            if inner.catalog.workspaces.contains_key(&path) {
                // The repository's own main workspace: only the record that
                // it also held a sibling checkout goes.
                inner.catalog.sibling_commits.remove(&path);
                changed = true;
                continue;
            }
            if inner
                .catalog
                .workspaces
                .keys()
                .any(|key| overlaps(Path::new(key), Path::new(&path)))
            {
                // Discarding it would take a nested workspace with it.
                continue;
            }
            if let Err(error) = self.validate_destinations(&path, None, &[]) {
                tracing::warn!(path, %error, "legacy sibling checkout is not under an owned workspace root; leaving it in place");
                inner.catalog.sibling_commits.remove(&path);
                changed = true;
                continue;
            }
            match self.discard(Path::new(&path)) {
                Ok(()) => {
                    tracing::info!(path, "discarded unused legacy sibling checkout");
                    inner.catalog.sibling_commits.remove(&path);
                    changed = true;
                }
                Err(error) => {
                    tracing::warn!(path, %error, "cannot discard legacy sibling checkout; maintenance retries")
                }
            }
        }
        if changed {
            let catalog = self.catalog_snapshot(&inner);
            drop(inner);
            if let Err(error) = catalog.and_then(|catalog| self.write_catalog(catalog)) {
                tracing::error!(%error, "cannot persist legacy sibling checkout cleanup");
            }
        }
    }

    /// Whether anything may still use the legacy sibling checkout at `path`:
    /// a reservation, a Prepare, an admission or a drop of an overlapping
    /// workspace, a job that has not ended whose workspace overlaps it, or a
    /// running job started with its siblings bound from their host paths
    /// that declares an overlapping sibling.
    fn legacy_checkout_in_use(inner: &Inner, path: &str) -> bool {
        let near = |key: &str| !key.is_empty() && overlaps(Path::new(key), Path::new(path));
        inner.reserved.keys().any(|key| near(key))
            || inner.preparing.values().any(|r| near(&r.workspace_key))
            || inner.admitting.values().any(|r| near(&r.workspace_key))
            || inner.dropping.iter().any(|key| near(key))
            || inner.jobs.values().any(|job| {
                let Some(request) = &job.request else {
                    return false;
                };
                (SnapshotReferences::live(inner, job)
                    && (near(&request.workspace_key) || near(&request.cwd)))
                    || (Self::legacy_sibling_user(inner, job)
                        && Self::workspace_uses_sibling(path, &request.siblings))
            })
    }
}
