use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use motor_protocol::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::Ipv4Addr,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::Command,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

mod diagnostics;
pub(crate) mod dns;
mod network;
mod paths;
mod resize;
mod runner_socket;
mod snapshot;
mod trim;

const ARGS: &str = "/tenant/runner-args";
pub(crate) use crate::tenant::DNS_MARKER;
/// The tenant's workspace roots, each bound from the tenant disk over the
/// image's own directory before the runner starts. Nothing the runner runs
/// may live beneath them in the image.
pub(crate) const ROOTS: [&str; 2] = ["/Users", "/home"];

pub async fn serve_agent() -> io::Result<()> {
    let listener = crate::vsock::Listener::bind(54)?;
    let attach = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let mut tasks = tokio::task::JoinSet::new();
    let trim_lock = attach.clone();
    tasks.spawn(async move { trim::watch(trim_lock).await });
    loop {
        let socket = tokio::select! {
            socket = listener.accept() => socket?,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
        };
        let attach = attach.clone();
        tasks.spawn(async move {
            if let Err(error) = agent_session(socket, attach).await {
                tracing::warn!(%error, "tenant agent channel detached");
            }
        });
    }
}

async fn read_json<S: AsyncRead + Unpin>(socket: &mut S) -> io::Result<Option<Value>> {
    let mut header = [0; 4];
    if socket.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    socket.read_exact(&mut header[1..]).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(io::Error::other("invalid agent frame length"));
    }
    let mut data = vec![0; len];
    socket.read_exact(&mut data).await?;
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(io::Error::other)
}

async fn write_json<S: AsyncWrite + Unpin>(socket: &mut S, value: &Value) -> io::Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > MAX_FRAME_BYTES {
        return Err(io::Error::other("agent response exceeds frame limit"));
    }
    socket.write_all(&(data.len() as u32).to_be_bytes()).await?;
    socket.write_all(&data).await?;
    socket.flush().await
}

async fn agent_session<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: S,
    attach: std::sync::Arc<tokio::sync::Mutex<()>>,
) -> io::Result<()> {
    while let Some(value) = read_json(&mut socket).await? {
        let result = dispatch_value(value, &attach).await;
        write_json(&mut socket, &agent_result(result)).await?;
    }
    Ok(())
}

fn agent_result(result: io::Result<Value>) -> Value {
    match result {
        Ok(value) => value,
        Err(error) if error.to_string().starts_with("tenant_recovery_failed") => {
            json!({"ok":false,"error":"tenant_recovery_failed","detail":error.to_string()})
        }
        Err(error) => json!({"ok":false,"error":error.to_string()}),
    }
}

async fn dispatch_value(value: Value, attach: &tokio::sync::Mutex<()>) -> io::Result<Value> {
    // Image capabilities can evolve without extending the shared job protocol.
    // Resize uses the same host-only agent listener as attachment and snapshots.
    if value["op"] == "apply_resize" {
        let args = resize::parse(value)?;
        let _guard = attach.lock().await;
        return resize::apply(args).await;
    }
    dispatch(serde_json::from_value(value)?, attach).await
}

