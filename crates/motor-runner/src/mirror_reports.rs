use super::*;

impl Runner {
    pub(super) fn report_imported_refs(&self, request: &Run) -> io::Result<()> {
        let Some(events) = self.tenant_events().filter(|e| e.reports_enabled()) else {
            return Ok(());
        };
        let root = paths::request(request).caller_repository_root.text();
        for repository in request
            .bundles
            .iter()
            .map(|b| &b.repository)
            .collect::<BTreeSet<_>>()
        {
            let mut refs = Vec::new();
            for (repo, commit) in std::iter::once((root.as_str(), request.snapshot_commit.as_str()))
                .chain(
                    request
                        .siblings
                        .iter()
                        .map(|s| (paths::sibling(s).caller.text().as_str(), s.commit.as_str())),
                )
            {
                if repo == repository {
                    refs.push(MirrorRef {
                        repository: Some(repository.clone()),
                        dependency: None,
                        reference: format!("refs/snapshots/{commit}"),
                        commit: commit.to_owned(),
                    });
                }
            }
            if repository == root && !request.integration_tip.is_empty() {
                refs.push(MirrorRef {
                    repository: Some(repository.clone()),
                    dependency: None,
                    reference: format!("refs/tips/{}", request.integration_tip),
                    commit: request.integration_tip.clone(),
                });
            }
            events.mirror_refs(true, &self.core.trees.mirror(repository), refs)?;
        }
        for dependency in &request.git_dependencies {
            if dependency.bundle.is_none() {
                continue;
            }
            let refs = dependency
                .commits
                .iter()
                .map(|commit| MirrorRef {
                    repository: None,
                    dependency: Some(dependency.url.clone()),
                    reference: format!("refs/ck-motor/dep/{commit}"),
                    commit: commit.clone(),
                })
                .collect();
            events.mirror_refs(
                true,
                &self.core.trees.dependency_mirror(&dependency.url),
                refs,
            )?;
        }
        Ok(())
    }

    pub(super) fn report_retired_refs(
        &self,
        mirror: &Path,
        references: &[String],
    ) -> io::Result<()> {
        let Some(events) = self.tenant_events().filter(|e| e.reports_enabled()) else {
            return Ok(());
        };
        let listed = workspace::git(
            mirror,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )?;
        let (repository, dependency) = workspace::mirror_identity(mirror);
        let refs = listed
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(reference, _)| references.iter().any(|r| r == reference))
            .map(|(reference, commit)| MirrorRef {
                repository: repository.clone(),
                dependency: dependency.clone(),
                reference: reference.to_owned(),
                commit: commit.to_owned(),
            })
            .collect();
        // Queue retirements before deletion so motor-host can discard its cached
        // list of held commits even if the event channel detaches. A retirement
        // reported for a ref that survives only causes a harmless resend.
        events.mirror_refs(false, mirror, refs)
    }

    pub(super) fn report_retired_mirror(&self, mirror: &Path) -> io::Result<()> {
        if !self.tenant_events().is_some_and(|e| e.reports_enabled()) {
            return Ok(());
        }
        if !mirror.exists() {
            return Ok(());
        }
        let listed = workspace::git(mirror, &["for-each-ref", "--format=%(refname)"])?;
        self.report_retired_refs(
            mirror,
            &listed.lines().map(str::to_owned).collect::<Vec<_>>(),
        )
    }
}
