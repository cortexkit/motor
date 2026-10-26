use super::*;

pub(super) fn javascript_entry_lock(
    state: &Path,
    path: &Path,
) -> io::Result<Option<platform::JavascriptCacheLock>> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let key = name.split('.').next().unwrap_or_default();
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let lock = platform::javascript_cache_lock(state, key)?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(Some(lock)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

struct Candidate {
    path: PathBuf,
    workspace: Option<String>,
    last_used_ms: u64,
    cache_disk: bool,
    /// An unreferenced sibling snapshot, deleted by the snapshot deletion
    /// protocol rather than removed in place.
    snapshot: bool,
}

// Caches used in the last half hour are likely to be reused; deleting them costs a cold rebuild.
const SOFT_PRESSURE_MIN_AGE_MS: u64 = 30 * 60 * 1000;

/// Protects a Run request's workspace, its canonical aliases, its siblings and
/// its git-dependency mirrors from disk eviction while the request is being
/// admitted. Eviction skips these paths exactly as it skips a running job's.
/// Once the job is inserted into the job table, the job's own protection covers
/// them, and dropping the guard releases this one, including on error and
/// duplicate returns.
pub(super) struct AdmissionGuard {
    runner: Runner,
    token: Uuid,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.runner
            .core
            .inner
            .lock()
            .unwrap()
            .admitting
            .remove(&self.token);
    }
}

