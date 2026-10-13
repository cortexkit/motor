use super::*;
use crate::backend;
use motor_winagent::{Bounds, Frame, Peer, Phase, Protocol, Stream};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::unix::net::UnixStream,
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
}
impl Runtime {
    pub(super) fn defer_cleanup(&self, resources: super::vm::Resources) {
        self.cleanup_ids.lock().unwrap().insert(resources.id);
        self.cleanups.lock().unwrap().push_back(resources);
    }
    pub(crate) fn cleanup_pending(&self, id: Uuid) -> bool {
        self.cleanup_ids.lock().unwrap().contains(&id)
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
    #[cfg(target_os = "linux")]
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
// of growing a queue. The I/O thread checks them even while a socket write blocks.
#[derive(Default)]
struct Control {
    release: AtomicBool,
    cancel: AtomicU8,
    abandoned: AtomicBool,
    done: AtomicBool,
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
    _registration: Registration,
    control: Arc<Control>,
    output: Mutex<Option<backend::Output>>,
    completion: mpsc::Receiver<io::Result<ExecutionOutcome>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for PreparedVm {
    fn drop(&mut self) {
        self.control.abandoned.store(true, Ordering::Release);
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
        self.control.abandoned.store(true, Ordering::Release);
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
                self.bounds.received(self.clock.now())?;
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
    fn reply(&mut self, vm: &mut super::vm::Resources) -> io::Result<Frame> {
        loop {
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
            match self.receive() {
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
                            tx.try_send(bytes)
                                .map_err(|_| invalid("Windows output consumer stalled"))?;
                        }
                    }
                    Frame::Exit { code } => break code,
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
        _registration: registration,
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
        return Err(error);
    }
    Ok(Prepared {
        cwd: PathBuf::from(&setup.request.cwd),
        target: PathBuf::from(&setup.request.workspace_key).join("target"),
        wrapper: None,
        sandbox: None,
        windows: Some(Mutex::new(Some(handle))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, os::fd::AsRawFd};

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
                        version: 1,
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
