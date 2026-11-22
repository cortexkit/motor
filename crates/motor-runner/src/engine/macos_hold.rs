#[cfg(unix)]
use super::Inner;
use super::Runner;
#[cfg(unix)]
use crate::macos::hold::Snapshot;

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
        let _ = self.observe_macos_hold();
        self.macos_hold_status_cached(false)
    }

    pub(super) fn macos_hold_status_cached(&self, _record_refusal: bool) -> Option<String> {
        #[cfg(unix)]
        if self.core.config.macos_instance() {
            let mut hold = self.core.macos_hold.lock().unwrap();
            let now = self.now();
            return if _record_refusal {
                hold.admission_refusal(now)
            } else {
                hold.status(now)
            };
        }
        None
    }
}
