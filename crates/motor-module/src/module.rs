use crate::{storage::JobAttribution, Config, JobStream, Motor, SshConnector};
use cortexkit_claustrum_client::{CredentialResolver, UnavailableCredentialResolver};
use cortexkit_store_types::{StorageBackend, StorageDescriptor};
use motor_protocol::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};
use subc_client_rs::{
    async_trait, BindDecision, ConnectionEnd, HandlerOutcome, HealthReport, HealthStatus,
    ModuleHandler, RequestCtx, RouteBindRequest, RouteHandle,
};
use subc_protocol::{
    manifest::{
        Bindings, CapabilityDeclarations, Concurrency, IdentityBinding, ManagementOperation,
        ManagementOperationKind, ModuleManifest, ProviderRole, StorageBinding, StorageKind,
        StorageScope, TrustTier,
    },
    scope::{ScopeStamp, FLOW_SCOPES_CAPABILITY},
    ModuleHelloAckBody, Principal,
};

pub const CAPABILITY: &str = "exec-remote/v1";
pub const DEFAULT_MODULE_ID: &str = "ck-motor";
// subc-client-rs 0.26.1 lib.rs:86 fixes provider handler capacity at 64.
pub const SDK_HANDLER_CAPACITY: usize = 64;
// Reserve slots for status, cancel and attach when long-lived run streams fill
// the provider. These control operations must remain usable under saturation.
pub const CONTROL_HEADROOM: usize = 8;
pub const RUN_ADMISSION_LIMIT: usize = SDK_HANDLER_CAPACITY - CONTROL_HEADROOM;
/// How long a request that needs module storage waits for startup to open it
/// before it is refused as `motor_unavailable`.
///
/// The SDK reads no frames until `on_hello_ack` returns, so the handler opens
/// storage in the background and requests wait for it instead. The bound is
/// under the SDK consumer's default call timeout (30 s), so a caller on default
/// options receives this typed, retryable refusal rather than its own timeout.
/// It still covers the first eight attempts of the storage lease retry (backoff
/// 0.1 s doubling to a 5 s cap, about 16 s of sleeps), long enough for an old
/// process that is still shutting down during a restart to release the lease.
pub const STORAGE_READY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Where startup is in opening module storage. Requests wait while it is
/// `Opening`; any other state answers them at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorageState {
    /// No HELLO_ACK has started a storage open on this connection.
    NotStarted,
    /// The startup task is opening storage, or retrying a lease another
    /// process holds.
    Opening,
    Ready,
    /// Startup gave up; `startup_error` says why.
    Failed,
}
struct RunPermit(Arc<std::sync::atomic::AtomicUsize>);
impl RunPermit {
    fn acquire(count: &Arc<std::sync::atomic::AtomicUsize>) -> Option<Self> {
        let mut current = count.load(std::sync::atomic::Ordering::Acquire);
        loop {
            if current >= RUN_ADMISSION_LIMIT {
                return None;
            }
            match count.compare_exchange_weak(
                current,
                current + 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self(count.clone())),
                Err(observed) => current = observed,
            }
        }
    }
}
impl Drop for RunPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
pub const OPERATIONS: &[&str] = &[
    "exec.run",
    "exec.attach",
    "exec.cancel",
    "exec.status",
    "runner.health",
    "shadow.report",
    "workspace.prepare",
    "workspace.drop",
];
/// The one flow admitted to ck-motor: it reads `runner.health` every half hour
/// and wakes MOTOR when the build server's health changes. Its source lives in
/// `.cortexkit/flows/motor-build-server-health/`.
pub(crate) const HEALTH_FLOW_ID: &str = "motor-build-server-health";

/// A flow route is the health flow when the daemon stamped it with a scope that
/// prefrontal-core owns and was authorized to set, naming that flow. The stamp
/// is the daemon's, so a flow can't claim another flow's id.
fn is_health_flow(scope: Option<&ScopeStamp>) -> bool {
    scope.is_some_and(|scope| {
        matches!(&scope.owner, Principal::Reserved { module_id } if module_id == "prefrontal-core")
            && scope.owner_authorized
            && scope.attributes.flow_id.as_deref() == Some(HEALTH_FLOW_ID)
    })
}

/// Advertised only when the Windows runner is configured; AFT sends
/// `runon: "windows"` only to a module that lists it. The daemon refuses a
/// module's HELLO unless every capability is spelled `<name>/v<N>`.
pub(crate) const WINDOWS_CAPABILITY: &str = "exec-remote-windows/v1";
pub(crate) const MACOS_CAPABILITY: &str = "exec-remote-macos/v1";
pub(crate) const NETWORK_CAPABILITY: &str = "exec-remote-net/v1";
/// Always advertised: an `exec.run` request may carry `return_paths`, and the
/// job's stream then has a `return_result` record before its terminal record.
/// Callers send non-empty `return_paths` only to a module that lists it.
pub(crate) const RETURN_CAPABILITY: &str = "exec-remote-return/v1";
/// Always advertised next to `RETURN_CAPABILITY`: an `exec.run` request may set
/// `return_chunked`, and the job's stream then has `return_chunk` records and
/// one `return_manifest` before its terminal record instead of `return_result`.
/// A runner too old to send them refuses the job with
/// `return_chunked_unsupported` before it runs.
pub(crate) const RETURN_CHUNKED_CAPABILITY: &str =
    cortexkit_exec_remote_types::RETURN_CHUNKED_CAPABILITY;

pub fn manifest(module_id: &str) -> ModuleManifest {
    manifest_with_config(module_id, &Config::default())
}

pub fn manifest_with_config(module_id: &str, config: &Config) -> ModuleManifest {
    let mut provides = vec![
        CAPABILITY.into(),
        FLOW_SCOPES_CAPABILITY.into(),
        RETURN_CAPABILITY.into(),
        RETURN_CHUNKED_CAPABILITY.into(),
        motor_protocol::SUBMISSION_KEY_CAPABILITY.into(),
    ];
    if !config.windows_runners.is_empty()
        || (config.windows_runner_instance.is_some() && config.windows_runner_state_root.is_some())
    {
        provides.push(WINDOWS_CAPABILITY.into());
    }
    if config.network_routing {
        provides.push(NETWORK_CAPABILITY.into());
    }
    if config.macos_runner.is_some() {
        provides.push(MACOS_CAPABILITY.into());
    }
    ModuleManifest::builder(module_id, env!("CARGO_PKG_VERSION"))
        .trust_tier(Some(TrustTier::FirstParty))
        .protocol_ver(subc_protocol::PROTOCOL_VERSION)
        .capabilities(Some(CapabilityDeclarations {
            // The daemon routes a flow's requests (scheduled automation run by
            // Basal) only to modules that declare they handle flow scopes.
            provides,
            requires: Vec::new(),
            must_never_reach: Vec::new(),
        }))
        .bindings(Some(Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: Vec::new(),
                optional: Vec::new(),
            },
        }))
        .provides(vec![ProviderRole::ManagementSurface {
            concurrency: Concurrency::ModuleManaged,
            operations: OPERATIONS
                .iter()
                .map(|name| ManagementOperation {
                    name: (*name).into(),
                    kind: if matches!(*name, "exec.status" | "runner.health" | "shadow.report") {
                        ManagementOperationKind::Query
                    } else {
                        ManagementOperationKind::Mutate
                    },
                    description: None,
                })
                .collect(),
            config_schema: json!({"type":"object"}),
            observability: Vec::new(),
            identity_scope: Vec::new(),
        }])
        .provenance(Some(build_provenance_from_build_facts(
            option_env!("CK_BUILD_GIT_SHA"),
            option_env!("CK_BUILD_GIT_STATUS"),
            option_env!("CK_BUILD_LOCK_DIGEST"),
        )))
        .build()
}

