use motor_runner::{serve, Config, Runner};
use std::{io, path::PathBuf};
#[cfg(unix)]
use std::{
    io::{Read, Write},
    time::Instant,
};

const DRAIN_TRANSPORT_MARGIN_S: u64 = 20;
const MAX_DRAIN_TIMEOUT_S: u64 = 86_400;

#[cfg(windows)]
mod native_service;

fn main() -> io::Result<()> {
    // A Boat job unit execs the runner as its PID namespace init. Dispatch it
    // first: it must fork while the process is still single-threaded, before
    // any runtime or logging starts, and its arguments carry the job command
    // as raw bytes.
    #[cfg(target_os = "linux")]
    {
        let mut raw = std::env::args_os().skip(1);
        if raw
            .next()
            .is_some_and(|mode| mode == motor_runner::JOB_PID_INIT_MODE)
        {
            motor_runner::job_pid_init(raw.collect());
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if args.first().is_some_and(|arg| arg == "service") {
        return native_service::dispatch();
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args))
}

async fn run(args: Vec<String>) -> io::Result<()> {
    if version_requested(&args) {
        println!("{}", binary_version());
        return Ok(());
    }
    #[cfg(windows)]
    if args == ["probe-peers"] {
        return motor_runner::hyperv_probe_peers();
    }
    #[cfg(target_os = "linux")]
    if args.first().is_some_and(|arg| arg == "physical-chmod") {
        return motor_runner::physical_chmod(&args[1..]);
    }
    #[cfg(target_os = "linux")]
    if args == ["permission-helper-digest"] {
        println!("{}", motor_runner::permission_helper_digest());
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if args == ["check-permissions-helper"] {
        return motor_runner::check_permissions_helper();
    }
    #[cfg(target_os = "linux")]
    if args
        .first()
        .is_some_and(|arg| arg == "check-tenant-cargo-fetch")
    {
        if args.len() != 3 || args[1] != "--scratch" {
            return Err(io::Error::other(
                "check-tenant-cargo-fetch requires --scratch DIR",
            ));
        }
        motor_runner::check_tenant_cargo_fetch(std::path::Path::new(&args[2]))?;
        println!(
            "PASS runner trusted cargo prefetch: itoa 1.0.18 fetched online into a cold cache"
        );
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if args
        .first()
        .is_some_and(|arg| arg == "check-javascript-setup")
    {
        return check_javascript_setup(&args[1..]);
    }
    #[cfg(target_os = "linux")]
    if args.first().is_some_and(|arg| arg == "tenant-agent") {
        if args.len() != 1 {
            return Err(io::Error::other("tenant-agent takes no arguments"));
        }
        let _ = motor_protocol::logging::init(LOG_PREFIX);
        return motor_runner::serve_agent().await;
    }
    #[cfg(unix)]
    if args.first().is_some_and(|arg| arg == "windows-prepare") {
        if args.len() != 2 {
            return Err(io::Error::other("windows-prepare requires a plan file"));
        }
        return motor_runner::windows_prepare(std::path::Path::new(&args[1]));
    }
    let mut args = args.into_iter();
    let operation = args.next().unwrap_or_else(|| "serve".into());
    let home = PathBuf::from(
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| io::Error::other("HOME/USERPROFILE is unset"))?,
    );
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let mut config = Config::new(data.join("cortexkit/motor-runner"), "main");
    // Never silently fall back to the runner's administrative account on Linux.
    config.isolate_jobs = cfg!(target_os = "linux");
    config.build_slot_root = data.join("cortexkit/motor-build-slots");
    if operation == "forget-caller-root" {
        return forget_caller_root_args(args.collect());
    }
    let (job, drain_timeout_s, exit_after) = configure_options(&mut config, args)?;
    match operation.as_str() {
        "check-options" => Ok(()),
        #[cfg(windows)]
        "qualify-prep" => motor_runner::hyperv_qualify_prep(config),
        #[cfg(windows)]
        "qualify-prep-offline-cargo" => motor_runner::hyperv_qualify_prep_offline_cargo(config),
        #[cfg(windows)]
        "qualify-prep-local-registry" => motor_runner::hyperv_qualify_prep_local_registry(config),
        #[cfg(windows)]
        "qualify-disconnect" => motor_runner::hyperv_qualify_disconnect(config),
        "drain" | "resume" => {
            use motor_protocol::*;
            let resume = operation == "resume";
            return tokio::time::timeout(
                if resume {
                    std::time::Duration::from_secs(20)
                } else {
                    drain_transport_timeout(drain_timeout_s)
                },
                async {
                    let mut socket = motor_runner::local_transport::connect(&config.socket_path()).await?;
                    write_frame_async(
                        &mut socket,
                        &Frame::Hello { return_paths: false, return_chunked: false, sibling_paths: false, protocol_version: PROTOCOL_VERSION, started: true, default_branch_tip: false, gate_jobs: false, platforms: Vec::new() },
                    )
                    .await?;
                    loop {
                        match read_frame_async(&mut socket).await? {
                            Some(Frame::Hello {
                                protocol_version: PROTOCOL_VERSION,
                                ..
                            }) => break,
                            Some(Frame::Heartbeat { .. }) => {}
                            _ => {
                                return Err(io::Error::other(
                                    if resume { "runner did not negotiate resume protocol" } else { "runner did not negotiate drain protocol" },
                                ))
                            }
                        }
                    }
                    write_frame_async(
                        &mut socket,
                        &if resume { Frame::Resume } else { Frame::Drain {
                            deadline_s: drain_timeout_s,
                            exit_after,
                        } },
                    )
                    .await?;
                    loop {
                        match read_frame_async(&mut socket).await? {
                    Some(Frame::ResumeReply { resumed: true }) if resume => {
                        println!("runner admission resumed");
                        return Ok(());
                    }
                    Some(Frame::ResumeReply { resumed: false }) if resume => {
                        return Err(io::Error::other("runner refused to resume admission"));
                    }
                    Some(Frame::DrainReply { timed_out: false, restart_recoverable_jobs }) if !resume => {
                        if let Some(jobs) = restart_recoverable_jobs {
                            println!("runner drain ended without a deadline timeout; leaving {jobs} restart-recoverable jobs to reattach");
                        } else {
                            println!("runner drain ended without a deadline timeout (older daemon did not report restart-recoverable jobs)");
                        }
                        return Ok(());
                    }
                    Some(Frame::DrainReply { timed_out: true, .. }) if !resume => {
                        return Err(io::Error::other(
                            "drain deadline exceeded; surviving jobs will be recovered on restart",
                        ))
                    }
                    None => {
                        if resume {
                            return Err(io::Error::other("runner closed before acknowledging resume (older daemons may not support Resume)"));
                        }
                        return Err(io::Error::other("runner closed before acknowledging drain"))
                    }
                    _ => {}
                }
                    }
                },
            )
            .await
            .map_err(|_| io::Error::other("drain transport deadline exceeded"))?;
        }
        #[cfg(unix)]
        "connect" | "forward" => tokio::task::spawn_blocking(move || -> io::Result<()> {
            let _ = motor_protocol::logging::init(LOG_PREFIX);
            let started = Instant::now();
            tracing::info!(?job, instance = %config.instance, "runner bridge starting");
            let mut read = std::os::unix::net::UnixStream::connect(config.socket_path())?;
            tracing::info!(
                ?job,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "runner bridge socket connected"
            );
            let mut write = read.try_clone()?;
            std::thread::spawn(move || {
                let _ = io::copy(&mut io::stdin().lock(), &mut write);
                let _ = write.shutdown(std::net::Shutdown::Write);
            });
            match forward_output(&mut read, &mut io::stdout().lock(), started) {
                // Closing an SSH channel is normal, not a runner failure.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                result => result,
            }
        })
        .await
        .map_err(io::Error::other)?,
        #[cfg(windows)]
        "connect" | "forward" => {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let pipe = motor_runner::local_transport::connect(&config.socket_path()).await?;
            let (mut read, mut write) = tokio::io::split(pipe);
            let mut input = tokio::io::stdin();
            let mut output = tokio::io::stdout();
            let _ = job;
            let result = tokio::select! {
                result = tokio::io::copy(&mut input, &mut write) => result,
                result = async {
                    let mut buffer = [0; 16 * 1024];
                    let mut total = 0;
                    loop {
                        let count = read.read(&mut buffer).await?;
                        if count == 0 { return Ok::<u64, io::Error>(total); }
                        output.write_all(&buffer[..count]).await?;
                        output.flush().await?;
                        total += count as u64;
                    }
                } => result,
            };
            // Stdin uses a blocking OS read. Exiting also cancels that read when
            // sshd closes the channel, rather than retaining a bridge process.
            if let Err(error) = result { if error.kind() != io::ErrorKind::BrokenPipe { return Err(error); } }
            std::process::exit(0)
        }
        "serve" => {
            // Without a subscriber every tracing event the runner reports (dispatch
            // and persistence failures among them) is silently discarded. Route
            // them to stderr, which the systemd user unit sends to the journal.
            #[cfg(unix)]
            if let Err(error) = motor_protocol::logging::init(LOG_PREFIX) {
                eprintln!("{LOG_PREFIX} logging unavailable: {error}");
            }
            #[cfg(windows)]
            {
                let path = config.state_root.parent().ok_or_else(|| io::Error::other("native state root has no parent"))?.join("evidence/runner.log");
                let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                motor_protocol::logging::init_with_writer(LOG_PREFIX, file).map_err(io::Error::other)?;
            }
            // A tenant runner is root inside its own VM; its privileged helpers
            // run directly, starting with the ones Runner::open itself needs.
            #[cfg(target_os = "linux")]
            if config.tenant_vm {
                motor_runner::run_helpers_directly_as_root()?;
            }
            let runner = Runner::open(config)?;
            #[cfg(windows)]
            native_service::watch_power_and_stop(runner.clone())?;
            #[cfg(unix)]
            {
                let mut signal =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
                let r = runner.clone();
                tokio::spawn(async move {
                    if signal.recv().await.is_some() {
                        tokio::task::spawn_blocking(move || {
                            r.begin_drain(
                                std::time::Duration::from_secs(drain_timeout_s.min(86400)),
                                true,
                            )
                        })
                        .await
                        .ok();
                    }
                });
            }
            let socket = runner.config().socket_path();
            #[cfg(unix)]
            let listener = {
            if socket.exists() {
                std::fs::remove_file(&socket)?;
            }
            let listener = tokio::net::UnixListener::bind(&socket)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            listener
            };
            #[cfg(windows)]
            let listener = motor_runner::local_transport::Listener::bind(&socket,
                std::fs::read_to_string(runner.config().state_root.parent().unwrap().join("ssh-sid.txt"))?.trim().to_owned())?;
            tracing::info!(
                instance = %runner.config().instance,
                socket = %socket.display(),
                version = binary_version(),
                "motor-runner serving"
            );
            #[cfg(target_os = "linux")]
            if runner.config().tenant_vm {
                if let Some(directory) = &runner.config().tenant_socket_dir {
                    let tenant = motor_runner::serve_tenant_local(runner.clone(), directory);
                    tokio::select! { result = serve(runner.clone(), listener) => result?, result = tenant => result? }
                } else {
                    let tenant = motor_runner::serve_tenant(runner.clone());
                    tokio::select! { result = serve(runner, listener) => result?, result = tenant => result? }
                }
            } else {
                serve(runner, listener).await?;
            }
            #[cfg(not(target_os = "linux"))]
            serve(runner, listener).await?;
            // A bounded drain must not wait indefinitely for a filesystem task
            // on Tokio's blocking pool while the runtime is being destroyed.
            #[cfg(unix)]
            std::process::exit(0);
            #[cfg(windows)]
            Ok(())
        }
        _ => Err(io::Error::other(
              "usage: motor-runner serve|connect|drain|resume|tenant-agent|forget-caller-root --prefix PREFIX [--instance NAME] [--drain-timeout-s SECONDS] [--backend host|vm|windows-vm|macos-vm] [--tenant-vm] [--vm-config FILE] [--development-allow-host-backend] [--remote-cache true|false]",
        )),
    }
}

/// `forget-caller-root --prefix P <serve options>`: retire one mapping with
/// the runner stopped. Serve options are parsed exactly as `check-options`
/// parses them, so instance, state root, boat profile and owned roots match
/// the serve process. A `--caller-root` entry for the forgotten prefix must
/// use the recorded host root; entries for other prefixes are parsed and
/// ignored.
fn forget_caller_root_args(args: Vec<String>) -> io::Result<()> {
    let mut prefix = None;
    let mut serve = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--prefix" {
            prefix =
                Some(args.next().ok_or_else(|| {
                    io::Error::other("forget-caller-root requires --prefix PREFIX")
                })?);
        } else {
            serve.push(arg);
        }
    }
    let prefix =
        prefix.ok_or_else(|| io::Error::other("forget-caller-root requires --prefix PREFIX"))?;
    let home = PathBuf::from(
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| io::Error::other("HOME/USERPROFILE is unset"))?,
    );
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let mut config = Config::new(data.join("cortexkit/motor-runner"), "main");
    config.isolate_jobs = cfg!(target_os = "linux");
    config.build_slot_root = data.join("cortexkit/motor-build-slots");
    let _ = configure_options(&mut config, serve.into_iter())?;
    motor_runner::forget_caller_root(&config, &prefix).map(|_| ())
}

