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
    pub(super) fn protect_admission(&self, inner: &mut Inner, request: &Run) -> AdmissionGuard {
        let token = Uuid::now_v7();
        inner.admitting.insert(token, request.clone());
        AdmissionGuard {
            runner: self.clone(),
            token,
        }
    }

    pub(crate) fn transfer(&self, id: Uuid) -> TransferGuard {
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
        workspace::validate(&r.workspace_key, &r.repository_root, &r.siblings)?;
        let refusal = |reason: RefusalReason, disk_full, detail: String| Refusal {
            retry_after_ms: if reason == RefusalReason::RunnerDraining {
                self.drain_retry_after_ms()
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
            &r.workspace_key,
            &r.repository_root,
            &r.siblings,
        ) {
            return Ok(Err(refusal(
                RefusalReason::WorkspaceSetupFailed,
                None,
                detail,
            )));
        }
        if let Err(error) = self.validate_writer_paths(&r.workspace_key, None, &r.siblings) {
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
        if let Some(full) = self.ensure_disk_space()? {
            return Ok(Err(refusal(
                RefusalReason::RunnerDiskFull,
                Some(full.clone()),
                full.diagnostic(),
            )));
        }
        let mut inner = self.core.inner.lock().unwrap();
        if let Some(detail) =
            nested_workspace_detail(&inner, &r.workspace_key, &r.repository_root, &r.siblings)
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
        if Self::deleting(&inner, &r.workspace_key, &r.siblings)
            || self.dependencies_evicting(&inner, &r.git_dependencies)
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
        let (status, permits_held) = {
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
            (self.status_with_jobs(&inner, queue_depth, running), permits)
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
            let mut counters = self.core.health.lock().unwrap();
            RunnerHealth {
                runner_version: env!("CARGO_PKG_VERSION").into(),
                since_ms: counters.since_ms,
                window_minutes: health::WINDOW_MINUTES,
                free_bytes,
                queue_depth,
                running,
                failed_job_units,
                counts: counters.counts(now),
                underused_reservations,
                repository_usage,
                load_1m,
                threads,
                mem_available_bytes,
                permits_held: Some(permits_held),
                permits_cap: Some(self.core.config.max_weight),
                queue_wait_1h: Some(counters.queue_wait(now)),
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
        let mut passive: Vec<_> = inner
            .catalog
            .repositories
            .keys()
            .map(PathBuf::from)
            .chain(inner.catalog.sibling_commits.keys().map(PathBuf::from))
            .collect();
        passive.extend(passive.clone().iter().filter_map(|p| p.canonicalize().ok()));
        for job in inner.jobs.values() {
            if let Some(r) = &job.request {
                if job.base {
                    protected.push(PathBuf::from(&r.cwd));
                }
                if job.terminal.is_none() || inner.starting.contains(&job.id) {
                    protected.push(PathBuf::from(&r.workspace_key));
                    protected.extend(r.siblings.iter().map(|s| PathBuf::from(&s.canonical_path)));
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
        for (key, r) in &inner.reserved {
            protected.push(PathBuf::from(key));
            protected.extend(r.siblings.iter().map(|s| PathBuf::from(&s.canonical_path)));
        }
        for r in inner.admitting.values() {
            protected.push(PathBuf::from(&r.workspace_key));
            protected.extend(r.siblings.iter().map(|s| PathBuf::from(&s.canonical_path)));
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
            protected.push(PathBuf::from(&r.workspace_key));
            protected.extend(r.siblings.iter().map(|s| PathBuf::from(&s.canonical_path)));
        }
        protected.extend(inner.dropping.iter().map(PathBuf::from));
        // Include canonical aliases: an allowed root cannot turn a symlink into
        // permission to delete a protected path under another spelling.
        protected.extend(
            protected
                .clone()
                .iter()
                .filter_map(|p| p.canonicalize().ok()),
        );
        let roots: Vec<_> = self
            .core
            .config
            .workspace_roots
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();
        let mut candidates = Vec::new();
        for (key, workspace) in &inner.catalog.workspaces {
            let path = PathBuf::from(key);
            let Ok(canonical) = path.canonicalize() else {
                continue;
            };
            if !workspace::absolute(key)
                || self.validate_destinations(key, None, &[]).is_err()
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
                workspace: Some(key.clone()),
                last_used_ms: workspace.used_ms,
                cache_disk: false,
            });
        }
        // These are the runner's actual upload, deletion and launch staging
        // roots. Never scan /srv/build or infer ownership from directory names.
        for kind in [
            "spools",
            "trash",
            "sandboxes",
            "git-deps",
            "js-installs",
            "js-manager-caches",
        ] {
            let root = self.core.trees.state.join(kind);
            let Ok(entries) = fs::read_dir(&root) else {
                continue;
            };
            let canonical_root = root.canonicalize()?;
            let canonical_state = self.core.trees.state.canonicalize()?;
            if !canonical_root.starts_with(&canonical_state) {
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
                } else if kind == "trash" {
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
                    });
                }
            }
        }
        let fleet = self.core.config.sccache_dir();
        if inner.writer.is_none() && !inner.evicting_caches.contains(&fleet) {
            if let Ok(metadata) = fs::symlink_metadata(&fleet) {
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    candidates.push(Candidate {
                        path: fleet,
                        workspace: None,
                        cache_disk: false,
                        last_used_ms: metadata
                            .modified()?
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                    });
                }
            }
        }
        candidates.sort_by(|a, b| (a.last_used_ms, &a.path).cmp(&(b.last_used_ms, &b.path)));
        Ok(candidates)
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
            .map(|r| caches.disk(&r.workspace_key))
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
                });
            }
        }
        candidates.sort_by(|a, b| (a.last_used_ms, &a.path).cmp(&(b.last_used_ms, &b.path)));
        Ok(candidates)
    }

    /// Delete synchronously off the state lock and probe after each tree. Rename
    /// to trash is not enough: on XFS shared extents may reclaim no free space.
    pub(super) fn ensure_disk_space(&self) -> io::Result<Option<RunnerDiskFull>> {
        self.ensure_admission_ownership()?;
        let _pressure = self.core.disk.lock().unwrap();
        let floor = self.core.config.free_space_floor_bytes;
        if floor == 0 {
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
        if !self.core.quarantined.is_empty() {
            // Admission can proceed after every live unit is accounted for, but
            // damaged historical records still cannot authorize destructive GC.
            return Ok((free < floor).then_some(RunnerDiskFull {
                free_bytes: free,
                floor_bytes: floor,
            }));
        }
        if free >= soft {
            return Ok(None);
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
                    // Only caches are worth keeping for reuse. A dead job's private
                    // tree and pending deletions are garbage at any age.
                    let garbage = c.path.parent().is_some_and(|parent| {
                        parent == sandboxes || parent == trash || staging.as_deref() == Some(parent)
                    });
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
            let Some(_tree_access) = crate::tree_access::try_lock(&candidate.path) else {
                let mut inner = self.core.inner.lock().unwrap();
                inner.evicting_dependencies.remove(&candidate.path);
                inner.evicting_caches.remove(&candidate.path);
                if let Some(key) = &candidate.workspace {
                    inner.dropping.remove(key);
                }
                continue;
            };
            self.long_work("disk_remove");
            let authorization = candidate
                .workspace
                .as_ref()
                .map_or(Ok(()), |key| self.validate_destinations(key, None, &[]));
            #[cfg(target_os = "linux")]
            let access = authorization.and_then(|()| {
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
            let deleted = access.and_then(|()| {
                let bytes = allocated_bytes(&candidate.path, device)?;
                if candidate.cache_disk {
                    caches.as_ref().unwrap().remove(&candidate.path)?;
                } else if candidate.path == self.core.config.sccache_dir() {
                    let trash = self.trash().join(format!("sccache-{}", Uuid::now_v7()));
                    fs::create_dir_all(self.trash())?;
                    fs::rename(&candidate.path, &trash)?;
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
            match deleted {
                Ok(bytes) => {
                    if candidate.path == self.core.config.sccache_dir() {
                        for repository in inner.catalog.repositories.values_mut() {
                            repository.filled_descriptor = None;
                            repository.filled_descriptors.clear();
                            repository.recorded_without_writer.clear();
                            repository.miss_refills.clear();
                        }
                    }
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
                    let retry_after_ms = self.now().saturating_add(5 * 60 * 1000);
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
            free = probe()?;
            if free >= soft {
                self.core.health.lock().unwrap().record(self.now(), |c| {
                    c.skipped_recent += skipped_recent.len() as u64;
                });
                return Ok(None);
            }
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
