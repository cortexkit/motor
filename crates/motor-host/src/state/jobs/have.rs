use super::*;
use motor_protocol::{
    Frame, GitDependencyCandidates, MirrorRef, MirrorRefReport, RepositoryCandidates,
};

// At most 4096 ref pins per disk, oldest first. Eviction only costs a full resend.
const MAX_HAVE_REFS: usize = 4096;

fn same_mirror(a: &MirrorRef, b: &MirrorRef) -> bool {
    a.repository == b.repository && a.dependency == b.dependency
}

fn known(reference: &MirrorRef) -> bool {
    reference.repository.is_some() != reference.dependency.is_some()
        && !reference.reference.is_empty()
        && !reference.commit.is_empty()
}

pub(crate) fn invalidate(snapshot: &mut super::super::Snapshot, job: &str) {
    if let Some(execution) = snapshot.per_job.executions.get_mut(job) {
        execution.have_invalidated = true;
        if let Some(disk) = snapshot.per_job.disks.get_mut(&execution.disk_id) {
            disk.have.clear();
        }
    }
}

pub(crate) fn transfer_terminal(
    snapshot: &mut super::super::Snapshot,
    job: &str,
    frame: &serde_json::Value,
) {
    if frame["outcome"]["type"] == "refused_before_start"
        && matches!(
            frame["outcome"]["reason"].as_str(),
            Some("transfer_interrupted" | "bundle_rejected")
        )
        && snapshot
            .leases
            .get(job)
            .is_some_and(|l| l.started_at_ms.is_none())
    {
        invalidate(snapshot, job);
    }
}

pub(crate) fn report(snapshot: &mut super::super::Snapshot, job: &str, report: &MirrorRefReport) {
    let execution = &snapshot.per_job.executions[job];
    let disk = &snapshot.per_job.disks[&execution.disk_id];
    // Never let a replay from an old VM repopulate a recovered or reused disk.
    if execution.ended || execution.have_invalidated || disk.attached_job.as_deref() != Some(job) {
        return;
    }
    if report.dropped_reports != 0 || report.refs.iter().any(|reference| !known(reference)) {
        invalidate(snapshot, job);
        return;
    }
    let disk = snapshot.per_job.disks.get_mut(&execution.disk_id).unwrap();
    for reference in &report.refs {
        disk.have.retain(|old| {
            !same_mirror(old, reference)
                || (old.reference != reference.reference
                    && (report.imported || old.commit != reference.commit))
        });
        if report.imported {
            disk.have.push(reference.clone());
        }
    }
    let excess = disk.have.len().saturating_sub(MAX_HAVE_REFS);
    disk.have.drain(..excess);
}

impl Host {
    pub(crate) fn job_have(
        &self,
        job: &str,
        mut repositories: Vec<RepositoryCandidates>,
        dependencies: Vec<String>,
    ) -> Frame {
        let disk = &self.snapshot.per_job.disks[&self.snapshot.per_job.executions[job].disk_id];
        let commits = |repository: Option<&str>, dependency: Option<&str>| -> Vec<String> {
            if disk.quarantined {
                return Vec::new();
            }
            disk.have
                .iter()
                .filter(|r| {
                    r.repository.as_deref() == repository && r.dependency.as_deref() == dependency
                })
                .map(|r| r.commit.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        for repository in &mut repositories {
            // Client candidates select a mirror, but never establish possession.
            repository.commits = commits(Some(&repository.repository), None);
        }
        Frame::HaveReply {
            repositories,
            git_dependencies: Some(
                dependencies
                    .into_iter()
                    .map(|url| GitDependencyCandidates {
                        commits: commits(None, Some(&url)),
                        url,
                    })
                    .collect(),
            ),
        }
    }

    pub(crate) fn job_transfer_refused(&mut self, job: &str) -> Result<(), Error> {
        if self
            .snapshot
            .leases
            .get(job)
            .is_some_and(|l| l.started_at_ms.is_none())
        {
            let mut next = self.snapshot.clone();
            invalidate(&mut next, job);
            self.save(next)?;
        }
        Ok(())
    }
}