/// A connection keeps uploads and preparations protected until fetch/setup ends,
/// including when its job was cancelled during the upload.
pub(crate) struct TransferGuard {
    runner: Runner,
    id: Uuid,
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        let mut inner = self.runner.core.inner.lock().unwrap();
        inner.transfers.remove(&self.id);
        inner.preparing.remove(&self.id);
        if let Some(transfer) = inner.catalog.transferred.get_mut(&self.id) {
            transfer.finished_ms = self.runner.now();
            let catalog = self.runner.catalog_snapshot(&inner);
            drop(inner);
            if let Err(error) = catalog.and_then(|catalog| self.runner.write_catalog(catalog)) {
                tracing::error!(%error, "cannot persist completed transfer retention");
            }
        }
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl Config {
    pub(crate) fn boat_workspace_trash_roots(&self) -> Vec<PathBuf> {
        if !self.boat_profile {
            return Vec::new();
        }
        self.owned_workspace_roots
            .iter()
            .map(|root| root.join(format!(".motor-boat-trash-{}", self.instance)))
            .collect()
    }
}

/// Count allocated inode blocks physically, without following symlinks or
/// crossing mounts. Shared reflink extents are counted per inode, not reclaimed
/// bytes; only the free-space probe can establish that deletion met the floor.
fn allocated_bytes(path: &Path, device: u64) -> io::Result<u64> {
    let mut pending = vec![path.to_path_buf()];
    let mut inodes = BTreeSet::new();
    let mut bytes: u64 = 0;
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        let (dev, ino, allocated) = crate::filesystem::identity(&path)?;
        if dev != device {
            return Err(invalid("pressure cleanup cannot cross a mount"));
        }
        if inodes.insert((dev, ino)) {
            bytes = bytes.saturating_add(allocated);
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(bytes)
}

impl Runner {
    // Only profiles with a disk reserve need this barrier. It is taken before
    // inner so waiting for allocation never prevents status or cancel reads.
    pub(super) fn reserve_restore_barrier(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        (self.core.config.disk_reserve_bytes != 0)
            .then(|| self.core.reserve_restore.lock().unwrap())
    }

    pub(super) fn protect_admission(&self, inner: &mut Inner, request: &Run) -> AdmissionGuard {
        let token = Uuid::now_v7();
        inner.admitting.insert(token, request.clone());
        AdmissionGuard {
            runner: self.clone(),
            token,
        }
    }

    pub(crate) fn transfer(&self, id: Uuid) -> TransferGuard {
        let _restore = self.reserve_restore_barrier();
        self.core.inner.lock().unwrap().transfers.insert(id);
        TransferGuard {
            runner: self.clone(),
            id,
        }
    }

    /// Reserve a prepare before acknowledging/uploading it, not just once its
    /// checkout begins. Otherwise pressure cleanup can remove its spool or tree.
    pub(crate) fn admit_prepare(&self, r: &Prepare) -> io::Result<Result<TransferGuard, Refusal>> {
        if let Err(error) = self.allow_prepare() {
            return Ok(Err(Refusal {
                job_id: None,
                transfer_id: Some(r.transfer_id),
                reason: RefusalReason::RequestRejected,
                retry_after_ms: None,
                disk_full: None,
                detail: Some(self.safe_refusal_detail(
                    format!("prepare_unsupported: {error}"),
                    std::iter::empty(),
                )),
            }));
        }
        workspace::validate_request(
            crate::caller_path::request(r),
            &r.siblings,
            self.core.config.sibling_snapshot_mode(),
        )?;
        let refusal = |reason: RefusalReason, disk_full, detail: String| Refusal {
            retry_after_ms: if reason == RefusalReason::RunnerDraining {
                Some(self.drain_retry_after_ms())
            } else {
                None
            },
            job_id: None,
            transfer_id: Some(r.transfer_id),
            reason,
            disk_full,
            detail: Some(self.safe_refusal_detail(detail, std::iter::empty::<String>())),
        };
        if let Some(detail) = nested_workspace_detail(
            &self.core.inner.lock().unwrap(),
            crate::caller_path::request(r),
            &r.siblings,
        ) {
            return Ok(Err(refusal(
                RefusalReason::WorkspaceSetupFailed,
                None,
                detail,
            )));
        }
        if let Err(error) = self.validate_writer_paths(
            crate::caller_path::request(r).host_workspace,
            None,
            &r.siblings,
        ) {
            return Ok(Err(refusal(
                RefusalReason::WorkspaceKeyRejected,
                None,
                error.to_string(),
            )));
        }
        if workspace::validate_dependencies(&r.git_dependencies).is_err() {
            return Ok(Err(refusal(
                RefusalReason::BundleRejected,
                None,
                "Git dependency bundles were rejected".into(),
            )));
        }
        if self.is_draining() {
            return Ok(Err(refusal(
                RefusalReason::RunnerDraining,
                None,
                "runner_draining".into(),
            )));
        }
        let _restore = self.reserve_restore_barrier();
        if let Some(full) = self.ensure_disk_space()? {
            return Ok(Err(refusal(
                RefusalReason::RunnerDiskFull,
                Some(full.clone()),
                full.diagnostic(),
            )));
        }
        let mut inner = self.core.inner.lock().unwrap();
        if let Some(detail) =
            nested_workspace_detail(&inner, crate::caller_path::request(r), &r.siblings)
        {
            return Ok(Err(refusal(
                RefusalReason::WorkspaceSetupFailed,
                None,
                detail,
            )));
        }
        if inner.drain_deadline.is_some() {
            return Ok(Err(refusal(
                RefusalReason::RunnerDraining,
                None,
                "runner_draining".into(),
            )));
        }
        if Self::deleting(
            &inner,
            crate::caller_path::request(r).host_workspace,
            &r.siblings,
        ) || self.dependencies_evicting(&inner, &r.git_dependencies)
            || inner.transfers.contains(&r.transfer_id)
        {
            return Ok(Err(refusal(
                RefusalReason::WorkspaceSetupFailed,
                None,
                "workspace, sibling, or transfer is already being prepared or removed".into(),
            )));
        }
        inner.preparing.insert(r.transfer_id, r.clone());
        inner.transfers.insert(r.transfer_id);
        Ok(Ok(TransferGuard {
            runner: self.clone(),
            id: r.transfer_id,
        }))
    }

    pub fn runner_status(&self) -> RunnerStatusReply {
        let now = self.now();
        // Keep retained job history out of the state-lock path of each probe.
        let (status, permits_held, fill_permits) = {
            let inner = self.core.inner.lock().unwrap();
            let usage = self.core.repository_usage.lock().unwrap();
            let active = &usage.active;
            let queue_depth = active.values().filter(|(_, pending)| *pending).count() as u32;
            let running = active
                .values()
                .filter(|(_, pending)| !pending)
                .map(|(job, _)| job.clone())
                .collect();
            let permits = active
                .iter()
                .filter(|(id, (_, pending))| !pending || inner.starting.contains(id))
                .map(|(_, (job, _))| job.weight)
                .sum();
            // Only a tenant VM reports its fill: its host counts these permits
            // as claimed, so the broker places new jobs elsewhere meanwhile.
            let fill_permits = self.core.config.tenant_vm.then(|| {
                inner
                    .writer
                    .and_then(|id| inner.jobs.get(&id))
                    .filter(|job| job.terminal.is_none())
                    .map_or(0, |job| job.weight)
            });
            (
                self.status_with_jobs(&inner, queue_depth, running),
                permits,
                fill_permits,
            )
        };
        let (repository_usage, underused_reservations) = {
            let mut usage = self.core.repository_usage.lock().unwrap();
            (
                usage.snapshot(now),
                usage.underused.values().cloned().collect(),
            )
        };
        let (load_1m, threads, mem_available_bytes) = health::host_sample(
            &fs::read_to_string("/proc/loadavg").unwrap_or_default(),
            &fs::read_to_string("/proc/cpuinfo").unwrap_or_default(),
            &fs::read_to_string("/proc/meminfo").unwrap_or_default(),
        );
        let free_bytes = match (self.core.config.free_space_probe)(&self.core.trees.state) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                tracing::warn!(%error, "runner free-space probe failed");
                None
            }
        };
        #[cfg(any(target_os = "linux", test))]
        let failed_job_units = {
            let result = self.core.backend.failed_job_units(&self.core.config);
            match result {
                Ok(count) => Some(count),
                Err(error) => {
                    tracing::warn!(%error, "runner failed-job-unit query failed");
                    None
                }
            }
        };
        #[cfg(not(any(target_os = "linux", test)))]
        let failed_job_units = None;
        let queue_depth = status.queue_depth;
        let running = status
            .running_jobs
            .iter()
            .map(|job| HealthRunningJob {
                job_id: job.job_id,
                weight: job.weight,
            })
            .collect();
        let health = {
            // VM backends do not participate in the Linux compile cache.
            let remote_cache =
                (!self.core.config.vm_instance()).then(|| self.remote_cache_health(now));
            let mut counters = self.core.health.lock().unwrap();
            RunnerHealth {
                runner_version: env!("CARGO_PKG_VERSION").into(),
                since_ms: counters.since_ms,
                window_minutes: health::WINDOW_MINUTES,
                free_bytes,
                queue_depth,
                running,
                failed_job_units,
                cleanup_blocked_by_quarantine: self
                    .core
                    .quarantine_cleanup
                    .lock()
                    .unwrap()
                    .blocked
                    .clone(),
                counts: counters.counts(now),
                underused_reservations,
                repository_usage,
                load_1m,
                threads,
                mem_available_bytes,
                permits_held: Some(permits_held),
                permits_cap: Some(self.core.config.max_weight),
                fill_permits,
                queue_wait_1h: Some(counters.queue_wait(now)),
                windows_guest_agent: self.core.config.windows_runtime.guest_agent_readiness(),
                remote_cache,
            }
        };
        RunnerStatusReply {
            status,
            disk: Some(RunnerDiskStatus {
                free_bytes,
                floor_bytes: self.core.config.free_space_floor_bytes,
                last_eviction: self.core.inner.lock().unwrap().last_eviction.clone(),
            }),
            health: Some(health),
        }
    }

