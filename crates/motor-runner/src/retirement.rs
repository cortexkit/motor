//! Retiring repositories the runner has not used for `MIRROR_UNUSED_MS`.
//!
//! The runner keeps one git mirror per repository it has received, holding
//! that repository's history. A mirror no job has used for 30 days is
//! deleted, together with everything derived from that one repository that
//! would otherwise keep the mirror (or the repository's code) alive: its Head
//! workspaces, its sibling checkout, its warm-build record and fill checkout,
//! and its JavaScript install and manager caches. Shared caches not keyed to
//! one repository (the Cargo registry, the compile cache) are not touched.
use super::*;

/// How long a repository mirror may go unused before it is retired. The
/// public privacy page promises that a repository's git mirror is deleted
/// after 30 days in which no job used it.
pub(crate) const MIRROR_UNUSED_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// What the runner knows about one mirror's use while it decides.
#[derive(Default)]
struct Use {
    /// The latest use any dependent records.
    last_ms: u64,
    /// The first dependent that keeps the mirror whatever its age: something
    /// in flight, or a record that still needs the mirror's objects.
    blocked: Option<&'static str>,
    /// Repository roots that hash to this mirror (normally exactly one).
    repositories: BTreeSet<String>,
}

impl Runner {
    /// Retires every mirror unused for `MIRROR_UNUSED_MS` that nothing still
    /// depends on, and keeps the durable last-use clock (`mirror_used_ms` in
    /// the catalog) up to date for the rest.
    ///
    /// Use is read from the dependents' own timestamps (job acceptance and
    /// finish, workspace use, transfer finish, warm-build finish). Only work
    /// in flight right now (a prepare, a workspace setup, an admission) counts
    /// as use at the current time, and none of it survives a restart, so a
    /// restart neither resets the clock nor counts as use. A mirror first seen
    /// with no stamp at all (one fetched before this clock existed) is
    /// stamped with the current time.
    ///
    /// The whole pass holds `ref_mutation` and then the state lock, the order
    /// `prune_refs` uses. Admission, transfers, prepares and have-replies all
    /// need one of them, so a job arriving meanwhile waits and then finds
    /// either the repository whole or no mirror at all, and starts fresh.
    /// Each removal is a rename into trash with a background purge.
    pub(crate) fn retire_unused_repositories(&self) -> io::Result<()> {
        if !self.core.quarantined.is_empty() {
            return Ok(());
        }
        let _refs = self.core.ref_mutation.lock().unwrap();
        let mut inner = self.core.inner.lock().unwrap();
        let now = self.now();
        let mut uses = self.mirror_uses(&inner, now);
        let entries = match fs::read_dir(&self.core.trees.mirrors) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut present = BTreeSet::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            present.insert(entry.file_name().to_string_lossy().into_owned());
        }
        let mut changed = false;
        for name in &present {
            let observed = uses.get(name).map_or(0, |u| u.last_ms);
            let stamp = match inner.catalog.mirror_used_ms.get(name) {
                Some(stamp) => (*stamp).max(observed),
                None => now.max(observed),
            };
            if inner.catalog.mirror_used_ms.get(name) != Some(&stamp) {
                inner.catalog.mirror_used_ms.insert(name.clone(), stamp);
                changed = true;
            }
            let usage = uses.remove(name).unwrap_or_default();
            if usage.blocked.is_some() || now.saturating_sub(stamp) < MIRROR_UNUSED_MS {
                continue;
            }
            match self.retire_repository(&mut inner, name, &usage.repositories, stamp) {
                Ok(()) => changed = true,
                Err(error) => {
                    // What was already discarded is out of the catalog; the
                    // mirror stays until a later pass completes the rest.
                    changed = true;
                    tracing::warn!(mirror = %name, %error, "unused repository retirement failed; retrying at the next maintenance");
                }
            }
        }
        let before = inner.catalog.mirror_used_ms.len();
        inner
            .catalog
            .mirror_used_ms
            .retain(|name, _| present.contains(name));
        if changed || before != inner.catalog.mirror_used_ms.len() {
            // Written after the state lock is released, like every catalog
            // save that need not hold it.
            let catalog = self.catalog_snapshot(&inner)?;
            drop(inner);
            self.write_catalog(catalog)?;
        }
        Ok(())
    }

    /// Every record that uses or depends on a mirror, keyed by mirror name.
    fn mirror_uses(&self, inner: &Inner, now: u64) -> BTreeMap<String, Use> {
        let mut uses = BTreeMap::<String, Use>::new();
        let mut touch = |repository: &str, at: u64, blocked: Option<&'static str>| {
            let usage = uses.entry(workspace::name(repository)).or_default();
            usage.last_ms = usage.last_ms.max(at);
            usage.repositories.insert(repository.to_owned());
            if usage.blocked.is_none() {
                usage.blocked = blocked;
            }
        };
        let siblings = |siblings: &[SiblingSnapshot]| -> Vec<String> {
            siblings.iter().map(|s| s.canonical_path.clone()).collect()
        };
        // A retained job (finished within the 24-hour retention, or still
        // queued, fetching or running) may still read the mirror: a rerun,
        // a Windows staging copy, a warm build or a checkout of its siblings.
        for job in inner.jobs.values() {
            let Some(request) = &job.request else {
                continue;
            };
            let at = [Some(job.accepted_ms), job.started_ms, job.finished_ms]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or(0);
            for repository in
                std::iter::once(request.repository_root.clone()).chain(siblings(&request.siblings))
            {
                touch(&repository, at, Some("retained job"));
            }
        }
        // Checkouts are `git clone --shared`: their objects/info/alternates
        // points into the mirror. A Head workspace is retired with its
        // repository; any other workspace keeps the mirror until it expires.
        for (key, w) in &inner.catalog.workspaces {
            let blocked = if Self::deleting(inner, key, &[]) {
                Some("workspace being dropped")
            } else if w.kind == WorkspaceKind::Head {
                None
            } else {
                Some("workspace")
            };
            touch(&w.repository, w.used_ms, blocked);
            for sibling in siblings(&w.siblings) {
                touch(
                    &sibling,
                    w.used_ms,
                    if w.kind == WorkspaceKind::Head {
                        None
                    } else {
                        Some("workspace sibling")
                    },
                );
            }
        }
        for t in inner.catalog.transferred.values() {
            for repository in std::iter::once(t.repository.clone()).chain(siblings(&t.siblings)) {
                touch(&repository, t.finished_ms, Some("retained transfer"));
            }
        }
        for p in inner.preparing.values() {
            for repository in
                std::iter::once(p.repository_root.clone()).chain(siblings(&p.siblings))
            {
                touch(&repository, now, Some("prepare"));
            }
        }
        for r in inner.reserved.values() {
            for repository in std::iter::once(r.repository.clone()).chain(siblings(&r.siblings)) {
                touch(&repository, now, Some("workspace setup"));
            }
        }
        for r in inner.admitting.values() {
            for repository in
                std::iter::once(r.repository_root.clone()).chain(siblings(&r.siblings))
            {
                touch(&repository, now, Some("admission"));
            }
        }
        for checkout in inner.catalog.legacy_checkouts.values() {
            touch(&checkout.repository, 0, Some("legacy checkout"));
        }
        // A warm-build record retires with its repository unless a fill is
        // running; its unfinished base job is a retained job above.
        for (root, record) in &inner.catalog.repositories {
            let building = record.result.as_ref() == Some(&RebuildResult::Building);
            touch(
                root,
                record.finished_ms.unwrap_or(0),
                building.then_some("warm-build fill"),
            );
        }
        // A sibling checkout lives at its repository's own path and borrows
        // that repository's mirror objects; it retires with the repository.
        for path in inner.catalog.sibling_commits.keys() {
            touch(path, 0, None);
        }
        let mut block = |mirror: &Path, at: u64, reason: &'static str| {
            let Some(name) = mirror.file_name() else {
                return;
            };
            let usage = uses.entry(name.to_string_lossy().into_owned()).or_default();
            usage.last_ms = usage.last_ms.max(at);
            usage.blocked.get_or_insert(reason);
        };
        // Commits a have-reply reported are pinned until their bundle can
        // have arrived; the sender relies on the mirror holding them.
        for (mirror, commits) in self.core.held_offers.lock().unwrap().iter() {
            if let Some(expires) = commits.values().copied().filter(|e| *e > now).max() {
                block(mirror, expires.saturating_sub(HELD_OFFER_MS), "held refs");
            }
        }
        for mirror in self.core.trees.compacting.lock().unwrap().iter() {
            block(mirror, 0, "compaction");
        }
        uses
    }

    /// Discards one repository's derived state, then its mirror, each by
    /// rename into trash. A dependent that cannot be removed leaves the
    /// mirror in place: the dependent may still borrow its objects.
    fn retire_repository(
        &self,
        inner: &mut Inner,
        name: &str,
        repositories: &BTreeSet<String>,
        last_used_ms: u64,
    ) -> io::Result<()> {
        let mut removed = Vec::new();
        let mut discarded = BTreeSet::new();
        for repository in repositories {
            let heads: Vec<_> = inner
                .catalog
                .workspaces
                .iter()
                .filter(|(_, w)| w.kind == WorkspaceKind::Head && &w.repository == repository)
                .map(|(key, _)| key.clone())
                .collect();
            for key in heads {
                self.discard_dependent(&key, "head workspace", &mut discarded, &mut removed)?;
                inner.catalog.workspaces.remove(&key);
            }
            if inner.catalog.sibling_commits.contains_key(repository) {
                self.discard_dependent(
                    repository,
                    "sibling checkout",
                    &mut discarded,
                    &mut removed,
                )?;
                inner.catalog.sibling_commits.remove(repository);
            }
            if let Some(record) = inner.catalog.repositories.get(repository) {
                // The warm build's own checkout, derived as `queue_base` does.
                if let Some(parent) = record
                    .desired
                    .as_ref()
                    .and_then(|d| Path::new(&d.workspace).parent().map(Path::to_path_buf))
                {
                    let checkout = parent.join(format!(
                        ".motor-base-{}-{}",
                        self.core.config.instance,
                        workspace::name(repository)
                    ));
                    self.discard_dependent(
                        &checkout.to_string_lossy(),
                        "warm-build checkout",
                        &mut discarded,
                        &mut removed,
                    )?;
                }
                inner.catalog.repositories.remove(repository);
                removed.push(format!("warm-build record {repository}"));
            }
        }
        for kind in ["js-installs", "js-manager-caches"] {
            let root = self.core.trees.state.join(kind);
            let Ok(entries) = fs::read_dir(&root) else {
                continue;
            };
            for entry in entries {
                let path = entry?.path();
                let owned = fs::read_to_string(path.join(workspace::JAVASCRIPT_CACHE_OWNER))
                    .is_ok_and(|owner| owner.trim() == name);
                if !owned {
                    continue;
                }
                // A cache in use by a job right now is left to the disk
                // eviction; a job using it would also have kept the mirror.
                let Some(_lock) = disk::javascript_entry_lock(&self.core.trees.state, &path)?
                else {
                    continue;
                };
                self.discard(&path)?;
                removed.push(format!("{kind} {}", path.display()));
            }
        }
        let mirror = self.core.trees.mirrors.join(name);
        self.long_work("retire_mirror");
        self.discard(&mirror)?;
        removed.push(format!("mirror {}", mirror.display()));
        inner.catalog.mirror_used_ms.remove(name);
        self.core.held_offers.lock().unwrap().remove(&mirror);
        tracing::info!(
            repository = ?repositories,
            mirror = %name,
            last_used_ms,
            unused_days = self.now().saturating_sub(last_used_ms) / (24 * 60 * 60 * 1000),
            removed = ?removed,
            "retired unused repository"
        );
        Ok(())
    }

    /// Discards one runner-owned path that depends on a retiring mirror, once,
    /// after checking it lies under the owned workspace roots.
    fn discard_dependent(
        &self,
        path: &str,
        what: &str,
        discarded: &mut BTreeSet<String>,
        removed: &mut Vec<String>,
    ) -> io::Result<()> {
        if !discarded.insert(path.to_owned()) || fs::symlink_metadata(path).is_err() {
            return Ok(());
        }
        self.validate_destinations(path, None, &[])?;
        self.discard(Path::new(path))?;
        removed.push(format!("{what} {path}"));
        Ok(())
    }
}
