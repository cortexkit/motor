use crate::helper::{read_json, write_json, Control, FRAME_LIMIT};
use crate::state::{ConnectionState, Error, Host, LeaseState, QuotaUpdate, Refusal};
use anyhow::{bail, ensure, Context, Result};
use motor_lease::{
    bind_requester, verify_token, AuthError, Challenge, Claims, KeySet, Nonces, Prelude, Purpose,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub struct Runtime {
    pub host_id: String,
    pub host: Mutex<Host>,
    pub keys: RwLock<KeySet>,
    pub control: Arc<dyn Control>,
    pub fetch_keys: AtomicBool,
    last_key_refresh: Mutex<u64>,
    event_workers: Mutex<BTreeSet<String>>,
    tenant_operations: BTreeMap<String, Arc<Mutex<()>>>,
}
impl Runtime {
    pub fn new(host_id: String, host: Host, control: Arc<dyn Control>) -> Arc<Self> {
        let tenant_operations = host
            .tenants
            .values()
            .map(|tenant| (tenant.tenant.clone(), Arc::new(Mutex::new(()))))
            .collect();
        Arc::new(Self {
            host_id,
            host: Mutex::new(host),
            keys: RwLock::new(KeySet::default()),
            control,
            event_workers: Mutex::new(BTreeSet::new()),
            tenant_operations,
            last_key_refresh: Mutex::new(0),
            fetch_keys: AtomicBool::new(false),
        })
    }
    pub fn refresh_keys(&self, minimum_interval_s: u64) {
        if !self.fetch_keys.load(Ordering::Relaxed) {
            return;
        }
        let mut last = self.last_key_refresh.lock().expect("key refresh lock");
        let now = unix_ms() / 1000;
        if now.saturating_sub(*last) < minimum_interval_s {
            return;
        }
        *last = now;
        let fetched = (|| -> Result<motor_lease::Jwks> {
            Ok(reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?
                .get("https://runners.cortexkit.io/.well-known/jwks.json")
                .send()?
                .error_for_status()?
                .json()?)
        })();
        let _ = self
            .keys
            .write()
            .expect("keys lock")
            .refresh(fetched.map_err(|_| AuthError::Auth));
    }
    pub fn recover(self: &Arc<Self>) -> Result<()> {
        let tenants: Vec<_> = self
            .host
            .lock()
            .expect("host lock")
            .tenants
            .values()
            .map(|t| t.tenant.clone())
            .collect();
        for tenant in tenants {
            if self.control.adopt(&tenant)? {
                self.ensure(&tenant)?;
            }
        }
        Ok(())
    }
    pub fn authenticate(
        &self,
        token: &str,
        jwk: &motor_lease::RequesterJwk,
        now_s: u64,
    ) -> Result<Claims, Error> {
        let keys = self.keys.read().expect("keys lock");
        let claims = verify_token(token, &keys, &self.host_id, None, Purpose::Run, now_s)
            .or_else(|e| {
                if e == AuthError::Expired {
                    Err(e)
                } else {
                    verify_token(token, &keys, &self.host_id, None, Purpose::Attach, now_s)
                }
            })
            .map_err(|e| {
                Error::Refused(if e == AuthError::Expired {
                    Refusal::Expired
                } else {
                    Refusal::Auth
                })
            })?;
        uuid::Uuid::parse_str(&claims.jti).map_err(|_| Error::Refused(Refusal::Auth))?;
        if claims.platform != "linux-x64" {
            return Err(Error::Refused(Refusal::Auth));
        }
        bind_requester(&claims, jwk).map_err(|_| Error::Refused(Refusal::Auth))?;
        self.host.lock().expect("host lock").binding(&claims)?;
        Ok(claims)
    }
    fn ensure(self: &Arc<Self>, tenant: &str) -> Result<()> {
        let operations = self.tenant_operations(tenant)?;
        let _operation = operations.lock().expect("tenant operations lock");
        self.ensure_locked(tenant)?;
        self.start_events(tenant.to_owned());
        Ok(())
    }
    fn tenant_operations(&self, tenant: &str) -> Result<Arc<Mutex<()>>> {
        self.tenant_operations
            .get(tenant)
            .cloned()
            .context("unknown tenant operations")
    }
    fn ensure_locked(&self, tenant: &str) -> Result<()> {
        let (ready, generation) = {
            let host = self.host.lock().expect("host lock");
            ensure!(
                !host.is_suspending(tenant),
                Error::Refused(Refusal::Capacity)
            );
            (
                host.is_ready(tenant),
                host.snapshot
                    .vms
                    .get(tenant)
                    .and_then(|v| v.initialized_generation),
            )
        };
        if !ready {
            let generation = self.control.wake(tenant, generation)?;
            let mut host = self.host.lock().expect("host lock");
            host.guest_ready(tenant, generation)?;
        }
        let update = self
            .host
            .lock()
            .expect("host lock")
            .quota_update(tenant, unix_ms())?;
        if let Some(update) = update {
            self.apply_quota_locked(&update)?;
        }
        Ok(())
    }
    pub(crate) fn apply_quota(&self, requested: QuotaUpdate) -> Result<()> {
        let operations = self.tenant_operations(&requested.tenant)?;
        let _operation = operations.lock().expect("tenant operations lock");
        self.apply_quota_locked(&requested)
    }
    // The caller owns this tenant's operation lock. Generation numbers order
    // host-side work, not disk generations or extra privileged RPC arguments.
    fn apply_quota_locked(&self, requested: &QuotaUpdate) -> Result<()> {
        loop {
            let update = self
                .host
                .lock()
                .expect("host lock")
                .quota_update(&requested.tenant, unix_ms())?;
            let Some(update) = update else {
                return Ok(());
            };
            ensure!(
                update.generation >= requested.generation,
                "quota generation regressed"
            );
            self.control.quota(&update.tenant, update.vcpus)?;
            self.host.lock().expect("host lock").quota_applied(&update);
        }
    }
    fn start_events(self: &Arc<Self>, tenant: String) {
        if !self
            .event_workers
            .lock()
            .expect("workers lock")
            .insert(tenant.clone())
        {
            return;
        }
        let runtime = self.clone();
        std::thread::spawn(move || {
            loop {
                if !runtime.host.lock().expect("host lock").is_ready(&tenant) {
                    let operations = runtime
                        .tenant_operations(&tenant)
                        .expect("registered tenant");
                    let _operation = operations.lock().expect("tenant operations lock");
                    // Retire under the wake operation's lock so a concurrent
                    // wake cannot miss starting its lifecycle reader.
                    if !runtime.host.lock().expect("host lock").is_ready(&tenant) {
                        runtime
                            .event_workers
                            .lock()
                            .expect("workers lock")
                            .remove(&tenant);
                        return;
                    }
                }
                let result = (|| -> Result<()> {
                    let mut stream = {
                        let operations = runtime.tenant_operations(&tenant)?;
                        let _operation = operations.lock().expect("tenant operations lock");
                        let available = {
                            let host = runtime.host.lock().expect("host lock");
                            host.is_ready(&tenant) && !host.is_suspending(&tenant)
                        };
                        ensure!(available, "tenant stopped or suspending");
                        runtime.control.relay(&tenant, 53)?
                    };
                    runtime
                        .host
                        .lock()
                        .expect("host lock")
                        .events_connected(&tenant, Some(unix_ms()));
                    loop {
                        let frame = read_json(&mut stream)?;
                        ensure!(frame["type"] == "event", "invalid lifecycle frame");
                        let event = serde_json::from_value(frame)?;
                        let update = {
                            let mut host = runtime.host.lock().expect("host lock");
                            host.event(&tenant, &event, unix_ms())?;
                            host.quota_update(&tenant, unix_ms())?
                        };
                        if let Some(update) = update {
                            runtime.apply_quota(update)?;
                        }
                        write_json(
                            &mut stream,
                            &json!({"type":"ack", "event_seq":event.event_seq}),
                        )?;
                    }
                })();
                runtime
                    .host
                    .lock()
                    .expect("host lock")
                    .events_connected(&tenant, None);
                if result.is_err() {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        });
    }
    pub fn tick(&self, now_ms: u64) -> Result<()> {
        let (cancels, quotas, candidates) = {
            let mut host = self.host.lock().expect("host lock");
            let cancels = host.expire(now_ms)?;
            let mut quotas = Vec::new();
            let mut candidates = Vec::new();
            for tenant in self.tenant_operations.keys() {
                if let Some(update) = host.quota_update(tenant, now_ms)? {
                    quotas.push(update);
                }
                if host.suspend_candidate(tenant, now_ms) {
                    candidates.push(tenant.clone());
                }
            }
            (cancels, quotas, candidates)
        };
        let mut failure = None;
        for (tenant, job) in cancels {
            let result = (|| -> Result<()> {
                let reply = self.admin(&tenant, &json!({"op":"cancel", "job_id":job}))?;
                ensure!(reply["ok"] == true, "guest cancel refused");
                self.host
                    .lock()
                    .expect("host lock")
                    .cancel_delivered(&job)?;
                Ok(())
            })();
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        for update in quotas {
            if let Err(error) = self.apply_quota(update) {
                failure.get_or_insert(error);
            }
        }
        if !candidates.is_empty() {
            match self.control.available_bytes() {
                Ok(available) => {
                    for tenant in candidates {
                        if let Err(error) = self.suspend_tenant(&tenant, now_ms, available) {
                            failure.get_or_insert(error);
                        }
                    }
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn suspend_tenant(&self, tenant: &str, now_ms: u64, available_bytes: u64) -> Result<()> {
        let operations = self.tenant_operations(tenant)?;
        if !self
            .host
            .lock()
            .expect("host lock")
            .begin_suspend(tenant, now_ms, available_bytes)
        {
            return Ok(());
        }
        let _operation = operations.lock().expect("tenant operations lock");
        {
            let mut host = self.host.lock().expect("host lock");
            if !host.suspend_still_idle(tenant) {
                host.finish_suspend(tenant, false);
                return Ok(());
            }
        }
        let result = self.control.suspend(tenant);
        let still_idle = self
            .host
            .lock()
            .expect("host lock")
            .finish_suspend(tenant, result.is_ok());
        if result.is_ok() && !still_idle {
            tracing::info!(tenant, "suspended_with_new_activity");
        }
        result
    }
    fn close_connection(&self, id: u64, tenant: &str) -> Result<()> {
        let update = {
            let mut host = self.host.lock().expect("host lock");
            host.close(id)?;
            host.quota_update(tenant, unix_ms())?
        };
        if let Some(update) = update {
            self.apply_quota(update)?;
        }
        Ok(())
    }
    pub fn admin(&self, tenant: &str, request: &Value) -> Result<Value> {
        ensure!(
            self.host
                .lock()
                .expect("host lock")
                .tenants
                .values()
                .any(|t| t.tenant == tenant),
            "unknown tenant"
        );
        ensure!(
            matches!(
                request["op"].as_str(),
                Some("drain" | "resume" | "cancel" | "status")
            ),
            "invalid admin operation"
        );
        // Decode the closed guest schema so the local socket cannot become an
        // alternate route to disk attachment or clock/identity mutation.
        let request: motor_protocol::TenantAgentRequest = serde_json::from_value(request.clone())?;
        let operations = self.tenant_operations(tenant)?;
        let _operation = operations.lock().expect("tenant operations lock");
        self.control.admin(tenant, &serde_json::to_value(request)?)
    }
    fn deliver(
        self: &Arc<Self>,
        claims: &Claims,
        id: u64,
        alive: &AtomicBool,
    ) -> Result<std::os::unix::net::UnixStream> {
        let tenant = self
            .host
            .lock()
            .expect("host lock")
            .binding(claims)?
            .tenant
            .clone();
        let operations = self.tenant_operations(&tenant)?;
        let _operation = operations.lock().expect("tenant operations lock");
        self.ensure_locked(&tenant)?;
        self.start_events(tenant.clone());
        let mut stream = self.control.relay(&tenant, 52)?;
        {
            let mut host = self.host.lock().expect("host lock");
            ensure!(host.connection_live(id), "connection superseded");
            ensure!(
                alive.load(Ordering::Acquire),
                "client closed before delivery"
            );
            host.delivered(id)?;
        }
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        write_json(
            &mut stream,
            &json!({"type":"verified_lease", "jti":claims.jti, "purpose":claims.purpose, "sub":claims.sub, "vcpus":claims.vcpus, "mem_mib":claims.mem_mib, "platform":claims.platform, "net":claims.net, "max_s":claims.max_s, "exp":claims.exp}),
        )?;
        Ok(stream)
    }
}

struct ConnectionGuard {
    runtime: Arc<Runtime>,
    id: u64,
    alive: Arc<AtomicBool>,
    tenant: String,
    jti: String,
    armed: bool,
}
impl ConnectionGuard {
    fn cleanup(&self) -> impl FnOnce() + Send + 'static {
        let runtime = self.runtime.clone();
        let id = self.id;
        let tenant = self.tenant.clone();
        let jti = self.jti.clone();
        move || {
            if let Err(error) = runtime.close_connection(id, &tenant) {
                tracing::error!(%jti, %tenant, %error, "connection_cleanup_failed");
            }
        }
    }
    async fn finish(mut self) {
        self.alive.store(false, Ordering::Release);
        self.armed = false;
        if tokio::task::spawn_blocking(self.cleanup()).await.is_err() {
            tracing::error!(jti = %self.jti, "connection_cleanup_failed");
        }
    }
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if self.armed {
            let cleanup = self.cleanup();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(cleanup);
            } else {
                std::thread::spawn(cleanup);
            }
        }
    }
}
struct PreludeGuard {
    runtime: Arc<Runtime>,
    tenant: String,
}
impl Drop for PreludeGuard {
    fn drop(&mut self) {
        self.runtime
            .host
            .lock()
            .expect("host lock")
            .prelude(&self.tenant, false);
    }
}

pub async fn serve<S>(runtime: Arc<Runtime>, mut client: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let first = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut client)).await??;
    let Prelude::LeaseHello {
        token,
        requester_jwk,
        ..
    } = serde_json::from_value(first)?
    else {
        bail!("expected lease hello");
    };
    let auth_runtime = runtime.clone();
    let auth_requester = requester_jwk.clone();
    let authentication = tokio::task::spawn_blocking(move || {
        let result = auth_runtime.authenticate(&token, &auth_requester, unix_ms() / 1000);
        if matches!(result, Err(Error::Refused(Refusal::Auth))) {
            auth_runtime.refresh_keys(60);
            auth_runtime.authenticate(&token, &auth_requester, unix_ms() / 1000)
        } else {
            result
        }
    })
    .await?;
    let claims = match authentication {
        Ok(c) => c,
        Err(Error::Refused(r)) => {
            write_frame(
                &mut client,
                &json!({"type":"lease_refused", "reason":r.wire()}),
            )
            .await?;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let tenant = runtime
        .host
        .lock()
        .expect("host lock")
        .binding(&claims)?
        .tenant
        .clone();
    runtime
        .host
        .lock()
        .expect("host lock")
        .prelude(&tenant, true);
    let prelude_guard = PreludeGuard {
        runtime: runtime.clone(),
        tenant,
    };
    let nonce: String = rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let challenge = Challenge {
        nonce,
        host_id: runtime.host_id.clone(),
        ts: unix_ms() / 1000,
    };
    write_frame(&mut client, &json!({"type":"challenge", "nonce":challenge.nonce, "host_id":challenge.host_id, "ts":challenge.ts})).await?;
    let response = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut client)).await??;
    let Prelude::ChallengeResponse { sig } = serde_json::from_value(response)? else {
        bail!("expected challenge response");
    };
    let verified = (|| -> Result<_, Error> {
        let mut nonces = Nonces::default();
        nonces
            .verify(&challenge, &claims, &requester_jwk, &sig, unix_ms() / 1000)
            .map_err(|_| Error::Refused(Refusal::Auth))?;
        let mut host = runtime.host.lock().expect("host lock");
        host.consume_nonce(challenge.nonce, challenge.ts, unix_ms() / 1000)?;
        let (id, _) = host.claim(claims.clone(), unix_ms())?;
        Ok((id, host.watch_connection(id)?))
    })();
    let (id, mut updates) = match verified {
        Ok(value) => value,
        Err(Error::Refused(r)) => {
            write_frame(
                &mut client,
                &json!({"type":"lease_refused", "reason":r.wire()}),
            )
            .await?;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    tracing::info!(jti = %claims.jti, "lease_connected");
    let alive = Arc::new(AtomicBool::new(true));
    let connection = ConnectionGuard {
        runtime: runtime.clone(),
        id,
        alive: alive.clone(),
        tenant: prelude_guard.tenant.clone(),
        jti: claims.jti.clone(),
        armed: true,
    };
    drop(prelude_guard);
    let outcome = async {
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(claims.exp.saturating_mul(1000).saturating_sub(unix_ms()));
    match await_admission(&mut updates, deadline).await {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(reason) => {
            write_frame(
                &mut client,
                &json!({"type":"lease_refused", "reason":reason.wire()}),
            )
            .await?;
            return Ok(());
        }
    }
    let connected = async {
        write_frame(&mut client, &json!({"type":"lease_ok"})).await?;
        // Wait for a client frame before waking the guest. A client that disappears
        // immediately after lease_ok has never delivered a lease to the runner.
        let remaining = claims.exp.saturating_mul(1000).saturating_sub(unix_ms());
        let first = tokio::time::timeout(Duration::from_millis(remaining), read_frame(&mut client))
            .await??;
        let delivery_runtime = runtime.clone();
        let delivery_claims = claims.clone();
        let mut delivery = tokio::task::spawn_blocking(move || {
            delivery_runtime.deliver(&delivery_claims, id, &alive)
        });
        let mut buffered = Vec::new();
        let mut buffer = [0; 8192];
        let stream = loop {
            tokio::select! {
                biased;
                n = client.read(&mut buffer), if buffered.len() < FRAME_LIMIT => {
                    let n = n?;
                    ensure!(n > 0, "client closed before delivery");
                    buffered.extend_from_slice(&buffer[..n]);
                }
                stream = &mut delivery => { break stream??; }
            }
        };
        stream.set_nonblocking(true)?;
        let mut guest = tokio::net::UnixStream::from_std(stream)?;
        write_frame(&mut guest, &first).await?;
        guest.write_all(&buffered).await?;
        let (mut cr, mut cw) = tokio::io::split(client);
        let (mut gr, mut gw) = guest.split();
        let to_guest = tokio::io::copy(&mut cr, &mut gw);
        let to_client = async {
            loop {
                let frame = read_frame(&mut gr).await?;
                if let Some(seq) = frame["seq"].as_u64() {
                    runtime
                        .host
                        .lock()
                        .expect("host lock")
                        .watermark(&claims.jti, seq)?;
                }
                write_frame(&mut cw, &frame).await?;
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! { result = to_guest => { result?; }, result = to_client => { result?; } }
        Ok(())
    };
    tokio::select! { result = connected => result, _ = connection_ended(&mut updates, (claims.purpose == Purpose::Run).then_some(deadline)) => Ok(()) }
    }.await;
    connection.finish().await;
    outcome
}

async fn await_admission(
    updates: &mut tokio::sync::watch::Receiver<ConnectionState>,
    deadline: tokio::time::Instant,
) -> Result<bool, Refusal> {
    loop {
        let state = *updates.borrow_and_update();
        match state {
            ConnectionState::Refused(reason) => return Err(reason),
            ConnectionState::Closed => return Ok(false),
            _ if tokio::time::Instant::now() >= deadline => return Err(Refusal::Expired),
            ConnectionState::Ready | ConnectionState::Live(_) => return Ok(true),
            ConnectionState::Waiting => {}
        }
        tokio::select! {
            biased;
            changed = updates.changed() => { if changed.is_err() { return Ok(false); } },
            _ = tokio::time::sleep_until(deadline) => return Err(Refusal::Expired),
        }
    }
}

async fn connection_ended(
    updates: &mut tokio::sync::watch::Receiver<ConnectionState>,
    deadline: Option<tokio::time::Instant>,
) {
    loop {
        let state = *updates.borrow_and_update();
        if matches!(state, ConnectionState::Closed | ConnectionState::Refused(_)) {
            return;
        }
        let before_admission = !matches!(
            state,
            ConnectionState::Live(LeaseState::Admitted | LeaseState::Started | LeaseState::Ended)
        );
        tokio::select! {
            biased;
            changed = updates.changed() => { if changed.is_err() { return; } },
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if before_admission && deadline.is_some() => return,
        }
    }
}

pub async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Result<Value> {
    let n = reader.read_u32().await? as usize;
    ensure!(n > 0 && n <= FRAME_LIMIT, "invalid frame length");
    let mut bytes = vec![0; n];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
pub async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), frame: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    ensure!(bytes.len() <= FRAME_LIMIT, "frame too large");
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn admin_socket(runtime: Arc<Runtime>, path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        // SAFETY: geteuid has no arguments or preconditions.
        ensure!(
            metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
            "admin path is not an owned socket"
        );
        match std::os::unix::net::UnixStream::connect(path) {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)?
            }
            _ => bail!("admin socket already active"),
        }
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (mut stream, _) = listener.accept().await?;
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let result = async {
                let mut frame = read_frame(&mut stream).await?;
                let tenant = frame["tenant"].as_str().context("tenant")?.to_owned();
                frame
                    .as_object_mut()
                    .context("admin object")?
                    .remove("tenant");
                let result =
                    tokio::task::spawn_blocking(move || runtime.admin(&tenant, &frame)).await??;
                write_frame(&mut stream, &result).await
            }
            .await;
            if result.is_err() {
                let _ =
                    write_frame(&mut stream, &json!({"ok":false,"error":"admin_refused"})).await;
            }
        });
    }
}