async fn dispatch(
    request: TenantAgentRequest,
    attach: &tokio::sync::Mutex<()>,
) -> io::Result<Value> {
    match request {
        request @ TenantAgentRequest::AttachTenant { .. } => {
            let _guard = attach.lock().await;
            tokio::task::spawn_blocking(move || attach_tenant(request))
                .await
                .map_err(io::Error::other)??;
            Ok(json!({"ok":true}))
        }
        TenantAgentRequest::PostRestore { unix_ms, seed } => {
            let _guard = attach.lock().await;
            tokio::task::spawn_blocking(move || clock_and_rng(unix_ms, &seed))
                .await
                .map_err(io::Error::other)??;
            Ok(json!({"ok":true}))
        }
        TenantAgentRequest::ArgsStatus => {
            let args = RunnerArgs::read(Path::new(ARGS))?;
            let mut value = serde_json::to_value(args.status)?;
            value["ok"] = json!(true);
            Ok(value)
        }
        TenantAgentRequest::Cancel { job_id } => {
            forward(Frame::Cancel(CancelRequest::new(job_id))).await
        }
        TenantAgentRequest::Drain {
            deadline_s,
            exit_after,
        } => {
            forward(Frame::Drain {
                deadline_s,
                exit_after,
            })
            .await
        }
        TenantAgentRequest::Resume => forward(Frame::Resume).await,
        TenantAgentRequest::Status => forward(Frame::Status).await,
        TenantAgentRequest::Diagnostics {
            https,
            since_unix_s,
            until_unix_s,
            jti,
        } => {
            let query = diagnostics::JournalQuery {
                since_unix_s,
                until_unix_s,
                jti,
            };
            diagnostics::collect(https, query).await
        }
        TenantAgentRequest::PrepareSnapshot { kind } => {
            let _guard = attach.lock().await;
            snapshot::prepare(kind).await
        }
        TenantAgentRequest::Thaw => {
            let _guard = attach.lock().await;
            snapshot::thaw().await?;
            Ok(json!({"ok":true}))
        }
        TenantAgentRequest::RecoveryStatus { generation } => {
            let recovered = fs::read_to_string("/run/motor-tenant-recovered")?
                .trim()
                .parse::<u64>()
                .map_err(io::Error::other)?;
            if recovered != generation {
                return Err(io::Error::other("tenant recovery generation mismatch"));
            }
            Ok(json!({"ok":true}))
        }
    }
}

async fn forward(frame: Frame) -> io::Result<Value> {
    let config = crate::Config::new("/tenant/state", "main");
    let timeout = match frame {
        Frame::Drain { deadline_s, .. } => deadline_s.min(86400) + 20,
        _ => 20,
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(timeout),
        forward_socket(&config.socket_path(), frame, runner_socket::main_pid),
    )
    .await
    .map_err(io::Error::other)?
}

async fn forward_socket(
    path: &Path,
    frame: Frame,
    mut current_pid: impl FnMut() -> io::Result<Option<u32>>,
) -> io::Result<Value> {
    let expected =
        current_pid()?.ok_or_else(|| io::Error::other("tenant runner has no current MainPID"))?;
    let mut socket = tokio::net::UnixStream::connect(path).await?;
    if !runner_socket::peer_is_current(&socket, expected, &mut current_pid)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runner socket {} does not belong to the current runner MainPID",
                path.display()
            ),
        ));
    }
    write_frame_async(
        &mut socket,
        &Frame::Hello {
            return_paths: false,
            return_chunked: false,
            sibling_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: vec![],
        },
    )
    .await?;
    loop {
        match read_frame_async(&mut socket).await? {
            Some(Frame::Hello {
                protocol_version: PROTOCOL_VERSION,
                ..
            }) => break,
            Some(Frame::Heartbeat { .. }) => {}
            _ => return Err(io::Error::other("runner did not negotiate agent protocol")),
        }
    }
    if !runner_socket::peer_is_current(&socket, expected, &mut current_pid)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runner socket {} changed MainPID before the administrative request",
                path.display()
            ),
        ));
    }
    write_frame_async(&mut socket, &frame).await?;
    loop {
        let reply = match read_frame_async(&mut socket).await? {
            Some(Frame::Heartbeat { .. }) => continue,
            Some(reply) => reply,
            None => return Err(io::Error::other("runner closed before agent reply")),
        };
        let expected = matches!(
            (&frame, &reply),
            (Frame::Cancel(_), Frame::CancelReply(_))
                | (Frame::Drain { .. }, Frame::DrainReply { .. })
                | (Frame::Resume, Frame::ResumeReply { .. })
                | (Frame::Status, Frame::StatusReply(_))
        );
        if !expected {
            return Err(io::Error::other("unexpected runner agent reply"));
        }
        let mut value = agent_reply_value(reply)?;
        value["ok"] = json!(true);
        return Ok(value);
    }
}