/// `check-javascript-setup --scratch DIR [--tenant-vm] [--boat-profile]`:
/// the role flags are the ones `serve` takes, so the check looks for the
/// JavaScript tools exactly where that serving runner will.
#[cfg(target_os = "linux")]
fn check_javascript_setup(args: &[String]) -> io::Result<()> {
    let mut config = Config::new("/nonexistent", "javascript-setup-check");
    let mut scratch = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tenant-vm" => config.tenant_vm = true,
            "--boat-profile" => config.boat_profile = true,
            "--scratch" => {
                scratch =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        io::Error::other("--scratch needs a directory")
                    })?))
            }
            _ => return Err(io::Error::other(format!("unknown option {arg}"))),
        }
    }
    let scratch =
        scratch.ok_or_else(|| io::Error::other("check-javascript-setup requires --scratch DIR"))?;
    println!(
        "{}",
        motor_runner::check_javascript_setup(&config, &scratch)?
    );
    println!(
        "PASS runner JavaScript setup: bun.lock, package-lock.json and pnpm-lock.yaml installs"
    );
    Ok(())
}

fn configure_options(
    config: &mut Config,
    mut args: impl Iterator<Item = String>,
) -> io::Result<(Option<motor_protocol::Uuid>, u64, bool)> {
    let mut job = None;
    let mut drain_timeout_s = 1800;
    let mut exit_after = true;
    while let Some(arg) = args.next() {
        if arg == "--tenant-vm" {
            config.tenant_vm = true;
            continue;
        }
        if arg == "--boat-profile" {
            config.boat_profile = true;
            config.disk_reserve_bytes = motor_runner::BOAT_DISK_RESERVE_BYTES;
            continue;
        }
        if arg == "--development-allow-host-backend" {
            config.development_allow_host_backend = true;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| io::Error::other("missing option value"))?;
        match arg.as_str() {
            "--instance" => config.instance = value,
            "--remote-cache" => config.remote_cache = value.parse().map_err(io::Error::other)?,
            "--remote-cache-forwarder-port" => {
                let (tool, port) = value
                    .split_once(':')
                    .ok_or_else(|| io::Error::other("expected tool:port"))?;
                if !matches!(tool, "sccache" | "ccache") {
                    return Err(io::Error::other("unknown cache tool"));
                }
                config
                    .remote_cache_forwarder_ports
                    .insert(tool.into(), port.parse().map_err(io::Error::other)?);
            }
            "--state-root" => config.state_root = value.into(),
            "--caller-root" => config
                .caller_roots
                .push(motor_runner::CallerRoot::parse(&value)?),
            "--tenant-socket-dir" => config.tenant_socket_dir = Some(value.into()),
            "--build-slot-root" => config.build_slot_root = value.into(),
            "--max-weight" => config.max_weight = value.parse().map_err(io::Error::other)?,
            "--reserve-daemon-core" => {
                config.reserve_daemon_core = value.parse().map_err(io::Error::other)?
            }
            "--boat-memory-max-bytes" => {
                config.boat_memory_max_bytes = Some(value.parse().map_err(io::Error::other)?)
            }
            "--max-queue-depth" => {
                config.max_queue_depth = value.parse().map_err(io::Error::other)?
            }
            "--rustc-slots" => config.rustc_slots = value.parse().map_err(io::Error::other)?,
            "--sccache-size-gib" => {
                config.sccache_size_gib = value.parse().map_err(io::Error::other)?
            }
            "--free-space-floor-bytes" => {
                config.free_space_floor_bytes = value.parse().map_err(io::Error::other)?
            }
            "--free-space-eviction-bytes" => {
                config.free_space_eviction_bytes = Some(value.parse().map_err(io::Error::other)?)
            }
            "--workspace-root" => config.workspace_roots.push(value.into()),
            "--snapshot-prefetch-timeout-s" => {
                config.snapshot_prefetch_timeout =
                    std::time::Duration::from_secs(value.parse().map_err(io::Error::other)?)
            }
            "--snapshot-javascript-timeout-s" => {
                config.snapshot_javascript_timeout =
                    std::time::Duration::from_secs(value.parse().map_err(io::Error::other)?)
            }
            "--sibling-freshness-entries" => {
                config.sibling_freshness_entries = value.parse().map_err(io::Error::other)?
            }
            "--sibling-snapshot-cap" => {
                config.sibling_snapshot_cap = value.parse().map_err(io::Error::other)?
            }
            "--sibling-snapshot-idle-s" => {
                config.sibling_snapshot_idle =
                    std::time::Duration::from_secs(value.parse().map_err(io::Error::other)?)
            }
            "--owned-workspace-root" => config.owned_workspace_roots.push(value.into()),
            "--backend" => config.backend = value.parse()?,
            "--job-network" => config.job_network = Some(value.parse()?),
            "--vm-config" => config.vm_config = Some(value.into()),
            "--windows-cpus" => config.windows_cpus = Some(value),
            "--windows-slot-lock" => config.windows_slot_lock = Some(value.into()),
            "--linux-cpus" => config.linux_cpus = Some(value),
            "--job" => job = value.parse::<motor_protocol::Uuid>().ok(),
            "--drain-timeout-s" => {
                drain_timeout_s = value.parse::<u64>().map_err(io::Error::other)?
            }
            "--exit-after-drain" => exit_after = value.parse::<bool>().map_err(io::Error::other)?,
            _ => return Err(io::Error::other(format!("unknown option {arg}"))),
        }
    }
    if config.tenant_socket_dir.is_some() && !config.tenant_vm {
        return Err(io::Error::other("--tenant-socket-dir requires --tenant-vm"));
    }
    if config.tenant_vm
        && (!cfg!(target_os = "linux") || config.backend != motor_runner::BackendKind::Host)
    {
        return Err(io::Error::other(
            "--tenant-vm requires the Linux host backend",
        ));
    }
    if config.boat_profile
        && (!cfg!(target_os = "linux")
            || config.backend != motor_runner::BackendKind::Host
            || (config.tenant_vm && config.tenant_socket_dir.is_none()))
    {
        return Err(io::Error::other(
            "--boat-profile requires the Linux host backend; tenant mode must use root-private local sockets",
        ));
    }
    config.check_options()?;
    Ok((job, drain_timeout_s, exit_after))
}

