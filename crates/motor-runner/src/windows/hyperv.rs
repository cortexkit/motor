//! Native Hyper-V VM ownership. Framing, cancellation and collection live in agent.rs.
use super::*;
use crate::native_process::Child;
use std::{
    fs::File,
    process::{ExitStatus, Stdio},
};

pub(crate) fn path_error(operation: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
}

pub(super) fn control(
    config: &Config,
    operation: &str,
    id: Option<Uuid>,
    extra: &[String],
) -> io::Result<Command> {
    let mut cmd = Command::new("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    let script = std::env::current_exe()?
        .parent()
        .ok_or_else(|| invalid("runner has no parent directory"))?
        .join("hyperv.ps1");
    cmd.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(script)
    .args(["-Operation", operation, "-Root"])
    .arg(&config.windows.as_ref().unwrap().layout_root);
    if let Some(id) = id {
        cmd.args(["-Job", &id.to_string()]);
    }
    cmd.args(extra).stdin(Stdio::null());
    Ok(cmd)
}
pub(super) fn checked(command: Command) -> io::Result<std::process::Output> {
    checked_control(command, None, || false)
}
pub(super) fn checked_validation(command: Command) -> io::Result<std::process::Output> {
    // Only startup validation may hash a multi-gigabyte baseline. Leave ordinary
    // native controls on their shorter deadline, with the same Job Object cleanup.
    let output = checked_control(
        command,
        Some(std::time::Duration::from_secs(15 * 60)),
        || false,
    )?;
    tracing::info!(validation = %String::from_utf8_lossy(&output.stdout).trim(), "Hyper-V startup validation completed");
    Ok(output)
}
fn checked_setup(config: &Config, command: Command, id: Uuid) -> io::Result<std::process::Output> {
    checked_control(command, None, || config.windows_runtime.setup_aborted(id))
}
fn checked_control(
    mut command: Command,
    timeout: Option<std::time::Duration>,
    cancelled: impl Fn() -> bool,
) -> io::Result<std::process::Output> {
    let operation = command
        .get_args()
        .skip_while(|arg| *arg != std::ffi::OsStr::new("-Operation"))
        .nth(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into());
    let started = std::time::Instant::now();
    let output = match timeout {
        Some(timeout) => {
            crate::native_process::output_with_timeout(&mut command, timeout, cancelled)
        }
        None => crate::native_process::output(&mut command, cancelled),
    }
    .and_then(check_output)
    .map_err(|error| {
        invalid(format!(
            "Hyper-V {operation} control failed after {:.1}s: {error}",
            started.elapsed().as_secs_f64()
        ))
    })?;
    tracing::info!(%operation, elapsed_ms = started.elapsed().as_millis() as u64, "Hyper-V control completed");
    Ok(output)
}
fn check_output(output: std::process::Output) -> io::Result<std::process::Output> {
    if !output.status.success() {
        return Err(invalid(format!(
            "Hyper-V control failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output)
}

fn qualification_event(
    config: &Config,
    id: Uuid,
    event: &str,
    detail: serde_json::Value,
) -> io::Result<()> {
    use std::io::Write;
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis() as u64;
    let path = config
        .windows
        .as_ref()
        .unwrap()
        .layout_root
        .join(format!("evidence/{id}-qualification.jsonl"));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({"unix_ms":unix_ms,"event":event,"detail":detail}),
    )
    .map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

fn qualification_settle(
    config: &Config,
    id: Uuid,
    worker_pid: u32,
    dropped: std::time::Instant,
) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Threading::{
        GetProcessIoCounters, OpenProcess, IO_COUNTERS, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // Qualification-only timing heuristic: production never depends on guest
    // logs surviving power-off. Process I/O is not a disk durability barrier;
    // the nonce-bound offline guest EOF/count assertion remains mandatory.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, worker_pid) };
    let open_error = raw
        .is_null()
        .then(|| io::Error::last_os_error().to_string());
    let process = if raw.is_null() {
        None
    } else {
        Some(unsafe { OwnedHandle::from_raw_handle(raw) })
    };
    let mut previous = None;
    let mut last_activity = Instant::now();
    let mut last_record = dropped;
    let mut quiet = false;
    let mut available = false;
    qualification_event(
        config,
        id,
        "host-io-settle-start",
        serde_json::json!({
            "source":"vmwp-process-io-not-per-disk", "worker_pid":worker_pid,
            "open_error":open_error,"minimum_ms":15000,"quiet_window_ms":5000,"maximum_ms":60000,
            "qualification_only":true,"production_log_durability_dependency":false
        }),
    )?;
    while dropped.elapsed() < Duration::from_secs(60) {
        let mut sample = None;
        if let Some(process) = &process {
            let mut counters: IO_COUNTERS = unsafe { std::mem::zeroed() };
            if unsafe { GetProcessIoCounters(process.as_raw_handle(), &mut counters) } != 0 {
                sample = Some((counters.ReadTransferCount, counters.WriteTransferCount));
            }
        }
        available = sample.is_some();
        if sample != previous {
            last_activity = Instant::now();
        }
        previous = sample;
        let elapsed_ms = dropped.elapsed().as_millis() as u64;
        let quiet_ms = last_activity.elapsed().as_millis() as u64;
        if last_record.elapsed() >= Duration::from_secs(1) {
            qualification_event(
                config,
                id,
                "host-io-settle-sample",
                serde_json::json!({
                    "source":"vmwp-process-io-not-per-disk", "worker_pid":worker_pid,
                    "since_drop_ms":elapsed_ms,"quiet_ms":quiet_ms,"available":available,
                    "read_bytes":sample.map(|s|s.0),"write_bytes":sample.map(|s|s.1)
                }),
            )?;
            last_record = Instant::now();
        }
        if super::agent::qualification_io_is_quiet(elapsed_ms, quiet_ms, available) {
            quiet = true;
            break;
        }
        std::thread::sleep(
            Duration::from_millis(250)
                .min(Duration::from_secs(60).saturating_sub(dropped.elapsed())),
        );
    }
    qualification_event(
        config,
        id,
        "host-io-settle-finished",
        serde_json::json!({
            "since_drop_ms":dropped.elapsed().as_millis() as u64,"available":available,
            "io_quiet":quiet,"reason":if quiet {"observed-process-io-quiet"} else {"bounded-window-ended-unverified"},
            "agent_log_live_observation":null
        }),
    )
}

fn verify_qualification_guest(
    config: &Config,
    id: Uuid,
    nonce: &str,
    written: usize,
    dropped: std::time::Instant,
) -> io::Result<()> {
    let evidence = config
        .windows
        .as_ref()
        .unwrap()
        .layout_root
        .join(format!("evidence/{id}-guest"));
    let log = fs::read_to_string(evidence.join("hyperv-agent.log"));
    let matches = log
        .as_ref()
        .is_ok_and(|text| super::agent::qualification_eof_matches(text, nonce, written));
    qualification_event(
        config,
        id,
        "offline-agent-log-observed",
        serde_json::json!({
            "since_drop_ms":dropped.elapsed().as_millis() as u64,"present":log.is_ok(),"current_nonce_eof_count_matches":matches,
            "observation":"first-safe-host-observation-after-power-off-not-guest-write-time",
            "error":log.as_ref().err().map(ToString::to_string)
        }),
    )?;
    for name in ["job-disk-binding.json", "mounted-disk-identity.json"] {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence.join(name))?).map_err(io::Error::other)?;
        if value["matched"] != true || value.get("error").is_some() {
            return Err(invalid(format!(
                "Guest disk identity assertion failed; inspect {}",
                evidence.join(name).display()
            )));
        }
    }
    if !matches {
        return Err(invalid("Guest EOF/read-count proof missing after bounded qualification settle; inspect guest evidence"));
    }
    Ok(())
}

pub(crate) struct Resources {
    config: Config,
    pub(super) id: Uuid,
    pub(super) staging: PathBuf,
    pub(super) child: Monitor,
    workspace: String,
    vm_id: Option<Uuid>,
    attached: bool,
    cleaned: bool,
    cleanup_on_drop: bool,
    _slot: Option<File>,
}
pub(super) struct Monitor(Option<Child>);
impl Monitor {
    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.0 {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }
}
impl Resources {
    pub(super) fn prepare(
        config: &Config,
        staging: PathBuf,
        id: Uuid,
        workspace: &str,
    ) -> io::Result<Self> {
        use fs2::FileExt;
        let vm = config.windows.as_ref().unwrap();
        let mut slot = None;
        for index in 0..vm.concurrent_jobs {
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(vm.layout_root.join(format!("slot-{index}.lock")))?;
            if file.try_lock_exclusive().is_ok() {
                slot = Some(file);
                break;
            }
        }
        let slot = slot.ok_or_else(|| invalid("windows_slot_busy"))?;
        fs::create_dir(&staging)?;
        let resources = Self {
            config: config.clone(),
            id,
            staging,
            child: Monitor(None),
            workspace: workspace.into(),
            vm_id: None,
            attached: false,
            cleaned: false,
            cleanup_on_drop: true,
            _slot: Some(slot),
        };
        Ok(resources)
    }
    pub(super) fn agent_path(&self) -> PathBuf {
        PathBuf::from(format!("hvsock:{}", self.id))
    }
    pub(super) fn boot(&mut self, config: &Config, nonce: &str) -> io::Result<()> {
        let vm = config.windows.as_ref().unwrap();
        let cache = vm.caches().disk(&self.workspace);
        checked_setup(
            config,
            control(
                config,
                "Prepare",
                Some(self.id),
                &[
                    "-Parent".into(),
                    vm.baseline_path().to_string_lossy().into(),
                    "-ParentSha256".into(),
                    vm.parent_sha256.clone(),
                    "-OverlayQuotaBytes".into(),
                    vm.overlay_growth_quota_bytes.to_string(),
                    "-Cache".into(),
                    cache.to_string_lossy().into(),
                    "-Nonce".into(),
                    nonce.into(),
                ],
            )?,
            self.id,
        )?;
        vm.caches().attach(&self.workspace, (config.clock)())?;
        self.attached = true;
        let output = checked(control(config, "Id", Some(self.id), &[])?)?;
        self.vm_id = Some(
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .map_err(invalid)?,
        );
        self.child.0 = Some(Child::spawn(
            control(config, "Run", Some(self.id), &[])?
                .stdout(Stdio::null())
                .stderr(Stdio::inherit()),
        )?);
        Ok(())
    }
    pub(super) fn connect_agent(&self) -> io::Result<super::hvsock::Stream> {
        super::hvsock::Stream::connect(
            self.vm_id
                .ok_or_else(|| invalid("Hyper-V VM not created"))?,
        )
    }
    fn cleanup(&mut self, known: bool) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        // Remove collects logs from the stopped guest before deleting its disk.
        // Finish or terminate the monitor first so a late Start-VM cannot race
        // that offline disk inspection.
        if let Some(mut child) = self.child.0.take() {
            if child.try_wait()?.is_none() {
                child.kill()?;
            }
            child.wait()?;
        }
        let extra = if known {
            vec!["-LightweightHarvest".into()]
        } else {
            Vec::new()
        };
        checked(control(&self.config, "Remove", Some(self.id), &extra)?)?;
        if self.attached {
            self.config.windows.as_ref().unwrap().caches().complete(
                &self.workspace,
                (self.config.clock)(),
                known,
                known,
                known,
            )?;
            self.attached = false;
        }
        super::trusted_prep::remove_staging(&self.config, &self.staging)?;
        self.cleaned = true;
        Ok(())
    }
    pub(super) fn finalize(&mut self) -> io::Result<()> {
        self.cleanup(false)
    }
    pub(super) fn finalize_known(&mut self) -> io::Result<()> {
        self.cleanup(true)
    }
}
impl Drop for Resources {
    fn drop(&mut self) {
        if self.cleanup_on_drop && !self.cleaned {
            if let Err(error) = self.cleanup(false) {
                tracing::error!(job_id = %self.id, %error, "Hyper-V resources and slot retained until cleanup retry succeeds");
                let runtime = self.config.windows_runtime.clone();
                let mut config = self.config.clone();
                config.windows_runtime = Default::default();
                runtime.defer_cleanup(Self {
                    config,
                    id: self.id,
                    staging: std::mem::take(&mut self.staging),
                    child: Monitor(self.child.0.take()),
                    workspace: std::mem::take(&mut self.workspace),
                    vm_id: self.vm_id,
                    attached: self.attached,
                    cleaned: false,
                    cleanup_on_drop: false,
                    _slot: self._slot.take(),
                });
            }
        }
    }
}
pub(crate) fn cleanup_orphans(config: &Config) -> io::Result<()> {
    checked(control(config, "Reconcile", None, &[])?)?;
    Ok(())
}

pub(crate) fn qualify_disconnect(config: &Config) -> io::Result<()> {
    use std::{
        io::{Read, Write},
        time::{Duration, Instant},
    };
    let id = Uuid::now_v7();
    let workspace = "/qualification/hyperv-disconnect";
    let staging = config
        .windows
        .as_ref()
        .unwrap()
        .layout_root
        .join("staging")
        .join(id.to_string());
    let mut resources = Resources::prepare(config, staging, id, workspace)?;
    let nonce = super::agent::fresh_hyperv_nonce();
    qualification_event(config, id, "vm-prepare-start", serde_json::json!({}))?;
    resources.boot(config, &nonce)?;
    qualification_event(
        config,
        id,
        "vm-run-requested",
        serde_json::json!({"vm_id":resources.vm_id}),
    )?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut stream = loop {
        match resources.connect_agent() {
            Ok(stream) => break stream,
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    qualification_event(config, id, "agent-connected", serde_json::json!({}))?;
    stream.set_nonblocking(true)?;
    let mut received = Vec::new();
    let hello = loop {
        let mut bytes = [0; 4096];
        match stream.read(&mut bytes) {
            Ok(0) => {
                return Err(invalid(
                    "agent closed before disconnect qualification Hello",
                ))
            }
            Ok(n) => received.extend_from_slice(&bytes[..n]),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error),
        }
        if received.len() >= 4 {
            let length = u32::from_be_bytes(received[..4].try_into().unwrap()) as usize;
            if length == 0 || length > motor_winagent::MAX_FRAME_BYTES {
                return Err(invalid("invalid qualification Hello length"));
            }
            if received.len() >= length + 4 {
                break motor_winagent::read_frame(&mut io::Cursor::new(&received))?
                    .ok_or_else(|| invalid("missing Hello"))?;
            }
        }
    };
    let mut protocol = motor_winagent::Protocol::new(nonce.clone())?;
    protocol.accept(motor_winagent::Peer::Agent, &hello)?;
    qualification_event(
        config,
        id,
        "hello-validated",
        serde_json::json!({"version":motor_winagent::PROTOCOL_VERSION}),
    )?;
    let inspect = checked(control(config, "QualifyInspect", Some(id), &[])?)?;
    let inspect: serde_json::Value =
        serde_json::from_slice(&inspect.stdout).map_err(io::Error::other)?;
    let worker_pid = inspect["worker_pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| invalid("Qualification VM worker PID unavailable"))?;
    qualification_event(config, id, "runtime-child-identity-checked", inspect)?;
    let mut frame = Vec::new();
    motor_winagent::write_frame(
        &mut frame,
        &motor_winagent::Frame::ReceiveTree {
            bytes: vec![0x5a; 65536],
        },
    )?;
    let half = frame.len() / 2;
    let mut written = 0;
    while written < half {
        match stream.write(&frame[written..half]) {
            Ok(0) => return Err(invalid("disconnect qualification write closed")),
            Ok(n) => written += n,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error),
        }
    }
    qualification_event(
        config,
        id,
        "partial-frame-written",
        serde_json::json!({"announced":frame.len(),"written":written}),
    )?;
    let dropped = Instant::now();
    drop(stream);
    qualification_event(config, id, "stream-dropped", serde_json::json!({}))?;
    qualification_settle(config, id, worker_pid, dropped)?;
    resources.finalize()?;
    verify_qualification_guest(config, id, &nonce, written, dropped)?;
    let cache = config.windows.as_ref().unwrap().caches().disk(workspace);
    if cache.exists() || cache.with_extension("attached").exists() {
        return Err(invalid("disconnect left a dirty cache"));
    }
    println!(
        "PASS mid-frame-disconnect job={id} announced={} written={written} dirty-cache-discarded",
        frame.len()
    );
    Ok(())
}
