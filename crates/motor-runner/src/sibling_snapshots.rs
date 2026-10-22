//! Sibling snapshots: immutable per-commit copies of a sibling repository that
//! isolated Linux jobs will mount read-only at the sibling's canonical path P.
//! Today a sibling is checked out in place at P, which is also that
//! repository's own main workspace, so the repository's own jobs and the jobs
//! that use it as a sibling exclude each other. With snapshots, isolated Linux
//! jobs mount a read-only copy at P, and P is only ever a main workspace.
//!
//! This module holds the mode predicates, the snapshot key, the store that
//! creates snapshots, the switch touch that keeps Cargo's freshness checks
//! correct across snapshots, and the per-job setup state that ties them
//! together. Nothing creates or mounts snapshots while
//! `Config::sibling_snapshot_mode` is false: on non-isolated runners and on
//! the Windows instance.

pub(crate) mod freshness;
#[cfg(any(target_os = "linux", test))]
pub(crate) mod helper;
pub(crate) mod store;

#[cfg(test)]
pub(crate) use store::AccessHook;

use crate::{engine::Config, storage::Catalog, workspace::Trees};
use motor_protocol::SiblingSnapshot;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, ffi::OsString, io, path::PathBuf, time::SystemTime};

/// Directory under the instance state that holds sibling snapshots, one
/// subdirectory per repository named like its mirror.
pub(crate) const SNAPSHOTS_DIRECTORY: &str = "sibling-snapshots";

/// Which sibling fences scheduling applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SiblingFences {
    /// Siblings are checked out at their host path P, which may also be that
    /// repository's main workspace: every sibling fence applies.
    InPlace,
    /// Siblings are mounted from snapshots and never touch P. Only a running
    /// job whose record has no `sibling_sources` still holds its siblings'
    /// paths: an older runner started it with P itself bound into its unit,
    /// so a main-workspace job must not check P out until it ends.
    Legacy,
    /// No sibling fence applies: the Windows instance gives each job its own
    /// staging export of every sibling and never uses a host path.
    None,
}

impl Config {
    /// True when jobs see each sibling from a snapshot directory instead of a
    /// checkout at its host path. Every step that differs between the two modes
    /// is gated on this predicate. Only isolated Linux runners can mount a
    /// snapshot over P; the Windows instance stages its own per-job exports.
    pub(crate) fn sibling_snapshot_mode(&self) -> bool {
        #[cfg(test)]
        if let Some(mode) = self.sibling_snapshot_mode_override {
            return mode;
        }
        self.isolate_jobs && !self.windows_instance()
    }

    /// True when siblings are checked out at their host path P, which may also
    /// be that repository's main workspace, so the sibling fences between those
    /// jobs are needed. Non-isolated runners (the macOS development runner and
    /// portable fixtures) stay in this mode. The Windows instance never checks
    /// a sibling out at P: it copies each one into a per-job staging export.
    pub(crate) fn in_place_siblings(&self) -> bool {
        #[cfg(test)]
        if let Some(in_place) = self.in_place_siblings_override {
            return in_place;
        }
        !self.sibling_snapshot_mode() && !self.windows_instance()
    }

    /// The sibling fences this runner applies. The legacy term is never
    /// evaluated on the Windows instance, whose jobs never bind a host path.
    pub(crate) fn sibling_fences(&self) -> SiblingFences {
        if self.in_place_siblings() {
            SiblingFences::InPlace
        } else if self.windows_instance() {
            SiblingFences::None
        } else {
            SiblingFences::Legacy
        }
    }

    /// `<state>/sibling-snapshots`, the root of every repository's snapshots.
    pub(crate) fn sibling_snapshots_dir(&self) -> PathBuf {
        self.state_dir().join(SNAPSHOTS_DIRECTORY)
    }
}

