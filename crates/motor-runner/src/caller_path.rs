//! The boundary between wire/catalog spellings and host filesystem paths.
//!
//! Conversions are deliberately identity-only: caller and host share the same bytes.
//! Keeping both namespaces in the plans lets consumers choose their intent
//! without changing the bytes stored in existing records.
use crate::storage::{BaseRequest, Job, LegacyCheckout, RetainedTransfer, Workspace};
use motor_protocol::{Prepare, Run, SiblingSnapshot};
use std::path::Path;

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

pub(crate) trait Request {
    fn paths(&self) -> RequestPlan<'_>;
}

pub(crate) fn request(value: &impl Request) -> RequestPlan<'_> {
    value.paths()
}

impl<T: Request> Request for &T {
    fn paths(&self) -> RequestPlan<'_> {
        (**self).paths()
    }
}

impl Request for Prepare {
    fn paths(&self) -> RequestPlan<'_> {
        RequestPlan {
            caller_workspace_key: CallerPath(&self.workspace_key),
            host_workspace: HostPath(Path::new(&self.workspace_key)),
            caller_repository_root: CallerPath(&self.repository_root),
            host_repository_root: HostPath(Path::new(&self.repository_root)),
        }
    }
}

impl Request for Run {
    fn paths(&self) -> RequestPlan<'_> {
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

pub(crate) fn set_run_key(request: &mut Run, value: String) {
    request.workspace_key = value;
}

pub(crate) fn job_key(job: &Job) -> &str {
    job.request
        .as_ref()
        .map(|r| r.workspace_key.as_str())
        .unwrap_or("")
}

pub(crate) struct RecordPlan<'a> {
    pub(crate) repository: PathPlan<'a>,
    pub(crate) siblings: Vec<PathPlan<'a>>,
}

pub(crate) fn workspace_record(record: &Workspace) -> RecordPlan<'_> {
    RecordPlan {
        repository: identity(&record.repository),
        siblings: record.siblings.iter().map(sibling).collect(),
    }
}

pub(crate) fn retained_transfer(record: &RetainedTransfer) -> RecordPlan<'_> {
    RecordPlan {
        repository: identity(&record.repository),
        siblings: record.siblings.iter().map(sibling).collect(),
    }
}

pub(crate) fn legacy_checkout(record: &LegacyCheckout) -> PathPlan<'_> {
    identity(&record.repository)
}

pub(crate) fn base(record: &BaseRequest) -> PathPlan<'_> {
    identity(&record.workspace)
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
}
