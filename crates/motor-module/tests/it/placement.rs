use crate::{module::Handler, Config, Connector, SshConnector};
use base64::Engine;
use cortexkit_claustrum_client::{
    ClaustrumCredentialResolver, CredentialErrorClass, CredentialGetTarget, CredentialPublicKey,
    CredentialResolver, CredentialResolverError, CredentialResolverFuture, ResolvedCredential,
};
use cortexkit_test_support::ScratchDir;
use ed25519_dalek::{Signer, Verifier};
use motor_protocol::{Outcome, Ran, RunRequest, StreamRecord, Uuid};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use subc_client_rs::{HealthReport, HealthStatus, ModuleHandler};
use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UnixStream},
    task::JoinHandle,
};

struct CredentialFailure(CredentialErrorClass);
impl CredentialResolver for CredentialFailure {
    fn resolve<'a>(
        &'a self,
        _: &'a str,
        _: Option<u64>,
        _: bool,
    ) -> CredentialResolverFuture<'a, Result<ResolvedCredential, CredentialResolverError>> {
        panic!("motor must never use a raw credential handle")
    }
    fn get_scoped<'a>(
        &'a self,
        id: &'a str,
    ) -> CredentialResolverFuture<'a, Result<ResolvedCredential, CredentialResolverError>> {
        panic!("motor must never read private material: {id}")
    }
    fn public_key<'a>(
        &'a self,
        id: &'a str,
    ) -> CredentialResolverFuture<'a, Result<CredentialPublicKey, CredentialResolverError>> {
        assert_eq!(id, "signing:ck-motor-ssh:1");
        Box::pin(async move { Err(CredentialResolverError::new(self.0, "test refusal")) })
    }
}

struct RecoveringResolver {
    resolver: Arc<dyn CredentialResolver>,
    attempts: AtomicUsize,
    failures: usize,
    class: CredentialErrorClass,
}
impl RecoveringResolver {
    fn new(failures: usize, class: CredentialErrorClass) -> Arc<Self> {
        Arc::new(Self {
            resolver: FakeVault::new().resolver(),
            attempts: AtomicUsize::new(0),
            failures,
            class,
        })
    }
}
impl CredentialResolver for RecoveringResolver {
    fn resolve<'a>(
        &'a self,
        _: &'a str,
        _: Option<u64>,
        _: bool,
    ) -> CredentialResolverFuture<'a, Result<ResolvedCredential, CredentialResolverError>> {
        panic!("motor must never use a raw credential handle")
    }
    fn get_scoped<'a>(
        &'a self,
        _: &'a str,
    ) -> CredentialResolverFuture<'a, Result<ResolvedCredential, CredentialResolverError>> {
        panic!("motor must never read private material")
    }
    fn public_key<'a>(
        &'a self,
        id: &'a str,
    ) -> CredentialResolverFuture<'a, Result<CredentialPublicKey, CredentialResolverError>> {
        Box::pin(async move {
            if self.attempts.fetch_add(1, Ordering::SeqCst) < self.failures {
                Err(CredentialResolverError::new(self.class, "boot refusal"))
            } else {
                self.resolver.public_key(id).await
            }
        })
    }
}

struct Rig {
    handler: Handler,
    daemon: TcpStream,
    serving: JoinHandle<Result<(), subc_client_rs::SubcModuleError>>,
    scratch: SocketScratch,
    root: std::path::PathBuf,
}

// The process-owned scratch path includes a long kernel start-time stamp. A
// short symlink inside the same shared scratch root keeps macOS Unix sockets
// below SUN_LEN without bypassing the shared-root cleanup contract.
pub(crate) struct SocketScratch {
    _owner: ScratchDir,
    path: std::path::PathBuf,
}
impl SocketScratch {
    pub fn new(prefix: &str) -> Self {
        let owner = ScratchDir::new(prefix);
        let path = loop {
            let nonce = Uuid::now_v7().simple().to_string();
            let path = owner
                .path()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!("m{}", &nonce[28..]));
            match std::os::unix::fs::symlink(owner.path(), &path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create short scratch alias: {error}"),
            }
        };
        Self {
            _owner: owner,
            path,
        }
    }
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}
impl std::ops::Deref for SocketScratch {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        self.path()
    }
}
impl Drop for SocketScratch {
    fn drop(&mut self) {
        fs::remove_file(&self.path).unwrap();
    }
}

