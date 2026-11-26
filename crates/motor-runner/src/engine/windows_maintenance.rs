use super::{JobState, Runner};

impl Runner {
    pub(super) fn maintain_windows(&self) {
        let config = &self.core.config;
        if !config.windows_instance() {
            return;
        }
        config.windows_runtime.retry_cleanups();
        // Take only the job IDs under the state lock; evidence I/O must not
        // hold up admission or dispatch.
        let (terminal, held) = {
            let inner = self.core.inner.lock().unwrap();
            let terminal = inner
                .jobs
                .values()
                .filter(|job| job.terminal.is_some() || job.state == JobState::Tombstone)
                .map(|job| job.id)
                .collect();
            let held = inner.jobs.keys().copied().collect();
            (terminal, held)
        };
        config.windows_runtime.reconcile_terminal(&terminal);
        if let Some(windows) = &config.windows {
            config
                .windows_runtime
                .prune_guest_evidence(&windows.layout_root, &held);
        }
    }
}
