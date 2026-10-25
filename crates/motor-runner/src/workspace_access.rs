//! The runner account's own read access to the caller checkouts it keeps on
//! this server.
//!
//! An isolated Linux job owns its workspace root while it runs: setup hands
//! the root to the job account so the job can create files there. The job can
//! therefore chmod the root to 0700, and on a directory that carries an ACL
//! that chmod rewrites the ACL mask rather than the group entry, so the
//! runner's named entry stays listed but grants nothing. The job can also
//! make any directory below the root unreadable.
//!
//! Admission and setup read inside the workspace before any privileged step
//! runs: they resolve symlinks in the caller's paths and list the workspace
//! for runner base checkouts. Those reads cannot become lexical checks,
//! because resolving symlinks is what keeps a caller path from escaping into
//! writer-owned storage. So a read that fails with a permission error on a
//! checkout under a runner-owned root goes through the runner's existing
//! access reclaim, the same privileged `setfacl` walk the next setup runs
//! anyway, and the validation is retried once per checkout. A failure that
//! survives the retry, or a checkout the runner may not repair, is reported
//! with the path, the step, the errno and the operator command that repairs
//! it, never as a bare `Permission denied`.

use crate::engine::Config;
use motor_protocol::SiblingSnapshot;
use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
};

/// Replaces the privileged reclaim in fixtures, which have neither sudo nor
/// the job account.
#[cfg(test)]
pub(crate) type RepairHook = std::sync::Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

/// A permission failure while the runner read a caller path, with the step
/// that read it.
#[derive(Debug)]
pub(crate) struct Unreadable {
    pub(crate) path: PathBuf,
    pub(crate) step: &'static str,
    source: io::Error,
}

impl Unreadable {
    /// Record which caller path and which step hit a permission failure.
    /// Any other error is returned unchanged.
    pub(crate) fn wrap(path: &Path, step: &'static str, error: io::Error) -> io::Error {
        if error.kind() != io::ErrorKind::PermissionDenied {
            return error;
        }
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            Self {
                path: path.to_path_buf(),
                step,
                source: error,
            },
        )
    }

    pub(crate) fn of(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref::<Self>()
    }

    fn errno(&self) -> &'static str {
        #[cfg(unix)]
        match self.source.raw_os_error() {
            Some(libc::EACCES) => return "EACCES",
            Some(libc::EPERM) => return "EPERM",
            _ => {}
        }
        "PermissionDenied"
    }
}

impl fmt::Display for Unreadable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot read {} while {}: {}",
            self.path.display(),
            self.step,
            self.source
        )
    }
}

impl std::error::Error for Unreadable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// What the runner did about one unreadable checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Repair {
    /// The reclaim ran and reported success.
    Restored,
    /// The reclaim ran and failed with this error.
    Failed(String),
    /// No reclaim was attempted, for this reason.
    NotAttempted(&'static str),
}

/// The repairs made for one request, keyed by checkout root: the workspace
/// or one of its siblings.
#[derive(Clone, Debug, Default)]
pub(crate) struct Repairs(BTreeMap<PathBuf, Repair>);

impl Repairs {
    /// Whether any checkout was repaired, so a validation that failed before
    /// may now pass.
    pub(crate) fn restored_any(&self) -> bool {
        self.0.values().any(|repair| *repair == Repair::Restored)
    }

    /// Repair the checkout that holds `path`, unless this request already
    /// tried. True only when a new repair succeeded, so validation is worth
    /// running again.
    fn attempt(
        &mut self,
        config: &Config,
        workspace: &str,
        siblings: &[SiblingSnapshot],
        path: &Path,
    ) -> bool {
        let Some(root) = checkout_root(workspace, siblings, path) else {
            return false;
        };
        if self.0.contains_key(Path::new(root)) {
            return false;
        }
        let repair = repair(config, root);
        let restored = repair == Repair::Restored;
        match &repair {
            Repair::Restored => {
                tracing::warn!(root, path = %path.display(), "restored runner access to an unreadable checkout")
            }
            Repair::Failed(error) => {
                tracing::warn!(root, path = %path.display(), error, "runner access repair failed")
            }
            Repair::NotAttempted(_) => {}
        }
        self.0.insert(PathBuf::from(root), repair);
        restored
    }