fn build_provenance_from_build_facts(
    revision: Option<&str>,
    git_status: Option<&str>,
    lock_digest: Option<&str>,
) -> subc_protocol::manifest::ManifestProvenance {
    use subc_protocol::manifest::{build_provenance_from_source, BuildGitShaSource, GitTreeState};

    let revision = revision.filter(|revision| !revision.is_empty());
    let source = match (revision, git_status) {
        (Some(revision), Some("clean")) => BuildGitShaSource::Git {
            revision,
            tree_state: GitTreeState::Clean,
        },
        (Some(revision), Some("dirty")) => BuildGitShaSource::Git {
            revision,
            tree_state: GitTreeState::Dirty,
        },
        (Some(_), _) => BuildGitShaSource::NeverDerived,
        (None, _) => BuildGitShaSource::NoGitDir,
    };

    let lock_digest = lock_digest.filter(|digest| !digest.is_empty());
    // The module builds this manifest when it registers, so a fact in an
    // unexpected form (a SHA-256 object format repository, say) must not stop
    // ck-motor from starting: declare the revision as never derived instead.
    build_provenance_from_source(source, lock_digest, None).unwrap_or_else(|_| {
        build_provenance_from_source(BuildGitShaSource::NeverDerived, None, None)
            .expect("provenance without facts is always well formed")
    })
}

#[cfg(test)]
mod provenance_tests {
    use super::build_provenance_from_build_facts;
    use subc_protocol::manifest::BuildGitShaAbsenceReason;

    const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    const LOCK_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn build_facts_map_clean_dirty_and_missing_git_to_provenance() {
        let clean =
            build_provenance_from_build_facts(Some(REVISION), Some("clean"), Some(LOCK_DIGEST));
        assert_eq!(clean.build_git_sha.as_deref(), Some(REVISION));
        assert!(clean.build_git_sha_absence_reason.is_none());
        assert_eq!(clean.build_lock_digest.as_deref(), Some(LOCK_DIGEST));

        let dirty =
            build_provenance_from_build_facts(Some(REVISION), Some("dirty"), Some(LOCK_DIGEST));
        assert!(dirty.build_git_sha.is_none());
        assert_eq!(
            dirty.build_git_sha_absence_reason,
            Some(BuildGitShaAbsenceReason::DeclinedDirty)
        );

        let no_git = build_provenance_from_build_facts(None, None, Some(LOCK_DIGEST));
        assert!(no_git.build_git_sha.is_none());
        assert_eq!(
            no_git.build_git_sha_absence_reason,
            Some(BuildGitShaAbsenceReason::NoGitDir)
        );

        // A 64-character revision (SHA-256 object format) is not a valid
        // build_git_sha; it must degrade to never-derived, not panic.
        let sha256_revision = "0".repeat(64);
        let odd = build_provenance_from_build_facts(
            Some(&sha256_revision),
            Some("clean"),
            Some(LOCK_DIGEST),
        );
        assert!(odd.build_git_sha.is_none());
        assert_eq!(
            odd.build_git_sha_absence_reason,
            Some(BuildGitShaAbsenceReason::NeverDerived)
        );
    }
}