pub(crate) struct FakeVault {
    pub key: ed25519_dalek::SigningKey,
    pub signs: AtomicUsize,
    pub refused: AtomicBool,
    pub invalid_public: AtomicBool,
}
impl FakeVault {
    pub fn new() -> Arc<Self> {
        use rand::RngCore;
        let mut seed = [0; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        Arc::new(Self {
            key: ed25519_dalek::SigningKey::from_bytes(&seed),
            signs: AtomicUsize::new(0),
            refused: AtomicBool::new(false),
            invalid_public: AtomicBool::new(false),
        })
    }
    fn key_id(&self) -> String {
        hex(&Sha256::digest(self.key.verifying_key().as_bytes())[..8])
    }
    pub fn resolver(self: &Arc<Self>) -> Arc<dyn CredentialResolver> {
        Arc::new(ClaustrumCredentialResolver::with_target(self.clone(), 2))
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
impl CredentialGetTarget for FakeVault {
    fn credential_get<'a>(
        &'a self,
        _: &'a str,
        _: Option<u64>,
        _: bool,
    ) -> CredentialResolverFuture<'a, Result<Value, CredentialResolverError>> {
        panic!("motor must never fetch private credential material")
    }
    fn credential_get_scoped<'a>(
        &'a self,
        _: &'a str,
    ) -> CredentialResolverFuture<'a, Result<Value, CredentialResolverError>> {
        panic!("motor must never fetch private credential material")
    }
    fn credential_public_key<'a>(
        &'a self,
        id: &'a str,
    ) -> CredentialResolverFuture<'a, Result<Value, CredentialResolverError>> {
        assert_eq!(id, "signing:ck-motor-ssh:1");
        Box::pin(async move {
            Ok(json!({"result":{
                "public_key_hex":if self.invalid_public.load(Ordering::SeqCst) { "invalid".into() } else { hex(self.key.verifying_key().as_bytes()) },
                "algorithm":"ed25519", "key_id":self.key_id()
            }}))
        })
    }
    fn credential_sign<'a>(
        &'a self,
        id: &'a str,
        data: &'a [u8],
    ) -> CredentialResolverFuture<'a, Result<Value, CredentialResolverError>> {
        assert_eq!(id, "signing:ck-motor-ssh:1");
        Box::pin(async move {
            self.signs.fetch_add(1, Ordering::SeqCst);
            if self.refused.load(Ordering::SeqCst) {
                return Ok(
                    json!({"result":{"error":{"code":"auth_required", "message":"private vault detail"}}}),
                );
            }
            Ok(json!({"result":{
                "signature_b64":base64::engine::general_purpose::STANDARD.encode(self.key.sign(data).to_bytes()),
                "key_id":self.key_id()
            }}))
        })
    }
}
impl Rig {
    async fn new(class: CredentialErrorClass) -> Self {
        Self::with_resolver(Arc::new(CredentialFailure(class))).await
    }
    async fn with_resolver(resolver: Arc<dyn CredentialResolver>) -> Self {
        Self::with_module_id(resolver, "ck-motor").await
    }

