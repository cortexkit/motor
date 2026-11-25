#[cfg(unix)]
use super::{paths, JobState};
use super::{Inner, Run, Runner, Uuid};
#[cfg(unix)]
use crate::macos::hold::Snapshot;

// Most macOS jobs set up in about 30 seconds, but staging and boot on a busy
// host can take several minutes. Budget for those slower starts.
#[cfg(unix)]
const SETUP_MARGIN_MS: u64 = 5 * 60 * 1_000;

#[derive(Default)]
pub(super) struct RunHoldCheck {
    detail: Option<String>,
    fit: Option<(u64, u64)>,
}

impl RunHoldCheck {
    pub(super) fn refusal_detail(&self) -> &str {
        self.detail.as_deref().unwrap_or("runner_draining")
    }

    pub(super) fn log_fit(&self, job_id: Uuid) {
        if let Some((latest_end_ms, committed_end_ms)) = self.fit {
            tracing::info!(%job_id, latest_end_ms, committed_end_ms, "job admitted while a macOS hold is pending");
        }
    }
}

impl Runner {
    #[cfg(unix)]
    pub(super) fn observe_macos_hold(&self) -> Option<Snapshot> {
        self.core
            .config
            .macos_instance()
            .then(|| self.core.macos_hold.lock().unwrap().observe(self.now()))
    }

    #[cfg(unix)]
    pub(super) fn refresh_macos_hold(&self, inner: &Inner, snapshot: Option<Snapshot>) {
        if let Some(snapshot) = snapshot {
            let idle = inner.jobs.values().all(|job| job.terminal.is_some())
                && inner.starting.is_empty()
                && inner.reserved.is_empty()
                && inner.preparing.is_empty()
                && inner.admitting.is_empty()
                && inner.admission_saves.is_empty()
                && inner.terminal_saves.is_empty()
                && inner.base_finishes.is_empty()
                && inner.base_settling.is_empty()
                && inner.finalizing.is_empty()
                && !self.core.config.macos_runtime.degraded()
                && self.core.quarantined.is_empty();
            self.core
                .macos_hold
                .lock()
                .unwrap()
                .apply(self.now(), idle, snapshot);
        }
    }

    pub(super) fn macos_hold_status(&self) -> Option<String> {
        #[cfg(unix)]
        if self.observe_macos_hold().is_some() {
            return self.core.macos_hold.lock().unwrap().status(self.now());
        }
        None
    }

    pub(super) fn macos_hold_run_refusal(
        &self,
        _inner: &Inner,
        _request: &Run,
        _final_check: Option<&mut RunHoldCheck>,
    ) -> Option<String> {
        #[cfg(unix)]
        if self.core.config.macos_instance() {
            let mut hold = self.core.macos_hold.lock().unwrap();
            let now = self.now();
            let mut check = RunHoldCheck::default();
            let refusal = hold.admission_refusal(now, _final_check.is_some(), || {
                match self.pending_hold_fit(_inner, _request, now) {
                    Ok(fit) => {
                        check.fit = Some(fit);
                        true
                    }
                    Err(detail) => {
                        check.detail = detail.map(|detail| format!("runner_draining: {detail}"));
                        false
                    }
                }
            });
            if let Some(final_check) = _final_check {
                *final_check = check;
            }
            return refusal;
        }
        None
    }

    #[cfg(unix)]
    fn pending_hold_fit(
        &self,
        inner: &Inner,
        request: &Run,
        now: u64,
    ) -> Result<(u64, u64), Option<String>> {
        fn deadline_ms(start_ms: u64, timeout_s: u64) -> Option<u64> {
            start_ms.checked_add(timeout_s.checked_mul(1_000)?)
        }

        let mut committed_end_ms = None;
        let mut weight = 4u32; // Every macOS VM job costs four permits.
        for job in inner.jobs.values().filter(|job| job.terminal.is_none()) {
            // An upload, queued job or setup in flight already has a claim on
            // a slot but no start time. Don't queue more work behind it.
            if job.state != JobState::Running || job.base {
                return Err(None);
            }
            let timeout = job
                .request
                .as_ref()
                .and_then(|request| request.timeout)
                .ok_or_else(|| Some("a running job has no time limit".into()))?;
            let start = job.started_ms.ok_or(None)?;
            let end = deadline_ms(start, timeout)
                .ok_or_else(|| Some("a running job's deadline is out of range".into()))?;
            committed_end_ms = Some(committed_end_ms.map_or(end, |other: u64| other.max(end)));
            weight = weight.saturating_add(job.weight);
        }
        let committed_end_ms = committed_end_ms.ok_or(None)?;
        let timeout = request
            .timeout
            .ok_or_else(|| Some("this job has no time limit".into()))?;
        let latest_end_ms = deadline_ms(now, timeout)
            .and_then(|end| end.checked_add(SETUP_MARGIN_MS))
            .ok_or_else(|| Some("this job's deadline with setup is out of range".into()))?;
        if latest_end_ms > committed_end_ms {
            return Err(Some(format!(
                "this job would end after the running jobs (latest end {latest_end_ms}, committed end {committed_end_ms}; Unix ms)"
            )));
        }

        let workspace = paths::request(request).caller_workspace_key.text();
        let runtime = &self.core.config.macos_runtime;
        if weight > self.core.config.max_weight
            || !inner.admission_saves.is_empty()
            || inner.restoring_reserve
            || !inner.starting.is_empty()
            || Self::occupied(inner, workspace)
            || Self::siblings_blocked(inner, &request.siblings)
            || runtime.workspace_busy(workspace, request.job_id)
            || runtime.degraded()
            || !self.core.quarantined.is_empty()
        {
            return Err(None);
        }
        Ok((latest_end_ms, committed_end_ms))
    }
}
