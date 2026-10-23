#[cfg(windows)]
use super::hvsock::Stream as UnixStream;
use super::*;
use crate::backend;
use motor_winagent::{Bounds, Frame, Peer, Phase, Protocol, Stream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    collections::VecDeque,
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Runtime {
    jobs: Mutex<std::collections::BTreeMap<Uuid, Arc<Control>>>,
    cleanups: Mutex<VecDeque<super::vm::Resources>>,
    cleanup_ids: Mutex<std::collections::BTreeSet<Uuid>>,
    #[cfg(test)]
    pub(crate) cache_timings: Mutex<std::collections::BTreeMap<Uuid, motor_winagent::CacheTimings>>,
}
impl Runtime {
    #[cfg(test)]
    pub(crate) fn output_blocked(&self, id: Uuid) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|control| control.output_blocked.load(Ordering::Acquire))
    }
    pub(super) fn defer_cleanup(&self, resources: super::vm::Resources) {
        self.cleanup_ids.lock().unwrap().insert(resources.id);
        self.cleanups.lock().unwrap().push_back(resources);
    }
    pub(crate) fn cleanup_pending(&self, id: Uuid) -> bool {
        self.cleanup_ids.lock().unwrap().contains(&id)
    }
    /// Mark `id`'s cleanup pending, or done, as a failed VM stop would.
    #[cfg(test)]
    pub(crate) fn set_cleanup_pending(&self, id: Uuid, pending: bool) {
        let mut ids = self.cleanup_ids.lock().unwrap();
        if pending {
            ids.insert(id);
        } else {
            ids.remove(&id);
        }
    }
    pub(crate) fn retry_cleanups(&self) {
        // Give each queued job at most one attempt per pass, and cap the work
        // so a broken unit cannot monopolize maintenance or starve later jobs.
        let attempts = self.cleanups.lock().unwrap().len().min(8);
        for _ in 0..attempts {
            let Some(mut resources) = self.cleanups.lock().unwrap().pop_front() else {
                break;
            };
            match resources.finalize() {
                Ok(()) => {
                    self.cleanup_ids.lock().unwrap().remove(&resources.id);
                }
                Err(error) => {
                    tracing::warn!(job_id = %resources.id, %error, "Windows VM cleanup retry failed");
                    self.cleanups.lock().unwrap().push_back(resources);
                }
            }
        }
    }
    pub(super) fn cancel(&self, id: Uuid, hard: bool) -> bool {
        if let Some(control) = self.jobs.lock().unwrap().get(&id) {
            control
                .cancel
                .fetch_max(if hard { 2 } else { 1 }, Ordering::Release);
            control.wake();
            true
        } else {
            false
        }
    }
    pub(super) fn alive(&self, id: Uuid) -> Option<bool> {
        if self.cleanup_pending(id) {
            return Some(true);
        }
        self.jobs
            .lock()
            .unwrap()
            .get(&id)
            .map(|c| !c.done.load(Ordering::Acquire))
    }
    pub(super) fn abort_setup(&self, id: Uuid) {
        self.jobs
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .cancel
            .store(2, Ordering::Release);
    }
    #[cfg(any(target_os = "linux", windows))]
    pub(super) fn setup_aborted(&self, id: Uuid) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|c| c.cancel.load(Ordering::Acquire) > 0)
    }
}
struct Registration {
    runtime: Arc<Runtime>,
    id: Uuid,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.runtime.jobs.lock().unwrap().remove(&self.id);
    }
}

// Atomic flags record start, cancellation and owner-drop requests without
// waiting for the guest socket. Repeated requests update the same flags instead
// of growing a queue. The I/O thread checks them while socket writes or output
// forwarding are blocked.
#[derive(Default)]
struct Control {
    release: AtomicBool,
    cancel: AtomicU8,
    abandoned: AtomicBool,
    done: AtomicBool,
    worker: Mutex<Option<std::thread::Thread>>,
    #[cfg(test)]
    output_blocked: AtomicBool,
}
impl Control {
    fn wake(&self) {
        if let Some(worker) = self.worker.lock().unwrap().as_ref() {
            worker.unpark();
        }
    }
    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
        self.wake();
    }
}
struct ChannelReader {
    receiver: mpsc::Receiver<Vec<u8>>,
    pending: VecDeque<u8>,
}
impl Read for ChannelReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.pending.is_empty() {
            match self.receiver.recv() {
                Ok(bytes) => self.pending.extend(bytes),
                Err(_) => return Ok(0),
            }
        }
        let n = buffer.len().min(self.pending.len());
        for byte in &mut buffer[..n] {
            *byte = self.pending.pop_front().unwrap();
        }
        Ok(n)
    }
}

