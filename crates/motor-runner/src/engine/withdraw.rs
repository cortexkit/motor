//! Withdrawing a queued job: the client takes it back before it starts,
//! either at its queue deadline, after an upload stall, or to move it to
//! another runner.

use super::*;

impl Runner {
    #[cfg(test)]
    pub(crate) fn withdraw(&self, id: Uuid) -> io::Result<WithdrawOutcome> {
        self.withdraw_for(id, None, false)
    }
    pub(crate) fn withdraw_for(
        &self,
        id: Uuid,
        reason: Option<RefusalReason>,
        spill: bool,
    ) -> io::Result<WithdrawOutcome> {
        self.ensure_job_readable(id)?;
        let log = self.job_log(id);
        let _write = log.lock_write();
        let mut inner = self.core.inner.lock().unwrap();
        let now = self.now();
        let index = DispatchIndex::build(&inner);
        // The refusal records queue wait through this withdrawal.
        self.update_queue_wait_clocks(&inner, now, &index);
        let stalled = reason == Some(RefusalReason::Unknown("transfer_stalled".into()));
        let queue_move = reason
            == Some(RefusalReason::Unknown(
                motor_protocol::QUEUE_MOVE_REASON.into(),
            ));
        let eligible = inner.jobs.get(&id).is_some_and(|job| {
            if queue_move {
                // Check the pending count and dispatch reservation under the same
                // lock used to pick the next job. The client's probe can be stale.
                return !spill
                    && job.state == JobState::Queued
                    && job.queued_ms.is_some()
                    && job.request.is_some()
                    && !inner.starting.contains(&id)
                    && !inner.terminal_saves.contains(&id)
                    && !self.core.config.cleanup_pending(id)
                    && !queue_wait::expired(job, now, 0)
                    && Self::pending_count(&inner) == 1;
            }
            (!inner.starting.contains(&id) || self.core.config.vm_instance())
                && job.pending()
                && if stalled {
                    job.state == JobState::AwaitingBundles
                } else {
                    // Windows VM boot stays withdrawable after setup reserves the job.
                    // Keep that setup interval separate from paused workspace-owner time.
                    let setup_wait =
                        if self.core.config.vm_instance() && inner.starting.contains(&job.id) {
                            now.saturating_sub(job.setup_ms.unwrap_or(now))
                        } else {
                            0
                        };
                    queue_wait::expired(job, now, setup_wait)
                }
        });
        let Some(mut job) = inner.jobs.get(&id).filter(|_| eligible).cloned() else {
            return Ok(if queue_move {
                WithdrawOutcome::Started
            } else {
                queue_wait::settled_withdraw_reply(inner.jobs.get(&id))
            });
        };
        let detail = if stalled {
            "transfer_stalled".into()
        } else if queue_move {
            "withdrawn to move to another runner".into()
        } else {
            self.queue_wait_detail_with_index(&inner, &index, id)
        };
        if self.core.config.vm_instance() && inner.starting.contains(&id) {
            self.core.backend.cancel(&self.core.config, &job, true)?;
        }
        // The terminal below stops the queue-wait clock this copy shares
        // with the queued job; kept to restore exactly if the save fails.
        let clock = QueueClock::of(&job);
        let safe_detail = self.safe_refusal_detail(
            &detail,
            job.request
                .as_ref()
                .into_iter()
                .flat_map(|request| request.env.values().cloned()),
        );
        // Why a caller was sent back while the runner could not start it: a
        // job waiting for enough free permits, or for another job to release
        // its workspace or its checkout, may be able to start once that passes.
        // `queue_wait_phase` names the phase from the same dispatch index, so a
        // job behind one that can never fit keeps no hint.
        let retry_after_ms = (!queue_move)
            .then(|| self.queue_clear_retry_after_ms(&inner, &index, id))
            .flatten();
        if spill && !stalled {
            // Keep the legacy outcome so old clients know the job never started,
            // but do not count a move to hosted capacity as a runner refusal.
            let detail = format!(
                "{}{safe_detail}",
                motor_protocol::SPILL_WITHDRAWAL_DETAIL_PREFIX
            );
            tracing::info!(job_id = %id, detail, "job spill_withdrawal");
            self.core
                .health
                .lock()
                .unwrap()
                .record_spill_withdrawal(self.now());
            job.terminal_ended(
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::QueueWaitExceeded,
                },
                self.now(),
            );
            job.terminal = job.terminal.take().map(|t| t.with_refusal_detail(detail));
        } else {
            refuse_before_start(
                &self.core.health,
                &mut job,
                if stalled {
                    RefusalReason::TransferInterrupted
                } else if queue_move {
                    RefusalReason::Unknown(motor_protocol::QUEUE_MOVE_REASON.into())
                } else {
                    RefusalReason::QueueWaitExceeded
                },
                &detail,
                Some(safe_detail),
                retry_after_ms,
                self.now(),
            );
        }
        job.state = JobState::Tombstone;
        // Saved without the state lock and published once saved. Meanwhile
        // the job is in `terminal_saves`: it stays queued in its place, so it
        // still holds back the jobs behind it, but dispatch stops at it
        // instead of starting it, and the drain refusal does not write over
        // it. Cancel and refuse wait for `log.write`, held here.
        inner.terminal_saves.insert(id);
        drop(inner);
        // Clears the marker, and gives back the queue clock unless the
        // tombstone was published, also if the save unwinds.
        let saving = Marker::new(self, move |inner: &mut Inner| {
            inner.terminal_saves.remove(&id);
            if let Some(queued) = inner.jobs.get(&id).filter(|job| job.terminal.is_none()) {
                clock.restore(queued);
            }
        });
        let saved = self.save(&job);
        let mut inner = self.core.inner.lock().unwrap();
        if let Err(error) = saved {
            saving.clear(&mut inner);
            return Err(error);
        }
        inner.jobs.insert(id, job);
        saving.clear(&mut inner);
        Ok(WithdrawOutcome::Withdrawn)
    }
}
