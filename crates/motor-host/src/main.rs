use anyhow::{bail, ensure, Context, Result};
use motor_host::enroll;
use motor_host::helper::Helper;
use motor_host::server::{admin_socket, serve_with_slot, unix_ms, Runtime};
use motor_host::state::{Host, Tenant};
use motor_host::store::SqliteJournal;
use motor_vm_helper::tenant::{validate_name, Registry};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    mode: motor_host::config::HostMode,
    per_job: Option<motor_host::config::PerJobSettings>,
    host_id: String,
    listen: String,
    certificate: PathBuf,
    private_key: PathBuf,
    host_signing_key: PathBuf,
    journal_dir: PathBuf,
    helper_socket: PathBuf,
    registry_dir: PathBuf,
    bindings: PathBuf,
    resolvers: Vec<std::net::Ipv4Addr>,
    admin_socket: PathBuf,
    storage: PathBuf,
    #[serde(default)]
    fake_broker_url: Option<String>,
}

impl Config {
    fn helper(&self) -> Helper {
        Helper {
            socket: self.helper_socket.clone(),
            storage: self.storage.clone(),
            capacity: motor_host::capacity::CONFIG.into(),
            vcpu_ceiling: BTreeMap::new(),
            guest_init: BTreeMap::new(),
            pressure_cgroups: BTreeMap::new(),
        }
    }

    fn checked_settings(
        &self,
        control: &dyn motor_host::helper::Control,
    ) -> Result<motor_host::config::HostSettings> {
        motor_host::config::HostSettings::from_config(self.mode, self.per_job.clone(), control)
    }