    fn disk_candidates(&self, inner: &Inner) -> io::Result<Vec<Candidate>> {
        if self.core.config.windows_instance() {
            return self.windows_disk_candidates(inner);
        }
        let mut dependency_roots = BTreeSet::new();
        let mut protected = vec![
            self.core.trees.state.clone(),
            self.core.config.build_slot_root.clone(),
        ];
        // Source/sibling paths remain protected for task cleanup. A recorded head
        // owns its checkout in both roles, so only active ownership protects it.
        let mut passive: Vec<_> = crate::caller_path::keys(&inner.catalog.repositories)
            .chain(crate::caller_path::keys(&inner.catalog.sibling_commits))
            .map(|path| path.host.as_path().to_path_buf())
            .collect();
        passive.extend(passive.clone().iter().filter_map(|p| p.canonicalize().ok()));
        for job in inner.jobs.values() {
            if let Some(r) = &job.request {
                if job.base {
                    protected.push(crate::caller_path::run(r).host_cwd.as_path().to_path_buf());
                }
                if job.terminal.is_none() || inner.starting.contains(&job.id) {
                    if !job.base {
                        protected.push(
                            crate::caller_path::request(r)
                                .host_workspace
                                .as_path()
                                .to_path_buf(),
                        );
                    }
                    protected.extend(
                        r.siblings
                            .iter()
                            .map(|s| crate::caller_path::sibling(s).host.as_path().to_path_buf()),
                    );
                }
                if job.terminal.is_none()
                    || inner.starting.contains(&job.id)
                    || inner.transfers.contains(&job.id)
                {
                    dependency_roots.extend(
                        r.git_dependencies
                            .iter()
                            .map(|d| self.core.trees.dependency_mirror(&d.url)),
                    );
                }
            }
        }
        for (key, r) in crate::caller_path::entries(&inner.reserved) {
            if workspace::absolute(key.caller.text()) {
                protected.push(key.host.as_path().to_path_buf());
            }
            protected.extend(
                r.siblings
                    .iter()
                    .map(|s| crate::caller_path::sibling(s).host.as_path().to_path_buf()),
            );
        }
        for r in inner.admitting.values() {
            protected.push(
                crate::caller_path::request(r)
                    .host_workspace
                    .as_path()
                    .to_path_buf(),
            );
            protected.extend(
                r.siblings
                    .iter()
                    .map(|s| crate::caller_path::sibling(s).host.as_path().to_path_buf()),
            );
            dependency_roots.extend(
                r.git_dependencies
                    .iter()
                    .map(|d| self.core.trees.dependency_mirror(&d.url)),
            );
        }
        for r in inner.preparing.values() {
            dependency_roots.extend(
                r.git_dependencies
                    .iter()
                    .map(|d| self.core.trees.dependency_mirror(&d.url)),
            );
            protected.push(
                crate::caller_path::request(r)
                    .host_workspace
                    .as_path()
                    .to_path_buf(),
            );
            protected.extend(
                r.siblings
                    .iter()
                    .map(|s| crate::caller_path::sibling(s).host.as_path().to_path_buf()),
            );
        }
        protected.extend(inner.dropping.iter().map(|path| {
            crate::caller_path::identity(path)
                .host
                .as_path()
                .to_path_buf()
        }));
        // Include canonical aliases: an allowed root cannot turn a symlink into
        // permission to delete a protected path under another spelling.
        protected.extend(
            protected
                .clone()
                .iter()
                .filter_map(|p| p.canonicalize().ok()),
        );
        let active_protected = protected.clone();
        protected.extend(self.core.config.boat_workspace_trash_roots());
        let roots: Vec<_> = self
            .core
            .config
            .workspace_roots
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();
        let mut candidates = Vec::new();
        for (key, workspace) in crate::caller_path::entries(&inner.catalog.workspaces) {
            let path = key.host.as_path().to_path_buf();
            let Ok(canonical) = path.canonicalize() else {
                continue;
            };
            if !workspace::absolute(key.caller.text())
                || self.validate_destinations(key.host, None, &[]).is_err()
                || path.symlink_metadata()?.file_type().is_symlink()
                || (workspace.kind != WorkspaceKind::Head
                    && (!roots
                        .iter()
                        .any(|root| canonical != *root && canonical.starts_with(root))
                        || passive
                            .iter()
                            .any(|p| overlaps(&path, p) || overlaps(&canonical, p))))
                || protected
                    .iter()
                    .any(|p| overlaps(&path, p) || overlaps(&canonical, p))
                || path
                    .file_name()
                    .is_some_and(|s| s.to_string_lossy().starts_with(".motor-base-"))
            {
                continue;
            }
            candidates.push(Candidate {
                path,
                workspace: Some(key.caller.text().clone()),
                last_used_ms: workspace.used_ms,
                cache_disk: false,
                snapshot: false,
            });
        }
        // These are the runner's actual upload, deletion and launch staging
        // roots. Never scan /srv/build or infer ownership from directory names.
        let state_roots = [
            "spools",
            "trash",
            "sandboxes",
            "boat-quarantine",
            "git-deps",
            "js-installs",
            "js-manager-caches",
        ]
        .into_iter()
        .map(|kind| (kind, self.core.trees.state.join(kind)));
        let workspace_trash = self
            .core
            .config
            .boat_workspace_trash_roots()
            .into_iter()
            .map(|root| ("boat-workspace-trash", root));
        for (kind, root) in state_roots.chain(workspace_trash) {
            if kind == "boat-quarantine" && !self.core.config.boat_profile {
                continue;
            }
            let Ok(entries) = fs::read_dir(&root) else {
                continue;
            };
            #[cfg(target_os = "linux")]
            if kind == "boat-workspace-trash" {
                Self::validate_boat_workspace_trash(&root)?;
            }
            if fs::symlink_metadata(&root)?.file_type().is_symlink() {
                continue;
            }
            let canonical_root = root.canonicalize()?;
            let canonical_state = self.core.trees.state.canonicalize()?;
            if kind != "boat-workspace-trash" && !canonical_root.starts_with(&canonical_state) {
                continue;
            }
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Err(error)
                        if matches!(kind, "js-installs" | "js-manager-caches")
                            && error.kind() == io::ErrorKind::NotFound =>
                    {
                        continue
                    }
                    result => result?,
                };
                if !metadata.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let live = if matches!(kind, "js-installs" | "js-manager-caches") {
                    javascript_entry_lock(&self.core.trees.state, &path)?.is_none()
                } else if kind == "git-deps" {
                    dependency_roots.contains(&path) || inner.evicting_dependencies.contains(&path)
                } else if kind == "boat-workspace-trash" {
                    active_protected.iter().any(|owned| overlaps(&path, owned))
                } else if matches!(kind, "trash" | "boat-quarantine") {
                    false
                } else {
                    name.parse::<Uuid>().ok().is_some_and(|id| {
                        inner.transfers.contains(&id)
                            || inner.starting.contains(&id)
                            || inner.jobs.get(&id).is_some_and(|j| j.terminal.is_none())
                    })
                };
                if !live {
                    let last_used_ms = metadata
                        .modified()?
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    candidates.push(Candidate {
                        path,
                        workspace: None,
                        last_used_ms,
                        cache_disk: false,
                        snapshot: false,
                    });
                }
            }
        }
        let fleet = self.core.config.sccache_dir();
        if inner.writer.is_none() && !inner.evicting_caches.contains(&fleet) {
            if let Ok(metadata) = fs::symlink_metadata(&fleet) {
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    // Jobs read the cache through a read-only mount, so the
                    // directory's own time shows only when the writer last
                    // added a top-level entry. Its last use is the latest of
                    // that, the last job to finish, and now while any job is
                    // admitted, queued or running: each of those reads it.
                    let modified_ms = metadata
                        .modified()?
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let in_flight = !inner.admitting.is_empty()
                        || inner.jobs.values().any(|job| job.terminal.is_none());
                    let last_used_ms = if in_flight {
                        self.now()
                    } else {
                        modified_ms.max(inner.catalog.compile_cache_used_ms.unwrap_or(0))
                    };
                    candidates.push(Candidate {
                        path: fleet,
                        workspace: None,
                        cache_disk: false,
                        snapshot: false,
                        last_used_ms,
                    });
                }
            }
        }
        candidates.sort_by(|a, b| (a.last_used_ms, &a.path).cmp(&(b.last_used_ms, &b.path)));
        // Unreferenced sibling snapshots go first, least recently used
        // first: a snapshot is rebuilt from the mirror in one setup, while a
        // workspace loses its build target.
        let mut snapshots = self.snapshot_disk_candidates(inner);
        snapshots.extend(candidates);
        Ok(snapshots)
    }

    /// Every sibling snapshot that nothing references, least recently used
    /// first. A listing failure leaves snapshots out rather than stopping
    /// the rest of the pass.
    fn snapshot_disk_candidates(&self, inner: &Inner) -> Vec<Candidate> {
        let references = super::sibling_eviction::SnapshotReferences::of(inner);
        let root = self.core.config.sibling_snapshots_dir();
        let listed =
            crate::sibling_snapshots::store::repositories(&root).and_then(|repositories| {
                let mut listed = Vec::new();
                for repository in repositories {
                    listed.extend(crate::sibling_snapshots::store::list(&repository)?);
                }
                Ok(listed)
            });
        let listed = listed.unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot list sibling snapshots for disk eviction");
            Vec::new()
        });
        let mut candidates: Vec<_> = listed
            .into_iter()
            .filter(|snapshot| !references.covers(&snapshot.directory))
            .map(|snapshot| Candidate {
                last_used_ms: snapshot
                    .used
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                path: snapshot.directory,
                workspace: None,
                cache_disk: false,
                snapshot: true,
            })
            .collect();
        candidates.sort_by(|a, b| (a.last_used_ms, &a.path).cmp(&(b.last_used_ms, &b.path)));
        candidates
    }

    /// Deletes an unreferenced sibling snapshot for pressure eviction, by the
    /// snapshot deletion protocol, and frees its space before returning, so
    /// the next free-space probe sees it. Returns the bytes it held, or
    /// `None` when the recheck found it referenced or being copied.
    fn evict_sibling_snapshot(&self, directory: &Path, device: u64) -> io::Result<Option<u64>> {
        let Some(aside) = self.delete_sibling_snapshot(directory)? else {
            return Ok(None);
        };
        self.long_work("disk_remove");
        let bytes = allocated_bytes(&aside, device);
        #[cfg(target_os = "linux")]
        if self.core.config.isolate_jobs {
            crate::isolation::restore_runner_access(&aside)?;
        }
        fs::remove_dir_all(&aside)?;
        bytes.map(Some)
    }

    /// Records the outcome of one pressure deletion: the eviction and its
    /// catalog changes, or a failure, which holds the path back from
    /// eviction for a while. A failed deletion of a path that no longer
    /// exists is not a failure. `cache_cleared` says the shared compiler
    /// cache was moved away from its path, which empties it even when a
    /// later step of its deletion failed.
    fn record_disk_removal(
        &self,
        candidate: &Candidate,
        deleted: io::Result<u64>,
        cache_cleared: bool,
    ) {
        let disappeared = deleted.is_err()
            && matches!(
                fs::symlink_metadata(&candidate.path),
                Err(error) if error.kind() == io::ErrorKind::NotFound
            );
        let mut inner = self.core.inner.lock().unwrap();
        let mut catalog = None;
        inner.evicting_dependencies.remove(&candidate.path);
        inner.evicting_caches.remove(&candidate.path);
        if let Some(key) = &candidate.workspace {
            inner.dropping.remove(key);
        }
        // The fill records describe the cache's contents, so they go with
        // them, also when the deletion failed after the cache had left.
        if cache_cleared || (deleted.is_ok() && candidate.path == self.core.config.sccache_dir()) {
            inner.catalog.forget_compiler_cache_fills();
            if deleted.is_err() {
                catalog = Some(self.catalog_snapshot(&inner));
            }
        }
        match deleted {
            Ok(bytes) => {
                if let Some(key) = &candidate.workspace {
                    inner.catalog.workspaces.remove(key);
                }
                let eviction = DiskEviction {
                    path: candidate.path.to_string_lossy().into_owned(),
                    bytes,
                    last_used_ms: candidate.last_used_ms,
                    evicted_ms: self.now(),
                };
                tracing::info!(path = %eviction.path, bytes, last_used_ms = eviction.last_used_ms, evicted_ms = eviction.evicted_ms, "runner disk eviction");
                self.core
                    .health
                    .lock()
                    .unwrap()
                    .record(eviction.evicted_ms, |c| {
                        c.evictions += 1;
                        c.eviction_bytes += bytes;
                    });
                inner.last_eviction = Some(eviction);
                // Saved after the state lock is released, below.
                catalog = Some(self.catalog_snapshot(&inner));
            }
            Err(_) if disappeared => {}
            Err(error) => {
                // Permission and filesystem failures rarely change between
                // admissions. Retry later rather than hammering the same tree.
                let delay =
                    if self.core.config.boat_profile && error.kind() == io::ErrorKind::TimedOut {
                        10_000
                    } else {
                        5 * 60 * 1000
                    };
                let retry_after_ms = self.now().saturating_add(delay);
                self.core
                    .health
                    .lock()
                    .unwrap()
                    .record(self.now(), |c| c.eviction_failures += 1);
                inner
                    .eviction_retry_after
                    .insert(candidate.path.clone(), retry_after_ms);
                tracing::warn!(path = %candidate.path.display(), %error, retry_after_ms, "runner disk eviction failed")
            }
        }
        drop(inner);
        if let Some(catalog) = catalog {
            if let Err(error) = catalog.and_then(|catalog| self.write_catalog(catalog)) {
                // Metadata writes can still fail when deleting reflinks
                // freed no blocks. Preserve the typed admission refusal.
                tracing::error!(%error, "cannot persist pressure cleanup catalog");
            }
        }
    }

    fn live_cache_disks(&self, inner: &Inner) -> BTreeSet<PathBuf> {
        let caches = self
            .core
            .config
            .windows
            .as_ref()
            .expect("Windows config")
            .caches();
        inner
            .jobs
            .values()
            .filter(|j| j.terminal.is_none() || inner.starting.contains(&j.id))
            .filter_map(|j| j.request.as_ref())
            .chain(inner.admitting.values())
            .map(|r| caches.disk(crate::caller_path::request(r).caller_workspace_key.text()))
            .collect()
    }

    fn windows_disk_candidates(&self, inner: &Inner) -> io::Result<Vec<Candidate>> {
        let vm = self.core.config.windows.as_ref().expect("Windows config");
        let device = crate::filesystem::device(&self.core.trees.state)?;
        let mut candidates: Vec<_> = vm
            .caches()
            .candidates(&self.live_cache_disks(inner), device)?
            .into_iter()
            .map(|c| Candidate {
                path: c.path,
                workspace: None,
                last_used_ms: c.last_used_ms,
                cache_disk: true,
                snapshot: false,
            })
            .collect();
        // On the Windows instance every deletable tree lives under these fixed
        // roots that the runner itself created. Caller workspace keys name
        // Linux checkouts, so resolving them here could evict a Linux workspace.
        for root in [
            vm.layout_root.join("staging"),
            self.core.trees.state.join("spools"),
            self.core.trees.state.join("trash"),
        ] {
            let metadata = match fs::symlink_metadata(&root) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if !metadata.is_dir() || crate::filesystem::device(&root)? != device {
                return Err(invalid(
                    "Windows cleanup root is a symlink or crosses a mount",
                ));
            }
            for entry in fs::read_dir(root)? {
                let entry = entry?;
                let metadata = fs::symlink_metadata(entry.path())?;
                if !metadata.is_dir() || crate::filesystem::device(&entry.path())? != device {
                    continue;
                }
                let id = entry.file_name().to_string_lossy().parse::<Uuid>().ok();
                if id.is_some_and(|id| {
                    self.core.config.windows_runtime.cleanup_pending(id)
                        || inner.transfers.contains(&id)
                        || inner.starting.contains(&id)
                        || inner.admitting.values().any(|r| r.job_id == id)
                        || inner.jobs.get(&id).is_some_and(|j| j.terminal.is_none())
                }) {
                    continue;
                }
                candidates.push(Candidate {
                    path: entry.path(),
                    workspace: None,
                    last_used_ms: metadata
                        .modified()?
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    cache_disk: false,
                    snapshot: false,
                });
            }
        }
        candidates.sort_by(|a, b| (a.last_used_ms, &a.path).cmp(&(b.last_used_ms, &b.path)));
        Ok(candidates)
    }

    /// Delete synchronously off the state lock and probe after each tree. Rename
    /// to trash is not enough: on XFS shared extents may reclaim no free space.
    pub(super) fn ensure_disk_space(&self) -> io::Result<Option<RunnerDiskFull>> {
        self.reclaim_disk_space(false)
    }

    /// The disk pass for admitting a Run request. A Boat sandbox serves one job
    /// at a time for one tenant, so a workspace that the next job does not
    /// reuse is unlikely to be used again, while keeping it can leave the next
    /// build too little room: with jobs back to back every workspace counts as
    /// recently used and the ordinary pass keeps it as long as the floor holds.
    /// There every idle workspace is evicted before admission, whatever the
    /// free space. The request's own workspace and siblings stay, because the
    /// caller registered them as admitting before this pass.
    pub(super) fn ensure_admission_disk_space(&self) -> io::Result<Option<RunnerDiskFull>> {
        self.reclaim_disk_space(self.core.config.boat_profile)
    }

    fn quarantine_cleanup_blockers(&self) -> Vec<Uuid> {
        let all = || self.core.quarantined.keys().copied().collect();
        if self.core.quarantined.is_empty() {
            return Vec::new();
        }
        // A cached admission decision is not proof for a destructive pass.
        // Do not race another inventory or accept an incomplete backend list.
        let Ok(_probe) = self.core.quarantine_probe.try_lock() else {
            return all();
        };
        let Ok(Some(units)) = self.core.backend.live_units(&self.core.config) else {
            return all();
        };
        {
            let inner = self.core.inner.lock().unwrap();
            if !inner.launching.is_empty() || !inner.starting.is_empty() {
                return all();
            }
            let accounted: BTreeSet<_> = inner
                .jobs
                .values()
                .filter(|job| job.terminal.is_none())
                .filter_map(|job| job.process.and_then(|process| process.unit))
                .map(|unit| self.core.backend.unit_name(&self.core.config, unit))
                .collect();
            if !units.is_subset(&accounted) {
                return all();
            }
        }
        self.core
            .quarantined
            .iter()
            .filter_map(|(id, job)| {
                let sandbox = self.core.trees.state.join("sandboxes").join(id.to_string());
                let absent = matches!(fs::symlink_metadata(sandbox), Err(error) if error.kind() == io::ErrorKind::NotFound);
                (job.request.is_some() || job.process.is_some() || !absent).then_some(*id)
            })
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn boat_private_candidate(&self, candidate: &Candidate) -> bool {
        self.core.config.boat_profile
            && (candidate.workspace.is_some()
                || candidate.path.parent() == Some(self.trash().as_path())
                || self.boat_workspace_garbage(&candidate.path)
                || candidate.path.parent()
                    == Some(self.core.trees.state.join("boat-quarantine").as_path()))
    }

    fn boat_workspace_garbage(&self, path: &Path) -> bool {
        self.core
            .config
            .boat_workspace_trash_roots()
            .iter()
            .any(|root| path.parent() == Some(root.as_path()))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn boat_workspace_trash(&self, path: &Path) -> io::Result<PathBuf> {
        // /Users is a bind mount. Even on the same XFS device, renaming from
        // that mount into runner-state trash fails with EXDEV. Retire under
        // the workspace root instead, keeping atomic detachment before the
        // bounded, no-follow deletion (and retry inventory) runs.
        self.core
            .config
            .owned_workspace_roots
            .iter()
            .zip(self.core.config.boat_workspace_trash_roots())
            .filter(|(root, _)| path != *root && path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, trash)| trash)
            .ok_or_else(|| io::Error::other("Boat workspace has no eviction root"))
    }

    #[cfg(target_os = "linux")]
    fn validate_boat_workspace_trash(trash: &Path) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(trash)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                "Boat workspace trash must be runner-private",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn remove_boat_candidate(&self, candidate: &Candidate) -> io::Result<u64> {
        if let Some(key) = &candidate.workspace {
            self.validate_destinations(crate::caller_path::host_str(key), None, &[])?;
        }
        let bytes = crate::boat_quota::cached_bytes(&candidate.path).unwrap_or_else(|error| {
            tracing::warn!(path = %candidate.path.display(), %error, "Boat eviction size unknown; deletion must not depend on a complete walk");
            0
        });
        let retired = if candidate.workspace.is_some() {
            use std::os::unix::fs::DirBuilderExt;
            let trash = self.boat_workspace_trash(&candidate.path)?;
            match fs::DirBuilder::new().mode(0o700).create(&trash) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                result => result?,
            }
            Self::validate_boat_workspace_trash(&trash)?;
            let retired = trash.join(format!("boat-cache-{}", Uuid::now_v7()));
            fs::rename(&candidate.path, &retired)?;
            File::open(&trash)?.sync_all()?;
            File::open(candidate.path.parent().unwrap())?.sync_all()?;
            retired
        } else {
            candidate.path.clone()
        };
        if let Err(error) = crate::boat_quota::remove_retired_tree(&retired) {
            if candidate.workspace.is_none() {
                return Err(error);
            }
            tracing::warn!(path = %retired.display(), %error, "Boat cache detached; bounded private garbage collection will continue later");
        }
        Ok(bytes)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn evict_boat_setup_workspace(&self, setup: &Setup) -> io::Result<()> {
        let key = crate::caller_path::request(&setup.request)
            .caller_workspace_key
            .text();
        let _pressure = self.core.disk.lock().unwrap();
        let used = {
            let mut inner = self.core.inner.lock().unwrap();
            if !self.core.config.boat_profile
                || setup.base
                || !inner.starting.contains(&setup.id)
                || !inner.reserved.contains_key(key)
                || crate::caller_path::job_key(&inner.jobs[&setup.id]) != key.as_str()
            {
                return Err(io::Error::other(
                    "cold eviction does not own the active Boat setup reservation",
                ));
            }
            inner.dropping.insert(key.clone());
            inner
                .catalog
                .workspaces
                .get(key)
                .map_or(self.now(), |workspace| workspace.used_ms)
        };
        let candidate = Candidate {
            path: crate::caller_path::request(&setup.request)
                .host_workspace
                .as_path()
                .to_path_buf(),
            workspace: Some(key.clone()),
            last_used_ms: used,
            cache_disk: false,
            snapshot: false,
        };
        let _tree = crate::tree_access::lock(crate::caller_path::host(&candidate.path));
        let deleted = self.remove_boat_candidate(&candidate);
        let result = deleted
            .as_ref()
            .map(|_| ())
            .map_err(|error| io::Error::new(error.kind(), error.to_string()));
        self.record_disk_removal(&candidate, deleted, false);
        result?;
        (self.core.config.boat_storage_trim)()
    }

    /// With `sweep_idle_workspaces`, every evictable catalogued workspace is
    /// deleted regardless of age or free space before the ordinary rules apply
    /// to everything else.
    fn reclaim_disk_space(
        &self,
        sweep_idle_workspaces: bool,
    ) -> io::Result<Option<RunnerDiskFull>> {
        let _pressure = self.core.disk.lock().unwrap();
        #[cfg(unix)]
        if self.core.config.macos_instance() {
            return crate::macos::evict(&self.core.config, self.core.trees.mirrors.clone());
        }
        let floor = self.core.config.free_space_floor_bytes;
        if floor == 0 {
            self.ensure_admission_ownership()?;
            return Ok(None);
        }
        // Reclaim cache before admission becomes unsafe. Missing the soft goal
        // must never refuse a job while the hard reserve is still available.
        let soft = self
            .core
            .config
            .free_space_eviction_bytes
            .unwrap_or_else(|| floor.saturating_mul(2));
        let probe = || (self.core.config.free_space_probe)(&self.core.trees.state);
        let mut free = probe()?;
        if free >= soft && !sweep_idle_workspaces {
            self.ensure_admission_ownership()?;
            return Ok(None);
        }
        let blocked = self.quarantine_cleanup_blockers();
        {
            let mut cleanup = self.core.quarantine_cleanup.lock().unwrap();
            cleanup.blocked = blocked.clone();
            let now = self.now();
            if !blocked.is_empty()
                && cleanup
                    .last_error_ms
                    .is_none_or(|last| now.saturating_sub(last) >= 300_000)
            {
                tracing::error!(quarantined_ids = ?blocked, "runner disk cleanup blocked by quarantine");
                cleanup.last_error_ms = Some(now);
            }
        }
        if !blocked.is_empty() {
            self.ensure_admission_ownership()?;
            return Ok((free < floor).then_some(RunnerDiskFull {
                free_bytes: free,
                floor_bytes: floor,
            }));
        }
        for id in self.core.quarantined.keys() {
            // Null sidecar fields do not prove that a job never ran. The fresh
            // inventory and absent sandbox only authorize GC, not replay/retry.
            tracing::warn!(quarantined_id = %id, "disk cleanup proceeding past ownerless quarantine; duplicate fence retained");
        }
        let device = crate::filesystem::device(&self.core.trees.state)?;
        let mut attempted = BTreeSet::new();
        let mut skipped_recent = BTreeSet::new();
        loop {
            let candidate = {
                let mut inner = self.core.inner.lock().unwrap();
                let now = self.now();
                inner.eviction_retry_after.retain(|_, retry| *retry > now);
                let candidates = match self.disk_candidates(&inner) {
                    Ok(candidates) => candidates,
                    Err(error) => {
                        tracing::warn!(%error, "cannot enumerate runner disk eviction candidates");
                        Vec::new()
                    }
                };
                let sandboxes = self.core.trees.state.join("sandboxes");
                let trash = self.core.trees.state.join("trash");
                let staging = self
                    .core
                    .config
                    .windows
                    .as_ref()
                    .map(|vm| vm.layout_root.join("staging"));
                let next = candidates.into_iter().find(|c| {
                    if attempted.contains(&c.path)
                        || inner.eviction_retry_after.contains_key(&c.path)
                    {
                        return false;
                    }
                    if sweep_idle_workspaces && c.workspace.is_some() {
                        return true;
                    }
                    // Only a sweep continues past the soft goal, and only for
                    // workspaces.
                    if free >= soft {
                        return false;
                    }
                    // Nothing uses an unreferenced snapshot; the next setup
                    // that needs it builds it again.
                    if c.snapshot {
                        return true;
                    }
                    // Only caches are worth keeping for reuse. A dead job's private
                    // tree and pending deletions are garbage at any age.
                    let garbage = c.path.parent().is_some_and(|parent| {
                        parent == sandboxes
                            || parent == trash
                            || (self.core.config.boat_profile
                                && parent == self.core.trees.state.join("boat-quarantine"))
                            || staging.as_deref() == Some(parent)
                    }) || self.boat_workspace_garbage(&c.path);
                    if (c.cache_disk
                        && now.saturating_sub(c.last_used_ms) <= SOFT_PRESSURE_MIN_AGE_MS)
                        || (!garbage
                            && free >= floor
                            && now.saturating_sub(c.last_used_ms) < SOFT_PRESSURE_MIN_AGE_MS)
                    {
                        skipped_recent.insert(c.path.clone());
                        return false;
                    }
                    true
                });
                if let Some(candidate) = &next {
                    if candidate.cache_disk || candidate.path == self.core.config.sccache_dir() {
                        inner.evicting_caches.insert(candidate.path.clone());
                    }
                    if let Some(key) = &candidate.workspace {
                        inner.dropping.insert(key.clone());
                    }
                    if candidate.path.parent()
                        == Some(self.core.trees.state.join("git-deps").as_path())
                    {
                        inner.evicting_dependencies.insert(candidate.path.clone());
                    }
                }
                next
            };
            let Some(candidate) = candidate else { break };
            if candidate.snapshot {
                attempted.insert(candidate.path.clone());
                let deleted = match self.evict_sibling_snapshot(&candidate.path, device) {
                    Ok(None) => continue,
                    Ok(Some(bytes)) => Ok(bytes),
                    Err(error) => Err(error),
                };
                self.record_disk_removal(&candidate, deleted, false);
                free = probe()?;
                if free >= soft && !sweep_idle_workspaces && !self.core.config.boat_profile {
                    self.core.health.lock().unwrap().record(self.now(), |c| {
                        c.skipped_recent += skipped_recent.len() as u64;
                    });
                    return Ok(None);
                }
                continue;
            }
            let caches = self.core.config.windows.as_ref().map(|vm| vm.caches());
            let _cache_lock = if candidate.cache_disk {
                let caches = caches.as_ref().unwrap();
                let result = (|| -> io::Result<Option<File>> {
                    let lock = caches.lock(&candidate.path)?;
                    let inner = self.core.inner.lock().unwrap();
                    if caches.attached(&candidate.path)?
                        || self.live_cache_disks(&inner).contains(&candidate.path)
                        || caches.last_used(&candidate.path)? != candidate.last_used_ms
                    {
                        Ok(None)
                    } else {
                        Ok(Some(lock))
                    }
                })();
                match result {
                    Ok(Some(lock)) => Some(lock),
                    result => {
                        self.core
                            .inner
                            .lock()
                            .unwrap()
                            .evicting_caches
                            .remove(&candidate.path);
                        result?;
                        attempted.insert(candidate.path.clone());
                        continue;
                    }
                }
            } else {
                None
            };
            let dependency_lock = self.core.trees.dependency_lock(&candidate.path);
            let _dependency_guard = dependency_lock.lock().unwrap();
            attempted.insert(candidate.path.clone());
            // Setup and eviction share the same per-key lock, including temporary
            // publications. Recheck last use after locking because a cache hit may
            // have completed since candidate selection.
            let _javascript_guard = if candidate.path.parent()
                == Some(self.core.trees.state.join("js-installs").as_path())
                || candidate.path.parent()
                    == Some(self.core.trees.state.join("js-manager-caches").as_path())
            {
                let Some(guard) = javascript_entry_lock(&self.core.trees.state, &candidate.path)?
                else {
                    continue;
                };
                let metadata = match fs::metadata(&candidate.path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    result => result?,
                };
                let modified_ms = metadata
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if modified_ms != candidate.last_used_ms {
                    continue;
                }
                Some(guard)
            } else {
                None
            };
            let Some(_tree_access) =
                crate::tree_access::try_lock(crate::caller_path::host(&candidate.path))
            else {
                let mut inner = self.core.inner.lock().unwrap();
                inner.evicting_dependencies.remove(&candidate.path);
                inner.evicting_caches.remove(&candidate.path);
                if let Some(key) = &candidate.workspace {
                    inner.dropping.remove(key);
                }
                continue;
            };
            self.long_work("disk_remove");
            let authorization = candidate.workspace.as_ref().map_or(Ok(()), |key| {
                self.validate_destinations(crate::caller_path::host_str(key), None, &[])
            });
            #[cfg(target_os = "linux")]
            let access = authorization.and_then(|()| {
                if self.boat_private_candidate(&candidate) {
                    return Ok(());
                }
                if self.core.config.isolate_jobs {
                    if candidate.path.parent()
                        == Some(self.core.trees.state.join("sandboxes").as_path())
                    {
                        crate::isolation::reclaim_runtime(&candidate.path)
                    } else {
                        crate::isolation::restore_runner_access(&candidate.path)
                    }
                } else {
                    Ok(())
                }
            });
            #[cfg(not(target_os = "linux"))]
            let access: io::Result<()> = authorization;
            let mut cache_cleared = false;
            let deleted = access.and_then(|()| {
                #[cfg(target_os = "linux")]
                if self.boat_private_candidate(&candidate) {
                    return self.remove_boat_candidate(&candidate);
                }
                let bytes = allocated_bytes(&candidate.path, device)?;
                if candidate.cache_disk {
                    caches.as_ref().unwrap().remove(&candidate.path)?;
                } else if candidate.path == self.core.config.sccache_dir() {
                    let trash = self.trash().join(format!("sccache-{}", Uuid::now_v7()));
                    fs::create_dir_all(self.trash())?;
                    #[cfg(target_os = "linux")]
                    if self.core.config.writer_account.is_some() {
                        crate::isolation::retire_writer_cache(&candidate.path, &trash)?;
                    } else {
                        fs::rename(&candidate.path, &trash)?;
                    }
                    #[cfg(not(target_os = "linux"))]
                    fs::rename(&candidate.path, &trash)?;
                    // From here on jobs see an empty cache, whatever fails next.
                    cache_cleared = true;
                    #[cfg(target_os = "linux")]
                    if let Some(account) = self.core.config.writer_account {
                        crate::isolation::prepare_writer_cache(&candidate.path, account)?;
                    } else {
                        fs::create_dir_all(&candidate.path)?;
                    }
                    #[cfg(not(target_os = "linux"))]
                    fs::create_dir_all(&candidate.path)?;
                    fs::remove_dir_all(trash)?;
                } else {
                    fs::remove_dir_all(&candidate.path)?;
                }
                Ok(bytes)
            });
            self.record_disk_removal(&candidate, deleted, cache_cleared);
            free = probe()?;
            if free >= soft && !sweep_idle_workspaces && !self.core.config.boat_profile {
                self.core.health.lock().unwrap().record(self.now(), |c| {
                    c.skipped_recent += skipped_recent.len() as u64;
                });
                return Ok(None);
            }
        }
        #[cfg(target_os = "linux")]
        if self.core.config.boat_profile && (!attempted.is_empty() || free < floor) {
            // Batch discard after eviction: XFS free space alone does not mean
            // the sparse loop image returned its blocks to the root disk.
            (self.core.config.boat_storage_trim)()?;
            free = probe()?;
        }
        if free >= soft {
            self.core.health.lock().unwrap().record(self.now(), |c| {
                c.skipped_recent += skipped_recent.len() as u64;
            });
            return Ok(None);
        }
        if free >= floor {
            self.core.health.lock().unwrap().record(self.now(), |c| {
                c.soft_goal_unmet_passes += 1;
                c.skipped_recent += skipped_recent.len() as u64;
            });
            tracing::info!(
                free_bytes = free,
                floor_bytes = floor,
                soft_bytes = soft,
                skipped_recent_candidates = skipped_recent.len(),
                "runner disk soft goal unmet; admitting above hard floor"
            );
            return Ok(None);
        }
        tracing::warn!(
            free_bytes = free,
            floor_bytes = floor,
            "runner_disk_full; refusing before start"
        );
        Ok(Some(RunnerDiskFull {
            free_bytes: free,
            floor_bytes: floor,
        }))
    }
}