#[derive(Clone)]
pub struct Handler {
    config: Config,
    module_id: String,
    motor: Arc<OnceLock<Motor>>,
    ssh: Arc<OnceLock<Arc<SshConnector>>>,
    resolver: Arc<dyn CredentialResolver>,
    account_tokens: Option<Arc<crate::AccountTokens>>,
    routes: Arc<Mutex<HashMap<RouteHandle, RouteBindRequest>>>,
    /// Jobs started by direct routes, keyed to the route that started them. The
    /// route handle is the daemon's own stamp for a bound route on this
    /// connection. This fast path needs no secret; other direct routes must
    /// prove access with the per-job secret whose hash is in storage.
    direct_jobs: Arc<Mutex<HashMap<Uuid, RouteHandle>>>,
    run_count: Arc<std::sync::atomic::AtomicUsize>,
    startup_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    startup_error: Arc<Mutex<Option<String>>>,
    storage_state: Arc<tokio::sync::watch::Sender<StorageState>>,
    storage_ready_deadline: std::time::Duration,
    /// Holds the startup task before its first storage open until a test adds
    /// a permit, so tests can observe requests that arrive during the open.
    #[cfg(test)]
    storage_gate: Option<Arc<tokio::sync::Semaphore>>,
    health_cache: Arc<Mutex<crate::health_cache::HealthCache>>,
    health_sampler: Arc<Mutex<crate::health_cache::Sampler>>,
    write_engram_catalog: bool,
}
pub(crate) fn module_data_dir(module_id: &str) -> io::Result<PathBuf> {
    module_data_dir_for(
        module_id,
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn module_data_dir_for(
    module_id: &str,
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> io::Result<PathBuf> {
    let data = match xdg_data_home {
        Some(data) => data,
        None => home
            .ok_or_else(|| io::Error::other("neither XDG_DATA_HOME nor HOME is set"))?
            .join(".local/share"),
    };
    Ok(data.join("cortexkit").join(module_id))
}

impl Handler {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            module_id: DEFAULT_MODULE_ID.into(),
            motor: Arc::new(OnceLock::new()),
            ssh: Arc::new(OnceLock::new()),
            resolver: Arc::new(UnavailableCredentialResolver),
            account_tokens: None,
            routes: Arc::new(Mutex::new(HashMap::new())),
            direct_jobs: Arc::new(Mutex::new(HashMap::new())),
            run_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            startup_task: Arc::new(Mutex::new(None)),
            startup_error: Arc::new(Mutex::new(None)),
            storage_state: Arc::new(tokio::sync::watch::Sender::new(StorageState::NotStarted)),
            storage_ready_deadline: STORAGE_READY_DEADLINE,
            #[cfg(test)]
            storage_gate: None,
            health_cache: Arc::new(Mutex::new(crate::health_cache::HealthCache::default())),
            health_sampler: Arc::new(Mutex::new(crate::health_cache::Sampler::default())),
            write_engram_catalog: false,
        }
    }
    pub fn with_engram_catalog(mut self) -> Self {
        self.write_engram_catalog = true;
        self
    }
    pub fn with_module_id(mut self, module_id: impl Into<String>) -> Self {
        self.module_id = module_id.into();
        self
    }
    /// Serve with an already open `Motor` instead of opening one from the
    /// daemon's storage descriptor, so tests can drive this handler against an
    /// in-process runner.
    #[cfg(test)]
    pub(crate) fn with_motor(self, motor: Motor) -> Self {
        let _ = self.motor.set(motor);
        self.storage_state.send_replace(StorageState::Ready);
        self
    }
    #[cfg(test)]
    pub(crate) fn with_storage_gate(mut self, gate: Arc<tokio::sync::Semaphore>) -> Self {
        self.storage_gate = Some(gate);
        self
    }
    #[cfg(test)]
    pub(crate) fn with_storage_ready_deadline(mut self, deadline: std::time::Duration) -> Self {
        self.storage_ready_deadline = deadline;
        self
    }
    #[cfg(test)]
    pub(crate) fn ssh_connector(&self) -> Option<Arc<SshConnector>> {
        self.ssh.get().cloned()
    }
    pub fn with_credentials(mut self, resolver: Arc<dyn CredentialResolver>) -> Self {
        self.resolver = resolver;
        self
    }
    pub fn with_account_tokens(mut self, tokens: Arc<crate::AccountTokens>) -> Self {
        self.account_tokens = Some(tokens);
        self
    }
    pub async fn shutdown(&self) {
        let startup = self.startup_task.lock().unwrap().take();
        if let Some(startup) = startup {
            startup.abort();
            let _ = startup.await;
            // An aborted open will never finish, so release any waiting
            // request now instead of at its deadline.
            self.storage_state.send_if_modified(|state| {
                let aborted = *state == StorageState::Opening;
                if aborted {
                    *state = StorageState::NotStarted;
                }
                aborted
            });
        }
        let sampler = self.health_sampler.lock().unwrap().0.take();
        if let Some(sampler) = sampler {
            sampler.abort();
            let _ = sampler.await;
        }
        if let Ok(motor) = self.initialized() {
            motor.shutdown_shadows().await;
            let motor = motor.clone();
            match tokio::task::spawn_blocking(move || motor.shutdown_storage()).await {
                Ok(Ok(())) => {}
                result => tracing::error!(
                    ?result,
                    "storage shutdown export failed; run ck-motor store-checkpoint before rollback"
                ),
            }
        }
        if let Some(ssh) = self.ssh.get() {
            ssh.shutdown().await;
        }
    }
    fn storage_root(&self, ack: &ModuleHelloAckBody) -> io::Result<PathBuf> {
        self.storage_root_with_data_dir(ack, module_data_dir)
    }
    fn storage_root_with_data_dir(
        &self,
        ack: &ModuleHelloAckBody,
        data_dir_for_module: impl FnOnce(&str) -> io::Result<PathBuf>,
    ) -> io::Result<PathBuf> {
        match &ack.storage {
            Some(value) => {
                let descriptor: StorageDescriptor =
                    serde_json::from_value(value.clone()).map_err(io::Error::other)?;
                match descriptor.backend {
                    StorageBackend::Sqlite { path } => {
                        // This suffix identifies ck-motor's store format, not its module ID;
                        // SUBC can move the data folder while the SQLite file name stays stable.
                        Ok(PathBuf::from(format!("{path}.ck-motor")))
                    }
                    _ => Err(io::Error::other(format!(
                        "{} requires SQLite module storage",
                        self.module_id
                    ))),
                }
            }
            None => Ok(data_dir_for_module(&self.module_id)?.join(&self.config.runner_instance)),
        }
    }
    fn startup_failed(&self, detail: String) {
        tracing::warn!(%detail, "ck-motor storage unavailable");
        *self.startup_error.lock().unwrap() = Some(detail);
    }
    async fn initialize_storage(&self, root: PathBuf, catalog_data_dir: io::Result<PathBuf>) {
        #[cfg(test)]
        if let Some(gate) = &self.storage_gate {
            let _ = gate.acquire().await;
        }
        let mut backoff = std::time::Duration::from_millis(100);
        loop {
            let config = self.config.clone();
            let path = root.clone();
            let opened = tokio::task::spawn_blocking(move || Motor::open(config, &path)).await;
            match opened.unwrap_or_else(|error| Err(io::Error::other(error))) {
                Ok(mut motor) => {
                    if let Some(tokens) = &self.account_tokens {
                        motor = motor.with_account_tokens(tokens.clone());
                    }
                    // The lease must be held before initialization can replace an
                    // earlier process's private SSH agent directory.
                    let ssh = Arc::new(
                        SshConnector::initialize(
                            self.config.clone(),
                            &root,
                            Arc::clone(&self.resolver),
                        )
                        .await,
                    );
                    motor.set_connector(ssh.clone());
                    if self.write_engram_catalog {
                        // Catalog I/O must not delay readiness on a slow data volume.
                        let module_id = self.module_id.clone();
                        let data_dir = catalog_data_dir;
                        let catalog_root = root.clone();
                        drop(tokio::task::spawn_blocking(move || {
                            crate::engram_catalog::write_best_effort(
                                &module_id,
                                data_dir,
                                &catalog_root,
                            )
                        }));
                    }
                    self.ssh.set(ssh).ok();
                    self.motor.set(motor).ok();
                    self.start_health_sampler();
                    *self.startup_error.lock().unwrap() = None;
                    self.storage_state.send_replace(StorageState::Ready);
                    return;
                }
                Err(error) => {
                    let contended = error.kind() == io::ErrorKind::WouldBlock;
                    self.startup_failed(if contended {
                        "storage_lease_busy: another process owns module storage".into()
                    } else {
                        format!("storage_open_failed: {error}")
                    });
                    // A busy lease stays `Opening`: the retry below may still
                    // succeed, so waiting requests keep waiting for it.
                    if !contended {
                        self.storage_state.send_replace(StorageState::Failed);
                        return;
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
        }
    }
    fn start_health_sampler(&self) {
        let mut sampler = self.health_sampler.lock().unwrap();
        if sampler.0.is_some() {
            return;
        }
        let motor = self.motor.get().expect("storage initialized").clone();
        let cache = Arc::clone(&self.health_cache);
        sampler.0 = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(crate::health_cache::REFRESH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let sample = tokio::time::timeout(
                    crate::health_cache::SAMPLE_TIMEOUT,
                    motor.runner_health(),
                )
                .await
                .ok()
                .and_then(Result::ok);
                let windows = motor.windows_health().await;
                let mut cache = cache.lock().unwrap();
                cache.update(sample, std::time::Instant::now());
                cache.windows = windows;
            }
        }));
    }
    fn attribution(&self, handle: RouteHandle, preset: Option<String>) -> Option<JobAttribution> {
        let routes = self.routes.lock().unwrap();
        let route = routes.get(&handle)?;
        let scope = route.scope.as_ref()?;
        Some(JobAttribution {
            session: route.identity.session.clone(),
            preset,
            scope_ref: scope.scope_ref.clone(),
            scope_epoch: scope.scope_epoch,
        })
    }
}
/// The refusal reason a direct `exec.run` gets when its timeout is missing or
/// over [`Config::direct_exec_max_timeout_secs`].
pub const DIRECT_TIMEOUT_REFUSED: &str = "direct_timeout_refused";

/// Why a direct run's timeout is unacceptable, or `None` when it is within the
/// cap. A disconnected caller may never return with its secret, so the runner's
/// deadline still bounds the job. The CLI uses the same refusal text locally.
pub fn direct_timeout_refusal(timeout: Option<u64>, cap: u64) -> Option<String> {
    const WHY: &str = "a disconnected direct caller may not return with its secret, so the job must have a deadline";
    match timeout {
        None => Some(format!(
            "a direct exec.run needs a timeout of at most {cap} seconds: {WHY}"
        )),
        Some(timeout) if timeout > cap => Some(format!(
            "timeout {timeout} seconds exceeds the direct-job cap of {cap} seconds: {WHY}"
        )),
        Some(_) => None,
    }
}

/// The error code for a direct `ck motor run --hosted` this ck-motor can't
/// serve, because it has no broker URL or no broker token source. Nothing was
/// created; the CLI exits with its bad-arguments status.
pub(crate) const HOSTED_TEST_UNAVAILABLE: &str = "motor_hosted_test_unavailable";

/// What a direct caller gets for a job its route did not start. The bytes are
/// the same whether the job belongs to another caller or does not exist, so a
/// direct caller can't learn which job IDs are live.
fn direct_job_not_found(module_id: &str) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "motor_job_not_found".into(),
        message: format!("{module_id} has no such job for this caller"),
    }
}

