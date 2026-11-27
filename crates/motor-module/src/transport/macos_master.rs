use super::{stderr_lines, SshStderr, STDERR_TAIL_LINES, STDERR_WAIT};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::{
    process::Child,
    sync::{oneshot, Mutex, Notify},
};
use uuid::Uuid;

use super::EndpointPin;

const LOG_CAP: usize = 64 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// The identity of a connection: another endpoint or agent must not reuse it.
#[derive(PartialEq)]
pub(super) struct MasterEndpoint {
    pub(super) control: PathBuf,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) user: String,
    pub(super) host_key: String,
}

pub(super) struct EndpointMaster {
    pub(super) endpoint: MasterEndpoint,
    pub(super) child: Child,
    pub(super) health: Arc<MasterHealth>,
    retiring: bool,
    log: MasterLog,
    stderr: SshStderr,
    _pin: EndpointPin,
}

impl EndpointMaster {
    pub(super) fn new(
        endpoint: MasterEndpoint,
        mut child: Child,
        log: MasterLog,
        pin: EndpointPin,
    ) -> Self {
        let stderr = SshStderr::collect(child.stderr.take().unwrap());
        Self {
            endpoint,
            child,
            health: Arc::default(),
            retiring: false,
            log,
            stderr,
            _pin: pin,
        }
    }

    /// Check progress and signal the child under one lock: a still-usable
    /// session must be able to veto a Hello timeout up to the kill decision.
    pub(super) fn exited_or_suspect(&mut self) -> io::Result<bool> {
        if self.retiring || self.child.try_wait()?.is_some() {
            return Ok(true);
        }
        let health = self.health.state.lock().unwrap();
        if health.should_replace() {
            self.child.start_kill()?;
            self.retiring = true;
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) async fn retire(&mut self) -> Vec<String> {
        self.retiring = true;
        let _ = self.child.start_kill();
        let status = self.child.wait().await;
        let _ = std::fs::remove_file(&self.endpoint.control);
        let fallback = self.stderr.closed_lines(STDERR_WAIT).await;
        let tail = self.log.finish();
        let lines = if tail.is_empty() {
            // ssh may report an error to stderr before it opens the -E file.
            fallback
        } else {
            stderr_lines(&tail, STDERR_TAIL_LINES)
        };
        let status = status.map_or_else(|error| error.to_string(), |status| status.to_string());
        tracing::warn!(%status, ssh_stderr_tail = ?lines, "macOS SSH master exited");
        lines
    }

    pub(super) fn observe(&self, owner: &Arc<Mutex<Option<Self>>>) {
        let owner = Arc::downgrade(owner);
        let health = Arc::clone(&self.health);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = health.changed.notified() => {},
                    _ = tokio::time::sleep(CHECK_INTERVAL) => {},
                }
                let Some(owner) = owner.upgrade() else { break };
                let mut owned = owner.lock().await;
                let Some(master) = owned.as_mut() else { break };
                if !Arc::ptr_eq(&health, &master.health) {
                    break;
                }
                // Startup holds the owner lock. A missing socket here means
                // startup was canceled or an established master lost it.
                if !matches!(master.exited_or_suspect(), Ok(false))
                    || !master.endpoint.control.exists()
                {
                    master.retire().await;
                    *owned = None;
                    break;
                }
                let _ = master.log.cap();
            }
        });
    }
}

pub(super) struct MasterLog(PathBuf);

impl MasterLog {
    pub(super) fn create(root: &Path) -> io::Result<Self> {
        let path = root.join(format!("macos-ssh-master-{}.log", Uuid::now_v7()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(Self(path))
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }

    fn tail(file: &mut File, length: u64) -> io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(length.saturating_sub(LOG_CAP as u64)))?;
        let mut tail = Vec::new();
        file.take(LOG_CAP as u64).read_to_end(&mut tail)?;
        Ok(tail)
    }