#[cfg(test)]
mod tenant_options {
    use super::*;

    #[test]
    fn tenant_flag_takes_no_value_and_requires_linux_host_backend() {
        let mut config = Config::new("unused", "test");
        let result = configure_options(
            &mut config,
            ["--tenant-vm", "--max-weight", "2"]
                .into_iter()
                .map(str::to_owned),
        );
        assert_eq!(result.is_ok(), cfg!(target_os = "linux"));
        assert!(config.tenant_vm);
        assert_eq!(config.max_weight, 2);
        let mut config = Config::new("unused", "test");
        assert!(configure_options(
            &mut config,
            ["--tenant-vm", "--backend", "vm"]
                .into_iter()
                .map(str::to_owned)
        )
        .is_err());
    }
}

#[test]
fn boat_profile_is_an_explicit_linux_host_option() {
    let mut config = Config::new("/unused", "boat");
    let result = configure_options(&mut config, ["--boat-profile".into()].into_iter());
    assert_eq!(result.is_ok(), cfg!(target_os = "linux"));
    assert!(config.boat_profile);
    let mut config = Config::new("/unused", "boat");
    assert!(configure_options(
        &mut config,
        ["--boat-profile".into(), "--backend".into(), "vm".into()].into_iter()
    )
    .is_err());
    let mut config = Config::new("/unused", "boat");
    let local = configure_options(
        &mut config,
        [
            "--boat-profile".into(),
            "--tenant-vm".into(),
            "--tenant-socket-dir".into(),
            "/run/motor-tenant".into(),
        ]
        .into_iter(),
    );
    assert_eq!(local.is_ok(), cfg!(target_os = "linux"));
}

