//! The boundary between wire/catalog spellings and host filesystem paths.
//!
//! Conversions are deliberately identity-only: caller and host share the same bytes.
//! Keeping both namespaces in the plans lets consumers choose their intent
//! without changing the bytes stored in existing records.
use crate::storage::{BaseRequest, Job, LegacyCheckout, RetainedTransfer, Workspace};
use motor_protocol::{Prepare, Run, SiblingSnapshot};
use std::path::Path;
pub(crate) mod catalog;
pub(crate) mod owner;
pub(crate) mod startup;
pub use startup::CallerRoot;
#[cfg(test)]
mod backstop_tests;
#[cfg(all(test, target_os = "linux"))]
mod startup_tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CallerPath<'a>(&'a String);

impl<'a> CallerPath<'a> {
    pub(crate) fn text(self) -> &'a String {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostPath<'a>(&'a Path);

impl<'a> HostPath<'a> {
    pub(crate) fn as_path(self) -> &'a Path {
        self.0
    }
}

pub(crate) fn host(path: &Path) -> HostPath<'_> {
    HostPath(path)
}

pub(crate) fn host_str(value: &str) -> HostPath<'_> {
    HostPath(Path::new(value))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PathPlan<'a> {
    pub(crate) caller: CallerPath<'a>,
    pub(crate) host: HostPath<'a>,
}

pub(crate) fn identity(value: &String) -> PathPlan<'_> {
    PathPlan {
        caller: CallerPath(value),
        host: HostPath(Path::new(value)),
    }
}

pub(crate) fn entries<T>(
    records: &std::collections::BTreeMap<String, T>,
) -> impl Iterator<Item = (PathPlan<'_>, &T)> {
    records.iter().map(|(key, record)| (identity(key), record))
}

pub(crate) fn keys<T>(
    records: &std::collections::BTreeMap<String, T>,
) -> impl Iterator<Item = PathPlan<'_>> {
    records.keys().map(identity)
}

pub(crate) fn entries_mut<T>(
    records: &mut std::collections::BTreeMap<String, T>,
) -> impl Iterator<Item = (PathPlan<'_>, &mut T)> {
    records
        .iter_mut()
        .map(|(key, record)| (identity(key), record))
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum WorkspacePlan<'a> {
    Workspace(PathPlan<'a>),
    Fill {
        /// An opaque job key, never a checkout or a mount destination.
        identifier: &'a str,
        checkout: PathPlan<'a>,
    },
}

impl<'a> WorkspacePlan<'a> {
    pub(crate) fn checkout(self) -> PathPlan<'a> {
        match self {
            Self::Workspace(path) | Self::Fill { checkout: path, .. } => path,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RunPlan<'a> {
    pub(crate) workspace: WorkspacePlan<'a>,
    pub(crate) caller_cwd: CallerPath<'a>,
    pub(crate) host_cwd: HostPath<'a>,
    pub(crate) caller_return_paths: &'a Vec<String>,
}

pub(crate) fn run(request: &Run) -> RunPlan<'_> {
    RunPlan {
        workspace: if request.workspace_key.starts_with("base:") {
            WorkspacePlan::Fill {
                identifier: &request.workspace_key,
                checkout: identity(&request.cwd),
            }
        } else {
            WorkspacePlan::Workspace(identity(&request.workspace_key))
        },
        caller_cwd: CallerPath(&request.cwd),
        host_cwd: HostPath(Path::new(&request.cwd)),
        caller_return_paths: &request.return_paths,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestPlan<'a> {
    pub(crate) caller_workspace_key: CallerPath<'a>,
    pub(crate) host_workspace: HostPath<'a>,
    pub(crate) caller_repository_root: CallerPath<'a>,
    pub(crate) host_repository_root: HostPath<'a>,
}

impl<'a> RequestPlan<'a> {
    pub(crate) fn workspace_path(self) -> PathPlan<'a> {
        PathPlan {
            caller: self.caller_workspace_key,
            host: self.host_workspace,
        }
    }
    pub(crate) fn repository_path(self) -> PathPlan<'a> {
        PathPlan {
            caller: self.caller_repository_root,
            host: self.host_repository_root,
        }
    }
}

pub(crate) trait Request<'a> {
    fn paths(self) -> RequestPlan<'a>;
}

pub(crate) fn request<'a>(value: impl Request<'a>) -> RequestPlan<'a> {
    value.paths()
}

impl<'a> Request<'a> for &&'a Run {
    fn paths(self) -> RequestPlan<'a> {
        (*self).paths()
    }
}

impl<'a> Request<'a> for &&'a Prepare {
    fn paths(self) -> RequestPlan<'a> {
        (*self).paths()
    }
}