/// A runner reply as the JSON object the agent returns: the frame's own
/// fields, without its `type` tag.
fn agent_reply_value(reply: Frame) -> io::Result<Value> {
    let mut value = serde_json::to_value(reply)?;
    value
        .as_object_mut()
        .ok_or_else(|| io::Error::other("runner reply is not a JSON object"))?
        .remove("type");
    Ok(value)
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ArgsStatus {
    max_weight: u32,
    sccache_size_gib: u64,
    free_space_floor_bytes: u64,
    owned_workspace_roots: Vec<PathBuf>,
}
struct RunnerArgs {
    words: Vec<String>,
    status: ArgsStatus,
}
impl RunnerArgs {
    fn read(path: &Path) -> io::Result<Self> {
        Self::parse(&fs::read_to_string(path)?)
    }
    fn parse(text: &str) -> io::Result<Self> {
        // Provisioning writes whitespace-separated options with fixed absolute
        // roots, not shell source. Never interpret substitutions or escapes.
        let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
        let mut iter = words.iter();
        let mut max_weight = None;
        let mut sccache = None;
        let mut floor = None;
        let mut owned = Vec::new();
        let mut roots = Vec::new();
        let mut tenant = false;
        let mut host = false;
        let mut network = false;
        let mut slots = None;
        while let Some(option) = iter.next() {
            if option == "--tenant-vm" {
                tenant = true;
                continue;
            }
            let value = iter
                .next()
                .ok_or_else(|| io::Error::other("runner-args option lacks value"))?;
            match option.as_str() {
                "--backend" if value == "host" => host = true,
                "--job-network" if value == "veth" => network = true,
                "--max-weight" => {
                    max_weight = Some(value.parse::<u32>().map_err(io::Error::other)?)
                }
                "--rustc-slots" => slots = Some(value.parse::<u32>().map_err(io::Error::other)?),
                "--sccache-size-gib" => sccache = Some(value.parse().map_err(io::Error::other)?),
                "--free-space-floor-bytes" => {
                    floor = Some(value.parse().map_err(io::Error::other)?)
                }
                "--workspace-root" => roots.push(PathBuf::from(value)),
                "--owned-workspace-root" => owned.push(PathBuf::from(value)),
                "--state-root" if value == "/tenant/state" => {}
                _ => return Err(io::Error::other("unsupported runner-args option")),
            }
        }
        let required: Vec<PathBuf> = ROOTS.iter().map(PathBuf::from).collect();
        let mut actual_roots = roots;
        let mut actual_owned = owned.clone();
        actual_roots.sort();
        actual_owned.sort();
        let mut required = required;
        required.sort();
        if !host
            || !tenant
            || !network
            || max_weight.unwrap_or(0) == 0
            || slots != max_weight
            || actual_roots != required
            || actual_owned != required
        {
            return Err(io::Error::other(
                "runner-args does not describe a tenant runner",
            ));
        }
        Ok(Self {
            words,
            status: ArgsStatus {
                max_weight: max_weight.unwrap(),
                sccache_size_gib: sccache.ok_or_else(|| io::Error::other("missing cache size"))?,
                free_space_floor_bytes: floor
                    .ok_or_else(|| io::Error::other("missing free-space floor"))?,
                owned_workspace_roots: owned,
            },
        })
    }
}

fn command(program: &str, args: &[&str]) -> io::Result<()> {
    let result = Command::new(program).args(args).output()?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )));
    }
    Ok(())
}

