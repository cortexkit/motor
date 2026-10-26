use super::*;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;

/// Capture exact legacy checkout identities before discarding old catalog fields.
/// The durable pending list also covers a restart after a failed rename.
pub(super) fn identify_legacy_checkouts(
    catalog: &mut Catalog,
    jobs: &BTreeMap<Uuid, Job>,
    instance: &str,
) {
    if catalog.generation_migration_complete {
        return;
    }
    for job in jobs
        .values()
        .filter(|job| job.base && job.writer_descriptor.is_none())
    {
        if let Some(request) = &job.request {
            catalog.legacy_checkouts.insert(
                crate::caller_path::run(request).caller_cwd.text().clone(),
                LegacyCheckout {
                    repository: crate::caller_path::request(request)
                        .caller_repository_root
                        .text()
                        .clone(),
                    tip: Some(request.integration_tip.clone()).filter(|tip| !tip.is_empty()),
                },
            );
        }
    }
    for (root, repository) in crate::caller_path::entries_mut(&mut catalog.repositories)
        .map(|(path, record)| (path.caller.text(), record))
    {
        if repository.requested_descriptor.is_some() {
            continue;
        }
        let tip = repository
            .desired
            .as_ref()
            .map(|request| request.tip.clone())
            .or_else(|| repository.commit.clone());
        if tip.is_some() {
            for checkout in catalog
                .legacy_checkouts
                .values_mut()
                .filter(|checkout| checkout.repository == *root)
            {
                checkout.tip = tip.clone();
            }
        }
        let workspaces = crate::caller_path::entries(&catalog.workspaces)
            .map(|(path, record)| (path.caller.text(), record))
            .filter(|(_, workspace)| workspace.repository == *root)
            .map(|(path, _)| path.as_str())
            .chain(
                repository
                    .desired
                    .iter()
                    .map(|request| request.workspace.as_str()),
            );
        for workspace in workspaces {
            if let Some(parent) = Path::new(workspace).parent() {
                let path = parent.join(format!(".motor-base-{instance}-{}", workspace::name(root)));
                catalog
                    .legacy_checkouts
                    .entry(path.to_string_lossy().into_owned())
                    .or_insert_with(|| LegacyCheckout {
                        repository: root.clone(),
                        tip: tip.clone(),
                    });
            }
        }
        // Keep exact catalog-owned paths until their rename succeeds. Old client
        // HEAD commits may be unmerged: only a new trusted request can authorize
        // a writer fill, not a former target publication.
        *repository = Repository::default();
    }
    catalog.generation_migration_complete = true;
}

impl Runner {
    pub(super) fn migrate_legacy_storage(&self) -> io::Result<()> {
        if self.core.config.windows_instance() {
            return Ok(());
        }
        let pending = self
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .legacy_checkouts
            .clone();
        for plan in crate::caller_path::keys(&pending) {
            let path = plan.caller.text();
            match self.retire_legacy_path(plan.host.as_path()) {
                Ok(()) => {
                    let mut inner = self.core.inner.lock().unwrap();
                    inner.catalog.legacy_checkouts.remove(path);
                    self.catalog(&inner)?;
                }
                Err(error) => {
                    tracing::warn!(%path, errno = ?error.raw_os_error(), %error, "legacy checkout retirement deferred until next startup");
                }
            }
        }
        let generations = self.core.trees.state.join("generations");
        if self
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .legacy_generations_pending
        {
            match self.retire_legacy_path(&generations) {
                Ok(()) => {
                    let mut inner = self.core.inner.lock().unwrap();
                    inner.catalog.legacy_generations_pending = false;
                    self.catalog(&inner)?;
                }
                Err(error) => {
                    tracing::warn!(path = %generations.display(), errno = ?error.raw_os_error(), %error, "legacy generation retirement deferred until next startup");
                }
            }
        }
        Ok(())
    }

    /// Startup only detaches old storage. Recursive deletion runs in the purge
    /// thread, and a later writer materializes its own checkout from a fresh tip.
    fn retire_legacy_path(&self, path: &Path) -> io::Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        #[cfg(target_os = "linux")]
        if self.core.config.isolate_jobs
            && metadata.is_dir()
            && (metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o200 == 0)
        {
            // Linux needs write access to a directory moved between parents to
            // update its '..' entry. Repair this inode only; descendants stay
            // untouched until the background purge reclaims them.
            crate::isolation::restore_runner_checkout_root(path)?;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = metadata;
        // Legacy checkouts sit beside caller workspaces, which can be on another
        // filesystem from the runner's trash (on the build server, /Users is its
        // own mount, so a rename there fails with EXDEV). discard falls back to
        // a trash name beside the path in that case.
        self.discard(path)
    }
}
