//! Operator-owned authority for the privileged source-seal helper.
use super::*;

pub(crate) const CONFIG: &str = "/etc/motor-runner/source-seal.json";

#[derive(Deserialize)]
pub(crate) struct Authority {
    pub(crate) states: BTreeMap<PathBuf, Vec<PathBuf>>,
}

impl Authority {
    #[cfg(test)]
    /// A concrete fixture policy, not an exemption from state/root checks.
    pub(crate) fn for_fixture(config: &Config) -> Self {
        Self::from_config(config)
    }

    fn from_config(config: &Config) -> Self {
        Self {
            states: BTreeMap::from([(config.state_dir(), roots(config))]),
        }
    }

    /// A root runner's launch configuration is already privileged authority.
    /// Only the delegated sudo helper needs an independently installed policy
    /// to constrain paths supplied by an unprivileged runner.
    pub(crate) fn for_staging(config: &Config) -> io::Result<Self> {
        Self::staging_for_uid(config, unsafe { libc::geteuid() }, Self::load)
    }

    pub(crate) fn staging_for_uid(
        config: &Config,
        uid: u32,
        load: impl FnOnce() -> io::Result<Self>,
    ) -> io::Result<Self> {
        if uid == 0 {
            Ok(Self::from_config(config))
        } else {
            load()
        }
    }

    pub(crate) fn load() -> io::Result<Self> {
        Self::read(Path::new(CONFIG))
    }

    fn read(path: &Path) -> io::Result<Self> {
        for ancestor in path.ancestors() {
            let pin = crate::permissions::physical_root(ancestor).or_else(|error| {
                if ancestor == Path::new("/") {
                    File::open("/").map(Into::into)
                } else {
                    Err(error)
                }
            })?;
            let metadata = File::from(pin).metadata()?;
            if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return Err(io::Error::other(format!(
                    "unsafe source seal authority {}: must be root-owned and non-writable",
                    ancestor.display()
                )));
            }
        }
        let pin = File::from(crate::permissions::physical_root(path)?);
        let mut file = reopen_regular(&pin)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let authority: Self = serde_json::from_slice(&bytes)?;
        if authority.states.is_empty() {
            return Err(io::Error::other(
                "source seal authority has no delegated instances",
            ));
        }
        Ok(authority)
    }

    pub(crate) fn roots(&self, state: &Path) -> io::Result<&[PathBuf]> {
        crate::permissions::physical_root(state)?;
        self.states.get(state).map(Vec::as_slice).ok_or_else(|| {
            io::Error::other(format!(
                "source seal authority {CONFIG} has no state root {}",
                state.display()
            ))
        })
    }

    pub(crate) fn workspace(&self, state: &Path, workspace: &Path) -> io::Result<()> {
        let roots = self.roots(state)?;
        crate::permissions::physical_root(workspace)?;
        if !roots.iter().any(|root| {
            workspace.starts_with(root) && crate::permissions::physical_root(root).is_ok()
        }) {
            return Err(io::Error::other(format!("source seal authority {CONFIG}: workspace {} is not under an authorized root for {}", workspace.display(), state.display())));
        }
        Ok(())
    }

    pub(crate) fn runner(&self, config: &Config) -> io::Result<()> {
        let state = config.state_dir();
        let allowed = self.roots(&state)?;
        for root in roots(config) {
            if !allowed.contains(&root) {
                return Err(io::Error::other(format!(
                    "source seal authority {CONFIG}: missing workspace root {} for state {}",
                    root.display(),
                    state.display()
                )));
            }
            crate::permissions::physical_root(&root)?;
        }
        Ok(())
    }
}

pub(crate) fn roots(config: &Config) -> Vec<PathBuf> {
    config
        .owned_workspace_roots
        .iter()
        .cloned()
        .chain(
            config
                .caller_roots
                .iter()
                .map(|root| root.host_root.clone()),
        )
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn non_root_staging_still_requires_the_installed_policy() {
        let scratch = cortexkit_test_support::ScratchDir::new("staging-missing-policy");
        let config = Config::new(scratch.path(), "state");
        let missing = scratch.join("missing-source-seal.json");
        assert!(!missing.exists());
        let error = Authority::staging_for_uid(&config, 1000, || Authority::read(&missing))
            .err()
            .expect("non-root staging must load the policy");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
    #[test]
    fn unknown_state_and_outside_workspace_are_not_authorized() {
        let scratch = cortexkit_test_support::ScratchDir::new("source-seal-authority");
        let state = scratch.join("state");
        let root = scratch.join("workspaces");
        let workspace = root.join("task");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let authority = Authority {
            states: BTreeMap::from([(state.clone(), vec![root])]),
        };
        assert!(authority.workspace(&state, &workspace).is_ok());
        assert!(authority.workspace(&state, scratch.path()).is_err());
        assert!(authority.roots(scratch.path()).is_err());
        let link = workspace.join("escape");
        std::os::unix::fs::symlink(scratch.path(), &link).unwrap();
        assert!(authority.workspace(&state, &link).is_err());
        let mut config = Config::new(scratch.path(), "state");
        config.owned_workspace_roots = vec![scratch.path().into()];
        assert!(authority
            .runner(&config)
            .unwrap_err()
            .to_string()
            .contains("missing workspace root"));
    }
}