fn seed_bytes(seed: &str) -> io::Result<[u8; 32]> {
    URL_SAFE_NO_PAD
        .decode(seed)
        .map_err(io::Error::other)?
        .try_into()
        .map_err(|_| io::Error::other("seed must contain 32 bytes"))
}
fn clock_and_rng(unix_ms: u64, seed: &str) -> io::Result<()> {
    let seed = seed_bytes(seed)?;
    let time = libc::timespec {
        tv_sec: (unix_ms / 1000).try_into().map_err(io::Error::other)?,
        tv_nsec: ((unix_ms % 1000) * 1_000_000) as _,
    };
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &time) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // RNDADDENTROPY both mixes and credits the host's seed, unlike a plain
    // write to urandom, which does not establish RNG readiness after restore.
    #[repr(C)]
    struct Entropy {
        bits: libc::c_int,
        size: libc::c_int,
        bytes: [u8; 32],
    }
    let entropy = Entropy {
        bits: 256,
        size: 32,
        bytes: seed,
    };
    let random = OpenOptions::new().write(true).open("/dev/urandom")?;
    // `libc::Ioctl` is the request type of the target's libc: unsigned long on
    // glibc, int on musl (the tenant runner is a static musl build).
    if unsafe { libc::ioctl(random.as_raw_fd(), 0x40085203 as libc::Ioctl, &entropy) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::ioctl(random.as_raw_fd(), 0x5207 as libc::Ioctl) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn validate_identity(
    address: &str,
    gateway: &str,
    resolvers: &[String],
    hostname: &str,
    machine_id: &str,
) -> io::Result<()> {
    let ip = address
        .strip_suffix("/30")
        .ok_or_else(|| io::Error::other("address must use /30"))?
        .parse::<Ipv4Addr>()
        .map_err(io::Error::other)?
        .octets();
    let gateway = gateway
        .parse::<Ipv4Addr>()
        .map_err(io::Error::other)?
        .octets();
    if ip[0..2] != [10, 200] || ip[2] == 0 || ip[3] != 2 || gateway != [10, 200, ip[2], 1] {
        return Err(io::Error::other("invalid tenant address or gateway"));
    }
    if resolvers.is_empty() {
        return Err(io::Error::other("resolvers must not be empty"));
    }
    for resolver in resolvers {
        resolver.parse::<Ipv4Addr>().map_err(io::Error::other)?;
    }
    if hostname.is_empty()
        || hostname.len() > 63
        || !hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(io::Error::other("invalid hostname"));
    }
    if machine_id.len() != 32
        || !machine_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || machine_id.bytes().all(|b| b == b'0')
    {
        return Err(io::Error::other("invalid machine_id"));
    }
    Ok(())
}

