//! Operator benchmark requests against objects already held in a runner mirror.
//! Snapshot capture is intentionally omitted; framing, Hello negotiation and
//! stream handling use the same transport as the production Motor client.
use crate::transport::{Channel, Incoming};
use crate::Connector;
use motor_protocol::{Frame, GitDependency};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::Path,
    sync::Arc,
};
use uuid::Uuid;

/// Read committed lockfiles with the production snapshot parser, merging pins
/// by URL when Prepare needs both the base and reader-tip dependency graphs.
pub fn dependencies(mirror: &Path, commits: &[String]) -> io::Result<Vec<GitDependency>> {
    let mut grouped: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for commit in commits {
        for (url, revisions) in crate::snapshot::lockfile_sources(mirror, commit)? {
            grouped.entry(url).or_default().extend(revisions);
        }
    }
    Ok(grouped
        .into_iter()
        .map(|(url, commits)| GitDependency {
            url,
            commits: commits.into_iter().collect(),
            bundle: None,
        })
        .collect())
}

pub struct Client {
    connector: Arc<dyn Connector>,
}

impl Client {
    pub fn new(connector: Arc<dyn Connector>) -> Self {
        Self { connector }
    }

    /// Execute one mirror-only operation on a fresh production channel.
    /// Run and Prepare must not require uploads: the operator preloads the mirror.
    pub async fn execute(&self, request: Frame) -> io::Result<Vec<Frame>> {
        let id = match &request {
            Frame::Run(run)
                if run.bundles.is_empty()
                    && run
                        .git_dependencies
                        .iter()
                        .all(|dependency| dependency.bundle.is_none()) =>
            {
                run.job_id
            }
            Frame::Prepare(prepare)
                if prepare.bundles.is_empty()
                    && prepare
                        .git_dependencies
                        .iter()
                        .all(|dependency| dependency.bundle.is_none()) =>
            {
                prepare.transfer_id
            }
            Frame::Have { .. } | Frame::Status | Frame::Drop(_) => Uuid::now_v7(),
            _ => {
                return Err(io::Error::other(
                    "benchmark requires a mirror-only operation",
                ))
            }
        };
        let dependencies = match &request {
            Frame::Run(run) => &run.git_dependencies[..],
            Frame::Prepare(prepare) => &prepare.git_dependencies[..],
            _ => &[],
        };
        if !dependencies.is_empty() {
            let held = self
                .exchange(
                    Frame::Have {
                        repositories: vec![],
                        git_dependencies: dependencies
                            .iter()
                            .map(|dependency| dependency.url.clone())
                            .collect(),
                    },
                    Uuid::now_v7(),
                )
                .await?;
            let candidates = match &held[0] {
                Frame::HaveReply {
                    git_dependencies, ..
                } => git_dependencies.as_deref().unwrap_or(&[]),
                _ => return Err(io::Error::other("unexpected dependency Have response")),
            };
            for dependency in dependencies {
                for commit in &dependency.commits {
                    if !candidates.iter().any(|candidate| {
                        candidate.url == dependency.url && candidate.commits.contains(commit)
                    }) {
                        return Err(io::Error::other(format!(
                            "git dependency {} missing commit {}; preload it in the runner before benchmarking",
                            dependency.url, commit)));
                    }
                }
            }
        }
        self.exchange(request, id).await
    }

    async fn exchange(&self, request: Frame, id: Uuid) -> io::Result<Vec<Frame>> {
        // A completed operation closes its server session. Never retain this
        // channel for the next status poll or reader job.
        let mut channel = Channel::open(self.connector.as_ref(), id)
            .await
            .map_err(|reason| io::Error::other(format!("runner Hello failed: {reason:?}")))?;
        channel.send(request).await?;
        let mut rows = Vec::new();
        loop {
            let incoming = channel.next().await?;
            let Incoming::Frame(frame) = incoming else {
                return Err(io::Error::other("unknown benchmark response"));
            };
            if let Frame::Refusal(ref refusal) = frame {
                return Err(io::Error::other(format!(
                    "runner refused request: {refusal:?}"
                )));
            }
            let done = matches!(
                frame,
                Frame::HaveReply { .. }
                    | Frame::Prepared { .. }
                    | Frame::StatusReply(_)
                    | Frame::DropReply(_)
                    | Frame::Terminal(_)
            );
            rows.push(frame);
            if done {
                return Ok(rows);
            }
        }
    }
}
