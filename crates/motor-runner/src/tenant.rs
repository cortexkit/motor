use crate::storage::{atomic_json_private, read_json, Job};
use motor_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Default, Serialize, Deserialize)]
struct Progress {
    started: bool,
    terminal: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Events {
    next_seq: u64,
    pending: VecDeque<TenantEvent>,
    jobs: BTreeMap<Uuid, Progress>,
    idle: bool,
}

pub(crate) struct EventLog {
    path: PathBuf,
    state: Mutex<Events>,
}

impl Events {
    fn push(&mut self, event: TenantEventKind, jti: Option<Uuid>, cause: Option<RefusedCause>) {
        self.next_seq += 1;
        self.pending.push_back(TenantEvent {
            event_seq: self.next_seq,
            event,
            jti,
            cause,
        });
    }
}

impl EventLog {
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        let path = root.join("tenant-events.json");
        let mut state: Events = match read_json(&path) {
            Ok(state) => state,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Events::default(),
            Err(e) => return Err(e),
        };
        // A fresh runner must announce idleness even if the previous process
        // already had its idle event acknowledged.
        state.idle = false;
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }
    fn update(&self, change: impl FnOnce(&mut Events)) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        change(&mut next);
        atomic_json_private(&self.path, &next)?;
        *state = next;
        Ok(())
    }
    pub(crate) fn admit(&self, id: Uuid) -> io::Result<()> {
        self.update(|s| {
            if let std::collections::btree_map::Entry::Vacant(e) = s.jobs.entry(id) {
                e.insert(Progress::default());
                s.idle = false;
                s.push(TenantEventKind::Admitted, Some(id), None);
            }
        })
    }
    pub(crate) fn refused(&self, id: Uuid, cause: RefusedCause) -> io::Result<()> {
        self.update(|s| s.push(TenantEventKind::Refused, Some(id), Some(cause)))
    }
    pub(crate) fn observe(&self, job: &Job) -> io::Result<()> {
        let mut guard = self.state.lock().unwrap();
        let Some(progress) = guard.jobs.get(&job.id) else {
            return Ok(());
        };
        let started = job.started_ms.is_some() && !progress.started;
        let terminal = job.terminal.is_some() && !progress.terminal;
        if !started && !terminal {
            return Ok(());
        }
        let mut state = guard.clone();
        let progress = state.jobs.get_mut(&job.id).unwrap();
        progress.started |= started;
        progress.terminal |= terminal;
        if started {
            state.push(TenantEventKind::Started, Some(job.id), None);
        }
        if terminal {
            state.push(TenantEventKind::Terminal, Some(job.id), None);
        }
        atomic_json_private(&self.path, &state)?;
        *guard = state;
        Ok(())
    }
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn idle(&self, idle: bool) -> io::Result<()> {
        let mut guard = self.state.lock().unwrap();
        if guard.idle == idle {
            return Ok(());
        }
        let mut state = guard.clone();
        state.idle = idle;
        if idle {
            state.push(TenantEventKind::Idle, None, None);
        }
        atomic_json_private(&self.path, &state)?;
        *guard = state;
        Ok(())
    }
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn first(&self) -> Option<TenantEvent> {
        self.state.lock().unwrap().pending.front().cloned()
    }
    #[cfg(any(target_os = "linux", test))]
    fn replay_boundary(&self) -> TenantReplayBoundary {
        TenantReplayBoundary {
            through_seq: self.state.lock().unwrap().next_seq,
        }
    }
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn ack(&self, seq: u64) -> io::Result<()> {
        let mut guard = self.state.lock().unwrap();
        if guard.pending.front().map(|e| e.event_seq) != Some(seq) {
            return Err(io::Error::other("out-of-order tenant event ack"));
        }
        let mut state = guard.clone();
        state.pending.pop_front();
        atomic_json_private(&self.path, &state)?;
        *guard = state;
        Ok(())
    }
}

pub(crate) fn host_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

pub(crate) fn matches(lease: &VerifiedLease, run: &Run) -> bool {
    run.weight_hint == Some(lease.vcpus)
        && run
            .network
            .as_ref()
            .map(|n| n == &Network::Outbound)
            .unwrap_or(false)
            == lease.net
        && run
            .platform
            .as_ref()
            .map(|p| p == &Platform::Linux)
            .unwrap_or(true)
        && host_platform().is_some_and(|platform| lease.platform == platform)
        && run.timeout.is_some_and(|s| s <= lease.max_s)
}