impl<'a> Request<'a> for &'a Prepare {
    fn paths(self) -> RequestPlan<'a> {
        RequestPlan {
            caller_workspace_key: CallerPath(&self.workspace_key),
            host_workspace: HostPath(Path::new(&self.workspace_key)),
            caller_repository_root: CallerPath(&self.repository_root),
            host_repository_root: HostPath(Path::new(&self.repository_root)),
        }
    }
}

impl<'a> Request<'a> for &'a Run {
    fn paths(self) -> RequestPlan<'a> {
        RequestPlan {
            caller_workspace_key: CallerPath(&self.workspace_key),
            host_workspace: run(self).workspace.checkout().host,
            caller_repository_root: CallerPath(&self.repository_root),
            host_repository_root: HostPath(Path::new(&self.repository_root)),
        }
    }
}

pub(crate) fn sibling(sibling: &SiblingSnapshot) -> PathPlan<'_> {
    identity(&sibling.canonical_path)
}

pub(crate) fn drop_request(request: &motor_protocol::DropRequest) -> PathPlan<'_> {
    identity(&request.workspace_key)
}

pub(crate) fn job_key(job: &Job) -> &str {
    match job.request.as_ref() {
        Some(request) => match run(request).workspace {
            WorkspacePlan::Fill { identifier, .. } => identifier,
            WorkspacePlan::Workspace(path) => path.caller.text(),
        },
        None => "",
    }
}

pub(crate) fn job_workspace(job: &Job) -> HostPath<'_> {
    job.request
        .as_ref()
        .map(|r| request(r).host_workspace)
        .unwrap_or_else(|| host_str(""))
}

pub(crate) struct RecordPlan<'a> {
    pub(crate) repository: PathPlan<'a>,
    pub(crate) caller_siblings: &'a [SiblingSnapshot],
}

pub(crate) fn workspace_record(record: &Workspace) -> RecordPlan<'_> {
    RecordPlan {
        repository: identity(&record.repository),
        caller_siblings: &record.siblings,
    }
}

pub(crate) fn retained_transfer(record: &RetainedTransfer) -> RecordPlan<'_> {
    RecordPlan {
        repository: identity(&record.repository),
        caller_siblings: &record.siblings,
    }
}

pub(crate) fn legacy_checkout(record: &LegacyCheckout) -> PathPlan<'_> {
    identity(&record.repository)
}

pub(crate) fn base(record: &BaseRequest) -> PathPlan<'_> {
    identity(&record.workspace)
}

/// Each record pins only the mappings referenced by its own paths.
pub(crate) fn mappings<'a>(
    roots: &[CallerRoot],
    paths: impl IntoIterator<Item = &'a str>,
) -> Vec<CallerRoot> {
    let paths: Vec<_> = paths.into_iter().map(Path::new).collect();
    roots
        .iter()
        .filter(|root| {
            paths.iter().any(|path| {
                *path != Path::new(&root.caller_prefix) && path.starts_with(&root.caller_prefix)
            })
        })
        .cloned()
        .collect()
}

pub(crate) fn job_mappings(job: &Job, roots: &[CallerRoot]) -> Vec<CallerRoot> {
    let Some(request) = &job.request else {
        return vec![];
    };
    mappings(
        roots,
        [
            request.workspace_key.as_str(),
            request.repository_root.as_str(),
            request.cwd.as_str(),
        ]
        .into_iter()
        .chain(request.siblings.iter().map(|s| s.canonical_path.as_str())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_preserves_spelling_without_filesystem_access() {
        for text in ["/Users/u/./repo", "/Users//u/repo", "/Users/u/repo/"] {
            let value = text.to_owned();
            let plan = identity(&value);
            assert_eq!(plan.caller.text(), text);
            assert_eq!(plan.host.as_path().as_os_str(), text);
        }
    }

    #[test]
    fn fill_identifiers_never_become_checkout_paths() {
        let job: Job = serde_json::from_str(include_str!(
            "../test-fixtures/compatibility/job-record.json"
        ))
        .unwrap();
        let mut wire = job.request.unwrap();
        for (repository, checkout) in [
            ("/home/repo", "/home/.motor-base-main-repo"),
            ("/home/u/repo", "/home/u/.motor-base-main-repo"),
        ] {
            wire.repository_root = repository.into();
            wire.workspace_key = format!("base:{repository}");
            wire.cwd = checkout.into();
            let plan = run(&wire);
            match plan.workspace {
                WorkspacePlan::Fill {
                    identifier,
                    checkout: path,
                } => {
                    assert_eq!(identifier, format!("base:{repository}"));
                    assert_eq!(path.caller.text(), checkout);
                    assert_eq!(path.host.as_path(), Path::new(checkout));
                }
                WorkspacePlan::Workspace(_) => panic!("fill identifier became a workspace path"),
            }
            assert_eq!(request(&wire).host_workspace.as_path(), Path::new(checkout));
        }
    }
}
