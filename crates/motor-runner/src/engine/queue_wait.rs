use super::*;
use std::cell::RefCell;

pub(super) fn expired(job: &Job, now: u64, setup_wait: u64) -> bool {
    job.state == JobState::Queued
        && job.queued_ms.is_some()
        && job.request.as_ref().is_some_and(|request| {
            job.queue_wait_ms(now).saturating_add(setup_wait)
                >= request.queue_wait_limit_s.saturating_mul(1000)
        })
}

/// Whether dispatch should refuse `id` instead of starting it: its queue wait
/// has reached its limit while it waited behind other work. A job that can
/// start the first time it is considered still starts, even with a zero
/// budget: it never waited, so there is no late turn to refuse. Being in the
/// waiting history is the evidence that it was held back; that history is
/// in memory only, so after a runner restart the caller's own withdrawal is
/// what ends an expired job, as before this check existed.
pub(super) fn turn_came_too_late(inner: &Inner, id: Uuid, now: u64) -> bool {
    expired(&inner.jobs[&id], now, 0) && inner.queue_wait_history.0.borrow().contains_key(&id)
}

/// The reply to a withdrawal that found nothing left to withdraw. Dispatch can
/// refuse a job for queue wait while the caller's own withdrawal is on its way,
/// and an earlier withdrawal may have refused it already. Either way the job
/// was withdrawn, so say so, matching its terminal record; anything else has
/// started or ended for another reason.
pub(super) fn settled_withdraw_reply(job: Option<&Job>) -> WithdrawOutcome {
    let refused_for_queue_wait =
        job.and_then(|job| job.terminal.as_ref())
            .is_some_and(|terminal| {
                terminal.outcome
                    == Outcome::RefusedBeforeStart {
                        reason: RefusalReason::QueueWaitExceeded,
                    }
            });
    if refused_for_queue_wait {
        WithdrawOutcome::Withdrawn
    } else {
        WithdrawOutcome::Started
    }
}

#[derive(Default)]
pub(super) struct QueueWaitHistory(RefCell<BTreeMap<Uuid, QueueWaitDetail>>);

// The holders, reservations and queue order are shared by all jobs observed
// in one pass. Store each job's prefix length instead of copying its heads.
struct QueueWaitView {
    seen_ms: u64,
    held_permits: u32,
    holders: String,
    reservations: String,
    heads: Vec<String>,
}

struct QueueWaitDetail {
    view: Arc<QueueWaitView>,
    heads_ahead: usize,
    phase: String,
    annotations: String,
}

impl QueueWaitView {
    fn new(inner: &Inner, index: &DispatchIndex, heads: &[Uuid], now: u64) -> Self {
        let holders: Vec<_> = index
            .holder_ids
            .iter()
            .map(|id| {
                let job = &inner.jobs[id];
                let key = paths::job_key(job);
                let phase = if inner.starting.contains(id) {
                    "setup"
                } else {
                    "running"
                };
                let since_ms = job
                    .started_ms
                    .or_else(|| inner.reserved.get(key).map(|r| r.since_ms))
                    .unwrap_or(job.accepted_ms);
                format!(
                    "job={id} workspace={key} permits={} phase={phase} since_ms={since_ms}",
                    job.weight
                )
            })
            .collect();
        let reservations: Vec<_> = inner
            .reserved
            .iter()
            .map(|(key, r)| format!("workspace={key} since_ms={}", r.since_ms))
            .collect();
        let heads = heads
            .iter()
            .map(|id| {
                let job = &inner.jobs[id];
                format!(
                    "job={id} permits={} accepted_ms={}",
                    job.weight, job.accepted_ms
                )
            })
            .collect();
        Self {
            seen_ms: now,
            held_permits: index.weight,
            holders: holders.join("; "),
            reservations: reservations.join("; "),
            heads,
        }
    }
}

impl QueueWaitDetail {
    fn render(&self, job: &Job, max_weight: u32, last_seen: bool) -> String {
        // Keep the timestamp ahead of the lists, which can be truncated when
        // the detail is prepared for the caller.
        let seen = if last_seen {
            format!("; last_seen_ms={}", self.view.seen_ms)
        } else {
            String::new()
        };
        format!(
            "waiting_on={}{seen}{}; requested_permits={}; held_permits={}/{}; accepted_ms={}; holders=[{}]; reservations=[{}]; heads_ahead=[{}]",
            self.phase,
            self.annotations,
            job.weight,
            self.view.held_permits,
            max_weight,
            job.accepted_ms,
            self.view.holders,
            self.view.reservations,
            self.view.heads[..self.heads_ahead].join("; "),
        )
    }
}

