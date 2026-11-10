use crate::storage::{atomic_json_private, read_json, Job};
use motor_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::Mutex,
};
#[path = "tenant_mirror_reports.rs"]
mod mirror_reports;
use mirror_reports::MirrorJournal;

/// Written by tenant attachment once it has placed the daemon's private
/// resolver file. The runner that attachment starts reads it to decide whether
/// a snapshot helper unit binds that resolver; every runner is started before
/// the file exists. It lives on `/run`, which a fresh boot of the VM clears,
/// so no stale marker outlives the resolver it describes. Defined here, not in
/// the Linux-only tenant agent, because snapshot helpers name it in tests on
/// every OS.
#[cfg(any(target_os = "linux", test))]
pub(crate) const DNS_MARKER: &str = "/run/motor-tenant-dns";
/// The daemon's private resolver directory, as attachment installs it and as
/// the runner's own files outside the tenant agent refer to it.
#[cfg(any(target_os = "linux", test))]
pub(crate) const DNS_DIRECTORY: &str = "/run/motor-tenant-daemon";

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
    reports: std::sync::atomic::AtomicBool,
    per_job: bool,
    terminal_stored: std::sync::Mutex<Option<(Uuid, u64)>>,
    mirror_path: PathBuf,
    mirror_journal: Mutex<MirrorJournal>,
    // Samples are coalesced and ephemeral; only lifecycle events need replay.
    #[cfg(any(target_os = "linux", test))]
    samples: Mutex<BTreeMap<Uuid, MemorySample>>,
}

impl Events {
    fn push(&mut self, event: TenantEventKind, jti: Option<Uuid>, cause: Option<RefusedCause>) {
        self.next_seq += 1;
        self.pending.push_back(TenantEvent {
            event_seq: self.next_seq,
            event,
            jti,
            cause,
            setup: None,
            mem_peak_mib: None,
            mem_working_peak_mib: None,
            mem_over_mib_ms: None,
            remote_cache_totals: None,
            mirror_refs: None,
        });
    }
}

