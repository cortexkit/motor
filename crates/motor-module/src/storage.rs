use fs2::FileExt;
use motor_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Job {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate_job: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<JobAttribution>,
    pub queue_wait_limit_s: u64,
    pub pending_cancel: bool,
    pub local_terminal: Option<TerminalRecord>,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub(crate) struct JobAttribution {
    pub session: String,
    pub preset: Option<String>,
    pub scope_ref: String,
    pub scope_epoch: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    jobs: BTreeMap<Uuid, Job>,
    snapshots: BTreeMap<String, BTreeMap<String, String>>,
}

/// Atomic, fsynced module storage next to the daemon's delivered store path.
/// One process owns the lease; cancel intent is durable before a network attempt.
pub(crate) struct Storage {
    root: PathBuf,
    state: Mutex<State>,
    _lease: File,
}

impl Storage {
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("lease"))?;
        lease.try_lock_exclusive()?;
        let state = match fs::read(root.join("state.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            root: root.into(),
            state: Mutex::new(state),
            _lease: lease,
        })
    }
    fn update(&self, f: impl FnOnce(&mut State)) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        f(&mut next);
        let path = self.root.join("state.tmp");
        let mut file = File::create(&path)?;
        file.write_all(&serde_json::to_vec(&next).map_err(io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(path, self.root.join("state.json"))?;
        File::open(&self.root)?.sync_all()?;
        *state = next;
        Ok(())
    }
    pub fn job(&self, id: Uuid) -> Option<Job> {
        self.state.lock().unwrap().jobs.get(&id).cloned()
    }
    pub fn create(
        &self,
        id: Uuid,
        wait: u64,
        attribution: Option<JobAttribution>,
        runner_instance: Option<String>,
        gate_job: bool,
    ) -> io::Result<()> {
        self.update(|s| {
            s.jobs.entry(id).or_insert(Job {
                gate_job,
                queue_wait_limit_s: wait,
                attribution,
                runner_instance,
                ..Job::default()
            });
        })
    }
    pub fn cancel(&self, id: Uuid, pending: bool) -> io::Result<()> {
        self.update(|s| s.jobs.entry(id).or_default().pending_cancel = pending)
    }
    pub fn local_terminal(&self, t: TerminalRecord) -> io::Result<()> {
        let id = t.job_id;
        self.update(|s| s.jobs.entry(id).or_default().local_terminal = Some(t))
    }
    pub fn previous(&self, key: &str) -> BTreeMap<String, String> {
        self.state
            .lock()
            .unwrap()
            .snapshots
            .get(key)
            .cloned()
            .unwrap_or_default()
    }
    pub fn accepted(&self, key: String, commits: BTreeMap<String, String>) -> io::Result<()> {
        self.update(|s| {
            s.snapshots.insert(key, commits);
        })
    }
}