#[test]
fn daemon_core_reservation_is_disabled_only_by_an_explicit_flag() {
    let mut config = Config::new("/unused", "boat");
    assert!(config.reserve_daemon_core);
    configure_options(
        &mut config,
        ["--reserve-daemon-core".into(), "false".into()].into_iter(),
    )
    .unwrap();
    assert!(!config.reserve_daemon_core);
}

#[test]
fn remote_cache_requires_explicit_opt_in() {
    let mut config = Config::new("/unused", "main");
    assert!(!config.remote_cache);
    configure_options(
        &mut config,
        ["--remote-cache".into(), "true".into()].into_iter(),
    )
    .unwrap();
    assert!(config.remote_cache);
    configure_options(
        &mut config,
        ["--remote-cache".into(), "false".into()].into_iter(),
    )
    .unwrap();
    assert!(!config.remote_cache);
}

fn drain_transport_timeout(deadline_s: u64) -> std::time::Duration {
    std::time::Duration::from_secs(deadline_s.min(MAX_DRAIN_TIMEOUT_S) + DRAIN_TRANSPORT_MARGIN_S)
}

#[cfg(unix)]
fn forward_output(
    read: &mut impl Read,
    write: &mut impl Write,
    started: Instant,
) -> io::Result<()> {
    // On Linux io::copy specializes UnixStream -> stdout pipe into splice with
    // a near-2-GiB request. A blocked next splice can keep the pipe reader from
    // observing a short frame already copied into it. Hello then waits for more
    // socket data (or EOF), even though the runner answered immediately. Use a
    // bounded userspace read and flush each chunk before waiting for more input.
    let mut buffer = [0; 16 * 1024];
    let mut first = true;
    loop {
        let length = match read.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        write.write_all(&buffer[..length])?;
        write.flush()?;
        if first {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                bytes = length,
                "runner bridge first output forwarded"
            );
            first = false;
        }
    }
}