    async fn with_module_id(resolver: Arc<dyn CredentialResolver>, module_id: &str) -> Self {
        let handler = Handler::new(Config::default())
            .with_module_id(module_id)
            .with_credentials(resolver);
        let rig = Self::start(handler, module_id).await;
        // Tests built on this rig read storage-backed health straight away,
        // so wait until the background storage open has finished.
        tokio::time::timeout(Duration::from_secs(5), rig.handler.storage_opened())
            .await
            .expect("module storage opens")
            .unwrap();
        rig
    }
    /// Serves `handler` against a fake daemon and sends it HELLO_ACK, without
    /// waiting for anything the handler does after that.
    async fn start(handler: Handler, module_id: &str) -> Self {
        let scratch = SocketScratch::new("mp");
        let path = scratch.join("connection.json");
        let root = scratch.join("s.ck-motor");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = subc_transport::ConnectionInfo {
            schema: subc_transport::SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![subc_transport::Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: subc_transport::generate_key().unwrap(),
            daemon_id: subc_transport::generate_daemon_id().unwrap(),
            pid: std::process::id(),
            daemon_ver: "motor-test".into(),
        };
        // The shared test root may be group-writable, which write_atomic correctly
        // refuses because it can contain another user's replacement. This fixture
        // only needs an owner-only file for its reader; it does not test publication.
        let bytes = serde_json::to_vec_pretty(&connection).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(&bytes))
            .unwrap();
        let h = handler.clone();
        let module_id = module_id.to_owned();
        let manifest_module_id = module_id.clone();
        // The SDK starts reading frames only once the boot half, which runs
        // `on_hello_ack`, has returned. Running both halves in one task lets a
        // test send frames before that and see whether they are answered.
        let serving = tokio::spawn(async move {
            let (_, serving) = subc_client_rs::serve_with_handle(
                &path,
                crate::module::manifest(&manifest_module_id),
                h,
            )
            .await?;
            serving.await
        });
        let (mut daemon, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut daemon,
            &connection.key,
            &connection.daemon_id,
            &connection.daemon_ver,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let hello = read_frame(&mut daemon).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        let descriptor = cortexkit_store_types::StorageDescriptor {
            module_id: module_id.clone(),
            storage_namespace: module_id,
            isolation: cortexkit_store_types::Isolation::Module,
            backend: cortexkit_store_types::StorageBackend::Sqlite {
                path: scratch.join("s").to_string_lossy().into_owned(),
            },
        };
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: Vec::new(),
            subc_capabilities: Vec::new(),
            storage: Some(serde_json::to_value(descriptor).unwrap()),
            machine_id: None,
        };
        send(
            &mut daemon,
            FrameType::HelloAck,
            0,
            1,
            serde_json::to_value(ack).unwrap(),
        )
        .await;
        Self {
            handler,
            daemon,
            serving,
            scratch,
            root,
        }
    }
    async fn bind(&mut self, principal: Value, scope: Value) -> Frame {
        self.bind_target(
            principal,
            scope,
            json!({"kind":"management_surface", "module_id":"ck-motor"}),
        )
        .await
    }
    async fn bind_target(&mut self, principal: Value, scope: Value, target: Value) -> Frame {
        send(
            &mut self.daemon,
            FrameType::Request,
            0,
            2,
            json!({
                "op":"route.bind", "route_channel":7, "epoch":1,
                "target":target,
                // A self-asserted harness name must never authorize a direct caller.
                "identity":{"project_root":"/repo", "harness":"aft", "session":"session-42"},
                "principal":principal, "scope":scope
            }),
        )
        .await;
        receive(&mut self.daemon).await
    }
    async fn run(&mut self, preset: &str) -> Value {
        let request = RunRequest::new(
            "/not-a-worktree",
            "/not-a-repository",
            "/not-a-cwd",
            "echo forbidden",
        );
        send(
            &mut self.daemon,
            FrameType::Request,
            7,
            3,
            json!({"op":"exec.run", "params":request, "preset":preset}),
        )
        .await;
        let record = receive(&mut self.daemon).await;
        assert_eq!(record.header.ty, FrameType::StreamData);
        assert_eq!(
            receive(&mut self.daemon).await.header.ty,
            FrameType::StreamEnd
        );
        serde_json::from_slice(&record.body).unwrap()
    }
    async fn request(&mut self, op: &str, params: Value) -> Frame {
        send(
            &mut self.daemon,
            FrameType::Request,
            7,
            3,
            json!({"op":op, "params":params}),
        )
        .await;
        receive(&mut self.daemon).await
    }
    async fn stop(mut self) {
        send(&mut self.daemon, FrameType::Goodbye, 0, 0, Value::Null).await;
        tokio::time::timeout(Duration::from_secs(5), &mut self.serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.serving.abort();
    }
}
async fn send(daemon: &mut TcpStream, ty: FrameType, channel: u16, corr: u64, body: Value) {
    let frame = Frame::build(
        ty,
        Flags::new(false, Priority::Passive, false),
        channel,
        if channel == 0 { 0 } else { 1 },
        corr,
        if ty == FrameType::Goodbye {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        },
    )
    .unwrap();
    write_frame(daemon, &frame).await.unwrap();
    daemon.flush().await.unwrap();
}
async fn receive(daemon: &mut TcpStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(5), read_frame(daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
fn aft() -> Value {
    json!({"kind":"reserved", "module_id":"aft"})
}
fn core() -> Value {
    json!({"kind":"reserved", "module_id":"prefrontal-core"})
}

#[tokio::test]
async fn aft_bind_is_admitted() {
    let mut rig = Rig::new(CredentialErrorClass::CredentialAbsent).await;
    assert_eq!(
        rig.bind(aft(), Value::Null).await.header.ty,
        FrameType::Response
    );
    rig.stop().await;
}
#[tokio::test]
async fn direct_bind_is_refused_by_name_even_when_harness_claims_aft() {
    let mut rig = Rig::new(CredentialErrorClass::CredentialAbsent).await;
    let reply = rig.bind(json!({"kind":"direct"}), Value::Null).await;
    assert_eq!(reply.header.ty, FrameType::Error);
    let error: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(error["code"], "motor_direct_refused");
    assert!(error["message"].as_str().unwrap().contains("direct"));
    rig.stop().await;
}
#[tokio::test]
async fn unverified_and_absent_principals_are_refused() {
    for principal in [json!({"kind":"unverified"}), Value::Null] {
        let mut rig = Rig::new(CredentialErrorClass::CredentialAbsent).await;
        let reply = rig.bind(principal, Value::Null).await;
        assert_eq!(reply.header.ty, FrameType::Error);
        let error: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(error["code"], "motor_unverified_refused");
        rig.stop().await;
    }
}
#[tokio::test]
async fn core_bind_and_workspace_prepare_are_admitted() {
    let mut rig = Rig::new(CredentialErrorClass::CredentialAbsent).await;
    assert_eq!(
        rig.bind(core(), Value::Null).await.header.ty,
        FrameType::Response
    );
    let reply = rig
        .request(
            "workspace.prepare",
            json!({"workspace_key":"/not-a-worktree", "repository_root":"/not-a-repository", "base_commit":"base", "siblings":[]}),
        )
        .await;
    assert_eq!(reply.header.ty, FrameType::Response);
    // A typed workspace validation refusal proves dispatch reached prepare,
    // without requiring a real remote runner for the admission contract.
    let reply: motor_protocol::PrepareReply = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(
        reply.outcome,
        motor_protocol::PrepareOutcome::RefusedBeforeStart {
            reason: motor_protocol::RefusalReason::WorkspaceKeyRejected
        }
    );
    rig.stop().await;
}
#[tokio::test]
async fn core_is_refused_exec_run() {
    let mut rig = Rig::with_module_id(
        Arc::new(CredentialFailure(CredentialErrorClass::CredentialAbsent)),
        "motor",
    )
    .await;
    assert_eq!(
        rig.bind(core(), Value::Null).await.header.ty,
        FrameType::Response
    );
    let request = RunRequest::new(
        "/not-a-worktree",
        "/not-a-repository",
        "/not-a-cwd",
        "echo forbidden",
    );
    let reply = rig
        .request("exec.run", serde_json::to_value(request).unwrap())
        .await;
    assert_op_refused(&reply, "motor", "prefrontal-core", "exec.run");
    assert!(
        !rig.root.join("state.json").exists(),
        "authorization must precede job creation"
    );
    rig.stop().await;
}
#[tokio::test]
async fn aft_is_refused_workspace_prepare() {
    let mut rig = Rig::with_module_id(
        Arc::new(CredentialFailure(CredentialErrorClass::CredentialAbsent)),
        "motor",
    )
    .await;
    assert_eq!(
        rig.bind(aft(), Value::Null).await.header.ty,
        FrameType::Response
    );
    let reply = rig.request("workspace.prepare", json!({"workspace_key":"/not-a-worktree", "repository_root":"/not-a-repository", "base_commit":"base", "siblings":[]})).await;
    assert_op_refused(&reply, "motor", "aft", "workspace.prepare");
    rig.stop().await;
}

#[tokio::test]
async fn exec_status_empty_params_preserves_aggregate_reply_bytes() {
    let mut rig = Rig::new(CredentialErrorClass::CredentialAbsent).await;
    assert_eq!(
        rig.bind(aft(), Value::Null).await.header.ty,
        FrameType::Response
    );
    let reply = rig.request("exec.status", json!({})).await;
    assert_eq!(reply.header.ty, FrameType::Response);
    assert_eq!(reply.body, br#"{"queue_depth":0,"running_jobs":[],"server_reachable":false,"repositories":[],"rustc_version":""}"#);
    let disk = rig.request("exec.status", json!({"disk":true})).await;
    assert_eq!(disk.header.ty, FrameType::Response);
    // Older/unreachable runners have no disk observation. The opt-in request
    // still succeeds, and never changes the legacy empty-params serialization.
    assert_eq!(disk.body, reply.body);
    rig.stop().await;
}
fn assert_op_refused(reply: &Frame, self_module_id: &str, principal: &str, op: &str) {
    assert_eq!(reply.header.ty, FrameType::Error);
    let error: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(error["code"], "motor_op_not_permitted_for_principal");
    let message = error["message"].as_str().unwrap();
    assert!(message.starts_with(&format!("{self_module_id} refuses {op}")));
    assert!(message.contains(&format!("reserved:{principal}")));
    assert!(message.contains(op));
}
/// The daemon's stamp on a route that Basal opens for a flow.
fn flow_scope(flow_id: &str, owner: &str, owner_authorized: bool) -> Value {
    json!({"owner":{"kind":"reserved", "module_id":owner}, "ref":"flow-scope-1", "scope_epoch":1, "kind":"head", "attributes":{"flow_id":flow_id}, "owner_authorized":owner_authorized})
}
fn basal() -> Value {
    json!({"kind":"reserved", "module_id":"basal"})
}

#[tokio::test]
async fn remaining_cross_principal_and_unknown_operations_are_refused_before_params() {
    for (principal, scope, ops) in [
        (
            "prefrontal-core",
            Value::Null,
            vec!["exec.attach", "exec.cancel", "exec.status", "unknown"],
        ),
        ("aft", Value::Null, vec!["workspace.drop", "unknown"]),
        (
            "basal",
            flow_scope("motor-build-server-health", "prefrontal-core", true),
            vec![
                "exec.run",
                "exec.attach",
                "exec.cancel",
                "exec.status",
                "workspace.prepare",
                "workspace.drop",
                "unknown",
            ],
        ),
    ] {
        let mut rig = Rig::with_module_id(
            Arc::new(CredentialFailure(CredentialErrorClass::CredentialAbsent)),
            "motor",
        )
        .await;
        assert_eq!(
            rig.bind(json!({"kind":"reserved", "module_id":principal}), scope)
                .await
                .header
                .ty,
            FrameType::Response
        );
        for op in ops {
            let reply = rig.request(op, Value::Null).await;
            assert_op_refused(&reply, "motor", principal, op);
        }
        rig.stop().await;
    }
}
#[tokio::test]
async fn health_flow_reaches_only_the_read_only_health_report() {
    let mut rig = Rig::new(CredentialErrorClass::CredentialAbsent).await;
    assert_eq!(
        rig.bind(
            basal(),
            flow_scope("motor-build-server-health", "prefrontal-core", true)
        )
        .await
        .header
        .ty,
        FrameType::Response
    );
    // No runner is reachable in this rig, so the handler itself answers with
    // its structured unavailability error rather than a permission refusal.
    let reply = rig.request("runner.health", json!({})).await;
    assert_eq!(reply.header.ty, FrameType::Error);
    let error: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(error["code"], "runner_health_unavailable");
    rig.stop().await;
}
fn absent_credentials() -> Arc<dyn CredentialResolver> {
    Arc::new(CredentialFailure(CredentialErrorClass::CredentialAbsent))
}
/// After a restart many callers reattach at once. The SDK reads no frame, route
/// binds included, until `on_hello_ack` returns, so that hook must not wait for
/// the storage open: binds are answered while storage is still opening, and a
/// request that needs storage waits for it.
#[tokio::test]
async fn binds_are_answered_while_storage_opens_and_requests_wait_for_it() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let handler = Handler::new(Config::default())
        .with_credentials(absent_credentials())
        .with_storage_gate(Arc::clone(&gate));
    let mut rig = Rig::start(handler, "ck-motor").await;
    assert_eq!(
        rig.bind(aft(), Value::Null).await.header.ty,
        FrameType::Response
    );
    assert!(
        rig.handler.initialized().is_err(),
        "the bind must be answered before storage has opened"
    );
    send(
        &mut rig.daemon,
        FrameType::Request,
        7,
        3,
        json!({"op":"exec.status", "params":{}}),
    )
    .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), read_frame(&mut rig.daemon))
            .await
            .is_err(),
        "a request that needs storage must wait while it opens"
    );
    gate.add_permits(1);
    let reply = receive(&mut rig.daemon).await;
    assert_eq!(reply.header.ty, FrameType::Response);
    assert_eq!(reply.body, br#"{"queue_depth":0,"running_jobs":[],"server_reachable":false,"repositories":[],"rustc_version":""}"#);
    rig.stop().await;
}
/// A request is refused with the typed, retryable `motor_unavailable` once it
/// has waited the storage deadline, and this holds for the Basal health flow's
/// `runner.health` too.
#[tokio::test]
async fn a_request_is_refused_as_unavailable_when_storage_misses_the_deadline() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let deadline = Duration::from_millis(200);
    let handler = Handler::new(Config::default())
        .with_credentials(absent_credentials())
        .with_storage_gate(Arc::clone(&gate))
        .with_storage_ready_deadline(deadline);
    let mut rig = Rig::start(handler, "ck-motor").await;
    assert_eq!(
        rig.bind(
            basal(),
            flow_scope("motor-build-server-health", "prefrontal-core", true)
        )
        .await
        .header
        .ty,
        FrameType::Response
    );
    let started = std::time::Instant::now();
    let reply = rig.request("runner.health", json!({})).await;
    assert!(started.elapsed() >= deadline, "refused before the deadline");
    assert_eq!(reply.header.ty, FrameType::Error);
    let error: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(error["code"], "motor_unavailable");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("ck-motor unavailable: storage still opening after 200 ms"),
        "{error}"
    );
    rig.stop().await;
}
#[tokio::test]
async fn basal_routes_other_than_the_health_flow_are_refused_at_bind() {
    // Basal itself, another flow, a stamp core wasn't authorized to set, and a
    // stamp owned by another module all carry the basal principal.
    for scope in [
        Value::Null,
        flow_scope("some-other-flow", "prefrontal-core", true),
        flow_scope("motor-build-server-health", "prefrontal-core", false),
        flow_scope("motor-build-server-health", "basal", true),
    ] {
        let mut rig = Rig::with_module_id(
            Arc::new(CredentialFailure(CredentialErrorClass::CredentialAbsent)),
            "motor",
        )
        .await;
        let reply = rig.bind(basal(), scope.clone()).await;
        assert_eq!(reply.header.ty, FrameType::Error, "{scope}");
        let error: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(error["code"], "motor_principal_refused", "{scope}");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .starts_with("motor refuses reserved:basal"));
        rig.stop().await;
    }
}
#[tokio::test]
async fn other_reserved_principals_are_refused_at_bind() {
    let mut rig = Rig::with_module_id(
        Arc::new(CredentialFailure(CredentialErrorClass::CredentialAbsent)),
        "motor",
    )
    .await;
    let reply = rig
        .bind(
            json!({"kind":"reserved", "module_id":"claustrum"}),
            Value::Null,
        )
        .await;
    assert_eq!(reply.header.ty, FrameType::Error);
    let error: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(error["code"], "motor_principal_refused");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .starts_with("motor refuses reserved:claustrum"));
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("reserved:claustrum"));
    rig.stop().await;
}

