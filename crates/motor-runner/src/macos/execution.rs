use super::{
    fs::Dir,
    lifecycle::{self, Entry, Runtime},
    Prepared as Staged,
};
use crate::{
    backend::{self, Backend, Execution, ExecutionOutcome, Launch, Prepared, Setup},
    engine::Config,
    platform,
    storage::{invalid, Job},
    workspace::Trees,
};
use motor_macvm::{HelperReport, JobVmConfig, JobVmRequest, ShareLocation};
use motor_winagent::{Frame, Peer, Profile, Protocol, Stream};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

pub(crate) struct MacosBackend;
impl Backend for MacosBackend {
    fn validate(&self, _config: &Config) -> io::Result<()> {
        if !cfg!(target_os = "macos") && !cfg!(test) {
            return Err(invalid("macos-vm requires a native macOS host"));
        }
        #[cfg(target_os = "macos")]
        {
            let motor = unsafe { libc::getpwnam(c"motor".as_ptr()) };
            if motor.is_null()
                || unsafe { (*motor).pw_uid != libc::geteuid() || (*motor).pw_uid == 0 }
            {
                return Err(invalid(
                    "macos-vm requires the dedicated non-root motor account",
                ));
            }
        }
        Ok(())
    }
    fn initialize(&self, _config: &Config) -> io::Result<()> {
        Ok(())
    }
    fn materialize(&self, config: &Config, trees: &Trees, setup: &Setup) -> io::Result<Prepared> {
        let entry = config.macos_runtime.begin(
            &config.state_root,
            setup.id,
            &setup.request.workspace_key,
        )?;
        let result = prepare(config, trees, setup, entry.clone());
        if result.is_err() {
            config.macos_runtime.finish(&config.state_root, &entry);
        }
        result
    }
    fn launch(
        &self,
        _config: &Config,
        _trees: &Trees,
        setup: &Setup,
        prepared: &Prepared,
        _weight: u32,
    ) -> io::Result<Launch> {
        let execution = prepared
            .vm
            .as_ref()
            .ok_or_else(|| invalid("macOS VM not prepared"))?
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| invalid("macOS execution already transferred"))?;
        Ok(Launch {
            process: Some(platform::ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(setup.id),
            }),
            durable_output: false,
            execution,
        })
    }
    fn cancel(&self, config: &Config, job: &Job, _hard: bool) -> io::Result<()> {
        config.macos_runtime.cancel(job.id);
        Ok(())
    }
    fn unit_alive(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        Ok(process.and_then(|p| p.unit).map(|id| {
            config
                .macos_runtime
                .entry(id)
                .is_some_and(|e| !e.owned.lock().unwrap().record.stop_proven)
        }))
    }
    fn collect_changes(&self, _trees: &Trees, _job: &Job) -> io::Result<Option<Vec<String>>> {
        Ok(None)
    }
    fn reconcile(&self, config: &Config, _interrupted: &[Job]) -> io::Result<Vec<Uuid>> {
        config.macos_runtime.recover(&config.state_root)?;
        Ok(Vec::new())
    }
}