#[cfg(target_os = "linux")]
pub async fn serve_tenant(runner: crate::Runner) -> io::Result<()> {
    if !runner.config().tenant_vm {
        return Err(io::Error::other("tenant mode is disabled"));
    }
    let jobs = crate::vsock::Listener::bind(52)?;
    let events = crate::vsock::Listener::bind(53)?;
    let event_runner = runner.clone();
    let lifecycle = async move {
        loop {
            let socket = events.accept().await?;
            // One event consumer at a time preserves ack order across reconnects.
            if let Err(error) = event_session(&event_runner, socket).await {
                tracing::warn!(%error, "tenant event channel detached");
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    let job_runner = runner.clone();
    let job_connections = async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let socket = tokio::select! {
                socket = jobs.accept() => socket?,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                    if job_runner.drain_should_exit() { return Ok::<(), io::Error>(()); }
                    continue;
                }
            };
            let runner = job_runner.clone();
            tasks.spawn(async move {
                if let Err(error) = crate::server::tenant_channel(runner, socket).await {
                    tracing::warn!(%error, "tenant job channel detached");
                }
            });
        }
    };
    tokio::select! { result = lifecycle => result, result = job_connections => result }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) async fn event_session<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    runner: &crate::Runner,
    mut socket: S,
) -> io::Result<()> {
    let log = runner
        .tenant_events()
        .ok_or_else(|| io::Error::other("not a tenant runner"))?;
    // Capture the watermark before any await; newly allocated events are live.
    // This additive control frame leaves the retained event and ack bytes alone.
    let mut boundary = serde_json::to_value(log.replay_boundary()).map_err(io::Error::other)?;
    boundary["type"] = serde_json::json!("event_replay");
    let bytes = serde_json::to_vec(&boundary).map_err(io::Error::other)?;
    use tokio::io::AsyncWriteExt;
    socket.write_u32(bytes.len() as u32).await?;
    socket.write_all(&bytes).await?;
    socket.flush().await?;
    loop {
        log.idle(runner.tenant_idle())?;
        if let Some(event) = log.first() {
            let seq = event.event_seq;
            write_frame_async(&mut socket, &Frame::Event(event)).await?;
            match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                read_record_async(&mut socket),
            )
            .await
            .map_err(io::Error::other)??
            {
                Some(DecodedFrame::Frame(Frame::Ack { event_seq })) if event_seq == seq => {
                    log.ack(seq)?
                }
                _ => return Err(io::Error::other("expected tenant event ack")),
            }
        } else {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {},
                _ = read_record_async(&mut socket) => return Err(io::Error::other("unexpected event channel input")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn tenant_events_survive_reopen_and_ack_only_the_head() {
        let root = ScratchDir::new("motor-tenant-events");
        let id = Uuid::now_v7();
        let log = EventLog::open(&root).unwrap();
        log.refused(id, RefusedCause::Disconnect).unwrap();
        log.admit(id).unwrap();
        log.admit(id).unwrap();
        let mut job = Job::new(id, None, 1, 1, 2);
        job.started_ms = Some(10);
        log.observe(&job).unwrap();
        job.terminal(Outcome::Cancelled, 20);
        log.observe(&job).unwrap();
        log.observe(&job).unwrap();
        drop(log);
        let log = EventLog::open(&root).unwrap();
        assert!(log.ack(2).is_err());
        for (index, kind) in [
            TenantEventKind::Refused,
            TenantEventKind::Admitted,
            TenantEventKind::Started,
            TenantEventKind::Terminal,
        ]
        .into_iter()
        .enumerate()
        {
            let event = log.first().unwrap();
            assert_eq!(
                (event.event_seq, event.event, event.jti),
                (index as u64 + 1, kind, Some(id))
            );
            log.ack(event.event_seq).unwrap();
        }
        drop(log);
        let log = EventLog::open(&root).unwrap();
        assert!(log.first().is_none());
        log.idle(true).unwrap();
        assert_eq!(
            log.first().unwrap(),
            TenantEvent {
                event_seq: 5,
                event: TenantEventKind::Idle,
                jti: None,
                cause: None
            }
        );
    }
}
