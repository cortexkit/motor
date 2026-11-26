use super::*;

// Keep gates responsive without letting a steady stream of them consume an
// ordinary job's entire queue-wait budget.
const GATE_PRIORITY_WAIT_MS: u64 = 10 * 60 * 1_000;

fn priority(job: &Job, now: u64) -> bool {
    !job.base && (job.gate_job() || job.queue_wait_ms(now) >= GATE_PRIORITY_WAIT_MS)
}

fn past_backfill_grace(job: &Job, now: u64) -> bool {
    now.saturating_sub(job.queued_ms.unwrap_or(job.accepted_ms)) >= BACKFILL_GRACE_MS
}

pub(super) fn foreground_stops_backfill(job: &Job, now: u64) -> bool {
    !job.base && (priority(job, now) || past_backfill_grace(job, now))
}

fn workspace_head<'a>(inner: &'a Inner, queue: &[Uuid], now: u64) -> Option<&'a Job> {
    queue
        .iter()
        .map(|id| &inner.jobs[id])
        .min_by_key(|job| (!priority(job, now), job.order, job.id))
}

pub(super) fn heads_ahead(inner: &Inner, heads: &[Uuid], job: &Job, now: u64) -> usize {
    priority_heads_ahead(inner, heads, job, now).unwrap_or_else(|| {
        heads
            .iter()
            .position(|id| *id == job.id)
            .unwrap_or(heads.len())
    })
}

// A priority job can be second in its own workspace. In that case looking
// for its ID in the heads would report later gate jobs as ahead of it.
pub(super) fn priority_heads_ahead(
    inner: &Inner,
    heads: &[Uuid],
    job: &Job,
    now: u64,
) -> Option<usize> {
    (job.state == JobState::Queued && !inner.starting.contains(&job.id) && priority(job, now)).then(
        || {
            heads.partition_point(|id| {
                let head = &inner.jobs[id];
                priority(head, now) && (head.order, *id) < (job.order, job.id)
            })
        },
    )
}

impl DispatchIndex {
    /// Whether a queued workspace job holds `job` back from starting.
    ///
    /// A sibling is checked out in place at its canonical path, the same host
    /// directory a job whose main workspace is that path uses, so the two
    /// cannot run together. Jobs that use the path as a sibling at one commit
    /// may overlap one another, so under steady load they would keep the
    /// directory busy and the main workspace job would never start. The next
    /// job to start on a workspace therefore holds back every job that
    /// declares an overlapping sibling and was accepted after it. Holding
    /// only later arrivals keeps the waits acyclic: two repositories that
    /// name each other as siblings cannot hold each other back. Cache-fill
    /// jobs and jobs that can never fit the runner do not hold anyone. With
    /// snapshot siblings there is no shared checkout, so nobody is held.
    pub(super) fn held_by_queued_workspace(
        &self,
        inner: &Inner,
        job: &Job,
        max_weight: u32,
        now: u64,
    ) -> bool {
        if !self.in_place {
            return false;
        }
        let Some(request) = job.request.as_ref() else {
            return false;
        };
        if request.siblings.is_empty() {
            return false;
        }
        self.queued.iter().any(|(key, queue)| {
            request
                .siblings
                .iter()
                .any(|sibling| Self::overlap(key, paths::sibling(sibling).caller.text()))
                && workspace_head(inner, queue, now).is_some_and(|head| {
                    head.id != job.id
                        && !head.base
                        && head.weight <= max_weight
                        && head.order < job.order
                })
        })
    }

    /// Choose each workspace's head by priority and arrival. Dispatch tries
    /// priority heads first, then cache fills for the repositories whose
    /// foreground jobs arrived most recently, then heads queued for at least
    /// `BACKFILL_GRACE_MS`, then the rest, rotating between workspaces.
    pub(super) fn heads(&self, inner: &Inner, now: u64) -> Vec<Uuid> {
        let mut heads: Vec<_> = self
            .queued
            .values()
            .filter_map(|queue| workspace_head(inner, queue, now))
            .map(|job| {
                if priority(job, now) {
                    (0, job.order, 0, job.id)
                } else if job.base {
                    let newest = job
                        .request
                        .as_ref()
                        .and_then(|request| {
                            self.newest_arrivals
                                .get(paths::request(request).caller_repository_root.text())
                        })
                        .copied()
                        .unwrap_or(0);
                    (1, u64::MAX - newest, job.order, job.id)
                } else if past_backfill_grace(job, now) {
                    (2, job.order, 0, job.id)
                } else {
                    (
                        3,
                        inner
                            .rotation
                            .get(crate::caller_path::job_key(job))
                            .copied()
                            .unwrap_or(0),
                        job.order,
                        job.id,
                    )
                }
            })
            .collect();
        heads.sort();
        heads.into_iter().map(|(_, _, _, id)| id).collect()
    }
}
