use motor_runner::{serve, Config, Runner};
use std::{io, io::Read, io::Write, path::PathBuf, time::Instant};

const DRAIN_TRANSPORT_MARGIN_S: u64 = 20;
const MAX_DRAIN_TIMEOUT_S: u64 = 86_400;

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if version_requested(&args) {
        println!("{}", binary_version());
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if args.first().is_some_and(|arg| arg == "tenant-agent") {
        if args.len() != 1 {
            return Err(io::Error::other("tenant-agent takes no arguments"));
        }
        let _ = motor_protocol::logging::init(LOG_PREFIX);
        return motor_runner::serve_agent().await;
    }
    if args.first().is_some_and(|arg| arg == "windows-prepare") {
        if args.len() != 2 {
            return Err(io::Error::other("windows-prepare requires a plan file"));
        }
        return motor_runner::windows_prepare(std::path::Path::new(&args[1]));
    }
    let mut args = args.into_iter();
    let operation = args.next().unwrap_or_else(|| "serve".into());
    let home =
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is unset"))?);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let mut config = Config::new(data.join("cortexkit/motor-runner"), "main");
    // Never silently fall back to the runner's administrative account on Linux.
    config.isolate_jobs = cfg!(target_os = "linux");
    config.build_slot_root = data.join("cortexkit/motor-build-slots");
    let (job, drain_timeout_s, exit_after) = configure_options(&mut config, args)?;
    match operation.as_str() {
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
                    let mut socket = tokio::net::UnixStream::connect(config.socket_path()).await?;
                    write_frame_async(
                        &mut socket,
                        &Frame::Hello { return_paths: false, protocol_version: PROTOCOL_VERSION, started: true, default_branch_tip: false, gate_jobs: false, platforms: Vec::new() },
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
        "connect" => tokio::task::spawn_blocking(move || -> io::Result<()> {
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
        "serve" => {
            // Without a subscriber every tracing event the runner reports (dispatch
            // and persistence failures among them) is silently discarded. Route
            // them to stderr, which the systemd user unit sends to the journal.
            if let Err(error) = motor_protocol::logging::init(LOG_PREFIX) {
                eprintln!("{LOG_PREFIX} logging unavailable: {error}");
            }
            // A tenant runner is root inside its own VM; its privileged helpers
            // run directly, starting with the ones Runner::open itself needs.
            #[cfg(target_os = "linux")]
            if config.tenant_vm {
                motor_runner::run_helpers_directly_as_root()?;
            }
            let runner = Runner::open(config)?;
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
            if socket.exists() {
                std::fs::remove_file(&socket)?;
            }
            let listener = tokio::net::UnixListener::bind(&socket)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            tracing::info!(
                instance = %runner.config().instance,
                socket = %socket.display(),
                version = binary_version(),
                "motor-runner serving"
            );
            #[cfg(target_os = "linux")]
            if runner.config().tenant_vm {
                let tenant = motor_runner::serve_tenant(runner.clone());
                tokio::select! { result = serve(runner, listener) => result?, result = tenant => result? }
            } else {
                serve(runner, listener).await?;
            }
            #[cfg(not(target_os = "linux"))]
            serve(runner, listener).await?;
            // A bounded drain must not wait indefinitely for a filesystem task
            // on Tokio's blocking pool while the runtime is being destroyed.
            std::process::exit(0)
        }
        _ => Err(io::Error::other(
              "usage: motor-runner serve|connect|drain|resume|tenant-agent [--instance NAME] [--drain-timeout-s SECONDS] [--backend host|vm|windows-vm] [--tenant-vm] [--vm-config FILE]",
        )),
    }
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
        let value = args
            .next()
            .ok_or_else(|| io::Error::other("missing option value"))?;
        match arg.as_str() {
            "--instance" => config.instance = value,
            "--state-root" => config.state_root = value.into(),
            "--build-slot-root" => config.build_slot_root = value.into(),
            "--max-weight" => config.max_weight = value.parse().map_err(io::Error::other)?,
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
            "--workspace-root" => config.workspace_roots.push(value.into()),
            "--owned-workspace-root" => config.owned_workspace_roots.push(value.into()),
            "--backend" => config.backend = value.parse()?,
            "--job-network" => config.job_network = Some(value.parse()?),
            "--vm-config" => config.vm_config = Some(value.into()),
            "--windows-cpus" => config.windows_cpus = Some(value),
            "--windows-slot-lock" => config.windows_slot_lock = Some(value.into()),
            "--job" => job = value.parse::<motor_protocol::Uuid>().ok(),
            "--drain-timeout-s" => {
                drain_timeout_s = value.parse::<u64>().map_err(io::Error::other)?
            }
            "--exit-after-drain" => exit_after = value.parse::<bool>().map_err(io::Error::other)?,
            _ => return Err(io::Error::other(format!("unknown option {arg}"))),
        }
    }
    if config.tenant_vm
        && (!cfg!(target_os = "linux") || config.backend != motor_runner::BackendKind::Host)
    {
        return Err(io::Error::other(
            "--tenant-vm requires the Linux host backend",
        ));
    }
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

fn drain_transport_timeout(deadline_s: u64) -> std::time::Duration {
    std::time::Duration::from_secs(deadline_s.min(MAX_DRAIN_TIMEOUT_S) + DRAIN_TRANSPORT_MARGIN_S)
}

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
            .contains("expected host, vm or windows-vm"));
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