const LOG_PREFIX: &str = "[motor-runner]";

fn version_requested(args: &[String]) -> bool {
    args.len() == 1 && args[0] == "--version"
}

fn binary_version() -> &'static str {
    concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")")
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn check_options_accepts_installed_units_with_and_without_caller_roots_without_state() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-check-options");
        let units = [
            include_str!("../../../script/runner/units/motor-runner.service"),
            include_str!("../../../script/runner/units/motor-runner@e2e.service.d/binary.conf"),
            include_str!("../../../script/boat-image/motor-boat.service"),
        ];
        for unit in units {
            let exec = unit
                .lines()
                .filter_map(|line| line.strip_prefix("ExecStart="))
                .rfind(|line| !line.is_empty())
                .unwrap();
            let options: Vec<String> = exec
                .split_whitespace()
                .skip_while(|word| *word != "serve")
                .skip(1)
                .map(|word| word.replace("%i", "e2e"))
                .collect();
            for flagged in [false, true] {
                let mut args = vec!["check-options".to_string()];
                args.extend(options.clone());
                args.extend([
                    "--state-root".into(),
                    scratch.join("absent-state").to_str().unwrap().into(),
                ]);
                if flagged {
                    args.extend([
                        "--caller-root".into(),
                        "/home=/var/lib/motor-boat/state/callers/home".into(),
                    ]);
                }
                super::run(args).await.unwrap();
                assert!(!scratch.join("absent-state").exists());
            }
        }
        let args = [
            "check-options",
            "--state-root",
            scratch.join("absent-state").to_str().unwrap(),
            "--caller-root",
            "/home=/callers/home",
            "--caller-root",
            "/home/u=/callers/u",
        ]
        .map(String::from)
        .to_vec();
        let error = super::run(args).await.unwrap_err().to_string();
        assert!(
            error.contains("/home") && error.contains("/home/u"),
            "{error}"
        );
        assert!(!scratch.join("absent-state").exists());
    }

    use super::{configure_options, drain_transport_timeout, Config};
    use motor_runner::BackendKind;

    #[test]
    fn sccache_size_defaults_to_200_gib_and_accepts_a_cap() {
        let mut config = Config::new("unused", "main");
        assert_eq!(config.sccache_size_gib, 200);
        configure_options(
            &mut config,
            ["--sccache-size-gib".into(), "17".into()].into_iter(),
        )
        .unwrap();
        assert_eq!(config.sccache_size_gib, 17);
        assert!(configure_options(
            &mut config,
            ["--sccache-size-gib".into(), "invalid".into()].into_iter()
        )
        .is_err());
        for size in [0, u64::MAX] {
            config.sccache_size_gib = size;
            assert!(motor_runner::Runner::open(config.clone())
                .err()
                .unwrap()
                .to_string()
                .contains("invalid runner configuration"));
        }
    }

    #[test]
    fn backend_options_default_to_host_and_accept_explicit_selection() {
        let mut config = Config::new("unused", "main");
        configure_options(&mut config, [].into_iter()).unwrap();
        assert_eq!(config.backend, BackendKind::Host);
        assert_eq!(config.vm_config, None);
        configure_options(&mut config, ["--backend".into(), "host".into()].into_iter()).unwrap();
        assert_eq!(config.backend, BackendKind::Host);
        for flags in [
            ["--backend", "vm", "--vm-config", "vm.toml"],
            ["--vm-config", "vm.toml", "--backend", "vm"],
        ] {
            let mut config = Config::new("unused", "main");
            configure_options(&mut config, flags.map(String::from).into_iter()).unwrap();
            assert_eq!(config.backend, BackendKind::Vm);
            assert_eq!(config.vm_config, Some("vm.toml".into()));
        }
    }

    #[test]
    fn windows_avoidance_flags_are_opt_in_and_preserve_the_cpu_list() {
        let mut config = Config::new("unused", "main");
        assert!(config.windows_cpus.is_none());
        assert!(config.windows_slot_lock.is_none());
        configure_options(
            &mut config,
            [
                "--windows-cpus",
                "12-15",
                "--windows-slot-lock",
                "/srv/build/motor-windows/slot.lock",
            ]
            .map(String::from)
            .into_iter(),
        )
        .unwrap();
        assert_eq!(config.windows_cpus.as_deref(), Some("12-15"));
        assert_eq!(
            config.windows_slot_lock,
            Some("/srv/build/motor-windows/slot.lock".into())
        );
    }

    #[test]
    fn linux_cpu_list_is_opt_in_and_preserved() {
        let mut config = Config::new("unused", "main");
        assert!(config.linux_cpus.is_none());
        configure_options(
            &mut config,
            ["--linux-cpus", "12-15,28-31", "--max-weight", "16"]
                .map(String::from)
                .into_iter(),
        )
        .unwrap();
        assert_eq!(config.linux_cpus.as_deref(), Some("12-15,28-31"));
        assert_eq!(config.max_weight, 16);
    }

    #[test]
    fn backend_options_defer_config_validation_and_reject_unknown_backend() {
        let mut config = Config::new("unused", "main");
        configure_options(&mut config, ["--backend".into(), "vm".into()].into_iter()).unwrap();
        let error = motor_runner::Runner::open(config.clone())
            .err()
            .expect("VM configuration is required at backend selection");
        assert!(error.to_string().contains("--vm-config"));
        let error = configure_options(&mut config, ["--backend".into(), "typo".into()].into_iter())
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("expected host, vm, windows-vm or macos-vm"));
    }

    #[test]
    fn drain_transport_deadline_includes_the_server_deadline_and_margin() {
        assert_eq!(drain_transport_timeout(2).as_secs(), 22);
        assert_eq!(drain_transport_timeout(1200).as_secs(), 1220);
        assert_eq!(drain_transport_timeout(86_401).as_secs(), 86_420);
    }

    use super::{binary_version, version_requested};

    #[test]
    fn version_argument_is_recognized_without_runner_configuration() {
        assert!(version_requested(&["--version".into()]));
        assert!(!version_requested(&[]));
        assert!(!version_requested(&["--version".into(), "extra".into()]));
        assert!(binary_version().starts_with(concat!(env!("CARGO_PKG_VERSION"), " (")));
        assert!(binary_version().ends_with(')'));
    }
}

#[cfg(test)]
#[test]
fn job_network_flag_accepts_only_veth() {
    let mut config = Config::new("/unused", "main");
    configure_options(
        &mut config,
        ["--job-network".into(), "veth".into()].into_iter(),
    )
    .unwrap();
    assert_eq!(
        config.job_network,
        Some(motor_runner::network::JobNetwork::Veth)
    );
    assert!(configure_options(
        &mut config,
        ["--job-network".into(), "pasta".into()].into_iter()
    )
    .is_err());
}
