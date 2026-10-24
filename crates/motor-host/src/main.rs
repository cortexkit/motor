use anyhow::{bail, ensure, Context, Result};
use motor_host::enroll;
use motor_host::helper::Helper;
use motor_host::journal::FileJournal;
use motor_host::server::{admin_socket, serve_with_slot, unix_ms, Runtime};
use motor_host::state::{Host, Tenant};
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    tenant: String,
    sub: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    motor_protocol::logging::init("[motor-host]")?;
    match std::env::args().nth(1).as_deref() {
        Some("keygen") => return keygen(std::env::args().skip(2).collect()),
        Some("show-enrollment") => return show_enrollment(std::env::args().skip(2).collect()),
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
    let (journal, snapshot) = FileJournal::open(&config.journal_dir)?;
    let runtime = Runtime::with_broker(
        config.host_id,
        Host::new(tenants, snapshot, Box::new(journal)),
        Arc::new(Helper {
            socket: config.helper_socket,
            storage: config.storage,
            guest_init,
        }),
        broker_endpoint,
        jwks_url,
    );
    runtime
        .fetch_keys
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let startup = runtime.clone();
    tokio::task::spawn_blocking(move || {
        startup.refresh_keys(0);
        startup.recover()
    })
    .await??;
    let seed = enroll::read_signing_seed(&config.host_signing_key)?;
    let sender = runtime.clone();
    std::thread::spawn(move || {
        if motor_host::broker::run(sender, ed25519_dalek::SigningKey::from_bytes(&seed)).is_err() {
            tracing::error!("host_sender_failed");
        }
    });
    let refresher = runtime.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(600));
        refresher.refresh_keys(0);
    });
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
mod tick_logging_tests {
    use super::*;
    #[derive(Clone, Default)]
    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn tick_failure_log_contains_operation_context_and_root_cause() {
        let logs = Logs::default();
        let subscriber = motor_protocol::logging::LineSubscriber::new("[host]", logs.clone());
        let error = anyhow::anyhow!("snapshot memory write timed out").context("suspend tenant-a");
        tracing::subscriber::with_default(subscriber, || log_tick_failure(&error));
        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("host_tick_failed"), "{output}");
        assert!(output.contains("suspend tenant-a"), "{output}");
        assert!(
            output.contains("snapshot memory write timed out"),
            "{output}"
        );
    }
}