    fn startup_settings(
        &self,
        control: &dyn motor_host::helper::Control,
    ) -> Result<motor_host::config::HostSettings> {
        self.checked_settings(control)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    tenant: String,
    sub: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(version) = version_output(std::env::args_os().skip(1)) {
        println!("{version}");
        return Ok(());
    }
    disable_core_dumps()?;
    motor_protocol::logging::init("[motor-host]")?;
    match std::env::args().nth(1).as_deref() {
        Some("keygen") => return keygen(std::env::args().skip(2).collect()),
        Some("show-enrollment") => return show_enrollment(std::env::args().skip(2).collect()),
        Some("check-config") => {
            let args: Vec<_> = std::env::args_os().skip(2).collect();
            ensure!(
                args.len() == 1,
                "usage: motor-host check-config <config.json>"
            );
            check_config(std::path::Path::new(&args[0]))?;
            println!("host config settings valid");
            return Ok(());
        }
        Some("select-job-images") => {
            let mut args: Vec<_> = std::env::args_os().skip(2).collect();
            // `--replace-idle` is only for switching a host from tenant mode back
            // into per-job mode; see `Runtime::select_job_images_replacing_idle`.
            let replace_idle = args.len() == 3 && args[2] == "--replace-idle";
            if replace_idle {
                args.pop();
            }
            ensure!(
                args.len() == 2,
                "usage: motor-host select-job-images <config.json> <generation> [--replace-idle]"
            );
            let config: Config = serde_json::from_reader(File::open(&args[0])?)?;
            let helper = config.helper();
            let settings = config.checked_settings(&helper)?;
            ensure!(
                settings.mode() == motor_host::config::HostMode::PerJob,
                "select-job-images requires per_job mode"
            );
            // Opening the journal takes the same exclusive lock as the server.
            let (journal, snapshot) = SqliteJournal::open(&config.journal_dir)?;
            let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
            host.settings = settings;
            let runtime = Runtime::with_image_generations(config.host_id, host, Arc::new(helper));
            let generation = args[1].to_str().context("generation must be UTF-8")?;
            if replace_idle {
                runtime.select_job_images_replacing_idle(generation, unix_ms())?;
            } else {
                runtime.select_initial_job_images(generation, unix_ms())?;
            }
            println!("initial job image generation selected");
            return Ok(());
        }
        Some("export-journal") => {
            let directory = std::env::args_os()
                .nth(2)
                .context("usage: motor-host export-journal <journal_dir>")?;
            let written = SqliteJournal::export(std::path::Path::new(&directory))?;
            println!("{}", written.display());
            return Ok(());
        }
        _ => {}
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--sandbox")
    {
        let path = std::env::args_os()
            .nth(2)
            .context("usage: motor-host --sandbox <config.json>")?;
        let config = serde_json::from_reader(File::open(path)?)?;
        return motor_host::sandbox::run(config).await;
    }
    let path = std::env::args_os()
        .nth(1)
        .context("usage: motor-host <config.json>")?;
    let config: Config = serde_json::from_reader(File::open(&path)?)?;
    let mut helper = config.helper();
    let settings = config.startup_settings(&helper)?;
    let (broker_endpoint, jwks_url) = if let Some(url) = &config.fake_broker_url {
        motor_host::broker::trusted_override_config(std::path::Path::new(&path))?;
        let endpoint = motor_host::broker::fake_broker_endpoint(url, &config.listen)?;
        tracing::warn!(
            endpoint,
            "QUALIFICATION ONLY: fake broker override enabled; not production admission"
        );
        (
            endpoint.clone(),
            format!("{endpoint}/.well-known/jwks.json"),
        )
    } else {
        (motor_lease::ISSUER.into(), motor_lease::JWKS_URL.into())
    };
    ensure!(
        !config.host_id.is_empty() && !config.resolvers.is_empty(),
        "host identity and resolvers are required"
    );
    let bindings: BTreeMap<String, Binding> =
        serde_json::from_reader(File::open(&config.bindings)?)?;
    let mut tenants = BTreeMap::new();
    let mut guest_init = BTreeMap::new();
    let mut pressure_cgroups = BTreeMap::new();
    for (vm, binding) in bindings {
        ensure!(
            !vm.is_empty() && !guest_init.contains_key(&binding.tenant),
            "duplicate tenant or empty VM binding"
        );
        validate_name(&binding.tenant)?;
        let registry: Registry = serde_json::from_reader(File::open(
            config.registry_dir.join(format!("{}.json", binding.tenant)),
        )?)?;
        registry.validate(&binding.tenant)?;
        pressure_cgroups.insert(
            binding.tenant.clone(),
            motor_host::metrics::tenant_pressure_cgroup(registry.tap_index),
        );
        let identity = motor_lease::sha256_hex(
            format!("{}\n{}\n{}", config.host_id, binding.sub, vm).as_bytes(),
        );
        guest_init.insert(
            binding.tenant.clone(),
            json!({
                "address":format!("10.200.{}.2/30", registry.tap_index),
                "gateway":format!("10.200.{}.1", registry.tap_index),
                "resolvers":config.resolvers,
                "hostname":format!("motor-{}", registry.tap_index),
                "machine_id":&identity[..32],
            }),
        );
        tenants.insert(
            vm,
            Tenant {
                tenant: binding.tenant,
                sub: binding.sub,
                vcpu_ceiling: registry.vcpu_ceiling,
                max_weight: registry.max_weight,
                guest_ram_mib: registry.guest_ram_mib,
                idle_suspend_s: registry.idle_suspend_s,
                host_floor_bytes: registry.host_floor_bytes,
            },
        );
    }
    let (journal, snapshot) = SqliteJournal::open(&config.journal_dir)?;
    let mut host = Host::new(tenants, snapshot, Box::new(journal));
    host.settings = settings;
    // Starts latched: no lease is admitted until a probe finds the capacity
    // file valid and enough host storage free.
    host.enforce_storage();
    let vcpu_ceiling = host
        .tenants
        .values()
        .map(|t| (t.tenant.clone(), t.vcpu_ceiling))
        .collect();
    helper.vcpu_ceiling = vcpu_ceiling;
    helper.guest_init = guest_init;
    helper.pressure_cgroups = pressure_cgroups;
    let runtime = configured_runtime(
        config.host_id,
        host,
        Arc::new(helper),
        broker_endpoint,
        jwks_url,
    );
    runtime.configure_job_resolvers(config.resolvers.iter().map(ToString::to_string).collect())?;
    runtime
        .fetch_keys
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let startup = runtime.clone();
    tokio::task::spawn_blocking(move || {
        startup.prepare_job_startup()?;
        startup.refresh_keys(0);
        startup.recover()
    })
    .await??;
    let seed = enroll::read_signing_seed(&config.host_signing_key)?;
    let sender = runtime.clone();
    std::thread::spawn(move || {
        if motor_host::broker::run(
            sender,
            ed25519_dalek::SigningKey::from_bytes(&seed),
            config.storage,
        )
        .is_err()
        {
            tracing::error!("host_sender_failed");
        }
    });
    let refresher = runtime.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(600));
        refresher.refresh_keys(0);
    });
    if config.mode == motor_host::config::HostMode::PerJob {
        let sampler = runtime.clone();
        std::thread::spawn(move || loop {
            sampler.sample_job_residents(unix_ms());
            std::thread::sleep(Duration::from_secs(1));
        });
    }
    let ticker = runtime.clone();
    std::thread::spawn(move || loop {
        if let Err(error) = ticker.tick(unix_ms()) {
            log_tick_failure(&error);
        }
        std::thread::sleep(Duration::from_millis(100));
    });
    let admin = runtime.clone();
    tokio::spawn(async move {
        if admin_socket(admin, &config.admin_socket).await.is_err() {
            tracing::error!("admin_socket_failed");
        }
    });
    let certs = rustls_pemfile::certs(&mut BufReader::new(File::open(config.certificate)?))
        .collect::<std::io::Result<Vec<_>>>()?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(config.private_key)?))?
        .context("TLS private key")?;
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let ingress = Arc::new(motor_host::ingress::Ingress::default());
    let summary = ingress.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        loop {
            interval.tick().await;
            summary.summarize();
        }
    });
    loop {
        let (socket, peer) = listener.accept().await?;
        let Some(slot) = ingress.admit(peer) else {
            drop(socket);
            continue;
        };
        let acceptor = acceptor.clone();
        let runtime = runtime.clone();
        let ingress = ingress.clone();
        tokio::spawn(async move {
            let accepted =
                tokio::time::timeout(Duration::from_secs(10), acceptor.accept(socket)).await;
            match accepted {
                Ok(Ok(stream)) => {
                    if let Err(error) = serve_with_slot(runtime, stream, Some(slot)).await {
                        let report = motor_host::server::describe_close(&error);
                        ingress.closed(peer, &report);
                    }
                }
                Ok(Err(_)) => ingress.tls_closed(peer, "invalid TLS handshake"),
                Err(_) => ingress.tls_closed(peer, "timed out"),
            }
        });
    }
}