    fn cap(&self) -> io::Result<()> {
        let mut file = OpenOptions::new().read(true).write(true).open(&self.0)?;
        let length = file.metadata()?.len();
        if length > LOG_CAP as u64 {
            let tail = Self::tail(&mut file, length)?;
            // ssh keeps the append descriptor open, so renaming the live file
            // would not bound disk use. Compact in place; don't trim when a
            // concurrent append was observed while taking the snapshot.
            if file.metadata()?.len() == length {
                file.rewind()?;
                file.write_all(&tail)?;
                if file.metadata()?.len() == length {
                    file.set_len(tail.len() as u64)?;
                }
            }
        }
        Ok(())
    }

    fn finish(&self) -> Vec<u8> {
        let tail = (|| {
            self.cap()?;
            let mut file = File::open(&self.0)?;
            let length = file.metadata()?.len();
            Self::tail(&mut file, length)
        })()
        .unwrap_or_default();
        // Keep only the last exited generation, alongside any live master.
        let _ = std::fs::rename(
            &self.0,
            self.0.with_file_name("macos-ssh-master.previous.log"),
        );
        tail
    }
}

impl Drop for MasterLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Default)]
pub(super) struct MasterHealth {
    pub(super) state: StdMutex<Activity>,
    changed: Notify,
}

#[derive(Default)]
pub(super) struct Activity {
    sequence: u64,
    active: HashSet<u64>,
    // Only the last progress from two distinct sessions is needed: the
    // session whose Hello timed out must not count its own bytes as a veto.
    progress: [Option<(u64, u64)>; 2],
    timeout: Option<u64>,
    consecutive_failures: usize,
}

impl Activity {
    fn reached_peer_since(&self, opened: u64) -> bool {
        self.progress
            .iter()
            .flatten()
            .any(|&(session, at)| session != opened && at > opened)
    }

    pub(super) fn should_replace(&self) -> bool {
        self.timeout
            .is_some_and(|opened| !self.reached_peer_since(opened))
            || self.active.is_empty() && self.consecutive_failures >= 2
    }

    pub(super) fn register(&mut self, health: &Arc<MasterHealth>) -> SessionActivity {
        self.sequence += 1;
        self.active.insert(self.sequence);
        SessionActivity {
            health: Arc::clone(health),
            id: self.sequence,
        }
    }
}

#[derive(Clone)]
pub(super) struct SessionActivity {
    health: Arc<MasterHealth>,
    id: u64,
}

impl SessionActivity {
    pub(super) fn inbound_bytes(&self) {
        let mut state = self.health.state.lock().unwrap();
        state.sequence += 1;
        if !matches!(state.progress[0], Some((session, _)) if session == self.id) {
            state.progress[1] = state.progress[0];
        }
        state.progress[0] = Some((self.id, state.sequence));
        state.consecutive_failures = 0;
    }

    pub(super) fn hello_timed_out(&self) {
        let mut state = self.health.state.lock().unwrap();
        if !state.reached_peer_since(self.id) {
            state.timeout = Some(self.id);
            self.health.changed.notify_one();
        }
    }

    pub(super) fn observe(&self, mut child: Child) -> oneshot::Sender<()> {
        let (session, closed) = oneshot::channel();
        let activity = self.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                biased;
                result = child.wait() => result.ok(),
                _ = closed => None,
            };
            activity.closed(status);
        });
        session
    }

    fn closed(&self, status: Option<ExitStatus>) {
        let mut state = self.health.state.lock().unwrap();
        state.active.remove(&self.id);
        if status.and_then(|status| status.code()) == Some(255) {
            // 255 is ssh's failure status, but a single failed session does
            // not implicate the master. Require overlapping failures with no
            // progress or ordinary close between them.
            state.consecutive_failures += 1;
            if state.active.is_empty() {
                if state.consecutive_failures >= 2 {
                    self.health.changed.notify_one();
                } else {
                    state.consecutive_failures = 0;
                }
            }
        } else {
            state.consecutive_failures = 0;
        }
    }
}
