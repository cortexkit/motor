use super::*;
use crate::lock_order::{Lock, Mutex};
use std::os::fd::OwnedFd;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    Booting,
    Running,
    Exited,
    Released,
}

pub(super) struct JobState {
    pub record: Record,
    pub state: State,
    pub child: Option<Child>,
    pub pidfd: Option<Arc<OwnedFd>>,
    pub failures: BTreeSet<CollectionFailure>,
    pub cleanup_pending: bool,
}

impl JobState {
    pub fn new(record: Record, state: State, cleanup_pending: bool) -> Self {
        Self {
            record,
            state,
            child: None,
            pidfd: None,
            failures: BTreeSet::new(),
            cleanup_pending,
        }
    }
}

// Whole-record rewrites, child reaping, stop and launch share one writer lock.
// Holding it never requires another job's lock or a later admission decision.
pub(super) struct JobCell(pub Mutex<JobState>);

impl JobCell {
    pub fn new(state: JobState) -> Self {
        Self(Mutex::new(Lock::J, state))
    }
}

// Keep durable facts together in a copy of the unchanged record. Readers must
// never lock the cell while holding the registry: a writer publishes only after
// its fsync, with the per-job lock still excluding other whole-record writers.
#[derive(Clone)]
pub(super) struct JobView {
    pub record: Record,
    pub state: State,
    pub cleanup_pending: bool,
    // Retain the owned process identity even when no reader needs to poll it.
    pub _pidfd: Option<Arc<OwnedFd>>,
}

impl From<&JobState> for JobView {
    fn from(state: &JobState) -> Self {
        Self {
            record: state.record.clone(),
            state: state.state,
            cleanup_pending: state.cleanup_pending,
            _pidfd: state.pidfd.clone(),
        }
    }
}

pub(super) struct Entry {
    pub cell: Arc<JobCell>,
    pub view: JobView,
}

impl LinuxJobs {
    pub(super) fn launch_record(
        &self,
        record: Record,
        launch: impl FnOnce(&mut JobState) -> Result<()>,
    ) -> Result<Response> {
        crate::lock_order::assert_held(Lock::A);
        let id = record.start.job_id.clone();
        let cell = Arc::new(JobCell::new(JobState::new(record, State::Booting, false)));
        // Lock before insertion so adoption cannot mistake a half-built cgroup
        // for an exited VM. Durable intent reserves the disk before any launch.
        let mut state = cell.0.lock();
        self.write_record(&state.record)?;
        self.insert_cell(cell.clone(), &state);
        let result = launch(&mut state);
        if let Err(error) = result {
            if let Err(cleanup) = self.stop_locked(&mut state) {
                // Launch is over even when cleanup must be retried. Maintenance
                // may now collect it, but only explicit stop can release its disk.
                state.state = State::Running;
                self.publish(&state);
                return Err(Error(format!("{error}; job cleanup failed: {cleanup}")));
            }
            return Err(error);
        }
        state.state = State::Running;
        self.publish(&state);
        drop(state);
        self.status(&id)
    }

    pub(super) fn records(&self) -> BTreeMap<String, Record> {
        self.registry
            .lock()
            .iter()
            .map(|(id, entry)| (id.clone(), entry.view.record.clone()))
            .collect()
    }

    pub(super) fn snapshot(&self) -> Vec<(String, Arc<JobCell>, JobView)> {
        self.registry
            .lock()
            .iter()
            .map(|(id, entry)| (id.clone(), entry.cell.clone(), entry.view.clone()))
            .collect()
    }

    pub(super) fn cell(&self, id: &str) -> Result<Arc<JobCell>> {
        self.registry
            .lock()
            .get(id)
            .map(|entry| entry.cell.clone())
            .ok_or_else(|| Error("unknown job VM".into()))
    }

    pub(super) fn view(&self, id: &str) -> Option<JobView> {
        self.registry.lock().get(id).map(|entry| entry.view.clone())
    }

    pub(super) fn running_view(&self, id: &str) -> Result<JobView> {
        let view = self
            .view(id)
            .ok_or_else(|| Error("unknown job VM".into()))?;
        if view.state != State::Running || view.record.oom_kill.is_some() {
            return Err(Error("job VM is not running".into()));
        }
        Ok(view)
    }

    pub(super) fn insert_cell(&self, cell: Arc<JobCell>, state: &JobState) {
        self.registry.lock().insert(
            state.record.start.job_id.clone(),
            Entry {
                cell,
                view: JobView::from(state),
            },
        );
    }

    pub(super) fn publish(&self, state: &JobState) {
        self.registry
            .lock()
            .get_mut(&state.record.start.job_id)
            .unwrap()
            .view = JobView::from(state);
    }

    pub(super) fn persist(&self, state: &mut JobState, record: Record) -> Result<()> {
        self.write_record(&record)?;
        state.record = record;
        if state.record.released {
            state.state = State::Released;
        } else if state.record.oom_kill.is_some() {
            state.state = State::Exited;
        }
        self.publish(state);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn save(&self, record: &Record) -> Result<()> {
        // Fixture setup writes use the same writer lock as production intent.
        let cell = self.cell(&record.start.job_id).unwrap_or_else(|_| {
            Arc::new(JobCell::new(JobState::new(
                record.clone(),
                State::Running,
                false,
            )))
        });
        let _state = cell.0.lock();
        self.write_record(record)
    }

    #[cfg(test)]
    pub(super) fn insert_record(&self, record: Record) {
        let state = if record.released {
            State::Released
        } else if record.oom_kill.is_some() {
            State::Exited
        } else {
            State::Running
        };
        let cell = Arc::new(JobCell::new(JobState::new(record, state, false)));
        let guard = cell.0.lock();
        self.insert_cell(cell.clone(), &guard);
    }

    #[cfg(test)]
    pub(super) fn failures(&self, id: &str) -> BTreeSet<CollectionFailure> {
        self.cell(id).unwrap().0.lock().failures.clone()
    }

    #[cfg(test)]
    pub(super) fn all_failures(&self) -> BTreeMap<String, BTreeSet<CollectionFailure>> {
        self.snapshot()
            .into_iter()
            .filter_map(|(id, cell, _)| {
                let failures = cell.0.lock().failures.clone();
                (!failures.is_empty()).then_some((id, failures))
            })
            .collect()
    }
}