fn prepare(
    config: &Config,
    trees: &Trees,
    setup: &Setup,
    entry: Arc<Entry>,
) -> io::Result<Prepared> {
    let runtime = &config.macos_runtime;
    runtime.workspace(&config.state_root, &entry)?;
    entry.check()?;
    let staging = lifecycle::staging(config, trees.mirrors.clone())?;
    let staged = staging.prepare(&setup.request, entry.cancel.clone())?;
    // Prep never owns a VM slot. Both capacity observations and taking a slot
    // happen under the admission lock, after publication of the job's inputs.
    while !runtime.admit(&config.state_root, &entry)? {
        entry.check()?;
        std::thread::park_timeout(Duration::from_millis(10));
    }
    target_in(config, &entry, &staged)?;
    entry.check()?;
    let vm = config
        .macos
        .as_ref()
        .ok_or_else(|| invalid("missing macOS VM config"))?;
    let paths = entry.owned.lock().unwrap().record.paths.clone();
    motor_macvm::clone_image(&vm.image, &paths.clone)?;
    let nonce = Uuid::now_v7().simple().to_string();
    let share = |host: &Path, tag: String, guest: String| ShareLocation {
        host: host.to_path_buf(),
        tag,
        guest: guest.into(),
    };
    let job = JobVmConfig::new(JobVmRequest {
        clone: paths.clone.clone(),
        nonce_disk: paths.nonce.clone(),
        workspace: share(
            &staged.workspace,
            "workspace".into(),
            setup.request.workspace_key.clone(),
        ),
        siblings: staged
            .siblings
            .iter()
            .enumerate()
            .map(|(i, (guest, host))| share(host, format!("sibling-{i}"), guest.clone()))
            .collect(),
        cargo_home: share(
            &staged.cargo_home,
            "cargo-home".into(),
            motor_macvm::CARGO_HOME.into(),
        ),
    })?;
    motor_macvm::create_nonce_disk(&paths.nonce, &nonce, &job.mounts())?;
    {
        let mut owned = entry.owned.lock().unwrap();
        let helper = motor_macvm::spawn_job_helper(
            &vm.helper,
            &job,
            owned
                .slot
                .as_ref()
                .ok_or_else(|| invalid("missing macOS slot"))?,
        )?;
        // Keep the gate owned even if the following identity write fails.
        owned.helper = Some(helper);
        #[cfg(target_os = "macos")]
        {
            owned.record.helper = Some(motor_macvm::process_identity(
                owned.helper.as_ref().unwrap().child.id() as i32,
            )?);
        }
        #[cfg(not(target_os = "macos"))]
        return Err(invalid("macOS helper process identity unavailable"));
        #[cfg(target_os = "macos")]
        {
            Runtime::save(&config.state_root, &owned.record)?;
            entry.check()?;
            owned.record.gate_open = true;
            Runtime::save(&config.state_root, &owned.record)?;
            owned.helper.as_mut().unwrap().open_gate()?;
        }
    }
    #[allow(unreachable_code)]
    start_io(config, setup, entry, nonce, staged)
}