/// Digest of the runner-side inputs of the JavaScript install cache key: the
/// package manager versions, the Node version, the OS and architecture, and
/// the daemon environment that selects dependencies. A snapshot holds the
/// `node_modules` installed when it was created, so a change in any of these
/// must give a new snapshot rather than reuse an install made under the old
/// environment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct JavascriptEnvironment(String);

impl JavascriptEnvironment {
    /// `managers` maps each package manager name to its `--version` output.
    /// `selection_environment` is the sorted list of dependency-selection
    /// variables, as `isolation::javascript_selection_environment` returns it.
    pub(crate) fn digest(
        managers: &BTreeMap<String, Vec<u8>>,
        node_version: &[u8],
        selection_environment: &[(OsString, OsString)],
    ) -> Self {
        // Each field is length-prefixed so adjacent fields cannot run together.
        fn field(hash: &mut Sha256, bytes: &[u8]) {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let mut hash = Sha256::new();
        field(&mut hash, b"motor-sibling-snapshot-javascript-v1");
        field(&mut hash, std::env::consts::OS.as_bytes());
        field(&mut hash, std::env::consts::ARCH.as_bytes());
        field(&mut hash, node_version);
        field(&mut hash, &(managers.len() as u64).to_le_bytes());
        for (manager, version) in managers {
            field(&mut hash, manager.as_bytes());
            field(&mut hash, version);
        }
        field(
            &mut hash,
            &(selection_environment.len() as u64).to_le_bytes(),
        );
        for (name, value) in selection_environment {
            field(&mut hash, name.as_encoded_bytes());
            field(&mut hash, value.as_encoded_bytes());
        }
        Self(format!("{:x}", hash.finalize()))
    }

    /// The digest for this daemon's own dependency-selection environment.
    #[cfg(target_os = "linux")]
    pub(crate) fn of_runner(managers: &BTreeMap<String, Vec<u8>>, node_version: &[u8]) -> Self {
        Self::digest(
            managers,
            node_version,
            &crate::isolation::javascript_selection_environment(),
        )
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The digest for this runner as it is now: the installed package
    /// managers' and Node's versions and the daemon's dependency-selection
    /// environment. Only isolated Linux runners create snapshots.
    pub(crate) fn current(config: &Config) -> io::Result<Self> {
        #[cfg(test)]
        if let Some(environment) = &config.javascript_environment_override {
            return Ok(environment.clone());
        }
        let _ = config;
        #[cfg(target_os = "linux")]
        {
            let (managers, node_version) = crate::isolation::javascript_runner_versions();
            Ok(Self::of_runner(&managers, &node_version))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sibling snapshots need an isolated Linux runner",
            ))
        }
    }
}

/// Identity of one sibling snapshot: the sibling repository's canonical path,
/// the commit checked out, the caller commit the snapshot was made for, and
/// the JavaScript environment its `node_modules` were installed under.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SnapshotKey {
    repository: String,
    commit: String,
    caller: String,
    javascript_environment: JavascriptEnvironment,
}

impl SnapshotKey {
    /// Refuses a `commit` or `caller` that is not a full 40- or 64-character
    /// lowercase hexadecimal object ID. Request validation checks sibling paths
    /// only, not revisions, and both values become part of a directory name: a
    /// short or symbolic revision could name a different commit later, and an
    /// uppercase spelling would give one commit two snapshots.
    pub(crate) fn new(
        repository: &str,
        commit: &str,
        caller: &str,
        javascript_environment: &JavascriptEnvironment,
    ) -> io::Result<Self> {
        for (what, value) in [("commit", commit), ("caller commit", caller)] {
            if !full_object_id(value) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "sibling {repository} {what} {value:?} is not a full 40- or \
                         64-character lowercase hexadecimal object ID"
                    ),
                ));
            }
        }
        Ok(Self {
            repository: repository.to_string(),
            commit: commit.to_string(),
            caller: caller.to_string(),
            javascript_environment: javascript_environment.clone(),
        })
    }

    pub(crate) fn commit(&self) -> &str {
        &self.commit
    }

    /// The snapshot's directory name within its repository's directory:
    /// `<commit>-<environment>` when the caller commit is the commit itself,
    /// otherwise `<commit>-<caller>-<environment>`. Every name starts with its
    /// commit, so the snapshots of a commit can be found without reading them.
    /// The two forms have a different number of `-`-separated parts, so no
    /// name can be read both ways.
    pub(crate) fn directory_name(&self) -> String {
        let environment = self.javascript_environment.as_str();
        if self.caller == self.commit {
            format!("{}-{environment}", self.commit)
        } else {
            format!("{}-{}-{environment}", self.commit, self.caller)
        }
    }

    /// `<state>/sibling-snapshots/<mirror name>/<directory name>`. The
    /// repository's directory has the same name as its mirror, so retiring a
    /// repository can find both from one name.
    pub(crate) fn directory(&self, config: &Config) -> PathBuf {
        config
            .sibling_snapshots_dir()
            .join(crate::workspace::name(&self.repository))
            .join(self.directory_name())
    }
}