impl Handler {
    /// Why this ck-motor can't send a direct job straight to a hosted lease,
    /// or `None` when it can: that needs a configured broker URL and a broker
    /// token source able to mint a bearer (the account vault, or the fake
    /// token outside the main runner instance).
    fn hosted_test_unavailable(&self) -> Option<String> {
        if self.config.broker_url.is_none() {
            return Some(format!(
                "{} has no broker_url configured, so it cannot run a job on a hosted runner",
                self.module_id
            ));
        }
        match self.config.broker_token_source {
            crate::config::BrokerTokenSource::CkAccount if self.account_tokens.is_none() => {
                Some(format!(
                    "{} has no account token source, so it cannot run a job on a hosted runner",
                    self.module_id
                ))
            }
            _ => None,
        }
    }
    /// Requests on a route the daemon attested as `Principal::Direct`. Enabling
    /// `admit_direct_exec` permits these local non-agent callers to start jobs and
    /// attach to, cancel or check jobs their route started or whose secret they
    /// hold, plus the credential-only hosted operator check.
    async fn handle_direct(
        &self,
        ctx: RequestCtx,
        envelope: Envelope,
    ) -> io::Result<HandlerOutcome> {
        let route = ctx.route_handle();
        let op = envelope.op.as_str();
        if !matches!(
            op,
            "exec.run"
                | "exec.attach"
                | "exec.cancel"
                | "exec.status"
                | "hosted.check"
                | "shadow.report"
        ) {
            return Ok(HandlerOutcome::Error {
                code: "motor_op_not_permitted_for_principal".into(),
                message: format!("{} refuses {op} for direct", self.module_id),
            });
        }
        if op != "hosted.check" {
            // Every other direct op reads the store; wait for startup to open
            // it so the `initialized` calls below find it.
            self.ready().await?;
        }
        let owned = |id: Uuid| self.direct_jobs.lock().unwrap().get(&id) == Some(&route);
        fn parse<T: serde::de::DeserializeOwned>(params: Value) -> io::Result<T> {
            serde_json::from_value(params).map_err(io::Error::other)
        }
        match op {
            "shadow.report" => {
                let since = envelope
                    .params
                    .get("since")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                response(self.initialized()?.shadow_report(since)?)
            }
            "hosted.check" => response(
                crate::hosted::check::check(self.account_tokens.as_ref(), &self.config).await,
            ),
            "exec.run" => {
                #[derive(Deserialize, Default)]
                struct SpillOptions {
                    #[serde(default)]
                    allow_spill: bool,
                    /// `ck motor run --hosted`: send this job straight to a
                    /// hosted lease. Only this direct route reads it; the
                    /// agent route's request parser ignores the key.
                    #[serde(default)]
                    hosted: bool,
                }
                let spill: SpillOptions = parse(envelope.params.clone())?;
                let returns: crate::ReturnOptions = parse(envelope.params.clone())?;
                let request: RunRequest = match serde_json::from_value(envelope.params) {
                    Ok(request) => request,
                    Err(error) => {
                        return Ok(HandlerOutcome::Error {
                            code: "invalid_params".into(),
                            message: error.to_string(),
                        })
                    }
                };
                let motor = self.initialized()?;
                let secret = crate::direct_secret::DirectSecret::mint()?;
                if request.submission_key.is_some() {
                    let stream = motor.refuse_authorized(
                        RefusalReason::RequestRejected,
                        Some("submission_key_requires_attested_caller".into()),
                        Some(secret.hash()),
                    )?;
                    pump_direct_secret(&ctx, stream, Some(secret)).await?;
                    return Ok(HandlerOutcome::Streamed);
                }
                if spill.hosted {
                    if let Some(message) = self.hosted_test_unavailable() {
                        return Ok(HandlerOutcome::Error {
                            code: HOSTED_TEST_UNAVAILABLE.into(),
                            message,
                        });
                    }
                }
                if let Some(detail) = direct_timeout_refusal(
                    request.timeout,
                    self.config.direct_exec_max_timeout_secs,
                ) {
                    let stream = motor.refuse_authorized(
                        RefusalReason::RequestRejected,
                        Some(format!("{DIRECT_TIMEOUT_REFUSED}: {detail}")),
                        Some(secret.hash()),
                    )?;

                    pump_direct_secret(&ctx, stream, Some(secret)).await?;
                    return Ok(HandlerOutcome::Streamed);
                }
                let _permit = match RunPermit::acquire(&self.run_count) {
                    Some(permit) => permit,
                    None => {
                        let stream = motor.refuse_authorized(
                            RefusalReason::RunnerFull,
                            None,
                            Some(secret.hash()),
                        )?;

                        pump_direct_secret(&ctx, stream, Some(secret)).await?;
                        return Ok(HandlerOutcome::Streamed);
                    }
                };
                let stream = motor.run_direct_authorized(
                    request,
                    self.attribution(route, envelope.preset),
                    returns,
                    spill.allow_spill,
                    spill.hosted,
                    secret.hash(),
                )?;

                self.direct_jobs
                    .lock()
                    .unwrap()
                    .insert(stream.job_id, route);
                tracing::info!(caller = "direct", job_id = %stream.job_id, "direct job submitted");
                pump_direct_secret(&ctx, stream, Some(secret))
                    .await
                    .map(|()| HandlerOutcome::Streamed)
            }
            "exec.attach" => {
                let motor = self.initialized()?;
                let id = envelope
                    .params
                    .get("job_id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse().ok());
                let Some(id) = id else {
                    return Ok(direct_job_not_found(&self.module_id));
                };
                if !owned(id) && !motor.direct_authorized(id, envelope.direct_secret.as_str()) {
                    return Ok(direct_job_not_found(&self.module_id));
                }
                let request: AttachRequest = parse(envelope.params)?;
                pump_direct(&ctx, self.initialized()?.attach(request)?)
                    .await
                    .map(|()| HandlerOutcome::Streamed)
            }
            "exec.cancel" => {
                let motor = self.initialized()?;
                let id = envelope
                    .params
                    .get("job_id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse().ok());
                let Some(id) = id else {
                    return Ok(direct_job_not_found(&self.module_id));
                };
                if !owned(id) && !motor.direct_authorized(id, envelope.direct_secret.as_str()) {
                    return Ok(direct_job_not_found(&self.module_id));
                }
                let request: CancelRequest = parse(envelope.params)?;
                tracing::info!(caller = "direct", job_id = %request.job_id, "direct job cancel requested");
                response(self.initialized()?.cancel(request).await?)
            }
            _ => {
                // Aggregate runner status and disk reports describe every
                // caller's work, so a direct caller may ask only by job ID.
                let Some(id) = envelope.params.get("job_id") else {
                    return Ok(HandlerOutcome::Error {
                        code: "motor_op_not_permitted_for_principal".into(),
                        message: format!("{} refuses exec.status without a job_id for direct; a direct caller may check only its own jobs", self.module_id),
                    });
                };
                let Some(id) = id.as_str().and_then(|id| id.parse().ok()) else {
                    return Ok(direct_job_not_found(&self.module_id));
                };
                if !owned(id)
                    && !self
                        .initialized()?
                        .direct_authorized(id, envelope.direct_secret.as_str())
                {
                    return Ok(direct_job_not_found(&self.module_id));
                }
                response(self.initialized()?.job_status(id).await?)
            }
        }
    }
    pub(crate) fn initialized(&self) -> io::Result<&Motor> {
        self.motor.get().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!(
                    "{} unavailable: {}",
                    self.module_id,
                    self.startup_error
                        .lock()
                        .unwrap()
                        .as_deref()
                        .unwrap_or("module not initialized")
                ),
            )
        })
    }
    /// The open store, waiting up to the storage deadline while startup is
    /// still opening it. A failed startup, or one still opening at the
    /// deadline, is a `NotConnected` error, which `handle` reports as
    /// `motor_unavailable`.
    pub(crate) async fn ready(&self) -> io::Result<&Motor> {
        let mut state = self.storage_state.subscribe();
        let settled = tokio::time::timeout(
            self.storage_ready_deadline,
            state.wait_for(|state| *state != StorageState::Opening),
        )
        .await;
        if settled.is_err() {
            // Storage may have opened just as the deadline passed.
            if let Ok(motor) = self.initialized() {
                return Ok(motor);
            }
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!(
                    "{} unavailable: storage still opening after {} ms; retry later ({})",
                    self.module_id,
                    self.storage_ready_deadline.as_millis(),
                    self.startup_error
                        .lock()
                        .unwrap()
                        .as_deref()
                        .unwrap_or("first storage open has not finished")
                ),
            ));
        }
        self.initialized()
    }
    /// Waits for HELLO_ACK to start opening storage, then calls `ready`. Tests
    /// that send HELLO_ACK use it because they cannot tell when the serve task
    /// has handled it.
    #[cfg(test)]
    pub(crate) async fn storage_opened(&self) -> io::Result<&Motor> {
        let mut state = self.storage_state.subscribe();
        let _ = state
            .wait_for(|state| *state != StorageState::NotStarted)
            .await;
        self.ready().await
    }
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(alias = "method")]
    op: String,
    #[serde(default)]
    params: Value,
    // Kept as JSON until authorization, so malformed credentials receive the
    // same refusal as missing ones rather than a deserializer's error text.
    #[serde(default)]
    direct_secret: Value,
    /// AFT's selected preset is attribution, not a policy input to motor.
    #[serde(default)]
    preset: Option<String>,
}
#[async_trait]
impl ModuleHandler for Handler {
    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        if self.motor.get().is_some() {
            return;
        }
        let root = match self.storage_root(ack) {
            Ok(root) => root,
            Err(error) => {
                self.startup_failed(format!("invalid_storage_descriptor: {error}"));
                self.storage_state.send_replace(StorageState::Failed);
                return;
            }
        };
        let mut task = self.startup_task.lock().unwrap();
        if self.motor.get().is_some() || task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        // The SDK reads no frames, route binds included, until this returns, so
        // storage opens in the background. Requests that need it wait in
        // `ready`; binds are answered at once.
        self.storage_state.send_replace(StorageState::Opening);
        let handler = self.clone();
        *task = Some(tokio::spawn(async move {
            handler
                .initialize_storage(root, module_data_dir(&handler.module_id))
                .await
        }));
    }
    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        match request.principal.as_ref() {
            Some(Principal::Reserved { module_id })
                if module_id == "aft" || module_id == "prefrontal-core" => {}
            // Basal runs every scheduled flow, so all their routes share the
            // reserved:basal principal and it can't tell flows apart. The
            // daemon also stamps each route with the scope it was opened under,
            // which names the flow and can't be forged by the flow. Only the
            // build-server health flow is admitted, and the request handler
            // then allows it the read-only runner.health report only.
            Some(Principal::Reserved { module_id })
                if module_id == "basal" && is_health_flow(request.scope.as_ref()) => {}
            Some(Principal::Direct) if self.config.admit_direct_exec => {}
            Some(Principal::Direct) => {
                return BindDecision::reject(
                    "motor_direct_refused",
                    format!("{} refuses direct binds", self.module_id),
                )
            }
            Some(Principal::Unverified) | None => {
                return BindDecision::reject(
                    "motor_unverified_refused",
                    format!("{} refuses unverified binds", self.module_id),
                )
            }
            Some(Principal::Reserved { module_id }) => {
                return BindDecision::reject(
                    "motor_principal_refused",
                    format!("{} refuses reserved:{module_id}; only reserved:aft, reserved:prefrontal-core and the {HEALTH_FLOW_ID} flow may bind", self.module_id),
                )
            }
        }
        self.routes
            .lock()
            .unwrap()
            .insert(request.handle, request.clone());
        BindDecision::accept()
    }
    async fn on_route_gone(&self, handle: &RouteHandle) {
        self.routes.lock().unwrap().remove(handle);
        // A closed route's jobs keep running on the runner until they finish or
        // reach their timeout; a later route must present the job's secret.
        self.direct_jobs
            .lock()
            .unwrap()
            .retain(|_, owner| owner != handle);
    }
    async fn on_connection_end(&self, _end: ConnectionEnd) {
        self.shutdown().await;
        self.routes.lock().unwrap().clear();
        self.direct_jobs.lock().unwrap().clear();
    }
    async fn health(&self) -> HealthReport {
        let reason = if self.motor.get().is_none() {
            Some(
                self.startup_error
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "module_not_initialized".into()),
            )
        } else {
            self.ssh
                .get()
                .and_then(|ssh| ssh.health_refusal())
                .map(str::to_owned)
        };
        let mut report = self
            .health_cache
            .lock()
            .unwrap()
            .report(std::time::Instant::now());
        // The detail line stays one sentence; the refusal counters are
        // diagnostics, shown with `ck health ck-motor --verbose`.
        if let Some(serde_json::Value::Object(metrics)) = report.metrics.as_mut() {
            metrics.insert(
                "unreachable_refusals_since_boot".into(),
                crate::diagnostics::health_metrics(),
            );
            if let Some(motor) = self.motor.get() {
                metrics.insert("shadow".into(), motor.shadow_health());
            }
        }
        if let Some(reason) = reason {
            report.status = HealthStatus::Failing;
            report.detail = Some(match report.detail.take() {
                Some(detail) => format!("{reason}; {detail}"),
                None => reason,
            });
        }
        report
    }
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let result = async {
            let envelope: Envelope = serde_json::from_slice(&body).map_err(io::Error::other)?;
            let principal = self
                .routes
                .lock()
                .unwrap()
                .get(&ctx.route_handle())
                .and_then(|route| route.principal.clone());
            let module_id = match principal {
                Some(Principal::Reserved { module_id }) => module_id,
                // on_bind admits a direct route only while the switch is on;
                // the check is repeated so a route bound under another
                // configuration can't act.
                Some(Principal::Direct) if self.config.admit_direct_exec => {
                    return self.handle_direct(ctx, envelope).await;
                }
                _ => {
                    return Ok(HandlerOutcome::Error {
                        code: "motor_route_not_admitted".into(),
                        message: format!("{} requires an admitted reserved:aft, reserved:prefrontal-core or reserved:basal route", self.module_id),
                    })
                }
            };
            // AFT executes commands; core owns remote workspace lifecycle. Check
            // the daemon-attested route principal before parsing any op params.
            let permitted = match envelope.op.as_str() {
                "exec.run" | "exec.attach" | "exec.cancel" | "exec.status" => module_id == "aft",
                "runner.health" => {
                    module_id == "aft" || module_id == "prefrontal-core" || module_id == "basal"
                }
                "workspace.prepare" | "workspace.drop" => module_id == "prefrontal-core",
                _ => false,
            };
            if !permitted {
                return Ok(HandlerOutcome::Error {
                    code: "motor_op_not_permitted_for_principal".into(),
                    message: format!("{} refuses {} for reserved:{module_id}", self.module_id, envelope.op),
                });
            }
            let motor = self.ready().await?;
            let principal = format!("reserved:{module_id}");
            if matches!(envelope.op.as_str(), "exec.attach" | "exec.cancel" | "exec.status") {
                if let Some(id) = envelope.params.get("job_id")
                    .and_then(Value::as_str).and_then(|id| id.parse().ok()) {
                    if !motor.submission_owned(id, &principal) {
                        return Ok(HandlerOutcome::Error {
                            code: "motor_job_not_found".into(),
                            message: "job not found".into(),
                        });
                    }
                }
            }
            match envelope.op.as_str() {
                "exec.run" => {
                    let mut request: RunRequest = match serde_json::from_value(envelope.params) {
                        Ok(request) => request,
                        Err(error) => return Ok(HandlerOutcome::Error {
                            code: "invalid_params".into(),
                            message: error.to_string(),
                        }),
                    };
                    let _permit = if motor.has_submission(&principal, &request)? {
                        None
                    } else {
                        match RunPermit::acquire(&self.run_count) {
                            Some(permit) => Some(permit),
                            None => {
                                pump(&ctx, motor.refuse_capacity()?, 0).await?;
                                return Ok(HandlerOutcome::Streamed);
                            }
                        }
                    };
                    if request.submission_key.is_some() {
                        let cursor = request.resume_from_seq;
                        let stream = tokio::select! {
                            stream = motor.submit_agent(
                                request,
                                self.attribution(ctx.route_handle(), envelope.preset),
                                &principal,
                            ) => stream?,
                            _ = ctx.cancelled() => return Ok(HandlerOutcome::Streamed),
                        };
                        crate::agent_route::pump_after(&ctx, stream, cursor).await?;
                        return Ok(HandlerOutcome::Streamed);
                    }
                    let returns = match crate::agent_route::take_return_request(&mut request) {
                        Ok(returns) => returns,
                        Err(crate::agent_route::ReturnRequestError::InvalidParams(message)) => {
                            return Ok(HandlerOutcome::Error {
                                code: "invalid_params".into(),
                                message,
                            });
                        }
                        Err(crate::agent_route::ReturnRequestError::Rejected(code)) => {
                            let stream = motor.refuse_before_start(
                                RefusalReason::RequestRejected,
                                Some(code.into()),
                            )?;
                            pump(&ctx, stream, 0).await?;
                            return Ok(HandlerOutcome::Streamed);
                        }
                    };
                    pump(
                        &ctx,
                        motor.run_agent(
                            request,
                            self.attribution(ctx.route_handle(), envelope.preset),
                            returns,
                        )?,
                        0,
                    )
                    .await
                    .map(|()| HandlerOutcome::Streamed)
                }
                "exec.attach" => {
                    let request: AttachRequest =
                        serde_json::from_value(envelope.params).map_err(io::Error::other)?;
                    let from_seq = request.from_seq;
                    pump(&ctx, motor.attach(request)?, from_seq)
                        .await
                        .map(|()| HandlerOutcome::Streamed)
                }
                "exec.cancel" => response(
                    motor
                        .cancel(serde_json::from_value(envelope.params).map_err(io::Error::other)?)
                        .await?,
                ),
                "exec.status" => {
                    status_reply(motor, &envelope.params).await
                },
                "runner.health" => runner_health_reply_for(motor, &envelope.params).await,
                "workspace.prepare" => response(
                    motor
                        .prepare(serde_json::from_value(envelope.params).map_err(io::Error::other)?)
                        .await?,
                ),
                "workspace.drop" => response(
                    motor
                        .drop_workspace(
                            serde_json::from_value(envelope.params).map_err(io::Error::other)?,
                        )
                        .await?,
                ),
                _ => Err(io::Error::other("unknown operation")),
            }
        }
        .await;
        match result {
            Ok(outcome) => outcome,
            Err(e) => HandlerOutcome::Error {
                code: if e.kind() == io::ErrorKind::InvalidInput {
                    "invalid_params"
                } else if e.kind() == io::ErrorKind::NotConnected && self.motor.get().is_none() {
                    "motor_unavailable"
                } else {
                    "exec_remote_error"
                }
                .into(),
                message: e.to_string(),
            },
        }
    }
}
pub(crate) fn response(value: impl serde::Serialize) -> io::Result<HandlerOutcome> {
    Ok(HandlerOutcome::Response(
        serde_json::to_vec(&value).map_err(io::Error::other)?,
    ))
}
pub(crate) async fn status_reply(
    motor: &Motor,
    params: &serde_json::Value,
) -> io::Result<HandlerOutcome> {
    let platform = optional_platform(params)?;
    if let Some(id) = params.get("job_id") {
        let id = serde_json::from_value(id.clone()).map_err(io::Error::other)?;
        let reply = motor.job_status(id).await?;
        // Billing may have settled after the job's terminal record was
        // streamed; the status reply carries the figure as it stands now.
        response(crate::agent_route::status_with_hosted(
            reply,
            motor.hosted_state(id).as_ref(),
        ))
    } else if !matches!(platform, None | Some(motor_protocol::Platform::Linux)) {
        let reply = motor.runner_status_for(platform.as_ref()).await?;
        if params.get("disk").and_then(serde_json::Value::as_bool) == Some(true) {
            response(reply)
        } else {
            response(reply.status)
        }
    } else if params.get("disk").and_then(serde_json::Value::as_bool) == Some(true) {
        response(motor.disk_status().await)
    } else {
        response(motor.status().await)
    }
}
#[cfg(test)]
pub(crate) async fn runner_health_reply(motor: &Motor) -> io::Result<HandlerOutcome> {
    runner_health_reply_for(motor, &json!({})).await
}
pub(crate) async fn runner_health_reply_for(
    motor: &Motor,
    params: &serde_json::Value,
) -> io::Result<HandlerOutcome> {
    let platform = optional_platform(params)?;
    if platform != Some(motor_protocol::Platform::Windows) {
        if let Some(health) = motor.hosted_health() {
            return response(health);
        }
    }
    match motor.runner_health_for(platform.as_ref()).await {
        Ok(health) => response(health),
        Err(error) => Ok(HandlerOutcome::Error {
            code: "runner_health_unavailable".into(),
            message: error.to_string(),
        }),
    }
}
fn optional_platform(params: &serde_json::Value) -> io::Result<Option<motor_protocol::Platform>> {
    params
        .get("platform")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value(value.clone()).map_err(io::Error::other))
        .transpose()
}
async fn reply(ctx: &RequestCtx, value: impl serde::Serialize) -> io::Result<()> {
    ctx.emit(serde_json::to_vec(&value).map_err(io::Error::other)?)
        .await
        .map_err(io::Error::other)
}
/// Forward a job to an agent caller, whose resume cursor is `from_seq`.
async fn pump(ctx: &RequestCtx, stream: JobStream, from_seq: u64) -> io::Result<()> {
    crate::agent_route::pump(ctx, stream, from_seq).await
}