pub(super) fn target_in(config: &Config, entry: &Entry, staged: &Staged) -> io::Result<()> {
    let record = entry.owned.lock().unwrap().record.clone();
    let workspace = Dir::open(&staged.workspace, false)?;
    workspace.remove("target".as_ref())?;
    let target = config
        .state_root
        .join("targets")
        .join(lifecycle::key(&record.workspace));
    match Dir::open(&target, false) {
        Ok(store) => store.clone_into(
            &workspace.child("target".as_ref(), true)?,
            &lifecycle::control(entry.cancel.clone()),
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
pub(super) fn target_out(config: &Config, entry: &Entry, workspace: &Path) -> io::Result<()> {
    let record = entry.owned.lock().unwrap().record.clone();
    let source = match Dir::open(workspace, false)?.child("target".as_ref(), false) {
        Ok(source) => source,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let parent = Dir::open(&config.state_root.join("targets"), false)?;
    let temporary = record.paths.target_tmp.file_name().unwrap();
    let dest = parent.child(temporary, true)?;
    source.clone_into(&dest, &lifecycle::control(entry.cancel.clone()))?;
    dest.sync_all()?;
    #[cfg(test)]
    if let Some(hook) = &config.long_work {
        hook("macos-target-before-publish");
    }
    entry.check()?;
    parent.publish(temporary, lifecycle::key(&record.workspace).as_ref())
}

struct VmExecution {
    entry: Arc<Entry>,
    released: Arc<AtomicBool>,
    output: Mutex<Option<backend::Output>>,
    result: mpsc::Receiver<io::Result<ExecutionOutcome>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl Drop for VmExecution {
    fn drop(&mut self) {
        self.entry.cancel.cancel();
    }
}
impl Execution for VmExecution {
    fn release(&mut self) -> io::Result<()> {
        self.entry.check()?;
        self.released.store(true, Ordering::Release);
        Ok(())
    }
    fn output(&self) -> io::Result<backend::Output> {
        self.output
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| invalid("macOS output already taken"))
    }
    fn abort(&self) -> io::Result<()> {
        self.entry.cancel.cancel();
        Ok(())
    }
    fn wait(&mut self) -> io::Result<ExecutionOutcome> {
        self.result
            .recv()
            .unwrap_or_else(|_| Err(invalid("macOS I/O worker stopped: output_incomplete")))
    }
    fn reap(&self) {
        self.entry.cancel.cancel();
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}
struct ChannelReader {
    receiver: mpsc::Receiver<Vec<u8>>,
    pending: VecDeque<u8>,
}
impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pending.is_empty() {
            match self.receiver.recv() {
                Ok(bytes) => self.pending.extend(bytes),
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.pending.len());
        for byte in &mut out[..n] {
            *byte = self.pending.pop_front().unwrap();
        }
        Ok(n)
    }
}
fn reader(receiver: mpsc::Receiver<Vec<u8>>) -> Box<dyn Read + Send> {
    Box::new(ChannelReader {
        receiver,
        pending: VecDeque::new(),
    })
}
fn forward(sender: &SyncSender<Vec<u8>>, mut bytes: Vec<u8>, entry: &Entry) -> io::Result<()> {
    loop {
        entry.check()?;
        match sender.try_send(bytes) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(pending)) => {
                bytes = pending;
                #[cfg(test)]
                entry.output_blocked.store(true, Ordering::Release);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(invalid("macOS output reader disconnected"))
            }
        }
        std::thread::park_timeout(Duration::from_millis(5));
    }
}

fn start_io(
    config: &Config,
    setup: &Setup,
    entry: Arc<Entry>,
    nonce: String,
    staged: Staged,
) -> io::Result<Prepared> {
    let config = config.clone();
    let request = setup.request.clone();
    let (stdout_tx, stdout_rx) = mpsc::sync_channel(4);
    let (stderr_tx, stderr_rx) = mpsc::sync_channel(4);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let released = Arc::new(AtomicBool::new(false));
    let thread_entry = entry.clone();
    let thread_release = released.clone();
    let worker = std::thread::Builder::new()
        .name(format!("macos-{}", setup.id))
        .spawn(move || {
            let result = (|| {
                let (socket, mut reports) = {
                    let owned = thread_entry.owned.lock().unwrap();
                    let helper = owned
                        .helper
                        .as_ref()
                        .ok_or_else(|| invalid("missing VM helper"))?;
                    use std::os::fd::FromRawFd;
                    let fd = unsafe {
                        libc::fcntl(helper.reports.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5)
                    };
                    if fd < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    (helper.session.try_clone()?, unsafe {
                        std::fs::File::from_raw_fd(fd)
                    })
                };
                socket.set_nonblocking(true)?;
                let fd = reports.as_raw_fd();
                if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                let deadline = Instant::now() + Duration::from_secs(120);
                service_report(&config, &thread_entry, &mut reports, deadline)?;
                let mut acceptor = motor_macvm::SessionAcceptor::new(
                    nonce.clone(),
                    config.macos.as_ref().unwrap().image.display().to_string(),
                )?;
                acceptor.claim()?;
                let mut transport = Transport {
                    socket,
                    input: Vec::new(),
                    protocol: Protocol::with_profile(Profile::Macos, nonce)?,
                    entry: thread_entry.clone(),
                    deadline: Some(deadline),
                    acceptor: Some(acceptor),
                };
                if let Err(error) = transport.receive() {
                    return Err(hello_error(&mut reports, &thread_entry, &config, error));
                }
                transport.deadline = None;
                ready_tx
                    .send(Ok(()))
                    .map_err(|_| invalid("macOS setup abandoned"))?;
                while !thread_release.load(Ordering::Acquire) {
                    transport.pause()?;
                }
                transport.deadline = Some(Instant::now() + Duration::from_secs(30));
                transport.send(&Frame::TreeEnd {
                    command: motor_winagent::Command {
                        command: request.command,
                        cwd: request.cwd,
                        workspace: request.workspace_key,
                        source_commit: request.snapshot_commit,
                        env: request
                            .env
                            .into_iter()
                            .filter(|(key, _)| crate::allowed_env(key))
                            .collect(),
                    },
                })?;
                if !matches!(transport.receive()?, Frame::Started {}) {
                    return Err(invalid("expected macOS Started"));
                }
                transport.deadline = None;
                run_output(&mut transport, &stdout_tx, &stderr_tx)
            })();
            if let Err(error) = &result {
                let _ = ready_tx.try_send(Err(invalid(error.to_string())));
            }
            let stopped = config.macos_runtime.stop(&config.state_root, &thread_entry);
            if let Err(error) = stopped {
                thread_entry.failed.store(true, Ordering::Release);
                tracing::warn!(%error, "macOS VM stop failed; retaining command outcome and resource ownership");
            }
            if !thread_entry.cancel.0.load(Ordering::Acquire)
                && thread_entry.owned.lock().unwrap().record.stop_proven
            {
                if let Err(error) = target_out(&config, &thread_entry, &staged.workspace) {
                    tracing::warn!(%error, "macOS target copy-back not published");
                }
            }
            drop(stdout_tx);
            drop(stderr_tx);
            thread_entry.active.store(false,Ordering::Release);
            let _ = result_tx.send(result);
            // Tree deletion may be slow. It retains the workspace and slot in
            // the runtime, but must not hold terminal delivery behind unlink IO.
            let cleanup_runtime=config.macos_runtime.clone();
            let cleanup_root=config.state_root.clone();
            if let Err(error)=std::thread::Builder::new().name("macos-cleanup".into()).spawn(move||cleanup_runtime.finish(&cleanup_root,&thread_entry)) {
                tracing::warn!(%error,"macOS cleanup thread unavailable; maintenance will retry");
            }
        })?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Prepared {
            cwd: setup.request.cwd.clone().into(),
            target: staged_target(&setup.request.workspace_key),
            wrapper: None,
            sandbox: None,
            vm: Some(Mutex::new(Some(Box::new(VmExecution {
                entry,
                released,
                output: Mutex::new(Some(backend::Output {
                    stdout: reader(stdout_rx),
                    stderr: reader(stderr_rx),
                })),
                result: result_rx,
                thread: Mutex::new(Some(worker)),
            })))),
        }),
        result => {
            entry.cancel.cancel();
            let _ = worker.join();
            Err(match result {
                Ok(Err(e)) => e,
                _ => invalid("macOS setup worker stopped"),
            })
        }
    }
}
fn staged_target(workspace: &str) -> std::path::PathBuf {
    Path::new(workspace).join("target")
}
fn run_output(
    transport: &mut Transport,
    stdout: &SyncSender<Vec<u8>>,
    stderr: &SyncSender<Vec<u8>>,
) -> io::Result<ExecutionOutcome> {
    loop {
        match transport
            .receive()
            .map_err(|e| invalid(format!("output_incomplete: {e}")))?
        {
            Frame::Output { stream, bytes } => forward(
                if stream == Stream::Stdout {
                    stdout
                } else {
                    stderr
                },
                bytes,
                &transport.entry,
            )?,
            Frame::Exit { code } => return Ok(ExecutionOutcome::Unix(exit_status(code)?)),
            Frame::Signal { signal } => return Ok(ExecutionOutcome::Signal { signal }),
            _ => return Err(invalid("unexpected macOS agent frame")),
        }
    }
}
fn exit_status(code: u32) -> io::Result<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    if code > 255 {
        return Err(invalid("invalid POSIX exit status"));
    }
    Ok(std::process::ExitStatus::from_raw((code as i32) << 8))
}

