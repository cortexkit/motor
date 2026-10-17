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

const ARGS: &str = "/tenant/runner-args";
const ROOTS: [&str; 2] = ["/Users", "/home"];

pub async fn serve_agent() -> io::Result<()> {
    let listener = crate::vsock::Listener::bind(54)?;
    let attach = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let mut tasks = tokio::task::JoinSet::new();
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
        let result = match serde_json::from_value(value) {
            Ok(request) => dispatch(request, &attach).await,
            Err(error) => Err(io::Error::other(error)),
        };
        let reply = match result {
            Ok(value) => value,
            Err(error) => json!({"ok":false,"error":error.to_string()}),
        };
        write_json(&mut socket, &reply).await?;
    }
    Ok(())
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
        forward_socket(&config.socket_path(), frame),
    )
    .await
    .map_err(io::Error::other)?
}

async fn forward_socket(path: &Path, frame: Frame) -> io::Result<Value> {
    let mut socket = tokio::net::UnixStream::connect(path).await?;
    write_frame_async(
        &mut socket,
        &Frame::Hello {
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
        let mut value = serde_json::to_value(reply)?;
        value.as_object_mut().unwrap().remove("type");
        value["ok"] = json!(true);
        return Ok(value);
    }
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
    if unsafe { libc::ioctl(random.as_raw_fd(), 0x40085203 as libc::c_ulong, &entropy) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::ioctl(random.as_raw_fd(), 0x5207 as libc::c_ulong) } < 0 {
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

fn attach_tenant(request: TenantAgentRequest) -> io::Result<()> {
    let TenantAgentRequest::AttachTenant {
        generation,
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
    let resolv = resolvers
        .iter()
        .map(|ip| format!("nameserver {ip}\n"))
        .collect::<String>();
    fs::write("/etc/resolv.conf", resolv)?;
    command("hostname", &[&hostname])?;
    fs::write("/etc/hostname", format!("{hostname}\n"))?;
    fs::write("/etc/machine-id", format!("{machine_id}\n"))?;
    fs::create_dir_all("/tenant")?;
    command("mount", &["-t", "xfs", "/dev/vdb", "/tenant"])?;
    fs::create_dir_all("/tenant/state")?;
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
    fs::create_dir_all("/tenant/cargo-home/bin")?;
    let tool_bin = image_cargo.join("bin");
    let tool_bin = tool_bin
        .to_str()
        .ok_or_else(|| io::Error::other("image tool path is not UTF-8"))?;
    command("mount", &["--bind", tool_bin, "/tenant/cargo-home/bin"])?;
    command(
        "mount",
        &["-o", "remount,bind,ro", "/tenant/cargo-home/bin"],
    )?;
    for root in ROOTS {
        let source = format!("/tenant/roots{root}");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(root)?;
        command("mount", &["--bind", &source, root])?;
    }
    start_after_probes(
        || {
            for root in ROOTS {
                reflink_probe(Path::new("/tenant/state"), Path::new(root))?;
            }
            Ok(())
        },
        || start_runner(&args),
    )?;
    fs::write("/run/motor-tenant-generation", generation.to_string())?;
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
                0x40049409 as libc::c_ulong,
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

fn start_runner(args: &RunnerArgs) -> io::Result<()> {
    let mut cmd = Command::new("systemd-run");
    cmd.args([
        "--unit=motor-tenant-runner",
        "--collect",
        "--service-type=exec",
        "--setenv=CARGO_HOME=/tenant/cargo-home",
    ]);
    for name in ["RUSTUP_HOME", "HOME", "PATH"] {
        let value = std::env::var(name).map_err(io::Error::other)?;
        cmd.arg(format!("--setenv={name}={value}"));
    }
    let status = cmd
        .arg(std::env::current_exe()?)
        .args(["serve", "--state-root", "/tenant/state"])
        .args(&args.words)
        .output()?;
    if !status.status.success() {
        return Err(io::Error::other(format!(
            "tenant runner start failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ARGS: &str = "--backend host --tenant-vm --job-network veth --max-weight 2 --rustc-slots 2 --sccache-size-gib 5 --free-space-floor-bytes 2147483648 --workspace-root /Users --owned-workspace-root /Users --workspace-root /home --owned-workspace-root /home";

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
            let (result, ()) = tokio::join!(forward_socket(&path, request), task);
            assert_eq!(result.unwrap()["ok"], true);
        }
    }
}