#[tokio::test]
async fn unavailable_twice_recovers_health_without_restart() {
    assert_health_recovers(
        CredentialErrorClass::Unavailable,
        "runner_credential_unavailable",
    )
    .await;
}
#[tokio::test]
async fn missing_enrollment_recovers_health_without_restart() {
    assert_health_recovers(
        CredentialErrorClass::CredentialAbsent,
        "runner_credential_missing",
    )
    .await;
}
#[tokio::test]
async fn refused_grant_recovers_health_without_restart() {
    assert_health_recovers(
        CredentialErrorClass::Unauthorized,
        "runner_credential_grant_refused",
    )
    .await;
}
#[tokio::test]
async fn auth_required_recovers_health_without_restart() {
    assert_health_recovers(
        CredentialErrorClass::AuthRequired,
        "runner_credential_grant_refused",
    )
    .await;
}
/// A failing report leads with its reason, keeps the detail to one line, and
/// carries the unreachable-refusal counters in metrics rather than the detail.
fn assert_failing_detail_and_counters(health: &HealthReport, reason: &str) {
    let detail = health.detail.as_deref().unwrap_or_default();
    assert!(detail.starts_with(&format!("{reason}; ")), "{detail}");
    assert!(
        !detail.contains("unreachable_refusals_since_boot"),
        "{detail}"
    );
    let counters = &health.metrics.as_ref().unwrap()["unreachable_refusals_since_boot"];
    assert!(
        counters.is_object() && !counters.as_object().unwrap().is_empty(),
        "{counters}"
    );
}

