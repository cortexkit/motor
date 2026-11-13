use super::*;

impl LinuxJobs {
    pub fn start_exit_watcher(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let jobs = Arc::downgrade(self);
        // No join handle is retained by shutdown. Each removal is idempotent or
        // durable, so a process exit may interrupt a collector or the pruner.
        std::thread::spawn(move || loop {
            std::thread::park_timeout(Duration::from_millis(100));
            let Some(jobs) = jobs.upgrade() else {
                break;
            };
            jobs.watch_exits();
        })
    }

    pub(super) fn watch_exits(self: &Arc<Self>) {
        for cell in self.exiting_jobs() {
            if !cell.begin_collection() {
                continue;
            }
            let jobs = self.clone();
            let worker = cell.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("job-exit-collector".into())
                .spawn(move || jobs.collect_job(&worker))
            {
                cell.end_collection();
                cell.log_spawn_failure(&error);
            }
        }
    }

    pub(super) fn exiting_jobs(&self) -> Vec<Arc<JobCell>> {
        // The registry lock protects only this memory snapshot. Never wait for
        // a job's writer or perform cgroup I/O under it: one slow VM must not
        // delay detection.
        let snapshot: Vec<_> = self
            .registry
            .lock()
            .values()
            .filter(|entry| {
                entry.view.state == State::Running
                    || (entry.view.state == State::Exited && entry.view.cleanup_pending)
            })
            .map(|entry| (entry.cell.clone(), entry.view.clone()))
            .collect();
        snapshot
            .into_iter()
            .filter_map(|(cell, view)| {
                let signalled = view.state == State::Exited
                    || view.pidfd.as_ref().is_some_and(|fd| {
                        let mut poll = libc::pollfd {
                            fd: fd.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        (unsafe { libc::poll(&mut poll, 1, 0) }) > 0
                            && poll.revents & libc::POLLIN != 0
                    })
                    || match std::fs::read_to_string(
                        self.group(&view.record.start.job_id).join("cgroup.procs"),
                    ) {
                        Ok(procs) => procs.trim().is_empty(),
                        // Only a missing directory is an exit signal. A missing or
                        // unreadable control file in an existing group proves
                        // nothing about its processes.
                        Err(_) => self
                            .group(&view.record.start.job_id)
                            .try_exists()
                            .is_ok_and(|exists| !exists),
                    };
                signalled.then_some(cell)
            })
            .collect()
    }

    pub(super) fn collect_job(&self, cell: &JobCell) {
        struct Collecting<'a>(&'a JobCell);
        impl Drop for Collecting<'_> {
            fn drop(&mut self) {
                self.0.end_collection();
            }
        }
        let _collecting = Collecting(cell);
        #[cfg(test)]
        {
            self.hooks.lock().unwrap().collector_spawns += 1;
        }
        // A stop or a completed launch may have changed the view after the
        // watcher snapshot. The job-cell lock serializes both collection phases
        // with stop, launch completion and every record rewrite.
        let mut state = cell.0.lock();
        if matches!(state.state, State::Booting | State::Released)
            || (state.state == State::Exited && !state.cleanup_pending)
        {
            return;
        }
        if let Err(error) = self.reap_job(&mut state, Duration::ZERO) {
            self.log_collection_failure(&mut state, CollectionFailure::Reap, &error);
        }
        let _ = self.collect_exit_locked(&mut state);
    }
}