    /// The refusal text for a validation error: for an unreadable caller
    /// path it says that the runner's copy on the server is the side that
    /// failed, and who must act; any other error keeps its own text.
    pub(crate) fn describe(&self, error: &io::Error) -> String {
        let Some(unreadable) = Unreadable::of(error) else {
            return error.to_string();
        };
        let root = self
            .0
            .keys()
            .filter(|root| unreadable.path.starts_with(root))
            .max_by_key(|root| root.as_os_str().len());
        let remedy = match root.map(|root| (root, &self.0[root])) {
            Some((root, Repair::Restored)) => format!(
                "the runner restored its access to {}, but the path is still unreadable",
                root.display()
            ),
            Some((root, Repair::Failed(error))) => format!(
                "the runner could not restore its access to {}: {error}",
                root.display()
            ),
            Some((_, Repair::NotAttempted(reason))) => {
                format!("the runner did not repair it: {reason}")
            }
            None => "the runner did not repair it".to_string(),
        };
        let target = root.map_or(unreadable.path.as_path(), PathBuf::as_path);
        format!(
            "the runner's copy of the workspace on the build server is unreadable to the runner \
             account: path={} step={} errno={} ({}); {remedy}; retrying will not help until an \
             operator repairs it on the build server: {}",
            unreadable.path.display(),
            unreadable.step,
            unreadable.errno(),
            unreadable.source,
            operator_command(target),
        )
    }

    fn described(&self, error: io::Error) -> io::Error {
        if Unreadable::of(&error).is_none() {
            return error;
        }
        io::Error::new(io::ErrorKind::PermissionDenied, self.describe(&error))
    }
}

/// Run `validate`; when it fails because the runner cannot read a caller
/// checkout, repair that checkout and run it again, once per checkout. The
/// final error describes what was tried (see [`Repairs::describe`]).
pub(crate) fn validate_with_repair<T>(
    config: &Config,
    workspace: &str,
    siblings: &[SiblingSnapshot],
    validate: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut repairs = Repairs::default();
    validate_with_repairs(config, workspace, siblings, &mut repairs, validate)
        .map_err(|error| repairs.described(error))
}

/// As [`validate_with_repair`], but records the repairs in `repairs` and
/// returns the validation's own error, for a caller that validates again
/// later and describes that result.
pub(crate) fn validate_with_repairs<T>(
    config: &Config,
    workspace: &str,
    siblings: &[SiblingSnapshot],
    repairs: &mut Repairs,
    mut validate: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    loop {
        let error = match validate() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(path) = Unreadable::of(&error).map(|unreadable| unreadable.path.clone()) else {
            return Err(error);
        };
        // Each checkout is repaired at most once, so this ends after at most
        // one pass per checkout the request names.
        if !repairs.attempt(config, workspace, siblings, &path) {
            return Err(error);
        }
    }
}

/// The checkout the request names that holds `path`: the workspace or a
/// sibling, the deepest one if they nest.
fn checkout_root<'a>(
    workspace: &'a str,
    siblings: &'a [SiblingSnapshot],
    path: &Path,
) -> Option<&'a str> {
    std::iter::once(workspace)
        .chain(
            siblings
                .iter()
                .map(|sibling| sibling.canonical_path.as_str()),
        )
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.len())
}

fn repair(config: &Config, root: &str) -> Repair {
    // The runner owns a checkout by contract only beneath its owned
    // workspace roots, resolved; its writer storage lives there too, under
    // `.motor-base-` names, and caller paths must never reach it.
    if Path::new(root).components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .starts_with(".motor-base-")
    }) {
        return Repair::NotAttempted("the path names runner writer storage");
    }
    if crate::workspace::validate_destinations(&config.owned_workspace_roots, root, None, &[])
        .is_err()
    {
        return Repair::NotAttempted(
            "the runner could not confirm the path is beneath a runner-owned workspace root",
        );
    }
    #[cfg(test)]
    if let Some(hook) = &config.workspace_access_repair {
        return outcome(hook(Path::new(root)));
    }
    #[cfg(target_os = "linux")]
    if config.isolate_jobs {
        return outcome(crate::isolation::restore_workspace_access(Path::new(root)));
    }
    Repair::NotAttempted("this runner does not isolate jobs, so it holds no access repair")
}

#[cfg(any(test, target_os = "linux"))]
fn outcome(result: io::Result<()>) -> Repair {
    match result {
        Ok(()) => Repair::Restored,
        Err(error) => Repair::Failed(error.to_string()),
    }
}