impl EventLog {
    pub(crate) fn setup(&self, id: Uuid, phase: TenantSetupPhase) -> io::Result<()> {
        self.update(|s| {
            s.push(TenantEventKind::SetupPhase, Some(id), None);
            s.pending.back_mut().unwrap().setup = Some(phase);
        })
    }
    #[cfg(test)]
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        Self::open_for_mode(root, false)
    }
    pub(crate) fn open_for_mode(root: &Path, per_job: bool) -> io::Result<Self> {
        let path = root.join("tenant-events.json");
        let mut state: Events = match read_json(&path) {
            Ok(state) => state,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Events::default(),
            Err(e) => return Err(e),
        };
        // A fresh runner must announce idleness even if the previous process
        // already had its idle event acknowledged.
        state.idle = false;
        let mirror_path = root.join("mirror-reports.json");
        let mirror_journal = if per_job {
            MirrorJournal::open(&mirror_path)?
        } else {
            MirrorJournal::default()
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
            reports: std::sync::atomic::AtomicBool::new(false),
            per_job,
            terminal_stored: Mutex::new(None),
            mirror_path,
            mirror_journal: Mutex::new(mirror_journal),
            #[cfg(any(target_os = "linux", test))]
            samples: Mutex::new(BTreeMap::new()),
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
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn sample(&self, sample: MemorySample, accepted_burst_mib: Option<u64>) {
        // Older hosts reject non-lifecycle frames on this channel. Accepting
        // burst is the host's opt-in to receiving memory-sample frames; legacy
        // leases must keep their original lifecycle-only stream.
        if accepted_burst_mib.is_none() {
            return;
        }
        self.samples.lock().unwrap().insert(sample.jti, sample);
    }
    #[cfg(any(target_os = "linux", test))]
    async fn send_samples<W: tokio::io::AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
    ) -> io::Result<()> {
        let samples = std::mem::take(&mut *self.samples.lock().unwrap());
        for sample in samples.into_values() {
            write_frame_async(writer, &Frame::MemorySample(sample)).await?;
        }
        Ok(())
    }
    pub(crate) fn observe(&self, job: &Job) -> io::Result<()> {
        let mut guard = self.state.lock().unwrap();
        let Some(progress) = guard.jobs.get(&job.id) else {
            return Ok(());
        };
        let started = job.started_ms.is_some() && !progress.started;
        let terminal =
            job.terminal.is_some() && !job.memory_accounting_pending && !progress.terminal;
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
            state.pending.back_mut().unwrap().mem_peak_mib = job.mem_peak_mib;
            state.pending.back_mut().unwrap().mem_working_peak_mib = job.mem_working_peak_mib.get();
            state.pending.back_mut().unwrap().mem_over_mib_ms =
                job.mem_over_mib_ms.as_ref().map(|over| over.get());
            state.pending.back_mut().unwrap().remote_cache_totals = job.remote_cache_totals;
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
        let report = state.pending.pop_front().and_then(|e| e.mirror_refs);
        atomic_json_private(&self.path, &state)?;
        *guard = state;
        drop(guard);
        if let Some(report) = report {
            self.ack_report(&report)?;
        }
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

/// The command a job or fill VM runs to end itself once its work is settled.
/// It must be a reboot, not a power-off: Firecracker has no power-off exit, so
/// after `poweroff` the guest halts and the VMM keeps running, holding the
/// workspace disk and its memory, and the host never sees the VM end. With the
/// helper's `reboot=k` boot argument, a guest reboot resets through the
/// keyboard controller, which Firecracker handles by exiting. systemd still
/// stops services and unmounts every disk first, as for a power-off.
pub(crate) fn job_vm_exit_command() -> std::process::Command {
    let mut command = std::process::Command::new("systemctl");
    command.arg("reboot");
    command
}

#[cfg(target_os = "linux")]
pub(crate) fn end_job_vm(kind: &str) -> io::Result<()> {
    let status = job_vm_exit_command().status()?;
    if !status.success() {
        return Err(io::Error::other(format!("{kind} VM exit failed")));
    }
    Ok(())
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
    let control_runner = runner.clone();
    let control = async move {
        if control_runner.config().per_job
            || crate::fill_source::fill_boot(&std::fs::read_to_string("/proc/cmdline")?)
        {
            crate::fill_source::serve_control(control_runner).await
        } else {
            std::future::pending::<io::Result<()>>().await
        }
    };
    let job_connections = async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let socket = tokio::select! {
                socket = jobs.accept() => socket?,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                    if job_runner.job_poweroff_ready() {
                        end_job_vm("job")?;
                        return Ok::<(), io::Error>(());
                    }
                    if !job_runner.config().per_job && job_runner.drain_should_exit() { return Ok::<(), io::Error>(()); }
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
    tokio::select! { result = lifecycle => result, result = job_connections => result, result = control => result }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) async fn event_session<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    runner: &crate::Runner,
    socket: S,
) -> io::Result<()> {
    let log = runner
        .tenant_events()
        .ok_or_else(|| io::Error::other("not a tenant runner"))?;
    let (mut reader, mut writer) = tokio::io::split(socket);
    log.publish_reports()?;
    // Capture the watermark before any await; newly allocated events are live.
    // This additive control frame leaves the retained event and ack bytes alone.
    let mut boundary = serde_json::to_value(log.replay_boundary()).map_err(io::Error::other)?;
    boundary["type"] = serde_json::json!("event_replay");
    let bytes = serde_json::to_vec(&boundary).map_err(io::Error::other)?;
    use tokio::io::AsyncWriteExt;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    loop {
        log.send_samples(&mut writer).await?;
        log.idle(runner.tenant_idle())?;
        if let Some(event) = log.first() {
            let seq = event.event_seq;
            write_frame_async(&mut writer, &Frame::Event(event)).await?;
            let ack = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                read_record_async(&mut reader),
            );
            tokio::pin!(ack);
            let record = loop {
                tokio::select! {
                    result = &mut ack => break result.map_err(io::Error::other)??,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                        log.send_samples(&mut writer).await?;
                    }
                }
            };
            match record {
                Some(DecodedFrame::Frame(Frame::Ack { event_seq })) if event_seq == seq => {
                    log.ack(seq)?
                }
                _ => return Err(io::Error::other("expected tenant event ack")),
            }
        } else {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {},
                _ = read_record_async(&mut reader) => return Err(io::Error::other("unexpected event channel input")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn pre_opt_in_reports_are_bounded_durable_and_count_discarded_oldest_entries() {
        let root = ScratchDir::new("motor-deferred-mirror-reports");
        let log = EventLog::open_for_mode(&root, true).unwrap();
        for index in 0..mirror_reports::REPORT_LIMIT + 2 {
            log.mirror_refs(
                false,
                Path::new("mirror"),
                vec![MirrorRef {
                    repository: None,
                    dependency: None,
                    reference: format!("refs/snapshots/{index}"),
                    commit: index.to_string(),
                }],
            )
            .unwrap();
        }
        assert!(log.first().is_none());
        drop(log);
        let log = EventLog::open_for_mode(&root, true).unwrap();
        log.enable_reports(true, true).unwrap();
        for index in 0..mirror_reports::REPORT_LIMIT {
            let event = log.first().unwrap();
            let report = event.mirror_refs.unwrap();
            assert_eq!(report.dropped_reports, if index == 0 { 2 } else { 0 });
            assert_eq!(report.refs[0].commit, (index + 2).to_string());
            log.ack(event.event_seq).unwrap();
        }
        assert!(log.first().is_none());
    }

    #[test]
    fn corrupt_mirror_journal_is_quarantined_once_and_reports_loss_without_blocking_boot() {
        let root = ScratchDir::new("motor-corrupt-mirror-reports");
        std::fs::write(root.join("mirror-reports.json"), b"not-json").unwrap();
        let log = EventLog::open_for_mode(&root, true).unwrap();
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("mirror-reports.invalid-"))
                .count(),
            1
        );
        log.enable_reports(true, true).unwrap();
        let event = log.first().unwrap();
        let report = event.mirror_refs.as_ref().unwrap();
        assert_eq!(report.dropped_reports, 1);
        assert!(report.refs.is_empty());
        log.ack(event.event_seq).unwrap();
        drop(log);
        let log = EventLog::open_for_mode(&root, true).unwrap();
        log.enable_reports(true, true).unwrap();
        assert!(log.first().is_none());
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("mirror-reports.invalid-"))
                .count(),
            1
        );
    }

    #[test]
    fn tenant_mode_never_creates_a_retained_mirror_journal() {
        let root = ScratchDir::new("motor-tenant-no-mirror-journal");
        let log = EventLog::open_for_mode(&root, false).unwrap();
        log.enable_reports(false, true).unwrap();
        log.mirror_refs(
            false,
            Path::new("mirror"),
            vec![MirrorRef {
                repository: None,
                dependency: None,
                reference: "refs/snapshots/unknown".into(),
                commit: String::new(),
            }],
        )
        .unwrap();
        assert!(!root.join("mirror-reports.json").exists());
        assert!(log.first().is_none());
    }

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
                cause: None,
                setup: None,
                mem_peak_mib: None,
                mem_working_peak_mib: None,
                mem_over_mib_ms: None,
                remote_cache_totals: None,
                mirror_refs: None,
            }
        );
    }
}