/// `pump` for a direct caller, writing each direct job's acceptance and
/// terminal outcome to ck-motor's log with `caller=direct`.
async fn pump_direct(ctx: &RequestCtx, stream: JobStream) -> io::Result<()> {
    pump_direct_secret(ctx, stream, None).await
}

async fn pump_direct_secret(
    ctx: &RequestCtx,
    stream: JobStream,
    mut secret: Option<crate::direct_secret::DirectSecret>,
) -> io::Result<()> {
    let id = stream.job_id;
    let storage = stream.storage();
    let mut terminal = false;
    let mut return_context_sent = false;
    let result = forward(ctx.cancelled(), stream, |record| {
        match &record {
            StreamRecord::Accepted(_) => {
                tracing::info!(caller = "direct", job_id = %id, "direct job accepted")
            }
            StreamRecord::Terminal(t) => {
                terminal = true;
                tracing::info!(caller = "direct", job_id = %id, outcome = ?t.outcome,
                    killed = ?t.killed, "direct job terminal");
            }
            _ => {}
        }
        // Add the job's secret only to the live submission reply, after the
        // job records have left persistence. Replays must never disclose it
        // to another caller, including an agent that can attach to any job.
        let direct_job = secret
            .take()
            .map(|secret| serde_json::json!({"job_id": id, "secret": secret.expose()}));
        let hosted = storage.job(id).and_then(|j| j.hosted_route());
        let return_result = storage.job(id).and_then(|j| j.return_result);
        let return_paths = if return_context_sent {
            None
        } else {
            storage.job(id).and_then(|j| j.return_requested)
        };
        return_context_sent = true;
        let return_workspace = return_paths
            .as_ref()
            .and_then(|_| storage.job(id)?.workspace_key);
        async move {
            let mut value = serde_json::to_value(record).map_err(io::Error::other)?;
            if let Some(direct_job) = direct_job {
                value["direct_job"] = direct_job;
            }
            if let Some(hosted) = hosted {
                value["hosted"] = serde_json::to_value(hosted).map_err(io::Error::other)?;
            }
            if let Some(result) = return_result {
                value["return_result"] = serde_json::to_value(result).map_err(io::Error::other)?;
            }
            if let Some(paths) = return_paths {
                value["return_paths"] = serde_json::to_value(paths).map_err(io::Error::other)?;
            }
            if let Some(workspace) = return_workspace {
                value["return_workspace"] =
                    serde_json::to_value(workspace).map_err(io::Error::other)?;
            }
            reply(ctx, value).await
        }
    })
    .await;
    if !terminal {
        tracing::info!(caller = "direct", job_id = %id,
            "direct caller left before the terminal; the job runs until it ends or reaches its timeout");
    }
    result
}