fn version_output(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<String> {
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--version")) || args.next().is_some() {
        return None;
    }
    Some(format!(
        "motor-host {} {}",
        env!("CARGO_PKG_VERSION"),
        option_env!("MOTOR_BUILD_COMMIT").unwrap_or("unstamped")
    ))
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn version_is_an_exact_early_flag_and_reports_the_compile_time_stamp() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
                .into_iter()
        };
        let output = super::version_output(args(&["--version"])).unwrap();
        assert_eq!(
            output,
            format!(
                "motor-host 0.1.0 {}",
                option_env!("MOTOR_BUILD_COMMIT").unwrap_or("unstamped")
            )
        );
        for values in [
            vec![],
            vec!["host.json"],
            vec!["--version", "host.json"],
            vec!["--version-extra"],
        ] {
            assert!(super::version_output(args(&values)).is_none());
        }
    }
}

fn configured_runtime(
    host_id: String,
    host: Host,
    control: Arc<dyn motor_host::helper::Control>,
    broker_endpoint: String,
    jwks_url: String,
) -> Arc<Runtime> {
    if host.settings.mode() == motor_host::config::HostMode::PerJob {
        Runtime::with_broker_and_image_generations(
            host_id,
            host,
            control,
            broker_endpoint,
            jwks_url,
        )
    } else {
        Runtime::with_broker(host_id, host, control, broker_endpoint, jwks_url)
    }
}