pub(crate) struct PreparedVm {
    _registration: Option<Registration>,
    control: Arc<Control>,
    output: Mutex<Option<backend::Output>>,
    completion: mpsc::Receiver<io::Result<ExecutionOutcome>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for PreparedVm {
    fn drop(&mut self) {
        self.control.abandon();
    }
}
impl Execution for PreparedVm {
    fn release(&mut self) -> io::Result<()> {
        self.control.release.store(true, Ordering::Release);
        Ok(())
    }
    fn output(&self) -> io::Result<backend::Output> {
        Ok(self
            .output
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| backend::Output {
                stdout: Box::new(io::empty()),
                stderr: Box::new(io::empty()),
            }))
    }
    fn wait(&mut self) -> io::Result<ExecutionOutcome> {
        self.completion
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("Windows I/O thread stopped")))
    }
    fn reap(&self) {
        self.control.abandon();
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

pub(super) struct Clock {
    origin: Instant,
    #[cfg(test)]
    fake: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}
impl Clock {
    fn new(config: &Config) -> Self {
        let _ = config;
        Self {
            origin: Instant::now(),
            #[cfg(test)]
            fake: config.windows_clock.clone(),
        }
    }
    fn now(&self) -> u64 {
        #[cfg(test)]
        if let Some(fake) = &self.fake {
            return fake();
        }
        self.origin.elapsed().as_millis() as u64
    }
}

struct Transport {
    stream: UnixStream,
    agent_path: PathBuf,
    input: Vec<u8>,
    protocol: Protocol,
    bounds: Bounds,
    clock: Clock,
    control: Arc<Control>,
    cancelled: bool,
    pending_cache: Option<motor_winagent::CacheTimings>,
    #[cfg(test)]
    phase_hook: Option<Arc<dyn Fn(Phase) + Send + Sync>>,
    #[cfg(test)]
    blocked_write: Option<Arc<AtomicBool>>,
}
impl Transport {
    fn check(&self) -> io::Result<()> {
        if self.control.abandoned.load(Ordering::Acquire)
            || self.control.cancel.load(Ordering::Acquire) == 2
        {
            return Err(io::Error::other(
                "Windows VM destroyed by cancellation or owner drop",
            ));
        }
        self.bounds.check(self.clock.now())
    }
    fn idle(&self) -> io::Result<()> {
        self.check()?;
        std::thread::park_timeout(Duration::from_millis(5));
        Ok(())
    }
    fn enter(&mut self, phase: Phase) -> io::Result<()> {
        self.bounds.enter(phase, self.clock.now())?;
        #[cfg(test)]
        if let Some(hook) = &self.phase_hook {
            hook(phase);
        }
        Ok(())
    }
    fn send(&mut self, frame: &Frame) -> io::Result<()> {
        self.protocol.accept(Peer::Host, frame)?;
        let mut bytes = Vec::new();
        motor_winagent::write_frame(&mut bytes, frame)?;
        let mut at = 0;
        while at < bytes.len() {
            self.check()?;
            if matches!(frame, Frame::ReceiveTree { .. } | Frame::TreeEnd { .. }) {
                // Mounting precedes extraction. Read diagnostics even when the
                // guest is not consuming upload bytes, so a setup failure cannot
                // be hidden behind a full socket's transfer deadline.
                while let Some(frame) = self.receive()? {
                    self.cache_diagnostic(frame)?;
                }
            }
            match self.stream.write(&bytes[at..]) {
                Ok(0) => {
                    return Err(super::vm::path_error(
                        "write agent socket",
                        &self.agent_path,
                        io::Error::new(io::ErrorKind::WriteZero, "agent write returned zero"),
                    ))
                }
                Ok(n) => at += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    #[cfg(test)]
                    if let Some(blocked) = &self.blocked_write {
                        blocked.store(true, Ordering::Release);
                    }
                    self.idle()?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(super::vm::path_error(
                        "write agent socket",
                        &self.agent_path,
                        e,
                    ))
                }
            }
        }
        // Completed chunks refresh upload inactivity, rather than imposing one
        // 60-second limit on the entire snapshot. Cache heartbeats do not refresh it.
        if matches!(frame, Frame::ReceiveTree { .. }) {
            self.bounds.received(self.clock.now())?;
        }
        Ok(())
    }
    fn receive(&mut self) -> io::Result<Option<Frame>> {
        loop {
            self.check()?;
            // Read the four-byte length header and reject lengths above 1 MiB
            // before allocating space for the payload. Read only that frame's
            // bytes; receiving part of a frame does not extend the reply deadline,
            // so a guest cannot keep the host waiting by trickling out bytes.
            let wanted = if self.input.len() < 4 {
                4
            } else {
                let len = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
                if len == 0 || len > motor_winagent::MAX_FRAME_BYTES {
                    return Err(invalid("frame length must be between 1 byte and 1 MiB"));
                }
                len + 4
            };
            if self.input.len() == wanted && wanted > 4 {
                let frame = motor_winagent::read_frame(&mut io::Cursor::new(&self.input))?.unwrap();
                self.protocol.accept(Peer::Agent, &frame)?;
                if !matches!(
                    frame,
                    Frame::CacheProgress { .. } | Frame::CacheFailed { .. }
                ) {
                    self.bounds.received(self.clock.now())?;
                }
                self.input.clear();
                return Ok(Some(frame));
            }
            let mut buffer = [0; 16384];
            let count = (wanted - self.input.len()).min(buffer.len());
            match self.stream.read(&mut buffer[..count]) {
                Ok(0) => {
                    return Err(super::vm::path_error(
                        "read agent socket",
                        &self.agent_path,
                        io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "agent closed before dismount",
                        ),
                    ))
                }
                Ok(n) => self.input.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(super::vm::path_error(
                        "read agent socket",
                        &self.agent_path,
                        e,
                    ))
                }
            }
        }
    }
    fn cache_diagnostic(&mut self, frame: Frame) -> io::Result<()> {
        match frame {
            Frame::CacheProgress { step, elapsed_ms } => {
                tracing::info!(?step, elapsed_ms, "Windows guest cache mount progress");
                Ok(())
            }
            Frame::CacheMounted { timings } => {
                self.pending_cache = Some(timings);
                Ok(())
            }
            Frame::CacheFailed { failure }
                if failure.step == motor_winagent::cache_steps::CacheStep::VolumeAcl
                    && failure.message == "VolumeAcl: cache_grant_missing" =>
            {
                Err(io::Error::other(CacheGrantMissing))
            }
            Frame::CacheFailed { failure } => Err(invalid(failure.to_string())),
            _ => Err(invalid("unexpected agent frame during tree upload")),
        }
    }
    fn cancel_if_requested(&mut self) -> io::Result<()> {
        if !self.cancelled
            && self.control.cancel.load(Ordering::Acquire) == 1
            && matches!(
                self.protocol.state(),
                motor_winagent::State::AwaitStarted | motor_winagent::State::Running
            )
        {
            self.send(&Frame::Cancel {})?;
            self.cancelled = true;
        }
        Ok(())
    }
    fn forward_output(
        &mut self,
        tx: &mpsc::SyncSender<Vec<u8>>,
        mut bytes: Vec<u8>,
    ) -> io::Result<()> {
        // Output is forwarded only in Running, which has no guest deadline.
        // Keep the frame until storage catches up, without reading more socket
        // data. Collect's inactivity deadline starts after the exit is read.
        loop {
            self.check()?;
            self.cancel_if_requested()?;
            match tx.try_send(bytes) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(pending)) => {
                    bytes = pending;
                    #[cfg(test)]
                    self.control.output_blocked.store(true, Ordering::Release);
                    self.idle()?;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(invalid("Windows output consumer disconnected"));
                }
            }
        }
    }
    fn reply(&mut self, vm: &mut super::vm::Resources) -> io::Result<Frame> {
        loop {
            if let Some(timings) = self.pending_cache.take() {
                tracing::info!(job_id = %vm.id, ?timings, "Windows guest cache mount timings");
                #[cfg(test)]
                vm.record_cache_timings(timings);
            }
            self.cancel_if_requested()?;
            match self.receive() {
                Ok(Some(Frame::CacheMounted { timings })) => {
                    self.pending_cache = Some(timings);
                    continue;
                }
                Ok(Some(frame @ (Frame::CacheProgress { .. } | Frame::CacheFailed { .. }))) => {
                    self.cache_diagnostic(frame)?;
                    continue;
                }
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(match vm.child.try_wait()? {
                        Some(status) => unit_exit(status),
                        None => error,
                    });
                }
                Err(error) => return Err(error),
            }
            if let Some(status) = vm.child.try_wait()? {
                // The unit can write its final reply and exit after the read
                // returned WouldBlock. Its socket still holds those bytes even
                // after the chardev closes, so drain a complete frame before
                // treating the exit as a missing reply.
                return match self.receive() {
                    Ok(Some(frame)) => Ok(frame),
                    Ok(None) => Err(unit_exit(status)),
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                        Err(unit_exit(status))
                    }
                    Err(error) => Err(error),
                };
            }
            self.idle()?;
        }
    }
}
fn unit_exit(status: std::process::ExitStatus) -> io::Error {
    if status.code() == Some(75) {
        invalid("windows_slot_busy")
    } else {
        invalid(format!(
            "Windows VM unit exited before agent reply: {status}"
        ))
    }
}