/// How often an idle runner with a disk reserve checks whether its disk has
/// dropped under the admission floor.
pub(super) const DISK_UPKEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Whether a failed write ran out of space (ENOSPC).
pub(super) fn storage_full(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::StorageFull
}

impl Runner {
    fn disk_reserve_path(&self) -> PathBuf {
        self.core.trees.state.join("disk-reserve")
    }

    /// Keeps a runner that reserves disk (the Boat profile) able to admit a job
    /// without waiting for one to arrive. Admission is the only other place
    /// that evicts, and in a Boat sandbox nothing reaches admission while the
    /// disk is full: the endpoint's journal shares this disk and the endpoint
    /// withdraws its readiness below the floor. So when no job is live and free
    /// space is under the floor, this runs the ordinary pressure pass, which
    /// below the floor may evict any idle workspace, including the one the last
    /// job used. Once free space is back above the floor, it recreates the disk
    /// reserve if a full disk used it up.
    pub(crate) fn disk_upkeep(&self) -> io::Result<()> {
        let floor = self.core.config.free_space_floor_bytes;
        if self.core.config.disk_reserve_bytes == 0 || floor == 0 {
            return Ok(());
        }
        let Ok(_restore) = self.core.reserve_restore.try_lock() else {
            return Ok(());
        };
        let idle = |inner: &Inner| {
            !inner.restoring_reserve
                && inner.jobs.values().all(|job| job.terminal.is_some())
                && inner.starting.is_empty()
                && inner.admitting.is_empty()
                && inner.preparing.is_empty()
                && inner.transfers.is_empty()
                && inner.reserved.is_empty()
        };
        if !idle(&self.core.inner.lock().unwrap()) {
            return Ok(());
        }
        let probe = || (self.core.config.free_space_probe)(&self.core.trees.state);
        if probe()? < floor {
            self.ensure_disk_space()?;
            if probe()? < floor {
                return Ok(());
            }
        }
        // Claim the idle interval, not the global lock: writing the reserve can
        // stall on a stressed disk, but status/cancel/record work must proceed.
        let mut inner = self.core.inner.lock().unwrap();
        if !idle(&inner) {
            return Ok(());
        }
        inner.restoring_reserve = true;
        drop(inner);
        let claim = Marker::new(self, |inner: &mut Inner| inner.restoring_reserve = false);
        let temporary = self.write_disk_reserve()?;
        let mut inner = self.core.inner.lock().unwrap();
        let published = temporary.map_or(Ok(()), |temporary| {
            fs::rename(&temporary, self.disk_reserve_path()).inspect_err(|_| {
                let _ = fs::remove_file(temporary);
            })
        });
        claim.clear(&mut inner);
        published
    }