// Validate settings without opening keys, journals or sockets. Installation can
// check a staged configuration even before those resources exist.
fn check_config(path: &std::path::Path) -> Result<()> {
    let config: Config = serde_json::from_reader(File::open(path)?)?;
    config.checked_settings(&config.helper())?;
    Ok(())
}

fn disable_core_dumps() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod core_dump_tests {
    use super::*;

    #[test]
    fn host_startup_disables_dumpability() {
        // Isolate the irreversible process setting from other test threads.
        unsafe {
            let child = libc::fork();
            assert!(child >= 0);
            if child == 0 {
                let ok = libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) == 0
                    && disable_core_dumps().is_ok()
                    && libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) == 0;
                libc::_exit(if ok { 0 } else { 1 });
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert_eq!(status, 0);
        }
    }
}

const KEYGEN_USAGE: &str = "usage: motor-host keygen --signing-key <path> --tls-cert <path> \
                            --tls-key <path> --host-id <id> --endpoint <public IPv4>:<port>";
const SHOW_USAGE: &str =
    "usage: motor-host show-enrollment <config.json> --endpoint <public IPv4>:<port>";

/// Parses `--name value` pairs, refusing unknown, repeated or valueless flags.
fn flags(args: &[String], names: &[&str], usage: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let name = flag
            .strip_prefix("--")
            .filter(|name| names.contains(name))
            .with_context(|| format!("unexpected argument {flag:?}\n{usage}"))?;
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value\n{usage}"))?;
        if values.insert(name.to_owned(), value.clone()).is_some() {
            bail!("{flag} given twice\n{usage}");
        }
    }
    for name in names {
        ensure!(values.contains_key(*name), "missing --{name}\n{usage}");
    }
    Ok(values)
}

fn keygen(args: Vec<String>) -> Result<()> {
    let values = flags(
        &args,
        &["signing-key", "tls-cert", "tls-key", "host-id", "endpoint"],
        KEYGEN_USAGE,
    )?;
    let paths = enroll::KeyPaths {
        signing_key: values["signing-key"].clone().into(),
        tls_certificate: values["tls-cert"].clone().into(),
        tls_key: values["tls-key"].clone().into(),
    };
    let endpoint = enroll::parse_endpoint(&values["endpoint"])?;
    print!(
        "{}",
        enroll::keygen(&paths, &values["host-id"], endpoint)?.render()
    );
    Ok(())
}

fn show_enrollment(args: Vec<String>) -> Result<()> {
    let (path, rest) = args.split_first().context(SHOW_USAGE)?;
    let values = flags(rest, &["endpoint"], SHOW_USAGE)?;
    let config: Config = serde_json::from_reader(File::open(path)?)?;
    ensure!(
        config.fake_broker_url.is_none(),
        "a config with fake_broker_url is qualification-only and cannot be enrolled"
    );
    let listen: std::net::SocketAddr = config
        .listen
        .parse()
        .context("listen must be a socket address")?;
    let enrollment = enroll::show(
        &config.host_id,
        &config.host_signing_key,
        &config.certificate,
        &config.private_key,
        listen,
        enroll::parse_endpoint(&values["endpoint"])?,
    )?;
    print!("{}", enrollment.render());
    Ok(())
}

fn log_tick_failure(error: &anyhow::Error) {
    tracing::error!(detail = %format!("{error:#}"), "host_tick_failed");
}

#[cfg(test)]
mod config_tests {
    use super::*;
    use motor_host::config::{HostMode, HostSettings};

    struct Control;
    impl motor_host::helper::Control for Control {
        fn adopt(&self, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
            unreachable!()
        }
        fn quota(&self, _: &str, _: u32) -> Result<()> {
            unreachable!()
        }
        fn relay(&self, _: &str, _: u32) -> Result<std::os::unix::net::UnixStream> {
            unreachable!()
        }
        fn admin(&self, _: &str, _: &serde_json::Value) -> Result<serde_json::Value> {
            unreachable!()
        }
        fn suspend(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn available_bytes(&self) -> Result<u64> {
            unreachable!()
        }
        fn host_available_mib(&self) -> Result<u64> {
            Ok(100_000)
        }
        fn online_cpus(&self) -> Result<u32> {
            Ok(2)
        }
    }