/// A full SHA-1 (40) or SHA-256 (64) Git object ID in lowercase hexadecimal,
/// the form `git rev-parse` prints.
fn full_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The sibling snapshot part of one job's setup in snapshot mode, from the
/// workspace reservation to the launch. It follows the setup contract of
/// `freshness`: the workspace's prior record and the wall-clock start are
/// taken when the workspace is reserved, the snapshots are resolved and
/// touched in the setup thread, and the record updates are applied only once
/// the setup has completed.
pub(crate) struct JobSiblings {
    prior: freshness::Prior,
    started: SystemTime,
    /// One per declared sibling, in request order, once resolved.
    resolved: Vec<freshness::Pending>,
}

impl JobSiblings {
    /// Called under the state lock when the job's workspace is reserved.
    pub(crate) fn reserve(catalog: &Catalog, workspace: &str) -> Self {
        Self {
            prior: freshness::Prior::of(catalog, workspace),
            // Wall clock, not the runner clock: the switch touch compares it
            // with file mtimes.
            started: SystemTime::now(),
            resolved: Vec::new(),
        }
    }

    /// Resolves or creates each sibling's snapshot and touches it for this
    /// workspace. The caller holds the workspace's reservation and no lock.
    pub(crate) fn resolve(
        &mut self,
        config: &Config,
        store: &store::SnapshotStore,
        trees: &Trees,
        siblings: &[SiblingSnapshot],
    ) -> io::Result<()> {
        if siblings.is_empty() {
            return Ok(());
        }
        let environment = JavascriptEnvironment::current(config)?;
        self.resolved = freshness::prepare_siblings(
            config,
            store,
            trees,
            &self.prior,
            self.started,
            siblings,
            &environment,
        )?;
        Ok(())
    }

    /// Each resolved sibling as (snapshot directory, canonical path), the
    /// pairs the sandbox and the Cargo prefetch mount.
    #[cfg(target_os = "linux")]
    pub(crate) fn mounts(&self) -> Vec<(PathBuf, PathBuf)> {
        self.resolved
            .iter()
            .map(|pending| {
                (
                    pending.snapshot().to_path_buf(),
                    PathBuf::from(pending.sibling()),
                )
            })
            .collect()
    }

    /// The snapshot directories the job's unit binds, for its record's
    /// `sibling_sources`.
    pub(crate) fn sources(&self) -> Vec<String> {
        self.resolved
            .iter()
            .map(|pending| pending.snapshot().to_string_lossy().into_owned())
            .collect()
    }

    /// Stores the touched siblings' freshness records in the workspace's
    /// catalog entry. Call this only for a completed setup, after the
    /// workspace was recorded.
    pub(crate) fn apply(self, catalog: &mut Catalog, workspace: &str) {
        for pending in self.resolved {
            if !pending.apply(catalog, workspace) {
                tracing::warn!(
                    workspace,
                    "set-up workspace has no catalog entry for its sibling freshness record"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "sibling_snapshots_tests.rs"]
mod tests;