#[derive(Debug)]
struct CacheGrantMissing;
impl std::fmt::Display for CacheGrantMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cache_grant_missing")
    }
}
impl std::error::Error for CacheGrantMissing {}

pub(super) fn materialize(config: &Config, trees: &Trees, setup: &Setup) -> io::Result<Prepared> {
    let control = config
        .windows_runtime
        .jobs
        .lock()
        .unwrap()
        .entry(setup.id)
        .or_default()
        .clone();
    let registration = Registration {
        runtime: config.windows_runtime.clone(),
        id: setup.id,
    };
    let mut retried = false;
    let mut handle = loop {
        match materialize_attempt(config, trees, setup, control.clone()) {
            Ok(handle) => break handle,
            Err(error)
                if !retried
                    && error.get_ref().is_some_and(|e| e.is::<CacheGrantMissing>())
                    && control.cancel.load(Ordering::Acquire) == 0 =>
            {
                // The failed attempt has joined and discarded its cache before
                // returning this error. No execution gate has been opened.
                let cache = config.windows.as_ref().unwrap().caches();
                tracing::warn!(job_id = %setup.id, workspace_hash = %cache.key(&setup.request.workspace_key),
                    "cache_grant_missing: discarded Windows cache; retrying once with a fresh disk");
                control.abandoned.store(false, Ordering::Release);
                control.done.store(false, Ordering::Release);
                retried = true;
            }
            Err(error) => return Err(error),
        }
    };
    handle._registration = Some(registration);
    Ok(Prepared {
        cwd: PathBuf::from(&setup.request.cwd),
        target: PathBuf::from(&setup.request.workspace_key).join("target"),
        wrapper: None,
        sandbox: None,
        vm: Some(Mutex::new(Some(Box::new(handle)))),
    })
}

