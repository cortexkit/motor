use super::{
    journal::{self, Backend, Metrics},
    State,
};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Default)]
struct Control {
    stop: bool,
    requested: bool,
    completed: u64,
    error: Option<String>,
}

pub(super) struct Persistence {
    root: PathBuf,
    pub backend: Mutex<Backend>,
    checkpoint_lock: Mutex<()>,
    control: Mutex<Control>,
    wake: Condvar,
    stopped: AtomicBool,
    pub metrics: Arc<Metrics>,
}
impl Persistence {
    pub fn new(root: &Path, backend: Backend, metrics: Arc<Metrics>) -> Self {
        Self {
            root: root.into(),
            backend: Mutex::new(backend),
            checkpoint_lock: Mutex::new(()),
            control: Mutex::new(Control::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            metrics,
        }
    }
    pub fn check_running(&self) -> io::Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            Err(io::Error::other("storage is shutting down"))
        } else {
            Ok(())
        }
    }
    pub fn has_room(&self) -> bool {
        self.backend.lock().unwrap().bytes() < journal::HARD_BYTES
    }
    pub fn append(&self, before: &State, after: &State) -> io::Result<()> {
        self.backend
            .lock()
            .unwrap()
            .append(&self.root, before, after, &self.metrics)
    }
    pub fn request_if_due(&self) {
        if self.backend.lock().unwrap().due() {
            self.control.lock().unwrap().requested = true;
            self.wake.notify_all();
        }
    }
    pub fn wait_for_compaction(&self) -> io::Result<()> {
        let mut control = self.control.lock().unwrap();
        if let Some(error) = &control.error {
            return Err(io::Error::other(error.clone()));
        }
        let completed = control.completed;
        control.requested = true;
        self.wake.notify_all();
        control = self
            .wake
            .wait_while(control, |c| !c.stop && c.completed == completed)
            .unwrap();
        if let Some(error) = &control.error {
            return Err(io::Error::other(error.clone()));
        }
        self.check_running()
    }
    #[cfg(test)]
    pub fn wait_until_checkpointed(&self) -> bool {
        let control = self.control.lock().unwrap();
        let (control, timeout) = self
            .wake
            .wait_timeout_while(control, Duration::from_secs(10), |_| {
                self.backend.lock().unwrap().pending()
            })
            .unwrap();
        control.error.is_none() && !timeout.timed_out()
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.control.lock().unwrap().stop = true;
        self.wake.notify_all();
    }
    pub fn checkpoint(&self, state: &Mutex<State>) -> io::Result<()> {
        let _checkpoint = self.checkpoint_lock.lock().unwrap();
        let (snapshot, position) = {
            let state = state.lock().unwrap();
            let mut backend = self.backend.lock().unwrap();
            let position = match &mut *backend {
                Backend::Journal(j) => {
                    let Some(position) = j.begin_checkpoint(&self.root)? else {
                        return Ok(());
                    };
                    Some(position)
                }
                Backend::Legacy => None,
            };
            (state.clone(), position)
        };
        journal::snapshot(&self.root, &snapshot, position.as_ref(), &self.metrics)?;
        if let (Backend::Journal(j), Some(position)) =
            (&mut *self.backend.lock().unwrap(), position)
        {
            j.finish_checkpoint(&self.root, &position)?;
        }
        Ok(())
    }
}

pub(super) fn start(
    state: Arc<Mutex<State>>,
    persistence: Arc<Persistence>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name("motor-store-checkpoint".into()).spawn(move || {
        let mut failed = false;
        loop {
            let timeout = if failed { Duration::from_secs(60) } else {
                match &*persistence.backend.lock().unwrap() {
                    Backend::Legacy => journal::MAX_AGE,
                    Backend::Journal(j) => journal::MAX_AGE.saturating_sub(j.last_checkpoint.elapsed()),
                }
            };
            let mut control = persistence.control.lock().unwrap();
            if !control.stop && !control.requested {
                control = persistence.wake.wait_timeout(control, timeout).unwrap().0;
            }
            if control.stop { break; }
            let requested = std::mem::take(&mut control.requested);
            drop(control);
            let due = persistence.backend.lock().unwrap().due();
            // Also advance the idle timer on an empty log; otherwise an old
            // checkpoint with no mutations would busy-loop at a zero timeout.
            if requested || due || !failed {
                let result = persistence.checkpoint(&state);
                failed = result.is_err();
                let error = result.err().map(|e| e.to_string());
                if let Some(error) = &error {
                    tracing::error!(%error, "storage compaction failed; checkpoint freshness is not guaranteed");
                }
                let mut control = persistence.control.lock().unwrap();
                control.error = error;
                control.completed += 1;
                persistence.wake.notify_all();
            }
        }
    })
}