    fn config() -> serde_json::Value {
        json!({
            "host_id": "host", "listen": "127.0.0.1:443",
            "certificate": "cert", "private_key": "key", "host_signing_key": "seed",
            "journal_dir": "journal", "helper_socket": "helper.sock",
            "registry_dir": "registry", "bindings": "bindings.json",
            "resolvers": ["1.1.1.1"], "admin_socket": "admin.sock", "storage": "storage"
        })
    }

    fn per_job() -> serde_json::Value {
        json!({
            "vcpu_ceiling": 1, "permit_budget": 4,
            "host_reserve_mib": 1024, "boot_reserve_mib": 256,
            "per_vm_overhead_mib": 128, "reclaim_timeout_ms": 5000,
            "unused_memory_hold_percent": 25,
            "hysteresis_mib": 512, "workspace_disk_cap_bytes": 10737418240_u64,
            "workspace_growth_reservation_bytes": 2147483648_u64,
            "boot_deadline_ms": 30000, "recovery_timeout_ms": 120000,
            "exit_evidence_bound_ms": 10000, "teardown_timeout_ms": 5000
        })
    }

    #[test]
    fn existing_configs_and_explicit_tenant_mode_keep_tenant_settings() {
        let mut value = config();
        for explicit in [false, true] {
            if explicit {
                value["mode"] = json!("tenant");
            }
            let config: Config = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(config.mode, HostMode::Tenant);
            assert_eq!(
                config.startup_settings(&Control).unwrap(),
                HostSettings::Tenant
            );
        }
    }

    #[test]
    fn per_job_mode_requires_its_settings_object() {
        let mut value = config();
        value["mode"] = json!("per_job");
        for settings in [None, Some(serde_json::Value::Null)] {
            if let Some(settings) = settings {
                value["per_job"] = settings;
            }
            let config: Config = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(
                config.startup_settings(&Control).unwrap_err().to_string(),
                "per_job settings are required"
            );
        }
    }

    #[test]
    fn per_job_mode_refuses_each_unset_setting_without_defaults() {
        let mut value = config();
        value["mode"] = json!("per_job");
        for field in per_job().as_object().unwrap().keys() {
            for unset in [None, Some(serde_json::Value::Null)] {
                let mut settings = per_job();
                if let Some(unset) = unset {
                    settings[field] = unset;
                } else {
                    settings.as_object_mut().unwrap().remove(field);
                }
                value["per_job"] = settings;
                let error = serde_json::from_value::<Config>(value.clone())
                    .err()
                    .unwrap();
                if value["per_job"].get(field).is_none() {
                    assert!(error
                        .to_string()
                        .contains(&format!("missing field `{field}`")));
                } else {
                    assert!(error.to_string().contains("invalid type: null"));
                }
            }
        }
    }

    #[test]
    fn complete_per_job_config_selects_image_managed_runtime() {
        let mut value = config();
        value["mode"] = json!("per_job");
        value["per_job"] = per_job();
        let config: Config = serde_json::from_value(value).unwrap();
        let settings = config.startup_settings(&Control).unwrap();
        assert_eq!(settings.mode(), HostMode::PerJob);
        let HostSettings::PerJob(ref per_job) = settings else {
            panic!("per-job settings lost")
        };
        assert_eq!(per_job.host_reserve_mib, 1024);
        assert_eq!(per_job.boot_deadline_ms, 30000);
        assert_eq!(per_job.workspace_disk_cap_bytes, 10737418240);
        assert_eq!(per_job.workspace_growth_reservation_bytes, 2147483648);
        let dir = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        let (journal, snapshot) = motor_host::journal::FileJournal::open(dir.path()).unwrap();
        let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
        host.settings = settings;
        let runtime = configured_runtime(
            "host".into(),
            host,
            Arc::new(Control),
            "http://broker".into(),
            "http://keys".into(),
        );
        assert_eq!(runtime.broker_endpoint, "http://broker");
        assert_eq!(runtime.jwks_url, "http://keys");
        assert_eq!(
            runtime
                .publish_job_images("gen-1", 0)
                .unwrap_err()
                .to_string(),
            "image publication unavailable"
        );
    }