/// The command an operator runs on the build server to give the runner back
/// read access to `root`. `setfacl -m` recalculates every ACL mask it writes,
/// so this also undoes a mask a job zeroed with chmod.
fn operator_command(root: &Path) -> String {
    #[cfg(unix)]
    let account = unsafe { libc::geteuid() }.to_string();
    #[cfg(not(unix))]
    let account = "<runner uid>".to_string();
    format!(
        "sudo setfacl -R -P -m u:{account}:rwX -- '{}'",
        root.display()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::fs;
    use std::sync::{Arc, Mutex};

    fn denied(path: &str) -> io::Error {
        Unreadable::wrap(
            Path::new(path),
            "listing",
            io::Error::from_raw_os_error(libc::EACCES),
        )
    }

    fn sibling(path: &str) -> SiblingSnapshot {
        SiblingSnapshot {
            canonical_path: path.to_string(),
            commit: String::new(),
            tree_hash: String::new(),
            base_commit: None,
        }
    }

    /// A config whose owned root is `<scratch>/remote` and whose repair hook
    /// records each checkout it is asked to repair.
    fn config(scratch: &ScratchDir) -> (Config, Arc<Mutex<Vec<PathBuf>>>) {
        let mut config = Config::new(scratch.join("state"), "e2e");
        config.owned_workspace_roots = vec![scratch.join("remote")];
        let repaired = Arc::new(Mutex::new(Vec::new()));
        let seen = repaired.clone();
        config.workspace_access_repair = Some(std::sync::Arc::new(move |root: &Path| {
            seen.lock().unwrap().push(root.to_path_buf());
            Ok(())
        }));
        (config, repaired)
    }

    fn text(path: PathBuf) -> String {
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn the_workspace_and_a_sibling_are_each_repaired_once_and_validation_retried() {
        let scratch = ScratchDir::new("workspace-access");
        let (config, repaired) = config(&scratch);
        let workspace = text(scratch.join("remote/worktrees/a"));
        let commons = text(scratch.join("remote/source/commons"));
        for path in [&workspace, &commons] {
            fs::create_dir_all(path).unwrap();
        }
        let mut failures = vec![denied(&commons), denied(&format!("{workspace}/src"))];
        let mut calls = 0;
        validate_with_repair(&config, &workspace, &[sibling(&commons)], || {
            calls += 1;
            failures.pop().map_or(Ok(()), Err)
        })
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(
            *repaired.lock().unwrap(),
            [PathBuf::from(&workspace), PathBuf::from(&commons)]
        );
    }

    #[test]
    fn checkouts_the_runner_does_not_own_by_contract_are_not_repaired() {
        let scratch = ScratchDir::new("workspace-access");
        let (config, repaired) = config(&scratch);
        for (workspace, reason) in [
            (
                scratch.join("elsewhere/a"),
                "could not confirm the path is beneath a runner-owned workspace root",
            ),
            (
                scratch.join("remote/worktrees/.motor-base-e2e-main"),
                "the path names runner writer storage",
            ),
        ] {
            fs::create_dir_all(&workspace).unwrap();
            let workspace = text(workspace);
            let error = validate_with_repair(&config, &workspace, &[], || {
                Err::<(), _>(denied(&workspace))
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            let detail = error.to_string();
            assert!(detail.contains(reason), "{detail}");
            assert!(detail.contains(&format!("path={workspace} ")), "{detail}");
            assert!(detail.contains("errno=EACCES"), "{detail}");
        }
        assert!(repaired.lock().unwrap().is_empty());
    }

    #[test]
    fn other_validation_errors_pass_through_without_a_repair() {
        let scratch = ScratchDir::new("workspace-access");
        let (config, repaired) = config(&scratch);
        let workspace = text(scratch.join("remote/worktrees/a"));
        let error = validate_with_repair(&config, &workspace, &[], || {
            Err::<(), _>(io::Error::other(
                "caller path overlaps writer-owned storage",
            ))
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "caller path overlaps writer-owned storage"
        );
        // A permission failure from anywhere but a caller-path read carries no
        // path, so it is not repaired either.
        let error = validate_with_repair(&config, &workspace, &[], || {
            Err::<(), _>(io::Error::from_raw_os_error(libc::EACCES))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
        assert!(repaired.lock().unwrap().is_empty());
    }
}
