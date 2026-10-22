//! Sibling snapshots: immutable per-commit copies of a sibling repository that
//! isolated Linux jobs will mount read-only at the sibling's canonical path P.
//! Today a sibling is checked out in place at P, which is also that
//! repository's own main workspace, so the repository's own jobs and the jobs
//! that use it as a sibling exclude each other. With snapshots, isolated Linux
//! jobs mount a read-only copy at P, and P is only ever a main workspace.
//!
//! This module holds the parts that change no behaviour yet: the mode
//! predicates, the snapshot key and the store that creates snapshots. Nothing
//! creates, mounts or removes snapshots while `Config::sibling_snapshot_mode`
//! is false, and production builds keep it false; only tests override it.

pub(crate) mod freshness;
#[cfg(any(target_os = "linux", test))]
pub(crate) mod helper;
pub(crate) mod store;

#[cfg(test)]
pub(crate) use store::AccessHook;

use crate::engine::Config;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, ffi::OsString, io, path::PathBuf};

/// Directory under the instance state that holds sibling snapshots, one
/// subdirectory per repository named like its mirror.
pub(crate) const SNAPSHOTS_DIRECTORY: &str = "sibling-snapshots";

/// Whether this build mounts siblings from snapshots on isolated Linux runners.
/// It turns on only together with the fencing changes and snapshot eviction,
/// so no deployed runner creates snapshots it cannot also bound and remove.
const SNAPSHOT_MOUNTS_ENABLED: bool = false;

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
        SNAPSHOT_MOUNTS_ENABLED && self.isolate_jobs && !self.windows_instance()
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

#[cfg(test)]
#[path = "sibling_snapshots_tests.rs"]
mod tests;