async fn assert_health_recovers(class: CredentialErrorClass, reason: &str) {
    let resolver = RecoveringResolver::new(2, class);
    let rig = Rig::with_resolver(resolver.clone()).await;
    let health = rig.handler.health().await;
    assert_eq!(health.status, HealthStatus::Failing);
    assert_failing_detail_and_counters(&health, reason);
    assert_eq!(resolver.attempts.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(6), async {
        while rig.handler.health().await.status != HealthStatus::Ok {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background credential retries must restore health without a restart or connect");
    assert_eq!(resolver.attempts.load(Ordering::SeqCst), 3);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn credential_backoff_doubles_caps_and_stops_on_shutdown() {
    let scratch = ScratchDir::new("motor-retry-backoff");
    let resolver = RecoveringResolver::new(usize::MAX, CredentialErrorClass::Unavailable);
    let connector =
        SshConnector::initialize(Config::default(), scratch.path(), resolver.clone()).await;
    tokio::task::yield_now().await;
    for (index, seconds) in [1, 2, 4, 8, 16, 30, 30].into_iter().enumerate() {
        tokio::time::advance(Duration::from_millis(seconds * 1000 - 1)).await;
        tokio::task::yield_now().await;
        assert_eq!(resolver.attempts.load(Ordering::SeqCst), index + 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(resolver.attempts.load(Ordering::SeqCst), index + 2);
        assert_eq!(connector.refusal(), Some("runner_credential_unavailable"));
    }
    connector.shutdown().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(resolver.attempts.load(Ordering::SeqCst), 8);
    assert!(connector.connect(Uuid::now_v7()).await.is_err());
    assert_eq!(resolver.attempts.load(Ordering::SeqCst), 8);
    assert_eq!(connector.refusal(), Some("runner_agent_stopped"));
}

#[tokio::test(start_paused = true)]
async fn unresolved_connect_retries_once_immediately() {
    let scratch = ScratchDir::new("motor-retry-connect");
    let resolver = RecoveringResolver::new(usize::MAX, CredentialErrorClass::Unavailable);
    let connector =
        SshConnector::initialize(Config::default(), scratch.path(), resolver.clone()).await;
    assert_eq!(resolver.attempts.load(Ordering::SeqCst), 1);
    assert!(connector.connect(Uuid::now_v7()).await.is_err());
    assert_eq!(resolver.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(connector.refusal(), Some("runner_credential_unavailable"));
    connector.shutdown().await;
}

#[tokio::test]
async fn agent_start_failure_recovers_without_restart() {
    let scratch = SocketScratch::new("mr");
    let root = scratch.join("later");
    let connector =
        SshConnector::initialize(Config::default(), &root, FakeVault::new().resolver()).await;
    assert_eq!(connector.refusal(), Some("runner_agent_start_failed"));
    fs::create_dir(&root).unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        while connector.refusal().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("agent start must recover after its storage becomes available");
    connector.shutdown().await;
    assert_eq!(fs::read_dir(root).unwrap().count(), 0);
}
#[tokio::test]
async fn missing_key_fails_health_and_run_before_snapshot_or_ssh() {
    assert_credential_refusal(
        CredentialErrorClass::CredentialAbsent,
        "runner_credential_missing",
    )
    .await;
}
#[tokio::test]
async fn refused_grant_fails_health_and_run_before_snapshot_or_ssh() {
    assert_credential_refusal(
        CredentialErrorClass::Unauthorized,
        "runner_credential_grant_refused",
    )
    .await;
}
async fn assert_credential_refusal(class: CredentialErrorClass, reason: &str) {
    let mut rig = Rig::new(class).await;
    assert_eq!(
        rig.bind(aft(), Value::Null).await.header.ty,
        FrameType::Response
    );
    let health = rig.handler.health().await;
    assert_eq!(health.status, HealthStatus::Failing);
    assert_failing_detail_and_counters(&health, reason);
    let terminal: StreamRecord = serde_json::from_value(rig.run("any-preset").await).unwrap();
    let StreamRecord::Terminal(terminal) = terminal else {
        panic!("expected terminal refusal")
    };
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: motor_protocol::RefusalReason::RunnerAuthenticationFailed
        }
    );
    assert_eq!(terminal.refusal_detail(), Some(reason));
    assert_eq!(terminal.retry_after_ms(), None);
    assert!(!rig.root.join("spools").exists());
    rig.stop().await;
}
#[tokio::test]
async fn scoped_jobs_persist_session_preset_and_scope_without_preset_policy() {
    let mut rig = Rig::new(CredentialErrorClass::CredentialAbsent).await;
    let scope = json!({"owner":{"kind":"reserved", "module_id":"prefrontal-core"}, "ref":"scope-42", "scope_epoch":9, "kind":"head", "attributes":{}, "owner_authorized":true});
    assert_eq!(rig.bind(aft(), scope).await.header.ty, FrameType::Response);
    for preset in ["mason", "unknown-to-motor"] {
        let terminal = rig.run(preset).await;
        let state: Value =
            serde_json::from_slice(&fs::read(rig.root.join("state.json")).unwrap()).unwrap();
        let id = terminal["job_id"].as_str().unwrap();
        assert_eq!(
            state["jobs"][id]["attribution"],
            json!({"session":"session-42", "preset":preset, "scope_ref":"scope-42", "scope_epoch":9})
        );
    }
    let descriptor_store = rig.scratch.join("s");
    assert!(
        !descriptor_store.exists(),
        "motor uses the descriptor as a JSON sidecar location, not SQLite"
    );
    rig.stop().await;
}
#[test]
fn older_state_loads_without_attribution_and_keeps_cancel_intent() {
    let scratch = ScratchDir::new("motor-old-state");
    let id = Uuid::now_v7();
    fs::write(scratch.join("state.json"), serde_json::to_vec(&json!({"jobs":{id.to_string():{"queue_wait_limit_s":17,"pending_cancel":true,"local_terminal":null}}, "snapshots":{}})).unwrap()).unwrap();
    let store = crate::storage::Storage::open(scratch.path()).unwrap();
    let job = store.job(id).unwrap();
    assert_eq!(job.queue_wait_limit_s, 17);
    assert!(job.pending_cancel);
    assert_eq!(job.attribution, None);
}

#[tokio::test]
async fn vault_agent_has_no_private_material_on_disk_and_shuts_down() {
    let scratch = SocketScratch::new("ma");
    let vault = FakeVault::new();
    let resolver = vault.resolver();
    let public = resolver
        .public_key(crate::RUNNER_CREDENTIAL_ID)
        .await
        .unwrap();
    let agent = crate::agent::VaultAgent::start(scratch.path(), public, resolver)
        .await
        .unwrap();
    let runtime = agent.socket.parent().unwrap().to_path_buf();
    assert_eq!(
        fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let files: Vec<_> = fs::read_dir(&runtime)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    for path in &files {
        if path.is_file() {
            let bytes = fs::read(path).unwrap();
            assert!(!bytes
                .windows(b"PRIVATE KEY".len())
                .any(|s| s == b"PRIVATE KEY"));
            assert!(!bytes.windows(32).any(|s| s == vault.key.to_bytes()));
        }
    }
    assert_eq!(
        files.len(),
        2,
        "only the agent socket and public identity may persist"
    );
    assert_eq!(vault.signs.load(Ordering::SeqCst), 0);
    agent.shutdown().await;
    assert_eq!(agent.refusal(), Some("runner_agent_stopped"));
    assert!(!runtime.exists());
}
#[tokio::test]
async fn test_ssh_override_remains_available_without_claustrum() {
    let scratch = ScratchDir::new("motor-override");
    let path = scratch.join("ssh_config");
    let config = Config {
        runner_instance: "fixture".into(),
        ssh_config_override: Some(path.clone()),
        ..Config::default()
    };
    let connector = SshConnector::initialize(
        config,
        scratch.path(),
        Arc::new(CredentialFailure(CredentialErrorClass::CredentialAbsent)),
    )
    .await;
    assert_eq!(connector.refusal(), None);
    let command = connector.command(Uuid::now_v7());
    let args: Vec<_> = command
        .as_std()
        .get_args()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert_eq!(&args[..2], &["-F", path.to_str().unwrap()]);
    assert!(args.contains(&"IdentitiesOnly=yes".into()));
    assert!(args.contains(&"IdentityAgent=none".into()));
    assert!(!args.iter().any(|s| s.contains("alfonso_runner_ed25519")));
}

#[tokio::test]
async fn vault_public_key_initializes_connector_with_owned_identity_agent_and_master() {
    let scratch = SocketScratch::new("mc");
    let vault = FakeVault::new();
    let connector =
        SshConnector::initialize(Config::default(), scratch.path(), vault.resolver()).await;
    assert_eq!(connector.refusal(), None);
    let command = connector.command(Uuid::now_v7());
    let args: Vec<_> = command
        .as_std()
        .get_args()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert!(args.contains(&"IdentitiesOnly=yes".into()));
    assert!(args.contains(&"ControlMaster=auto".into()));
    assert!(args.contains(&"ControlPersist=no".into()));
    assert_eq!(
        &args[args.iter().position(|a| a == "-F").unwrap() + 1],
        "/dev/null"
    );
    let agent = args
        .iter()
        .find_map(|a| a.strip_prefix("IdentityAgent="))
        .unwrap();
    assert!(std::path::Path::new(agent).starts_with(scratch.path()));
    let control = args
        .iter()
        .find_map(|a| a.strip_prefix("ControlPath="))
        .unwrap();
    assert_eq!(
        std::path::Path::new(control).parent(),
        std::path::Path::new(agent).parent()
    );
    let public = &args[args.iter().position(|a| a == "-i").unwrap() + 1];
    assert!(std::path::Path::new(public).starts_with(scratch.path()));
    assert_eq!(
        ssh_key::PublicKey::from_openssh(&fs::read_to_string(public).unwrap())
            .unwrap()
            .key_data()
            .ed25519()
            .unwrap()
            .as_ref(),
        vault.key.verifying_key().as_bytes()
    );
    assert!(!args
        .iter()
        .any(|a| a.contains("~/.ssh/") || a.contains("alfonso_runner_ed25519")));
    connector.shutdown().await;
    assert_eq!(connector.refusal(), Some("runner_agent_stopped"));
    assert!(!std::path::Path::new(agent).exists());
}

#[tokio::test]
async fn invalid_public_key_refuses_before_start_without_agent_directory() {
    let scratch = ScratchDir::new("mi");
    let vault = FakeVault::new();
    vault.invalid_public.store(true, Ordering::SeqCst);
    let connector =
        SshConnector::initialize(Config::default(), scratch.path(), vault.resolver()).await;
    assert_eq!(connector.refusal(), Some("runner_credential_unavailable"));
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    assert!(connector.connect(Uuid::now_v7()).await.is_err());
}

pub(crate) async fn agent_packet(socket: &std::path::Path, packet: &[u8]) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        stream.write_u32(packet.len() as u32).await.unwrap();
        stream.write_all(packet).await.unwrap();
        let size = stream.read_u32().await.unwrap();
        let mut response = vec![0; size as usize];
        stream.read_exact(&mut response).await.unwrap();
        response
    })
    .await
    .unwrap()
}
fn ssh_string(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}
fn read_string<'a>(bytes: &mut &'a [u8]) -> &'a [u8] {
    let size = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    let value = &bytes[4..4 + size];
    *bytes = &bytes[4 + size..];
    value
}
async fn identity(socket: &std::path::Path) -> Vec<u8> {
    let response = agent_packet(socket, &[11]).await;
    assert_eq!(response[0], 12);
    assert_eq!(u32::from_be_bytes(response[1..5].try_into().unwrap()), 1);
    let mut fields = &response[5..];
    let key = read_string(&mut fields).to_vec();
    assert!(read_string(&mut fields).is_empty());
    assert!(fields.is_empty());
    key
}
fn sign_packet(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut packet = vec![13];
    ssh_string(&mut packet, key);
    ssh_string(&mut packet, data);
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet
}
async fn start_agent(scratch: &SocketScratch, vault: &Arc<FakeVault>) -> crate::agent::VaultAgent {
    let resolver = vault.resolver();
    let public = resolver
        .public_key(crate::RUNNER_CREDENTIAL_ID)
        .await
        .unwrap();
    crate::agent::VaultAgent::start(scratch.path(), public, resolver)
        .await
        .unwrap()
}
#[tokio::test]
async fn agent_identities_match_vault_public_key() {
    let scratch = SocketScratch::new("ai");
    let vault = FakeVault::new();
    let agent = start_agent(&scratch, &vault).await;
    let key = identity(&agent.socket).await;
    let mut fields = key.as_slice();
    assert_eq!(read_string(&mut fields), b"ssh-ed25519");
    assert_eq!(
        read_string(&mut fields),
        vault.key.verifying_key().as_bytes()
    );
    assert!(fields.is_empty());
    let disk =
        ssh_key::PublicKey::from_openssh(&fs::read_to_string(&agent.identity).unwrap()).unwrap();
    assert_eq!(
        disk.key_data().ed25519().unwrap().as_ref(),
        vault.key.verifying_key().as_bytes()
    );
    assert_eq!(vault.signs.load(Ordering::SeqCst), 0);
    agent.shutdown().await;
}
#[tokio::test]
async fn agent_sign_bridge_round_trips_and_verifies_with_vault_public_key() {
    let scratch = SocketScratch::new("as");
    let vault = FakeVault::new();
    let agent = start_agent(&scratch, &vault).await;
    let key = identity(&agent.socket).await;
    let challenge = b"SSH authentication challenge including session binding\0\xff";
    let response = agent_packet(&agent.socket, &sign_packet(&key, challenge)).await;
    assert_eq!(response[0], 14);
    let mut fields = &response[1..];
    let mut signature = read_string(&mut fields);
    assert!(fields.is_empty());
    assert_eq!(read_string(&mut signature), b"ssh-ed25519");
    let signed = ed25519_dalek::Signature::from_slice(read_string(&mut signature)).unwrap();
    assert!(signature.is_empty());
    vault
        .key
        .verifying_key()
        .verify_strict(challenge, &signed)
        .expect("vault-backed SSH signature must verify");
    assert!(vault
        .key
        .verifying_key()
        .verify(b"wrong challenge", &signed)
        .is_err());
    assert_eq!(vault.signs.load(Ordering::SeqCst), 1);
    agent.shutdown().await;
}
#[tokio::test]
async fn refused_sign_returns_agent_failure_typed_run_refusal_and_failing_health() {
    let vault = FakeVault::new();
    vault.refused.store(true, Ordering::SeqCst);
    let mut rig = Rig::with_resolver(vault.resolver()).await;
    assert_eq!(
        rig.bind(aft(), Value::Null).await.header.ty,
        FrameType::Response
    );
    assert_eq!(rig.handler.health().await.status, HealthStatus::Ok);
    let runtime = fs::read_dir(&rig.root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.join("s").exists())
        .unwrap();
    let socket = runtime.join("s");
    let key = identity(&socket).await;
    assert_eq!(
        agent_packet(&socket, &sign_packet(&key, b"challenge")).await,
        [5]
    );
    let health = rig.handler.health().await;
    assert_eq!(health.status, HealthStatus::Failing);
    assert_failing_detail_and_counters(&health, "runner_sign_grant_refused");
    let terminal: StreamRecord = serde_json::from_value(rig.run("any").await).unwrap();
    let StreamRecord::Terminal(terminal) = terminal else {
        panic!("expected refusal")
    };
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: motor_protocol::RefusalReason::RunnerAuthenticationFailed
        }
    );
    assert_eq!(terminal.refusal_detail(), Some("runner_sign_grant_refused"));
    assert_eq!(terminal.retry_after_ms(), None);
    assert!(!rig.root.join("spools").exists());
    assert_eq!(
        agent_packet(&socket, &sign_packet(&key, b"retry")).await,
        [5]
    );
    assert_eq!(
        vault.signs.load(Ordering::SeqCst),
        1,
        "terminal auth refusal must not retry"
    );
    rig.stop().await;
    assert!(!runtime.exists());
}
#[tokio::test]
async fn agent_rejects_foreign_key_flags_and_malformed_requests_without_signing() {
    let scratch = SocketScratch::new("ab");
    let vault = FakeVault::new();
    let agent = start_agent(&scratch, &vault).await;
    let key = identity(&agent.socket).await;
    let mut flags = sign_packet(&key, b"challenge");
    *flags.last_mut().unwrap() = 2;
    for packet in [
        flags,
        sign_packet(b"wrong-key", b"challenge"),
        vec![13],
        vec![17],
        vec![11, 0],
    ] {
        assert_eq!(agent_packet(&agent.socket, &packet).await, [5]);
    }
    assert_eq!(vault.signs.load(Ordering::SeqCst), 0);
    assert_eq!(agent.refusal(), None);
    agent.shutdown().await;
}