impl Runner {
    pub(super) fn dispatch_pending<'a>(
        &'a self,
        mut inner: crate::state_lock::StateGuard<'a, Inner>,
        mut now: u64,
    ) -> io::Result<()> {
        while let Some(id) = self.dispatch(&mut inner, now)? {
            // Withdrawal takes the job log before the state lock and saves
            // with the state lock released. Rebuild the index afterwards:
            // other jobs may have queued or finished during the save.
            drop(inner);
            self.withdraw_for(id, None, false)?;
            inner = self.core.inner.lock().unwrap();
            now = self.now();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn queue_wait_detail(&self, inner: &Inner, id: Uuid) -> String {
        self.queue_wait_detail_with_index(inner, &DispatchIndex::build(inner), id)
    }

    pub(super) fn queue_wait_detail_with_index(
        &self,
        inner: &Inner,
        index: &DispatchIndex,
        id: Uuid,
    ) -> String {
        let Some(job) = inner.jobs.get(&id) else {
            return "unknown job".into();
        };
        let now = self.now();
        let heads = index.heads(inner, now);
        let heads_ahead = heads
            .iter()
            .position(|head| *head == id)
            .unwrap_or(heads.len());
        let has_blockers = heads_ahead > 0
            || index.holder_ids.iter().any(|holder| *holder != id)
            || inner
                .reserved
                .keys()
                .any(|key| !inner.starting.contains(&id) || key != paths::job_key(job));
        let max_weight = self.core.config.max_weight;
        if !has_blockers {
            // A holder can finish before we write the refusal. A Windows boot
            // can also be the only live reservation, which doesn't explain
            // what held the refused job before it got the slot.
            if let Some(last) = inner.queue_wait_history.0.borrow().get(&id) {
                return last.render(job, max_weight, true);
            }
        }
        let (phase, annotations) = self.queue_wait_reason(inner, index, id, || {
            self.blocking_head(inner, &heads, id, index.weight, now)
        });
        QueueWaitDetail {
            view: Arc::new(QueueWaitView::new(inner, index, &heads, now)),
            heads_ahead,
            phase,
            annotations,
        }
        .render(job, max_weight, false)
    }

    fn queue_wait_reason(
        &self,
        inner: &Inner,
        index: &DispatchIndex,
        id: Uuid,
        blocking_head: impl FnOnce() -> Option<(Uuid, bool)>,
    ) -> (String, String) {
        let (phase, blocking) = self.queue_wait_label_with(inner, index, id, blocking_head);
        let annotations = if let Some((head, _)) = blocking {
            format!(
                "; blocked_by=job={head} permits={}",
                inner.jobs[&head].weight
            )
        } else if phase == SIBLING_USER_WAIT {
            // The holders list alone doesn't say which other jobs use this checkout.
            let users: Vec<_> = index
                .checkout_sibling_users(crate::caller_path::job_key(&inner.jobs[&id]))
                .into_iter()
                .map(|(user, weight)| {
                    let key = inner
                        .jobs
                        .get(&user)
                        .map_or("unknown", |j| paths::job_key(j));
                    format!("job={user} workspace={key} permits={weight}")
                })
                .collect();
            format!("; sibling_users=[{}]", users.join("; "))
        } else {
            String::new()
        };
        (phase, annotations)
    }

    pub(super) fn remember_queue_wait(
        &self,
        inner: &Inner,
        index: &DispatchIndex,
        now: u64,
        heads: &[Uuid],
        blockers: &BTreeMap<Uuid, Option<(Uuid, bool)>>,
        last_blocker: Option<(Uuid, bool)>,
    ) {
        let mut history = inner.queue_wait_history.0.borrow_mut();
        // Keep starting jobs too: a Windows job may still be withdrawn during setup.
        history.retain(|id, _| {
            inner
                .jobs
                .get(id)
                .is_some_and(|job| job.state == JobState::Queued && job.terminal.is_none())
        });
        let positions: BTreeMap<_, _> = heads.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let mut view = None;
        for id in index.queued.values().flatten().copied() {
            let heads_ahead = positions.get(&id).copied().unwrap_or(heads.len());
            if heads_ahead == 0 && index.holder_ids.is_empty() && inner.reserved.is_empty() {
                continue;
            }
            let (phase, annotations) = self.queue_wait_reason(inner, index, id, || {
                blockers.get(&id).copied().unwrap_or(last_blocker)
            });
            let view =
                view.get_or_insert_with(|| Arc::new(QueueWaitView::new(inner, index, heads, now)));
            history.insert(
                id,
                QueueWaitDetail {
                    view: view.clone(),
                    heads_ahead,
                    phase,
                    annotations,
                },
            );
        }
    }
}