/// The VM's own name-service files once it takes the tenant's host name.
///
/// Attach configures a VM-root default route, but that does not prove DNS is
/// reachable during boot or setup. Keep global name service local: the daemon
/// gets a private resolver mount and `net` jobs get their own resolver binding.
/// Keep a local hosts entry for the tenant hostname.
fn root_name_service_files(hostname: &str) -> [(&'static str, String); 2] {
    [
        ("/etc/resolv.conf", String::new()),
        (
            "/etc/hosts",
            format!("127.0.0.1\tlocalhost\n127.0.1.1\t{hostname}\n"),
        ),
    ]
}

fn attach_tenant(request: TenantAgentRequest) -> io::Result<()> {
    let TenantAgentRequest::AttachTenant {
        generation,
        recovery_required,
        unix_ms,
        seed,
        address,
        gateway,
        resolvers,
        hostname,
        machine_id,
    } = request
    else {
        unreachable!()
    };
    validate_identity(&address, &gateway, &resolvers, &hostname, &machine_id)?;
    seed_bytes(&seed)?;
    // A duplicate fresh-start request must not mount over a running tenant.
    if Path::new("/run/motor-tenant-generation").exists() {
        return Err(io::Error::other("tenant is already attached"));
    }
    clock_and_rng(unix_ms, &seed)?;
    command("ip", &["link", "set", "eth0", "up"])?;
    command("ip", &["address", "flush", "dev", "eth0"])?;
    command("ip", &["address", "add", &address, "dev", "eth0"])?;
    command(
        "ip",
        &[
            "route", "replace", "default", "via", &gateway, "dev", "eth0",
        ],
    )?;
    for (path, contents) in root_name_service_files(&hostname) {
        fs::write(path, contents)?;
    }
    dns::install(Path::new(dns::DIRECTORY), &resolvers, 0)?;
    fs::write(DNS_MARKER, generation.to_string())?;
    command("hostname", &[&hostname])?;
    fs::write("/etc/hostname", format!("{hostname}\n"))?;
    fs::write("/etc/machine-id", format!("{machine_id}\n"))?;
    match fs::create_dir("/tenant") {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let _mountpoint = paths::Directory::absolute(Path::new("/tenant"), 0)?;
    if recovery_required {
        recover_tenant_disk()?;
    } else {
        command("mount", &["-t", "xfs", "/dev/vdb", "/tenant"])?;
    }
    let layout = paths::prepare_disk_layout(Path::new("/tenant"), 0)?;
    layout.runner.remove_stale_socket("runner.sock")?;
    let args = RunnerArgs::read(Path::new(ARGS))?;
    let image_cargo = PathBuf::from(
        std::env::var_os("CARGO_HOME")
            .ok_or_else(|| io::Error::other("image CARGO_HOME is unset"))?,
    );
    if image_cargo.starts_with("/tenant") {
        return Err(io::Error::other(
            "image tool directory must be outside /tenant",
        ));
    }
    let tool_bin = image_cargo.join("bin");
    paths::Directory::absolute(&tool_bin, 0)?.bind(&layout.cargo_bin, true)?;
    let mut bound_roots = Vec::new();
    for (source, root) in layout.roots.iter().zip(ROOTS) {
        match fs::create_dir(root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let target = paths::Directory::absolute(Path::new(root), 0)?;
        bound_roots.push(source.bind(&target, false)?);
    }
    start_after_probes(
        || {
            for root in &bound_roots {
                reflink_probe(&layout.state.fd_path(), &root.fd_path())?;
            }
            Ok(())
        },
        || start_runner(&args, &layout.runner.fd_path().join("runner.sock")),
    )?;
    fs::write("/run/motor-tenant-generation", generation.to_string())?;
    if recovery_required {
        fs::write("/run/motor-tenant-recovered", generation.to_string())?;
    }
    Ok(())
}

fn recover_tenant_disk() -> io::Result<()> {
    recovery_steps(|step| match step {
        "replay" | "remount" => {
            recovery_command(step, "mount", &["-t", "xfs", "/dev/vdb", "/tenant"])
        }
        "unmount" => recovery_command(step, "umount", &["/tenant"]),
        "xfs_check" => recovery_command(step, "xfs_repair", &["-n", "/dev/vdb"]),
        "stores" => {
            let mirrors = Path::new("/tenant/state/main/mirrors");
            let entries = match fs::read_dir(mirrors) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    return Err(io::Error::other("bare store is not a directory"));
                }
                let path = entry.path();
                recovery_command(
                    "git_fsck",
                    "git",
                    &[
                        "--no-optional-locks",
                        "-c",
                        "safe.directory=*",
                        "-C",
                        path.to_str()
                            .ok_or_else(|| io::Error::other("non-UTF8 bare store"))?,
                        "fsck",
                        "--full",
                        "--no-dangling",
                    ],
                )?;
            }
            Ok(())
        }
        _ => unreachable!(),
    })
}

fn recovery_steps(mut run: impl FnMut(&str) -> io::Result<()>) -> io::Result<()> {
    // Only the guest kernel mounts tenant-controlled XFS. Check without repairs,
    // then verify retained Git objects before any runner can accept work.
    for step in ["replay", "unmount", "xfs_check", "remount", "stores"] {
        run(step).map_err(|error| {
            io::Error::other(format!("tenant_recovery_failed step={step}: {error}"))
        })?;
    }
    Ok(())
}

fn recovery_command(step: &str, program: &str, args: &[&str]) -> io::Result<()> {
    use std::{
        io::Read,
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut child = Command::new(program)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    fn bounded(
        mut reader: impl Read + Send + 'static,
    ) -> std::thread::JoinHandle<io::Result<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut captured = Vec::new();
            let mut bytes = [0; 1024];
            loop {
                let n = reader.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                let keep = n.min(2048usize.saturating_sub(captured.len()));
                captured.extend_from_slice(&bytes[..keep]);
            }
            Ok(captured)
        })
    }
    let stdout = bounded(child.stdout.take().unwrap());
    let stderr = bounded(child.stderr.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = stdout
        .join()
        .map_err(|_| io::Error::other("recovery output reader"))??;
    let err = stderr
        .join()
        .map_err(|_| io::Error::other("recovery error reader"))??;
    if !status.is_some_and(|s| s.success()) {
        return Err(io::Error::other(format!(
            "{step}: status={status:?} stdout={} stderr={}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        )));
    }
    Ok(())
}

fn start_after_probes(
    probe: impl FnOnce() -> io::Result<()>,
    start: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    probe()?;
    start()
}

fn reflink_probe(source: &Path, destination: &Path) -> io::Result<()> {
    let name = format!(".motor-reflink-probe-{}", Uuid::now_v7());
    let src = source.join(&name);
    let dst = destination.join(&name);
    let result = (|| {
        let mut source = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&src)?;
        source.write_all(b"tenant reflink probe\n")?;
        source.sync_all()?;
        let target = OpenOptions::new().create_new(true).write(true).open(&dst)?;
        if unsafe {
            libc::ioctl(
                target.as_raw_fd(),
                0x40049409 as libc::Ioctl,
                source.as_raw_fd(),
            )
        } < 0
        {
            return Err(io::Error::other(format!(
                "tenant FICLONE probe failed: {}",
                io::Error::last_os_error()
            )));
        }
        if fs::read(&dst)? != b"tenant reflink probe\n" {
            return Err(io::Error::other("tenant FICLONE probe contents differ"));
        }
        Ok(())
    })();
    let _ = fs::remove_file(src);
    let _ = fs::remove_file(dst);
    result
}

fn start_runner(args: &RunnerArgs, socket: &Path) -> io::Result<()> {
    let environment = ["RUSTUP_HOME", "HOME", "PATH"]
        .into_iter()
        .map(|name| {
            std::env::var(name)
                .map(|value| (name.to_owned(), value))
                .map_err(io::Error::other)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let status = runner_command(args, &std::env::current_exe()?, &environment).output()?;
    if !status.status.success() {
        return Err(io::Error::other(format!(
            "tenant runner start failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        )));
    }
    runner_socket::wait(
        socket,
        motor_protocol::TENANT_RUNNER_READINESS_WAIT,
        runner_socket::main_pid,
    )
}

fn runner_command(
    args: &RunnerArgs,
    executable: &Path,
    environment: &[(String, String)],
) -> Command {
    let mut cmd = Command::new("systemd-run");
    cmd.args([
        "--unit=motor-tenant-runner",
        "--collect",
        "--service-type=exec",
        "--property=OOMScoreAdjust=-900",
        "--property=OOMPolicy=continue",
        "--setenv=CARGO_HOME=/tenant/cargo-home",
        "--property=PrivateMounts=yes",
        "--property=BindReadOnlyPaths=/run/motor-tenant-daemon/resolv.conf:/etc/resolv.conf",
    ]);
    for (name, value) in environment {
        cmd.arg(format!("--setenv={name}={value}"));
    }
    cmd.arg(executable)
        .args(["serve", "--state-root", "/tenant/state"])
        .args(&args.words);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_replays_checks_read_only_then_remounts_and_verifies_stores_before_start() {
        let mut steps = Vec::new();
        recovery_steps(|step| {
            steps.push(step.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            steps,
            ["replay", "unmount", "xfs_check", "remount", "stores"]
        );
        for failed in ["replay", "unmount", "xfs_check", "remount", "stores"] {
            let mut seen = Vec::new();
            let error = recovery_steps(|step| {
                seen.push(step.to_owned());
                if step == failed {
                    Err(io::Error::other("probe refused"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(seen.last().unwrap(), failed);
            assert!(error.to_string().contains("tenant_recovery_failed"));
            assert!(error.to_string().contains(failed));
        }
    }

    const TEST_ARGS: &str = "--backend host --tenant-vm --job-network veth --max-weight 2 --rustc-slots 2 --sccache-size-gib 5 --free-space-floor-bytes 2147483648 --workspace-root /Users --owned-workspace-root /Users --workspace-root /home --owned-workspace-root /home";

    #[test]
    fn daemon_start_binds_private_dns_and_persistent_cargo_home() {
        let args = RunnerArgs::parse(TEST_ARGS).unwrap();
        let command = runner_command(&args, Path::new("/usr/local/bin/motor-runner"), &[]);
        let words: Vec<_> = command
            .get_args()
            .map(|word| word.to_str().unwrap())
            .collect();
        assert!(words.contains(&"--property=PrivateMounts=yes"));
        assert!(words.contains(&"--property=OOMScoreAdjust=-900"));
        assert!(words.contains(&"--property=OOMPolicy=continue"));
        assert!(words.contains(
            &"--property=BindReadOnlyPaths=/run/motor-tenant-daemon/resolv.conf:/etc/resolv.conf"
        ));
        assert!(words.contains(&"--setenv=CARGO_HOME=/tenant/cargo-home"));
        assert!(!words
            .iter()
            .any(|word| word.contains("PrivateNetwork=yes") || word.contains("JoinsNamespaceOf")));
        assert!(words
            .windows(3)
            .any(|words| words == ["serve", "--state-root", "/tenant/state"]));
    }

    #[test]
    fn args_status_uses_tenant_file_values() {
        let parsed = RunnerArgs::parse(TEST_ARGS).unwrap();
        assert_eq!(
            serde_json::to_value(parsed.status).unwrap(),
            json!({
                "max_weight":2,"sccache_size_gib":5,"free_space_floor_bytes":2147483648_u64,
                "owned_workspace_roots":["/Users","/home"]
            })
        );
        for invalid in [
            TEST_ARGS.replace("--tenant-vm", ""),
            TEST_ARGS.replace("--max-weight 2", "--max-weight 0"),
            TEST_ARGS.replace("--backend host", "--backend vm"),
            TEST_ARGS.replace("/Users", "/tmp"),
        ] {
            assert!(RunnerArgs::parse(&invalid).is_err());
        }
    }

    #[test]
    fn ficlone_failure_never_starts_runner() {
        let started = std::cell::Cell::new(false);
        let result = start_after_probes(
            || Err(io::Error::other("FICLONE failed")),
            || {
                started.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!started.get());
        start_after_probes(
            || Ok(()),
            || {
                started.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(started.get());
    }

    #[test]
    fn identity_and_entropy_formats_are_validated() {
        let valid = "0123456789abcdef0123456789abcdef";
        assert!(validate_identity(
            "10.200.1.2/30",
            "10.200.1.1",
            &["1.1.1.1".into()],
            "tenant-a",
            valid
        )
        .is_ok());
        for id in [
            "",
            "00000000000000000000000000000000",
            "0123456789ABCDEF0123456789ABCDEF",
        ] {
            assert!(validate_identity(
                "10.200.1.2/30",
                "10.200.1.1",
                &["1.1.1.1".into()],
                "tenant-a",
                id
            )
            .is_err());
        }
        assert!(validate_identity(
            "10.200.1.2/30",
            "10.200.2.1",
            &["1.1.1.1".into()],
            "tenant-a",
            valid
        )
        .is_err());
        assert!(seed_bytes(&URL_SAFE_NO_PAD.encode([1; 32])).is_ok());
        assert!(seed_bytes(&URL_SAFE_NO_PAD.encode([1; 31])).is_err());
    }

    #[test]
    fn attach_keeps_global_vm_resolver_empty_and_maps_its_host_name_locally() {
        let files = root_name_service_files("tenant-a");
        let resolv = &files
            .iter()
            .find(|(path, _)| *path == "/etc/resolv.conf")
            .unwrap()
            .1;
        assert!(!resolv.contains("nameserver"), "{resolv}");
        let hosts = &files
            .iter()
            .find(|(path, _)| *path == "/etc/hosts")
            .unwrap()
            .1;
        assert!(
            hosts.lines().any(|line| line
                .split_whitespace()
                .skip(1)
                .any(|name| name == "tenant-a")),
            "{hosts}"
        );
        assert!(hosts.lines().any(|line| line == "127.0.0.1\tlocalhost"));
        // Attachment announces the private resolver in a systemd-owned file
        // that survives its runner's restart and is gone after the VM boots
        // again, which is what tells a helper's unit whether to bind it.
        assert_eq!(DNS_MARKER, "/run/motor-tenant-dns");
        assert!(Path::new(dns::DIRECTORY).is_absolute());
    }

    #[tokio::test]
    async fn agent_forwards_drain_resume_cancel_and_status_over_local_socket() {
        use cortexkit_test_support::ScratchDir;
        let root = ScratchDir::new("motor-tenant-admin");
        let mut config = crate::Config::new(&*root, "main");
        config.manual_tick = true;
        let runner = crate::Runner::open(config).unwrap();
        let path = runner.config().socket_path();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let status = runner.runner_status();
        let id = Uuid::now_v7();
        let exchanges = [
            (
                Frame::Drain {
                    deadline_s: 7,
                    exit_after: false,
                },
                Frame::DrainReply {
                    timed_out: false,
                    restart_recoverable_jobs: Some(0),
                },
            ),
            (
                Frame::Drain {
                    deadline_s: 8,
                    exit_after: true,
                },
                Frame::DrainReply {
                    timed_out: false,
                    restart_recoverable_jobs: Some(0),
                },
            ),
            (Frame::Resume, Frame::ResumeReply { resumed: true }),
            (
                Frame::Cancel(CancelRequest::new(id)),
                Frame::CancelReply(CancelReply::new(id)),
            ),
            (Frame::Status, Frame::StatusReply(status)),
        ];
        for (request, reply) in exchanges {
            let expected = request.clone();
            let task = async {
                let (mut socket, _) = listener.accept().await.unwrap();
                assert!(matches!(
                    read_frame_async(&mut socket).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut socket,
                    &Frame::Hello {
                        return_paths: false,
                        return_chunked: false,
                        sibling_paths: false,
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: false,
                        platforms: vec![],
                    },
                )
                .await
                .unwrap();
                assert_eq!(read_frame_async(&mut socket).await.unwrap(), Some(expected));
                write_frame_async(&mut socket, &reply).await.unwrap();
            };
            let (result, ()) = tokio::join!(
                forward_socket(&path, request, || Ok(Some(std::process::id()))),
                task
            );
            assert_eq!(result.unwrap()["ok"], true);
        }
    }

    #[tokio::test]
    async fn agent_refuses_a_socket_not_owned_by_current_main_pid() {
        use cortexkit_test_support::ScratchDir;
        let root = ScratchDir::new("motor-tenant-wrong-peer");
        let path = root.join("runner.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let task = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(
                read_frame_async(&mut socket).await.unwrap().is_none(),
                "no administrative frame may reach the wrong peer"
            );
        };
        let (result, ()) = tokio::join!(
            forward_socket(&path, Frame::Status, || Ok(Some(std::process::id() + 1))),
            task
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn agent_rechecks_main_pid_after_hello_before_forwarding() {
        use cortexkit_test_support::ScratchDir;
        let root = ScratchDir::new("motor-tenant-peer-restart");
        let path = root.join("runner.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let task = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let hello = read_frame_async(&mut socket).await.unwrap().unwrap();
            assert!(matches!(hello, Frame::Hello { .. }));
            write_frame_async(&mut socket, &hello).await.unwrap();
            assert!(
                read_frame_async(&mut socket).await.unwrap().is_none(),
                "the previous runner must not receive the administrative request"
            );
        };
        let mut lookups = 0;
        let (result, ()) = tokio::join!(
            forward_socket(&path, Frame::Status, || {
                lookups += 1;
                Ok(Some(std::process::id() + u32::from(lookups > 2)))
            }),
            task
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