fn service_report(
    config: &Config,
    entry: &Entry,
    reports: &mut impl Read,
    deadline: Instant,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    loop {
        entry.check()?;
        if Instant::now() >= deadline {
            return Err(invalid("macOS VM boot timed out"));
        }
        let mut byte = [0];
        match reports.read(&mut byte) {
            Ok(0) => return Err(invalid("VM helper closed before reporting its service")),
            Ok(_) if byte[0] == b'\n' => {
                match serde_json::from_slice::<HelperReport>(&bytes).map_err(invalid)? {
                    HelperReport::Started { helper, service } => {
                        let mut owned = entry.owned.lock().unwrap();
                        if Some(helper) != owned.record.helper {
                            return Err(invalid("VM helper identity mismatch"));
                        }
                        owned.record.service = Some(service);
                        return Runtime::save(&config.state_root, &owned.record);
                    }
                    HelperReport::Failed { detail } => return Err(invalid(detail)),
                    _ => return Err(invalid("VM stopped before start")),
                }
            }
            Ok(_) => {
                bytes.push(byte[0]);
                if bytes.len() > 16384 {
                    return Err(invalid("VM helper report too large"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::park_timeout(Duration::from_millis(5))
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}
fn hello_error(
    reports: &mut impl Read,
    entry: &Entry,
    config: &Config,
    original: io::Error,
) -> io::Error {
    let deadline = Instant::now() + Duration::from_millis(150);
    let mut bytes = Vec::new();
    while Instant::now() < deadline && entry.check().is_ok() {
        let mut byte = [0];
        match reports.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => {
                if let Ok(HelperReport::Failed { detail }) = serde_json::from_slice(&bytes) {
                    return invalid(format!(
                        "macOS image {}: {detail}",
                        config.macos.as_ref().unwrap().image.display()
                    ));
                }
                bytes.clear();
            }
            Ok(_) => {
                bytes.push(byte[0]);
                if bytes.len() > 16384 {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::park_timeout(Duration::from_millis(5))
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    original
}

struct Transport {
    socket: UnixStream,
    input: Vec<u8>,
    protocol: Protocol,
    entry: Arc<Entry>,
    deadline: Option<Instant>,
    acceptor: Option<motor_macvm::SessionAcceptor>,
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
impl Transport {
    fn pause(&self) -> io::Result<()> {
        self.entry.check()?;
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(invalid("macOS agent timed out"));
        }
        std::thread::park_timeout(Duration::from_millis(5));
        Ok(())
    }
    fn send(&mut self, frame: &Frame) -> io::Result<()> {
        self.protocol.accept(Peer::Host, frame)?;
        let mut bytes = Vec::new();
        motor_winagent::write_frame(&mut bytes, frame)?;
        let mut at = 0;
        while at < bytes.len() {
            self.entry.check()?;
            match self.socket.write(&bytes[at..]) {
                Ok(0) => return Err(invalid("macOS agent write returned zero")),
                Ok(n) => at += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.pause()?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn receive(&mut self) -> io::Result<Frame> {
        loop {
            self.entry.check()?;
            let wanted = if self.input.len() < 4 {
                4
            } else {
                let n = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
                if n == 0 || n > motor_winagent::MAX_FRAME_BYTES {
                    return Err(invalid("invalid macOS frame length"));
                }
                n + 4
            };
            if wanted > 4 && self.input.len() == wanted {
                let frame = motor_winagent::read_frame(&mut io::Cursor::new(&self.input))?.unwrap();
                if let Some(acceptor) = self.acceptor.take() {
                    acceptor.authenticate(&frame)?;
                }
                self.protocol.accept(Peer::Agent, &frame)?;
                self.input.clear();
                return Ok(frame);
            }
            let mut buffer = [0; 16384];
            let count = (wanted - self.input.len()).min(buffer.len());
            match self.socket.read(&mut buffer[..count]) {
                Ok(0) => {
                    return Err(invalid(
                        "output_incomplete: macOS agent closed without Exit or Signal",
                    ))
                }
                Ok(n) => self.input.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.pause()?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}