fn materialize_attempt(
    config: &Config,
    trees: &Trees,
    setup: &Setup,
    control: Arc<Control>,
) -> io::Result<PreparedVm> {
    if control.cancel.load(Ordering::Acquire) > 0 {
        return Err(invalid("Windows setup cancelled"));
    }
    let vm_config = config.windows.as_ref().expect("validated Windows config");
    let staging = vm_config
        .layout_root
        .join("staging")
        .join(setup.id.to_string());
    let mut resources = super::vm::Resources::prepare(
        config,
        staging.clone(),
        setup.id,
        &setup.request.workspace_key,
    )
    .map_err(|error| super::vm::path_error("prepare job staging", &staging, error))?;
    let tar = super::prep::prepare(config, trees, setup, &resources.staging).map_err(|error| {
        super::vm::path_error("materialize job staging", &resources.staging, error)
    })?;
    if control.cancel.load(Ordering::Acquire) > 0 {
        return Err(invalid("Windows setup cancelled"));
    }
    let nonce = Uuid::now_v7().to_string();
    let clock = Clock::new(config);
    #[cfg(unix)]
    let listener = resources.listen(config)?;
    let agent_path = resources.agent_path();
    let unit_started_ms = clock.now();
    resources.boot(config, &nonce)?;
    let thread_config = config.clone();
    let id = setup.id;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (stdout_tx, stdout_rx) = mpsc::sync_channel(32);
    let (stderr_tx, stderr_rx) = mpsc::sync_channel(32);
    let command = motor_winagent::Command {
        command: setup.request.command.clone(),
        cwd: setup.request.cwd.clone(),
        workspace: setup.request.workspace_key.clone(),
        source_commit: setup.request.snapshot_commit.clone(),
        env: setup
            .request
            .env
            .iter()
            .filter(|(k, _)| crate::allowed_env(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    let thread_control = control.clone();
    let thread = std::thread::spawn(move || {
        *thread_control.worker.lock().unwrap() = Some(std::thread::current());
        let mut exit_code = None;
        let result = (|| {
            let bounds = Bounds::new(unit_started_ms);
            #[cfg(test)]
            let mut injected = resources.child.agent_stream();
            let stream = loop {
                if thread_control.abandoned.load(Ordering::Acquire)
                    || thread_control.cancel.load(Ordering::Acquire) > 0
                {
                    return Err(invalid("Windows setup withdrawn"));
                }
                bounds.check(clock.now())?;
                // qemu_args sets flock's conflict exit code to 75. Inspect
                // the child first so an occupied VM lock refuses immediately,
                // without waiting for a guest that never booted.
                if let Some(status) = resources.child.try_wait()? {
                    return Err(unit_exit(status));
                }
                #[cfg(test)]
                if let Some(stream) = injected.take() {
                    break stream;
                }
                #[cfg(windows)]
                match resources.connect_agent() {
                    Ok(stream) => break stream,
                    Err(error) => {
                        tracing::debug!(%error, "Hyper-V agent connect attempt");
                        std::thread::park_timeout(Duration::from_millis(
                            motor_winagent::transport::hvsock_retry_pause_ms(
                                error.raw_os_error().unwrap_or(0),
                            ),
                        ));
                    }
                }
                #[cfg(unix)]
                match listener.as_ref().expect("production VM listener").accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::park_timeout(Duration::from_millis(5))
                    }
                    Err(e) => {
                        return Err(super::vm::path_error("accept agent socket", &agent_path, e))
                    }
                }
            };
            stream.set_nonblocking(true).map_err(|error| {
                super::vm::path_error("set agent stream nonblocking", &agent_path, error)
            })?;
            let mut io = Transport {
                stream,
                agent_path: agent_path.clone(),
                input: Vec::new(),
                protocol: Protocol::new(nonce)?,
                bounds,
                clock,
                control: thread_control.clone(),
                cancelled: false,
                pending_cache: None,
                #[cfg(test)]
                phase_hook: thread_config.windows_phase.clone(),
                #[cfg(test)]
                blocked_write: None,
            };
            let _hello = io.reply(&mut resources)?;
            io.enter(Phase::Transfer)?;
            let mut tree = fs::File::open(&tar)
                .map_err(|error| super::vm::path_error("open transfer tree", &tar, error))?;
            let mut buffer = [0; 64 * 1024];
            loop {
                let n = tree
                    .read(&mut buffer)
                    .map_err(|error| super::vm::path_error("read transfer tree", &tar, error))?;
                if n == 0 {
                    break;
                }
                io.send(&Frame::ReceiveTree {
                    bytes: buffer[..n].to_vec(),
                })?;
            }
            io.send(&Frame::TreeEnd { command })?;
            let _received = io.reply(&mut resources)?;
            ready_tx
                .send(Ok(()))
                .map_err(|_| invalid("Windows setup owner disappeared"))?;
            // No transport activity is permitted to open the execution gate.
            while !thread_control.release.load(Ordering::Acquire) {
                if thread_control.abandoned.load(Ordering::Acquire) {
                    return Err(invalid("Windows execution not released"));
                }
                std::thread::park_timeout(Duration::from_millis(5));
            }
            io.enter(Phase::StartAcknowledgement)?;
            io.send(&Frame::Start {})?;
            let _started = io.reply(&mut resources)?;
            io.enter(Phase::Running)?;
            let code = loop {
                match io.reply(&mut resources)? {
                    Frame::Output { stream, bytes } => {
                        let tx = match stream {
                            Stream::Stdout => &stdout_tx,
                            Stream::Stderr => &stderr_tx,
                        };
                        if !bytes.is_empty() {
                            io.forward_output(tx, bytes)?;
                        }
                    }
                    Frame::Exit { code } => {
                        exit_code = Some(code);
                        break code;
                    }
                    _ => return Err(invalid("unexpected running frame")),
                }
            };
            io.enter(Phase::Collect)?;
            io.send(&Frame::Collect {})?;
            let mut collected = Vec::new();
            let deleted = loop {
                match io.reply(&mut resources)? {
                    Frame::Collected { bytes } => collected.extend(bytes),
                    Frame::CollectEnd { deleted } => break deleted,
                    _ => return Err(invalid("unexpected collect frame")),
                }
            };
            super::prep::store_collection(&thread_config, id, &collected, &deleted)?;
            io.enter(Phase::Dismount)?;
            io.send(&Frame::Dismount {})?;
            let _dismounted = io.reply(&mut resources)?;
            io.enter(Phase::UnitExit)?;
            loop {
                if let Some(status) = resources.child.try_wait()? {
                    if !status.success() {
                        return Err(invalid("Windows VM unit did not exit normally"));
                    }
                    break;
                }
                io.idle()?;
            }
            Ok(ExecutionOutcome::WindowsExit { code })
        })();
        let finalized = if result.is_ok() {
            resources.finalize_known()
        } else {
            resources.finalize()
        };
        let result = match finalized {
            Ok(()) => result,
            Err(error) => {
                tracing::warn!(job_id = %id, %error, "Windows VM cleanup deferred to maintenance");
                Err(super::vm::path_error(
                    "finalize job staging",
                    &resources.staging,
                    error,
                ))
            }
        };
        // Once received, the command status is independent of collection and
        // teardown. Failed teardown still takes the discard-cache path above.
        let result = match (exit_code, result) {
            (Some(code), Err(error)) => {
                tracing::warn!(job_id = %id, %error, code, "Windows post-exit cleanup failed; cache discarded or pending discard");
                Ok(ExecutionOutcome::WindowsExit { code })
            }
            (_, result) => result,
        };
        if let Err(error) = &result {
            let _ = ready_tx.try_send(Err(io::Error::new(error.kind(), error.to_string())));
        }
        drop(resources);
        drop(stdout_tx);
        drop(stderr_tx);
        thread_control.done.store(true, Ordering::Release);
        let _ = result_tx.send(result);
    });
    let handle = PreparedVm {
        _registration: None,
        control,
        output: Mutex::new(Some(backend::Output {
            stdout: Box::new(ChannelReader {
                receiver: stdout_rx,
                pending: VecDeque::new(),
            }),
            stderr: Box::new(ChannelReader {
                receiver: stderr_rx,
                pending: VecDeque::new(),
            }),
        })),
        completion: result_rx,
        thread: Mutex::new(Some(thread)),
    };
    let ready = ready_rx
        .recv()
        .unwrap_or_else(|_| Err(invalid("Windows setup I/O thread stopped")));
    if let Err(error) = ready {
        handle.reap();
        return Err(handle
            .completion
            .try_recv()
            .ok()
            .and_then(Result::err)
            .unwrap_or(error));
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, os::fd::AsRawFd};

    fn running_transport(stream: UnixStream, control: Arc<Control>) -> Transport {
        let mut protocol = Protocol::new("nonce").unwrap();
        for (peer, frame) in [
            (
                Peer::Agent,
                Frame::Hello {
                    version: motor_winagent::PROTOCOL_VERSION,
                    nonce: "nonce".into(),
                },
            ),
            (
                Peer::Host,
                Frame::TreeEnd {
                    command: motor_winagent::Command {
                        command: "true".into(),
                        cwd: "/w".into(),
                        workspace: "/w".into(),
                        source_commit: "abc".into(),
                        env: BTreeMap::new(),
                    },
                },
            ),
            (Peer::Agent, Frame::TreeReceived {}),
            (Peer::Host, Frame::Start {}),
            (Peer::Agent, Frame::Started {}),
        ] {
            protocol.accept(peer, &frame).unwrap();
        }
        let mut bounds = Bounds::new(0);
        bounds.enter(Phase::Running, 0).unwrap();
        Transport {
            stream,
            agent_path: PathBuf::from("<backpressured output stream>"),
            input: Vec::new(),
            protocol,
            bounds,
            clock: Clock {
                origin: Instant::now(),
                fake: Some(Arc::new(|| 600_000)),
            },
            control,
            cancelled: false,
            pending_cache: None,
            phase_hook: None,
            blocked_write: None,
        }
    }

    #[test]
    fn windows_backend_output_backpressure_wakes_for_cancel_and_abandon() {
        for abandon in [false, true] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            stream.set_nonblocking(true).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let control = Arc::new(Control::default());
            let mut transport = running_transport(stream, control.clone());
            let (tx, _unread) = mpsc::sync_channel(1);
            tx.send(vec![1]).unwrap();
            let (done, result) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                *transport.control.worker.lock().unwrap() = Some(std::thread::current());
                done.send(transport.forward_output(&tx, vec![2])).unwrap();
            });
            let began = Instant::now();
            while !control.output_blocked.load(Ordering::Acquire) {
                assert!(began.elapsed() < Duration::from_secs(1));
                std::thread::yield_now();
            }
            if abandon {
                control.abandon();
            } else {
                control.cancel.store(1, Ordering::Release);
                control.wake();
                assert_eq!(
                    motor_winagent::read_frame(&mut peer).unwrap(),
                    Some(Frame::Cancel {})
                );
                assert!(result.try_recv().is_err(), "soft cancel discarded output");
                control.cancel.store(2, Ordering::Release);
                control.wake();
            }
            let error = result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err();
            worker.join().unwrap();
            assert!(error.to_string().contains("cancellation or owner drop"));
        }
    }

    #[test]
    fn windows_backend_output_backpressure_retains_pending_frame_after_soft_cancel() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let control = Arc::new(Control::default());
        control.cancel.store(1, Ordering::Release);
        let mut transport = running_transport(stream, control);
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(vec![1]).unwrap();
        let worker = std::thread::spawn(move || transport.forward_output(&tx, vec![2]));
        assert_eq!(
            motor_winagent::read_frame(&mut peer).unwrap(),
            Some(Frame::Cancel {})
        );
        assert_eq!(rx.recv().unwrap(), [1]);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), [2]);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn windows_backend_cache_failure_interrupts_a_blocked_tree_upload() {
        use motor_winagent::cache_steps::{CacheFailure, CacheStep};
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let size = 4096i32;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const i32).cast(),
                    std::mem::size_of_val(&size) as _,
                )
            },
            0
        );
        while stream.write(&[0; 4096]).is_ok() {}
        while stream.write(&[0]).is_ok() {}
        motor_winagent::write_frame(
            &mut peer,
            &Frame::CacheFailed {
                failure: CacheFailure {
                    step: CacheStep::DiskLayout,
                    elapsed_ms: 12,
                    message: "DiskLayout: device not ready (os error 21)".into(),
                    completed: vec![],
                },
            },
        )
        .unwrap();
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let now = clock.clone();
        let mut protocol = Protocol::new("nonce").unwrap();
        protocol
            .accept(
                Peer::Agent,
                &Frame::Hello {
                    version: motor_winagent::PROTOCOL_VERSION,
                    nonce: "nonce".into(),
                },
            )
            .unwrap();
        let mut transport = Transport {
            stream,
            agent_path: PathBuf::from("<cache diagnostic stream>"),
            input: Vec::new(),
            protocol,
            bounds: Bounds::new(0),
            clock: Clock {
                origin: Instant::now(),
                fake: Some(Arc::new(move || now.load(Ordering::SeqCst))),
            },
            control: Arc::new(Control::default()),
            cancelled: false,
            pending_cache: None,
            phase_hook: None,
            blocked_write: None,
        };
        transport.enter(Phase::Transfer).unwrap();
        let (tx, rx) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            tx.send(transport.send(&Frame::ReceiveTree { bytes: vec![1] }))
                .unwrap();
        });
        let result = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(value) => value,
            Err(_) => {
                clock.store(60_000, Ordering::SeqCst);
                rx.recv_timeout(Duration::from_secs(1)).unwrap()
            }
        };
        sender.join().unwrap();
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("DiskLayout") && error.contains("device not ready"),
            "{error}"
        );
        assert!(!error.contains("deadline exceeded"), "{error}");
    }

    #[test]
    fn windows_backend_upload_progress_refreshes_only_the_inactivity_deadline() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let now = clock.clone();
        let mut protocol = Protocol::new("nonce").unwrap();
        protocol
            .accept(
                Peer::Agent,
                &Frame::Hello {
                    version: motor_winagent::PROTOCOL_VERSION,
                    nonce: "nonce".into(),
                },
            )
            .unwrap();
        let mut transport = Transport {
            stream,
            agent_path: PathBuf::from("<active upload stream>"),
            input: Vec::new(),
            protocol,
            bounds: Bounds::new(0),
            clock: Clock {
                origin: Instant::now(),
                fake: Some(Arc::new(move || now.load(Ordering::SeqCst))),
            },
            control: Arc::new(Control::default()),
            cancelled: false,
            pending_cache: None,
            phase_hook: None,
            blocked_write: None,
        };
        transport.enter(Phase::Transfer).unwrap();
        // Small, synchronously drained frames separate protocol time from socket
        // throughput and make every completed send's deadline refresh observable.
        for chunk in 1..=4 {
            clock.store(
                chunk * motor_winagent::REPLY_TIMEOUT_MS / 2,
                Ordering::SeqCst,
            );
            let frame = Frame::ReceiveTree { bytes: vec![42] };
            transport.send(&frame).unwrap();
            assert_eq!(motor_winagent::read_frame(&mut peer).unwrap(), Some(frame));
        }
        assert!(clock.load(Ordering::SeqCst) > motor_winagent::REPLY_TIMEOUT_MS);
        clock.fetch_add(motor_winagent::REPLY_TIMEOUT_MS, Ordering::SeqCst);
        assert_eq!(
            transport
                .send(&Frame::ReceiveTree { bytes: vec![42] })
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn windows_backend_cache_heartbeat_does_not_extend_transfer_deadline() {
        use motor_winagent::cache_steps::CacheStep;
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let now = clock.clone();
        let mut protocol = Protocol::new("nonce").unwrap();
        protocol
            .accept(
                Peer::Agent,
                &Frame::Hello {
                    version: motor_winagent::PROTOCOL_VERSION,
                    nonce: "nonce".into(),
                },
            )
            .unwrap();
        let mut transport = Transport {
            stream,
            agent_path: PathBuf::from("<cache heartbeat stream>"),
            input: Vec::new(),
            protocol,
            bounds: Bounds::new(0),
            clock: Clock {
                origin: Instant::now(),
                fake: Some(Arc::new(move || now.load(Ordering::SeqCst))),
            },
            control: Arc::new(Control::default()),
            cancelled: false,
            pending_cache: None,
            phase_hook: None,
            blocked_write: None,
        };
        transport.enter(Phase::Transfer).unwrap();
        clock.store(59_000, Ordering::SeqCst);
        motor_winagent::write_frame(
            &mut peer,
            &Frame::CacheProgress {
                step: CacheStep::DiskLayout,
                elapsed_ms: 1_000,
            },
        )
        .unwrap();
        assert!(matches!(
            transport.receive().unwrap(),
            Some(Frame::CacheProgress { .. })
        ));
        clock.store(60_000, Ordering::SeqCst);
        assert_eq!(
            transport.receive().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn windows_backend_blocked_start_writes_observe_deadline_and_hard_cancel() {
        for cancel in [false, true] {
            let (mut stream, _unread_peer) = UnixStream::pair().unwrap();
            stream.set_nonblocking(true).unwrap();
            let size = 4096i32;
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        (&size as *const i32).cast(),
                        std::mem::size_of_val(&size) as _,
                    )
                },
                0
            );
            loop {
                match stream.write(&[0; 4096]) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => panic!("fill send buffer: {e}"),
                }
            }
            while stream.write(&[0]).is_ok() {}
            let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let now = clock.clone();
            let control = Arc::new(Control::default());
            let mut protocol = Protocol::new("nonce").unwrap();
            protocol
                .accept(
                    Peer::Agent,
                    &Frame::Hello {
                        version: motor_winagent::PROTOCOL_VERSION,
                        nonce: "nonce".into(),
                    },
                )
                .unwrap();
            protocol
                .accept(
                    Peer::Host,
                    &Frame::TreeEnd {
                        command: motor_winagent::Command {
                            command: "true".into(),
                            cwd: "/w".into(),
                            workspace: "/w".into(),
                            source_commit: "abc".into(),
                            env: BTreeMap::new(),
                        },
                    },
                )
                .unwrap();
            protocol
                .accept(Peer::Agent, &Frame::TreeReceived {})
                .unwrap();
            let blocked = Arc::new(AtomicBool::new(false));
            let mut transport = Transport {
                stream,
                agent_path: PathBuf::from("<injected agent stream>"),
                input: Vec::new(),
                protocol,
                bounds: Bounds::new(0),
                clock: Clock {
                    origin: Instant::now(),
                    fake: Some(Arc::new(move || now.load(Ordering::SeqCst))),
                },
                control: control.clone(),
                cancelled: false,
                pending_cache: None,
                phase_hook: None,
                blocked_write: Some(blocked.clone()),
            };
            transport.enter(Phase::StartAcknowledgement).unwrap();
            let sender = std::thread::spawn(move || transport.send(&Frame::Start {}));
            let start = Instant::now();
            while !blocked.load(Ordering::Acquire) {
                assert!(start.elapsed() < Duration::from_secs(1));
                std::thread::yield_now();
            }
            if cancel {
                control.cancel.store(2, Ordering::Release);
            } else {
                clock.store(60_000, Ordering::SeqCst);
            }
            let error = sender.join().unwrap().unwrap_err();
            assert!(start.elapsed() < Duration::from_secs(1));
            if cancel {
                assert!(error.to_string().contains("cancellation"));
            } else {
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            }
        }
    }
}
