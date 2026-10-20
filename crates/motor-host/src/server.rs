use crate::helper::{read_json, write_json, Control, FRAME_LIMIT};
use crate::state::{ConnectionState, Error, Host, LeaseState, QuotaUpdate, Refusal};
use anyhow::{bail, ensure, Context, Result};
use motor_lease::{
    bind_requester, check_token, AuthError, Challenge, Claims, KeySet, Nonces, Prelude, Purpose,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Default)]
pub struct LifecycleStream {
    boundary: Option<u64>,
}
impl LifecycleStream {
    pub fn boundary(&mut self, frame: &Value) -> Result<bool> {
        if frame["type"] != "event_replay" {
            return Ok(false);
        }
        ensure!(self.boundary.is_none(), "duplicate replay boundary");
        let boundary: motor_protocol::TenantReplayBoundary = serde_json::from_value(frame.clone())?;
        self.boundary = Some(boundary.through_seq);
        Ok(true)
    }
    pub fn is_live(&self, seq: u64) -> bool {
        self.boundary.is_some_and(|through| seq > through)
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Why [`Runtime::authenticate`] refused a lease hello. It holds no token or
/// key material, so the endpoint logs it whole; the requester only receives
/// the wire reason of `error`.
#[derive(Debug)]
pub struct AuthRefusal {
    pub error: Error,
    /// The first check that failed, such as `identity`, `kid` or `mem_mib`.
    pub check: &'static str,
    /// The job id, once the broker's signature over the token has verified.
    pub jti: Option<String>,
}

/// Records every lease refusal the endpoint sends, with the step and failing
/// check that produced it. Never pass token, key or signature material here.
fn log_refusal(stage: &'static str, reason: Refusal, check: &str, jti: Option<&str>) {
    tracing::info!(
        stage,
        reason = reason.wire(),
        check,
        jti = jti.unwrap_or(""),
        "lease_refused"
    );
}

pub struct Runtime {
    pub broker_endpoint: String,
    pub jwks_url: String,
    pub host_id: String,
    pub host: Mutex<Host>,
    pub keys: RwLock<KeySet>,
    pub control: Arc<dyn Control>,
    pub fetch_keys: AtomicBool,
    last_key_refresh: Mutex<u64>,
    event_workers: Mutex<BTreeSet<String>>,
    terminal_workers: Mutex<BTreeSet<String>>,
    tenant_operations: BTreeMap<String, Arc<Mutex<()>>>,
    pub(crate) sandbox_identity: Option<crate::sandbox::IdentityFile>,
}
impl Runtime {
    pub fn new(host_id: String, host: Host, control: Arc<dyn Control>) -> Arc<Self> {
        Self::with_broker(
            host_id,
            host,
            control,
            motor_lease::ISSUER.into(),
            motor_lease::JWKS_URL.into(),
        )
    }
    /// A host that trusts tokens from `broker_endpoint`, whose keys are served
    /// at `jwks_url`. Production uses the real broker; qualification points
    /// this at a loopback fake broker.
    pub fn with_broker(
        host_id: String,
        host: Host,
        control: Arc<dyn Control>,
        broker_endpoint: String,
        jwks_url: String,
    ) -> Arc<Self> {
        Self::build(host_id, host, control, broker_endpoint, jwks_url, None)
    }
    /// An in-sandbox endpoint for the real broker that reads its sandbox
    /// identity fresh for each connection.
    pub(crate) fn with_identity(
        host_id: String,
        host: Host,
        control: Arc<dyn Control>,
        sandbox_identity: Option<crate::sandbox::IdentityFile>,
    ) -> Arc<Self> {
        Self::build(
            host_id,
            host,
            control,
            motor_lease::ISSUER.into(),
            motor_lease::JWKS_URL.into(),
            sandbox_identity,
        )
    }
    fn build(
        host_id: String,
        host: Host,
        control: Arc<dyn Control>,
        broker_endpoint: String,
        jwks_url: String,
        sandbox_identity: Option<crate::sandbox::IdentityFile>,
    ) -> Arc<Self> {
        let tenant_operations = host
            .tenants
            .values()
            .map(|tenant| (tenant.tenant.clone(), Arc::new(Mutex::new(()))))
            .collect();
        Arc::new(Self {
            broker_endpoint,
            jwks_url,
            host_id,
            host: Mutex::new(host),
            keys: RwLock::new(KeySet::default()),
            control,
            event_workers: Mutex::new(BTreeSet::new()),
            terminal_workers: Mutex::new(BTreeSet::new()),
            tenant_operations,
            last_key_refresh: Mutex::new(0),
            fetch_keys: AtomicBool::new(false),
            sandbox_identity,
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
        self.keys
            .write()
            .expect("keys lock")
            .should_refetch("", now);
        let fetched = (|| -> Result<motor_lease::Jwks> {
            Ok(reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?
                .get(&self.jwks_url)
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
        if self.sandbox_identity.is_none() {
            self.host
                .lock()
                .expect("host lock")
                .start_billing(unix_ms())?;
        }
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
                if self.ensure(&tenant).is_err() {
                    tracing::error!(tenant, "tenant_recovery_pending");
                }
            } else {
                self.host
                    .lock()
                    .expect("host lock")
                    .lose_tenant(&tenant, unix_ms())?;
            }
        }
        Ok(())
    }
    pub fn authenticate(
        &self,
        token: &str,
        jwk: &motor_lease::RequesterJwk,
        now_s: u64,
    ) -> Result<Claims, AuthRefusal> {
        let refuse = |check| AuthRefusal {
            error: Error::Refused(Refusal::Auth),
            check,
            jti: None,
        };
        let identity = self
            .sandbox_identity
            .as_ref()
            .map(|file| file.read())
            .transpose()
            .map_err(|_| refuse("identity"))?;
        let audience = identity
            .as_deref()
            .map(crate::sandbox::audience)
            .unwrap_or_else(|| self.host_id.clone());
        let keys = self.keys.read().expect("keys lock");
        // A token is accepted for either purpose. Try `attach` only when the run
        // check failed on purpose alone; any other failure applies to both, and
        // reporting the attach attempt would wrongly name `purpose`.
        let claims = check_token(token, &keys, &audience, None, Purpose::Run, now_s)
            .or_else(|refusal| {
                if refusal.check == "purpose" {
                    check_token(token, &keys, &audience, None, Purpose::Attach, now_s)
                } else {
                    Err(refusal)
                }
            })
            .map_err(|refusal| AuthRefusal {
                error: Error::Refused(if refusal.error == AuthError::Expired {
                    Refusal::Expired
                } else {
                    Refusal::Auth
                }),
                check: refusal.check,
                jti: refusal.jti,
            })?;
        let refuse = |check| AuthRefusal {
            jti: Some(claims.jti.clone()),
            ..refuse(check)
        };
        uuid::Uuid::parse_str(&claims.jti).map_err(|_| refuse("jti"))?;
        if claims.platform != "linux-x64" {
            return Err(refuse("platform"));
        }
        if identity
            .as_ref()
            .is_some_and(|id| claims.placement.vm.as_ref() != Some(id))
        {
            return Err(refuse("placement_vm"));
        }
        bind_requester(&claims, jwk).map_err(|_| refuse("requester_key"))?;
        self.host
            .lock()
            .expect("host lock")
            .binding(&claims)
            .map_err(|error| AuthRefusal {
                error,
                ..refuse("binding")
            })?;
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
            tracing::info!(tenant, initialized_generation = ?generation, "tenant_wake_started");
            let generation = self.control.wake(tenant, generation).map_err(|error| {
                tracing::error!(tenant, detail=%error, "tenant_recovery_or_initialization_failed");
                error
                    .downcast_ref::<Error>()
                    .and_then(|error| match error {
                        Error::Refused(reason) => Some(Error::Refused(*reason)),
                        _ => None,
                    })
                    .unwrap_or(Error::Refused(Refusal::InternalFailure))
            })?;
            tracing::info!(tenant, generation, "tenant_wake_ready");
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
            tracing::info!(
                tenant = update.tenant,
                vcpus = update.vcpus,
                generation = update.generation,
                "tenant_quota_requested"
            );
            self.control.quota(&update.tenant, update.vcpus).map_err(|error| {
                tracing::error!(tenant = update.tenant, vcpus = update.vcpus, detail = %error, "tenant_quota_failed");
                Error::Refused(Refusal::InternalFailure)
            })?;
            tracing::info!(
                tenant = update.tenant,
                vcpus = update.vcpus,
                generation = update.generation,
                "tenant_quota_applied"
            );
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
                    let mut observation = LifecycleStream::default();
                    loop {
                        let frame = read_json(&mut stream)?;
                        if observation.boundary(&frame)? {
                            continue;
                        }
                        ensure!(frame["type"] == "event", "invalid lifecycle frame");
                        let event: motor_protocol::TenantEvent = serde_json::from_value(frame)?;
                        let metrics = runtime.control.metrics(&tenant).ok();
                        let update = {
                            let mut host = runtime.host.lock().expect("host lock");
                            host.event_observed(
                                &tenant,
                                &event,
                                unix_ms(),
                                observation.is_live(event.event_seq),
                            )?;
                            if let Some(metrics) = metrics {
                                host.sample(&tenant, metrics)?;
                            }
                            host.billing_tick(unix_ms())?;
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
    pub fn tick(self: &Arc<Self>, now_ms: u64) -> Result<()> {
        if self.sandbox_identity.is_none() {
            self.fetch_detached_terminals();
        }
        let (cancels, quotas, candidates) = {
            let mut host = self.host.lock().expect("host lock");
            host.billing_tick(now_ms)?;
            let cancels = host.expire(now_ms)?;
            let mut quotas = Vec::new();
            let mut candidates = Vec::new();
            for tenant in self.tenant_operations.keys() {
                if let Some(update) = host.quota_update(tenant, now_ms)? {
                    quotas.push(update);
                }
                if self.sandbox_identity.is_none() && host.suspend_candidate(tenant, now_ms) {
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
        let stops = self.host.lock().expect("host lock").stops_due();
        for tenant in stops {
            let operations = self.tenant_operations(&tenant)?;
            let _operation = operations.lock().expect("tenant operations lock");
            if self
                .host
                .lock()
                .expect("host lock")
                .stops_due()
                .contains(&tenant)
            {
                self.control.stop(&tenant)?;
                self.host.lock().expect("host lock").stopped(&tenant)?;
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
    fn fetch_detached_terminals(self: &Arc<Self>) {
        let jobs: Vec<_> = {
            let host = self.host.lock().expect("host lock");
            host.snapshot
                .leases
                .iter()
                .filter(|(job, l)| {
                    l.state == LeaseState::Ended
                        && !host.snapshot.zero_usage.contains_key(*job)
                        && host
                            .snapshot
                            .billing
                            .get(*job)
                            .is_none_or(|b| b.terminal.is_none())
                })
                .map(|(job, l)| (job.clone(), l.clone()))
                .collect()
        };
        for (job, lease) in jobs {
            if !self
                .terminal_workers
                .lock()
                .expect("terminal workers")
                .insert(job.clone())
            {
                continue;
            }
            let runtime = self.clone();
            std::thread::spawn(move || {
                let result = (|| -> Result<()> {
                    // This host-CID-only attach reads the runner's real result;
                    // it neither admits a run nor reserves another CPU share.
                    let mut stream = runtime.control.relay(&lease.tenant, 52)?;
                    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
                    let c = &lease.claims;
                    write_json(
                        &mut stream,
                        &json!({"type":"verified_lease","jti":job,"purpose":"attach","sub":c.sub,"vcpus":c.vcpus,"mem_mib":c.mem_mib,"platform":c.platform,"net":c.net,"max_s":c.max_s,"exp":c.exp}),
                    )?;
                    write_json(
                        &mut stream,
                        &json!({"type":"hello", "protocol_version":1, "started":true}),
                    )?;
                    write_json(
                        &mut stream,
                        &json!({"type":"attach", "job_id":job, "from_seq":lease.highest_seq.saturating_add(1)}),
                    )?;
                    loop {
                        let frame = read_json(&mut stream)?;
                        if frame["type"] == "terminal" {
                            let metrics = runtime.control.metrics(&lease.tenant).ok();
                            let mut host = runtime.host.lock().expect("host lock");
                            if let Some(metrics) = metrics {
                                host.sample(&lease.tenant, metrics)?;
                            }
                            host.receive_terminal_observed(&job, frame, unix_ms(), false)?;
                            return Ok(());
                        }
                    }
                })();
                if result.is_err() {
                    tracing::warn!(jti = job, "terminal_fetch_retry");
                }
                runtime
                    .terminal_workers
                    .lock()
                    .expect("terminal workers")
                    .remove(&job);
            });
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
        let mut stream = self.control.relay(&tenant, 52).map_err(|error| {
            tracing::error!(tenant, port = 52, detail = %error, "tenant_job_relay_failed");
            Error::Refused(Refusal::InternalFailure)
        })?;
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
        if matches!(
            result,
            Err(AuthRefusal {
                error: Error::Refused(Refusal::Auth),
                ..
            })
        ) {
            let refetch = auth_runtime
                .keys
                .write()
                .expect("keys lock")
                .should_refetch(&token, unix_ms() / 1000);
            if refetch {
                auth_runtime.refresh_keys(60);
            }
            auth_runtime.authenticate(&token, &auth_requester, unix_ms() / 1000)
        } else {
            result
        }
    })
    .await?;
    let claims = match authentication {
        Ok(c) => c,
        Err(AuthRefusal {
            error: Error::Refused(r),
            check,
            jti,
        }) => {
            log_refusal("lease_hello", r, check, jti.as_deref());
            write_frame(
                &mut client,
                &json!({"type":"lease_refused", "reason":r.wire()}),
            )
            .await?;
            return Ok(());
        }
        Err(AuthRefusal { error, .. }) => return Err(error.into()),
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
        host_id: claims.aud.clone(),
        ts: unix_ms() / 1000,
    };
    write_frame(&mut client, &json!({"type":"challenge", "nonce":challenge.nonce, "host_id":challenge.host_id, "ts":challenge.ts})).await?;
    let response = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut client)).await??;
    let Prelude::ChallengeResponse { sig } = serde_json::from_value(response)? else {
        bail!("expected challenge response");
    };
    let verified = (|| -> Result<_, (Error, &'static str)> {
        let mut nonces = Nonces::default();
        nonces
            .verify(&challenge, &claims, &requester_jwk, &sig, unix_ms() / 1000)
            .map_err(|_| (Error::Refused(Refusal::Auth), "challenge_signature"))?;
        let mut host = runtime.host.lock().expect("host lock");
        host.consume_nonce(challenge.nonce, challenge.ts, unix_ms() / 1000)
            .map_err(|error| (error, "challenge_nonce"))?;
        let (id, _) = host
            .claim(claims.clone(), unix_ms())
            .map_err(|error| (error, "admission"))?;
        Ok((
            id,
            host.watch_connection(id)
                .map_err(|error| (error, "admission"))?,
        ))
    })();
    let (id, mut updates) = match verified {
        Ok(value) => value,
        Err((Error::Refused(r), check)) => {
            log_refusal("challenge", r, check, Some(&claims.jti));
            write_frame(
                &mut client,
                &json!({"type":"lease_refused", "reason":r.wire()}),
            )
            .await?;
            return Ok(());
        }
        Err((e, _)) => return Err(e.into()),
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
            log_refusal("admission_wait", reason, "admission", Some(&claims.jti));
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
        let stored = runtime.host.lock().expect("host lock").host_settled_frames(&claims.jti, 0).is_some();
        if stored {
            return serve_stored(&runtime, &claims.jti, &first, &mut client).await;
        }
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
                stream = &mut delivery => {
                    match stream? {
                        Ok(stream) => break stream,
                        Err(error) => {
                            let reason = error.downcast_ref::<Error>().and_then(|error| match error {
                                Error::Refused(reason) => Some(*reason),
                                _ => None,
                            }).unwrap_or(Refusal::InternalFailure);
                            tracing::error!(jti = claims.jti, detail = %error, reason = reason.wire(), "tenant_delivery_failed");
                            write_frame(&mut client,&json!({"type":"lease_refused","reason":reason.wire()})).await?;
                            return Err(error);
                        }
                    }
                }
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
            let mut cursor: u64 = 0;
            loop {
                let read = read_frame(&mut gr);
                tokio::pin!(read);
                let frame = loop {
                    tokio::select! {
                        result = &mut read => { break Some(result); },
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {
                            let frames = runtime.host.lock().expect("host lock").host_settled_frames(&claims.jti, cursor.saturating_add(1));
                            if let Some(frames) = frames {
                                for frame in frames { write_frame(&mut cw, &frame).await?; }
                                runtime.host.lock().expect("host lock").terminal_delivered(&claims.jti, unix_ms())?;
                                break None;
                            }
                        }
                    }
                };
                let Some(frame) = frame else { return Ok::<(),anyhow::Error>(()); };
                let settled = runtime.host.lock().expect("host lock").host_settled_frames(&claims.jti, cursor.saturating_add(1));
                if let Some(frames) = settled {
                    tracing::warn!(jti=claims.jti, "host_settled_runner_frame_ignored");
                    for frame in frames { write_frame(&mut cw,&frame).await?; }
                    runtime.host.lock().expect("host lock").terminal_delivered(&claims.jti, unix_ms())?;
                    return Ok::<(),anyhow::Error>(());
                }
                let frame = frame?;
                if let Some(seq) = frame["seq"].as_u64() {
                    runtime
                        .host
                        .lock()
                        .expect("host lock")
                        .watermark(&claims.jti, seq)?;
                    cursor = cursor.max(seq);
                }
                if frame["type"] == "terminal" {
                    if runtime.sandbox_identity.is_some() {
                        write_frame(&mut cw, &frame).await?;
                        return Ok::<(),anyhow::Error>(());
                    }
                    let control = runtime.control.clone();
                    let tenant = runtime.host.lock().expect("host lock").snapshot.leases[&claims.jti].tenant.clone();
                    let sample = tokio::task::spawn_blocking(move || control.metrics(&tenant).map(|s|(tenant,s))).await?;
                    {
                        let mut host = runtime.host.lock().expect("host lock");
                        if let Ok((tenant,sample)) = sample { host.sample(&tenant,sample)?; }
                        host.receive_terminal(&claims.jti, frame.clone(), unix_ms())?;
                    }
                    let hosted = loop {
                        let value = runtime.host.lock().expect("host lock").snapshot.billing.get(&claims.jti).and_then(|b| b.hosted_terminal.clone());
                        if let Some(value) = value { break value; }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    };
                    write_frame(&mut cw, &frame).await?;
                    write_frame(&mut cw, &hosted).await?;
                    runtime.host.lock().expect("host lock").terminal_delivered(&claims.jti, unix_ms())?;
                    return Ok::<(),anyhow::Error>(());
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

async fn serve_stored(
    runtime: &Runtime,
    job: &str,
    first: &Value,
    client: &mut (impl AsyncRead + AsyncWrite + Unpin),
) -> Result<()> {
    ensure!(first["type"] == "hello", "expected hello");
    write_frame(
        client,
        &json!({"type":"hello", "protocol_version":1,"started":true}),
    )
    .await?;
    loop {
        let request = read_frame(client).await?;
        ensure!(request["job_id"] == job, "operation_refused");
        match request["type"].as_str() {
            Some("attach") => {
                let from = request["from_seq"].as_u64().context("attach cursor")?;
                let frames = runtime
                    .host
                    .lock()
                    .expect("host lock")
                    .host_settled_frames(job, from)
                    .context("stored terminal")?;
                for frame in frames {
                    write_frame(client, &frame).await?;
                }
                runtime
                    .host
                    .lock()
                    .expect("host lock")
                    .terminal_delivered(job, unix_ms())?;
                return Ok(());
            }
            Some("job_status") => {
                let mut terminal = runtime.host.lock().expect("host lock").snapshot.billing[job]
                    .terminal
                    .clone()
                    .context("stored terminal")?;
                terminal.as_object_mut().context("terminal")?.remove("type");
                write_frame(client, &json!({"type":"job_status_reply","job_id":job,"state":"ended","terminal":terminal,"unit":null,"unit_alive":false})).await?;
            }
            Some("cancel") => {
                write_frame(client, &json!({"type":"cancel_reply","job_id":job})).await?
            }
            _ => bail!("operation_refused"),
        }
    }
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