/// Forward a job's records to its caller until the terminal record, or until the
/// request is cancelled. A cancelled request is a detach, never a job cancel:
/// the request is cancelled when the caller drops its stream (AFT does this on
/// Esc, a tool timeout or its own restart, then reattaches with exec.attach) and
/// when ck-motor itself shuts down. Only exec.cancel stops a job.
pub(crate) async fn forward<F, E>(
    cancelled: impl std::future::Future<Output = ()>,
    mut stream: JobStream,
    mut emit: E,
) -> io::Result<()>
where
    E: FnMut(StreamRecord) -> F,
    F: std::future::Future<Output = io::Result<()>>,
{
    tokio::pin!(cancelled);
    loop {
        tokio::select! {
            _ = &mut cancelled => {
                eprintln!("ck-motor: caller_detached job_id={} job keeps running; attach to resume", stream.job_id);
                return Ok(());
            },
            record = stream.records.recv() => match record {
                Some(record) => {
                    let record = record?;
                    let terminal = matches!(record, StreamRecord::Terminal(_));
                    emit(record).await?;
                    if terminal {
                        stream.acknowledge_terminal()?;
                        return Ok(());
                    }
                },
                None => return Err(io::Error::other("stream ended without terminal record")),
            }
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[test]
    fn run_permits_enforce_capacity_and_release_on_drop() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut permits: Vec<_> = (0..RUN_ADMISSION_LIMIT)
            .map(|_| RunPermit::acquire(&count).unwrap())
            .collect();
        assert!(RunPermit::acquire(&count).is_none());
        permits.pop();
        let replacement = RunPermit::acquire(&count).expect("a finished stream releases admission");
        drop(replacement);
        drop(permits);
        assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    struct CountingConnector(Arc<AtomicUsize>);
    impl crate::Connector for CountingConnector {
        fn connect(
            &self,
            _: Uuid,
        ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn crate::Duplex>>> + Send + '_>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(io::Error::other("offline")) })
        }
    }

    #[tokio::test]
    async fn health_check_reads_only_cache_without_runner_calls() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-health-cache");
        let calls = Arc::new(AtomicUsize::new(0));
        let mut motor = Motor::open(Config::default(), &scratch).unwrap();
        motor.set_connector(Arc::new(CountingConnector(calls.clone())));
        let handler = Handler::new(Config::default()).with_motor(motor);
        let sample: RunnerHealth = serde_json::from_value(json!({
            "runner_version": "cached", "since_ms": 0, "window_minutes": 60,
            "free_bytes": 100, "queue_depth": 2, "running": [],
            "counts": {"accepted": 0, "refusals": {}, "evictions": 0, "eviction_bytes": 0,
                "eviction_failures": 0, "soft_goal_unmet_passes": 0, "skipped_recent": 0},
            "permits_held": 4, "permits_cap": 8,
            "queue_wait_1h": {"p50_ms": 1000, "p90_ms": 90000, "refusals": 1}
        }))
        .unwrap();
        handler
            .health_cache
            .lock()
            .unwrap()
            .update(Some(sample), std::time::Instant::now());
        let report = tokio::time::timeout(Duration::from_millis(100), handler.health())
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "health.check must never connect to the runner"
        );
        assert_eq!(report.status, HealthStatus::Ok);
        assert_eq!(report.metrics.unwrap()["build_server"]["permits_held"], 4);
        assert!(report.detail.unwrap().starts_with("build server busy:"));
        handler.start_health_sampler();
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handler.shutdown().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the background sampler owns runner calls"
        );
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use std::{
        io::BufRead,
        process::{Command, Stdio},
        time::Duration,
    };

    #[test]
    fn module_data_directory_uses_the_runtime_module_id() {
        let data_home = PathBuf::from("/test/data");
        assert_eq!(
            module_data_dir_for("ck-motor", Some(data_home.clone()), None).unwrap(),
            data_home.join("cortexkit/ck-motor")
        );
        assert_eq!(
            module_data_dir_for("motor", Some(data_home.clone()), None).unwrap(),
            data_home.join("cortexkit/motor")
        );
    }

    #[test]
    fn storage_fallback_uses_the_runtime_module_data_directory() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-storage-fallback");
        let data_home = scratch.join("xdg-data");
        let ack = ModuleHelloAckBody {
            negotiated_ver: subc_protocol::PROTOCOL_VERSION,
            subc_ops: Vec::new(),
            subc_capabilities: Vec::new(),
            storage: None,
            machine_id: None,
        };

        for module_id in ["ck-motor", "motor"] {
            let handler = Handler::new(Config::default()).with_module_id(module_id);
            let storage_root = handler
                .storage_root_with_data_dir(&ack, |id| {
                    module_data_dir_for(id, Some(data_home.clone()), None)
                })
                .unwrap();
            assert_eq!(
                storage_root,
                data_home
                    .join("cortexkit")
                    .join(module_id)
                    .join(&handler.config.runner_instance),
                "module id: {module_id}"
            );
        }
    }

    #[test]
    fn lease_holder_process() {
        let Some(root) = std::env::var_os("MOTOR_TEST_LEASE_ROOT") else {
            return;
        };
        let _storage = crate::storage::Storage::open(std::path::Path::new(&root)).unwrap();
        println!("LEASE HELD");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
    }

    #[tokio::test]
    async fn startup_retries_a_live_process_lease_until_released() {
        let temp = cortexkit_test_support::ScratchDir::new("motor-startup-lease");
        let path = temp.join("module.sqlite");
        let root = PathBuf::from(format!("{}.ck-motor", path.display()));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "module::startup_tests::lease_holder_process",
                "--nocapture",
            ])
            .env("MOTOR_TEST_LEASE_ROOT", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert!(output.read_line(&mut line).unwrap() > 0);
            if line.contains("LEASE HELD") {
                break;
            }
            line.clear();
        }
        let handler = Handler::new(Config::default());
        let descriptor = StorageDescriptor {
            module_id: "ck-motor".into(),
            storage_namespace: "ck-motor".into(),
            isolation: cortexkit_store_types::Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: path.to_string_lossy().into_owned(),
            },
        };
        let ack = ModuleHelloAckBody {
            negotiated_ver: subc_protocol::PROTOCOL_VERSION,
            subc_ops: Vec::new(),
            subc_capabilities: Vec::new(),
            storage: Some(serde_json::to_value(descriptor).unwrap()),
            machine_id: None,
        };
        handler.on_hello_ack(&ack).await;
        // A busy lease is retried in the background, so the error becomes
        // visible only after the first attempt has run.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !handler
                .health()
                .await
                .detail
                .is_some_and(|detail| detail.contains("storage_lease_busy"))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the first attempt reports the busy lease");
        assert!(handler.initialized().is_err());
        assert!(matches!(
            handler.health().await.status,
            HealthStatus::Failing
        ));
        // A request arriving now waits for the retry instead of failing.
        let waiting = tokio::spawn({
            let handler = handler.clone();
            async move { handler.ready().await.map(|_| ()).map_err(|e| e.to_string()) }
        });
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("the waiting request is answered once the lease is free")
            .unwrap()
            .expect("startup recovers without another hello acknowledgement");
        handler.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_descriptor_leaves_startup_unavailable_without_panicking() {
        let handler = Handler::new(Config::default());
        let ack = ModuleHelloAckBody {
            negotiated_ver: subc_protocol::PROTOCOL_VERSION,
            subc_ops: Vec::new(),
            subc_capabilities: Vec::new(),
            storage: Some(json!({"backend": "invalid"})),
            machine_id: None,
        };
        handler.on_hello_ack(&ack).await;
        assert!(handler
            .initialized()
            .err()
            .unwrap()
            .to_string()
            .contains("invalid_storage_descriptor"));
        assert!(matches!(
            handler.health().await.status,
            HealthStatus::Failing
        ));
        assert!(handler.startup_task.lock().unwrap().is_none());
        // Startup has failed for good, so a request is refused at once rather
        // than after the storage deadline.
        let refusal = tokio::time::timeout(Duration::from_secs(1), handler.ready())
            .await
            .expect("a failed startup refuses without waiting")
            .err()
            .unwrap();
        assert_eq!(refusal.kind(), io::ErrorKind::NotConnected);
        assert!(refusal.to_string().contains("invalid_storage_descriptor"));
    }
}