    #[test]
    fn per_job_startup_validates_the_controls_online_cpu_count() {
        let mut value = config();
        value["mode"] = json!("per_job");
        value["per_job"] = per_job();
        value["per_job"]["vcpu_ceiling"] = json!(3);
        let config: Config = serde_json::from_value(value).unwrap();
        assert_eq!(
            config.startup_settings(&Control).unwrap_err().to_string(),
            "per_job vcpu_ceiling must be between 1 and online CPUs"
        );
    }

    #[test]
    fn config_check_validates_staged_settings_without_starting_or_writing() {
        let root = cortexkit_test_support::scratch_root();
        let dir = tempfile::tempdir_in(root).unwrap();
        let path = dir.path().join("host.json");
        let mut value = config();
        value["journal_dir"] = json!(dir.path().join("uncreated-journal"));
        let write = |value: &serde_json::Value| {
            std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        };
        write(&value);
        check_config(&path).unwrap();
        value["mode"] = json!("per_job");
        write(&value);
        assert_eq!(
            check_config(&path).unwrap_err().to_string(),
            "per_job settings are required"
        );
        value["per_job"] = per_job();
        for field in [
            "vcpu_ceiling",
            "permit_budget",
            "workspace_growth_reservation_bytes",
            "unused_memory_hold_percent",
        ] {
            let mut missing = value.clone();
            missing["per_job"].as_object_mut().unwrap().remove(field);
            write(&missing);
            assert!(check_config(&path)
                .unwrap_err()
                .to_string()
                .contains(&format!("missing field `{field}`")));
        }
        value["per_job"]["permit_budget"] = json!(1);
        for percent in [json!(101), json!(256), json!(-1), json!(u64::MAX)] {
            value["per_job"]["unused_memory_hold_percent"] = percent;
            write(&value);
            assert!(check_config(&path)
                .unwrap_err()
                .to_string()
                .contains("per_job.unused_memory_hold_percent"));
        }
        value["per_job"]["unused_memory_hold_percent"] = json!(25);
        write(&value);
        check_config(&path).unwrap();
        assert!(serde_json::from_value::<Config>(value.clone())
            .unwrap()
            .startup_settings(&Control)
            .is_ok());
        for (ceiling, permits, field) in [
            (0, 1, "vcpu_ceiling"),
            (u32::MAX, u32::MAX, "vcpu_ceiling"),
            (1, 0, "permit_budget"),
            (1, u32::MAX, "permit_budget"),
        ] {
            value["per_job"]["vcpu_ceiling"] = json!(ceiling);
            value["per_job"]["permit_budget"] = json!(permits);
            write(&value);
            assert!(check_config(&path).unwrap_err().to_string().contains(field));
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn config_check_validates_workspace_growth_reservation() {
        let dir = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        let path = dir.path().join("host.json");
        let mut value = config();
        value["mode"] = json!("per_job");
        value["per_job"] = per_job();
        for bytes in [0, 10737418240_u64 + 4096, 4097] {
            value["per_job"]["workspace_growth_reservation_bytes"] = json!(bytes);
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            let error = check_config(&path).unwrap_err().to_string();
            assert!(
                error.contains("workspace_growth_reservation_bytes"),
                "{error}"
            );
            assert!(serde_json::from_value::<Config>(value.clone())
                .unwrap()
                .startup_settings(&Control)
                .is_err());
        }
        for bytes in [4096, 10737418240_u64] {
            value["per_job"]["workspace_growth_reservation_bytes"] = json!(bytes);
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            check_config(&path).unwrap();
        }
    }

    #[test]
    fn mode_and_per_job_settings_reject_unknown_fields_and_values() {
        let mut value = config();
        value["mode"] = json!("unknown");
        assert!(serde_json::from_value::<Config>(value.clone()).is_err());
        value["mode"] = json!("per_job");
        value["per_job"] = per_job();
        value["per_job"]["typo"] = json!(1);
        assert!(serde_json::from_value::<Config>(value).is_err());
    }
}