    /// Returns a fully allocated, synced temporary file. The caller publishes it
    /// under the idle claim without keeping the state lock across this I/O.
    fn write_disk_reserve(&self) -> io::Result<Option<PathBuf>> {
        let bytes = self.core.config.disk_reserve_bytes;
        let path = self.disk_reserve_path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(invalid("runner disk reserve is not a regular file"))
            }
            Ok(metadata) if metadata.len() == bytes => return Ok(None),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let temporary = path.with_extension("tmp");
        let written = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)?;
            self.long_work("disk_reserve_write");
            let chunk = vec![0; 1 << 20];
            let mut left = bytes;
            while left > 0 {
                let n = left.min(chunk.len() as u64) as usize;
                file.write_all(&chunk[..n])?;
                left -= n as u64;
            }
            file.sync_all()
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written?;
        tracing::info!(bytes, "runner disk reserve allocated");
        Ok(Some(temporary))
    }

    /// Frees the reserve after a write ran out of space, so the records that
    /// let a finished job end (its terminal record, the catalog, the
    /// endpoint's journal) can still be written. Returns whether a reserve was
    /// removed.
    pub(super) fn release_disk_reserve(&self) -> bool {
        if self.core.config.disk_reserve_bytes == 0 {
            return false;
        }
        match fs::remove_file(self.disk_reserve_path()) {
            Ok(()) => {
                tracing::warn!(
                    bytes = self.core.config.disk_reserve_bytes,
                    "runner disk reserve released after the disk filled"
                );
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => {
                tracing::error!(%error, "cannot release runner disk reserve");
                false
            }
        }
    }
}
