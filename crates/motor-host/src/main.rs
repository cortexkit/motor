use anyhow::{ensure, Context, Result};
use motor_host::helper::Helper;
use motor_host::journal::FileJournal;
use motor_host::server::{admin_socket, serve, unix_ms, Runtime};
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
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    };
    let mut signing_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&config.host_signing_key)?;
    let metadata = signing_file.metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    ensure!(
        metadata.is_file()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::geteuid() },
        "host signing key must be a private, owned regular file"
    );
    let mut bytes = Vec::new();
    signing_file.by_ref().take(33).read_to_end(&mut bytes)?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("host_signing_key must contain a 32-byte Ed25519 seed"))?;
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
        if ticker.tick(unix_ms()).is_err() {
            tracing::error!("host_tick_failed");
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
    loop {
        let (socket, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let accepted =
                tokio::time::timeout(Duration::from_secs(10), acceptor.accept(socket)).await;
            if let Ok(Ok(stream)) = accepted {
                // Errors deliberately omit peer-controlled data and credentials.
                if serve(runtime, stream).await.is_err() {
                    tracing::warn!("connection_closed");
                }
            }
        });
    }
}
