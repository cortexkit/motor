use super::*;
use anyhow::{bail, ensure, Result};
use ed25519_dalek::SigningKey;
use motor_host::helper::Control;
use motor_host::jobs::{Assignment, Resources};
use motor_host::server::{read_frame, serve, unix_ms, write_frame, Runtime};
use motor_host::state::jobs::{Execution, HostResources};
use motor_host::store::SqliteJournal;
use motor_lease::{Jwks, JwksKey, RequesterJwk};
use motor_protocol::{Frame, MirrorRef, TenantEvent};
use motor_runner::{Config, Runner};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn commit(path: &Path, text: &str) -> String {
    std::fs::write(path.join("source"), text).unwrap();
    git(path, &["add", "source"]);
    git(
        path,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            text,
        ],
    );
    git(path, &["rev-parse", "HEAD"])
}
fn hello() -> Value {
    json!({"type":"hello","protocol_version":1,"started":true,"default_branch_tip":true})
}

const WORKSPACE_DISK_BYTES: u64 = 64 * 1024 * 1024;

struct Disk {
    root: PathBuf,
    handle: tokio::runtime::Handle,
    prepare_hold: Mutex<Option<PrepareHold>>,
    released: tokio::sync::Notify,
    workspaces: Mutex<BTreeMap<String, (String, u64)>>,
}
struct PrepareHold {
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Control for Disk {
    fn ensure_workspace(&self, account: &str, disk_id: &str, size_bytes: u64) -> Result<()> {
        motor_vm_helper::job::validate_account(account)?;
        motor_vm_helper::job::validate_id(disk_id)?;
        ensure!(
            size_bytes > 0 && size_bytes <= WORKSPACE_DISK_BYTES && size_bytes.is_multiple_of(512),
            "workspace cap exceeded"
        );
        let mut workspaces = self.workspaces.lock().unwrap();
        let directory = self.root.join("workspace-disks");
        let path = directory.join(disk_id);
        if let Some((owner, cap)) = workspaces.get(disk_id) {
            ensure!(
                owner == account && *cap == size_bytes,
                "workspace identity or size mismatch"
            );
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.len() == size_bytes,
                "workspace disk changed"
            );
            return Ok(());
        }
        // The native runner uses a directory instead of mounting a disk. Keep
        // a real capped sparse file alongside it to exercise boot provisioning.
        std::fs::create_dir_all(&directory)?;
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        file.set_len(size_bytes)?;
        file.sync_all()?;
        std::fs::File::open(directory)?.sync_all()?;
        workspaces.insert(disk_id.into(), (account.into(), size_bytes));
        Ok(())
    }
    fn start_job(&self, _: &motor_vm_helper::job::Start) -> Result<Value> {
        Ok(json!({"running":true,"tap_index":1}))
    }
    fn job_admin(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({"ok":true}))
    }
    fn stop_job(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn job_exits(&self) -> Result<Vec<motor_vm_helper::job::ExitReport>> {
        Ok(vec![])
    }
    fn adopt_job(&self, _: &str) -> Result<bool> {
        Ok(true)
    }
    fn job_relay(&self, _: &str, port: u32) -> Result<UnixStream> {
        if port != 52 {
            bail!("reports are relayed explicitly by the fixture");
        }
        let mut config = Config::new(self.root.join("disk"), "runner");
        config.tenant_vm = true;
        config.per_job = true;
        config.development_allow_host_backend = true;
        config.free_space_floor_bytes = 0;
        config.max_weight = 1;
        config.reserve_daemon_core = false;
        config.workspace_roots = vec![self.root.join("remote/worktrees")];
        config.owned_workspace_roots = vec![self.root.join("remote")];
        let state = config.state_dir();
        // Match a fresh job VM: retain mirrors/catalog/pins, not execution history.
        for path in [state.join("jobs"), state.join("tenant-events.json")] {
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        let runner = Runner::open(config).expect("open real runner fixture");
        let (host, guest) = UnixStream::pair()?;
        guest.set_nonblocking(true)?;
        let _entered = self.handle.enter();
        let guest = tokio::net::UnixStream::from_std(guest)?;
        self.handle.spawn(async move {
            motor_runner::serve_tenant_connection(runner, guest)
                .await
                .unwrap();
        });
        Ok(host)
    }
    fn adopt(&self, _: &str) -> Result<bool> {
        unreachable!()
    }
    fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
        unreachable!()
    }
    fn quota(&self, _: &str, _: u32) -> Result<()> {
        unreachable!()
    }
    fn relay(&self, _: &str, _: u32) -> Result<UnixStream> {
        unreachable!()
    }
    fn admin(&self, _: &str, _: &Value) -> Result<Value> {
        unreachable!()
    }
    fn suspend(&self, _: &str) -> Result<()> {
        unreachable!()
    }
    fn available_bytes(&self) -> Result<u64> {
        let path = std::ffi::CString::new(self.root.as_os_str().as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stat = unsafe { stat.assume_init() };
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
    fn capacity(&self) -> Result<motor_host::capacity::Capacity> {
        Ok(motor_host::capacity::Capacity {
            windows_vm_bytes: 0,
            host_reserve_bytes: 0,
            jobs_min_bytes: 1,
            storage_margin_bytes: 1024 * 1024,
            growth_allowance_bytes: 0,
            tenant_memory_reserve_bytes: 0,
        })
    }
}
impl Resources for Disk {
    fn acquire(&self, execution: &Execution) -> Result<Assignment> {
        Ok(Assignment {
            image_generation_id: "generation".into(),
            hello: hello(),
            guest_init: json!({}),
            spool: self.root.join(format!("{}.spool", execution.claims.jti)),
        })
    }
    fn prepare(&self, _: &Execution) -> Result<()> {
        let hold = self.prepare_hold.lock().unwrap().take();
        if let Some(hold) = hold {
            let _ = hold.entered.send(());
            // Dropping the release channel's sender unblocks recv if a test panics.
            let _ = hold.release.recv();
        }
        Ok(())
    }
    fn release(&self, _: &str) -> Result<()> {
        self.released.notify_one();
        Ok(())
    }
}

struct Fixture {
    root: tempfile::TempDir,
    runtime: Arc<Runtime>,
    disk: Arc<Disk>,
    broker: SigningKey,
    requester: SigningKey,
    repositories: Vec<PathBuf>,
    canonical: Vec<PathBuf>,
    base: Vec<String>,
    tips: Vec<String>,
    next: u64,
    relay_reports: bool,
}
impl Fixture {
    fn new() -> Self {
        let root = cortexkit_test_support::scratch_root();
        std::fs::create_dir_all(&root).unwrap();
        let root = tempfile::tempdir_in(root).unwrap();
        let repositories: Vec<_> = ["main", "sibling"]
            .map(|name| root.path().join("local").join(name))
            .into();
        let canonical = ["main", "sibling"]
            .map(|name| root.path().join("remote/source").join(name))
            .into();
        let mut base = Vec::new();
        let mut tips = Vec::new();
        for path in &repositories {
            std::fs::create_dir_all(path).unwrap();
            git(path, &["init"]);
            base.push(commit(path, "base"));
            tips.push(commit(path, "tip"));
        }
        let disk = Arc::new(Disk {
            root: root.path().into(),
            handle: tokio::runtime::Handle::current(),
            prepare_hold: Mutex::new(None),
            released: tokio::sync::Notify::new(),
            workspaces: Mutex::new(BTreeMap::new()),
        });
        let (journal, snapshot) = SqliteJournal::open(&root.path().join("host")).unwrap();
        let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
        host.settings = host_settings()[1].clone();
        if let motor_host::config::HostSettings::PerJob(settings) = &mut host.settings {
            settings.workspace_disk_cap_bytes = WORKSPACE_DISK_BYTES;
        }
        host.enforce_storage();
        host.record_host_resources(HostResources {
            at_ms: unix_ms(),
            available_mem_mib: 100_000,
            vcpu_ceiling: 4,
            permit_budget: 4,
        });
        let runtime = Runtime::with_job_resources("host".into(), host, disk.clone(), disk.clone());
        let broker = SigningKey::from_bytes(&[7; 32]);
        let requester = SigningKey::from_bytes(&[8; 32]);
        runtime
            .keys
            .write()
            .unwrap()
            .refresh(Ok(Jwks {
                keys: vec![JwksKey::from_key("key", &broker.verifying_key())],
            }))
            .unwrap();
        Self {
            root,
            runtime,
            disk,
            broker,
            requester,
            repositories,
            canonical,
            base,
            tips,
            next: 10,
            relay_reports: true,
        }
    }
    fn claims(&mut self) -> Claims {
        self.next += 1;
        let mut c = claims(&format!("00000000-0000-7000-8000-{:012}", self.next));
        c.iat = unix_ms() / 1000;
        c.exp = c.iat + 300;
        c.cnf.jkt = RequesterJwk::from_key(&self.requester.verifying_key())
            .thumbprint()
            .unwrap();
        c
    }
    fn refs(&self, commits: &[String]) -> Vec<MirrorRef> {
        self.canonical
            .iter()
            .zip(commits)
            .map(|(repository, commit)| MirrorRef {
                repository: Some(repository.to_str().unwrap().into()),
                dependency: None,
                reference: format!("refs/snapshots/{commit}"),
                commit: commit.clone(),
            })
            .collect()
    }
    fn report(&self, job: &str, sequence: u64, imported: bool, refs: Vec<MirrorRef>) {
        let event: TenantEvent = serde_json::from_value(json!({"event_seq":sequence,"event":"mirror_refs","mirror_refs":{"imported":imported,"refs":refs}})).unwrap();
        self.runtime
            .host
            .lock()
            .unwrap()
            .event(job, &event, unix_ms())
            .unwrap();
    }
    fn seed(&mut self, commits: &[String]) {
        let c = self.claims();
        let mut host = self.runtime.host.lock().unwrap();
        let id = host.claim(c.clone(), unix_ms()).unwrap().0;
        host.begin_job_boot(&c.jti).unwrap().unwrap();
        drop(host);
        let mut refs = self.refs(commits);
        refs.push(MirrorRef {
            repository: None,
            dependency: Some("https://example.com/dependency".into()),
            reference: format!("refs/ck-motor/dep/{}", commits[0]),
            commit: commits[0].clone(),
        });
        self.report(&c.jti, 1, true, refs);
        let mut host = self.runtime.host.lock().unwrap();
        host.job_stopped(&c.jti, false).unwrap();
        host.close(id).unwrap();
    }
    async fn connect(
        &self,
        c: &Claims,
    ) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let task = tokio::spawn(serve(self.runtime.clone(), server));
        let challenge =
            super::transport::hello(&mut client, &self.broker, &self.requester, c).await;
        assert_eq!(
            super::transport::respond(&mut client, &self.requester, c, challenge).await["type"],
            "lease_ok"
        );
        write_frame(&mut client, &hello()).await.unwrap();
        next(&mut client, "hello").await;
        (client, task)
    }
    async fn have(&self, client: &mut tokio::io::DuplexStream) -> Value {
        let repositories: Vec<_> = self
            .canonical
            .iter()
            .zip(&self.tips)
            .map(|(path, commit)| json!({"repository":path,"commits":[commit]}))
            .collect();
        write_frame(client, &json!({"type":"have","repositories":repositories,"git_dependencies":["https://example.com/dependency"]})).await.unwrap();
        next(client, "have_reply").await
    }
    async fn reattach_terminal(&self, claims: &Claims) -> Value {
        let mut claims = claims.clone();
        claims.purpose = Purpose::Attach;
        let (mut client, task) = self.connect(&claims).await;
        write_frame(
            &mut client,
            &json!({"type":"attach","job_id":claims.jti,"from_seq":0}),
        )
        .await
        .unwrap();
        let terminal = next(&mut client, "terminal").await;
        next(&mut client, "hosted_terminal").await;
        drop(client);
        task.await.unwrap().unwrap();
        terminal
    }
    fn request(&self, c: &Claims, have: &Value) -> (Value, Vec<Vec<u8>>) {
        let mut bundles = Vec::new();
        let mut bytes = Vec::new();
        for (i, path) in self.repositories.iter().enumerate() {
            let held = have["repositories"][i]["commits"].as_array().unwrap();
            if held.contains(&json!(self.tips[i])) {
                continue;
            }
            let reference = "refs/ck-motor/test/snapshot";
            git(path, &["update-ref", reference, &self.tips[i]]);
            let bundle = self.root.path().join(format!("{i}.bundle"));
            let mut args = vec!["bundle", "create", bundle.to_str().unwrap(), reference];
            let exclusions: Vec<_> = held
                .iter()
                .map(|value| format!("^{}", value.as_str().unwrap()))
                .collect();
            args.extend(exclusions.iter().map(String::as_str));
            git(path, &args);
            git(path, &["update-ref", "-d", reference]);
            let data = std::fs::read(bundle).unwrap();
            bundles.push(json!({"index":bytes.len(),"repository":self.canonical[i],"length":data.len(),"sha256":format!("{:x}",Sha256::digest(&data))}));
            bytes.push(data);
        }
        let workspace = self.root.path().join("remote/worktrees/job");
        let request = json!({"type":"run","job_id":c.jti,"workspace_key":workspace,"repository_root":self.canonical[0],"cwd":workspace,
            "command":"test -f source && printf command-ran", "env":{}, "weight_hint":1,"timeout":30,"queue_wait_limit_s":30,
            "snapshot_commit":self.tips[0],"tree_hash":git(&self.repositories[0], &["rev-parse", &format!("{}^{{tree}}",self.tips[0])]),"integration_tip":self.tips[0],
            "siblings":[{"canonical_path":self.canonical[1],"commit":self.tips[1],"tree_hash":git(&self.repositories[1], &["rev-parse", &format!("{}^{{tree}}",self.tips[1])])}],"bundles":bundles});
        (request, bytes)
    }
}

async fn next(client: &mut tokio::io::DuplexStream, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let frame = read_frame(client).await.unwrap();
            if frame["type"] == kind {
                return frame;
            }
            assert_eq!(frame["type"], "heartbeat", "{frame}");
        }
    })
    .await
    .expect("host reply deadline")
}

impl Fixture {
    fn stop(&self, c: &Claims) {
        let mut host = self.runtime.host.lock().unwrap();
        host.job_stop_intent(&c.jti, false).unwrap();
        host.job_stopped(&c.jti, false).unwrap();
    }
    fn disk_id(&self, job: &str) -> String {
        self.runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .per_job
            .executions[job]
            .disk_id
            .clone()
    }
    fn held(&self, job: &str) -> Vec<MirrorRef> {
        let disk = self.disk_id(job);
        self.runtime.host.lock().unwrap().snapshot.per_job.disks[&disk]
            .have
            .clone()
    }
    fn harvest(&self, job: &str) -> Vec<TenantEvent> {
        // Only the fixture reads guest storage; production receives these events
        // over the lifecycle socket and never opens the workspace disk.
        let path = self.root.path().join("disk/runner/tenant-events.json");
        let state: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let events: Vec<TenantEvent> = serde_json::from_value(state["pending"].clone()).unwrap();
        for event in &events {
            if event.mirror_refs.is_some() {
                self.runtime
                    .host
                    .lock()
                    .unwrap()
                    .event(job, event, unix_ms())
                    .unwrap();
            }
        }
        events
    }
    async fn finish(
        &self,
        c: &Claims,
        mut client: tokio::io::DuplexStream,
        mut task: tokio::task::JoinHandle<Result<()>>,
        request: &Value,
        bytes: &[Vec<u8>],
    ) -> (Value, bool) {
        write_frame(&mut client, request).await.unwrap();
        next(&mut client, "accepted").await;
        for (index, data) in bytes.iter().enumerate() {
            let chunk = Frame::BundleChunk {
                index: index as u32,
                offset: 0,
                data: motor_protocol::BytePayload(data.clone()),
            };
            write_frame(&mut client, &serde_json::to_value(chunk).unwrap())
                .await
                .unwrap();
        }
        let (terminal, started) = tokio::time::timeout(Duration::from_secs(30), async {
            let mut started = false;
            loop {
                let frame = match read_frame(&mut client).await {
                    Ok(frame) => frame,
                    Err(error) => panic!(
                        "host stream failed: {error}; serve result: {:?}",
                        (&mut task).await
                    ),
                };
                match frame["type"].as_str() {
                    Some("started") => started = true,
                    Some("terminal") => break (frame, started),
                    Some("heartbeat" | "output") => {}
                    _ => panic!("unexpected runner frame: {frame}"),
                }
            }
        })
        .await
        .expect("runner terminal deadline");
        drop(client);
        task.await.unwrap().unwrap();
        if self.relay_reports {
            self.harvest(&c.jti);
        }
        (terminal, started)
    }
    async fn submit(&mut self) -> (Claims, Value, Vec<Vec<u8>>, Value, bool) {
        let c = self.claims();
        let (mut client, task) = self.connect(&c).await;
        let have = self.have(&mut client).await;
        let (request, bytes) = self.request(&c, &have);
        let (terminal, started) = self.finish(&c, client, task, &request, &bytes).await;
        self.stop(&c);
        (c, have, bytes, terminal, started)
    }
}

#[tokio::test]
async fn storage_control_provisions_sparse_capped_disks_and_keeps_admission_enforced() {
    use std::os::unix::fs::MetadataExt;
    let mut f = Fixture::new();
    let account = f.claims().sub;
    f.disk
        .ensure_workspace(&account, "ws-fixture", WORKSPACE_DISK_BYTES)
        .unwrap();
    let path = f.root.path().join("workspace-disks/ws-fixture");
    let before = std::fs::metadata(&path).unwrap();
    assert_eq!(before.len(), WORKSPACE_DISK_BYTES);
    assert_eq!(before.blocks(), 0);
    f.disk
        .ensure_workspace(&account, "ws-fixture", WORKSPACE_DISK_BYTES)
        .unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), before.ino());
    assert!(f
        .disk
        .ensure_workspace(&account, "ws-fixture", WORKSPACE_DISK_BYTES / 2)
        .is_err());
    assert!(f
        .disk
        .ensure_workspace(
            &format!("{account}-other"),
            "ws-fixture",
            WORKSPACE_DISK_BYTES
        )
        .is_err());
    assert!(f
        .disk
        .ensure_workspace(&account, "too-large", WORKSPACE_DISK_BYTES + 512)
        .is_err());
    assert!(f
        .disk
        .ensure_workspace(&account, "../escape", WORKSPACE_DISK_BYTES)
        .is_err());
    assert!(!f.root.path().join("workspace-disks/too-large").exists());
    assert_eq!(std::fs::metadata(path).unwrap().len(), WORKSPACE_DISK_BYTES);
    assert_ne!(f.disk.available_bytes().unwrap(), u64::MAX);
    let c = f.claims();
    let mut host = f.runtime.host.lock().unwrap();
    assert!(host.storage_enforced());
    if let motor_host::config::HostSettings::PerJob(settings) = &mut host.settings {
        settings.workspace_disk_cap_bytes = u64::MAX;
    }
    assert!(matches!(
        host.claim(c, unix_ms()),
        Err(motor_host::state::Error::Refused(
            motor_host::state::Refusal::HostFull
        ))
    ));
    assert!(host.snapshot.per_job.is_empty());
}

#[tokio::test]
async fn false_two_repository_entries_refuse_once_clear_every_mirror_and_resend_full_history() {
    let mut f = Fixture::new();
    f.seed(&f.base.clone());
    let (first, have, bytes, terminal, started) = f.submit().await;
    assert_eq!(bytes.len(), 2);
    assert_eq!(have["git_dependencies"][0]["commits"], json!([f.base[0]]));
    for (index, data) in bytes.iter().enumerate() {
        assert_eq!(
            have["repositories"][index]["commits"],
            json!([f.base[index]])
        );
        assert!(
            String::from_utf8_lossy(data).contains(&format!("-{}", f.base[index])),
            "bundle must be thin"
        );
    }
    assert!(!started);
    assert_eq!(terminal["ran"], "none");
    assert_eq!(terminal["outcome"]["reason"], "transfer_interrupted");
    assert!(f.held(&first.jti).is_empty());
    let (_, have, bytes, terminal, started) = f.submit().await;
    assert_eq!(bytes.len(), 2);
    assert!(have["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["commits"] == json!([])));
    assert_eq!(have["git_dependencies"][0]["commits"], json!([]));
    assert!(
        bytes
            .iter()
            .all(|b| !String::from_utf8_lossy(b).contains("\n-")),
        "resend must have no prerequisites"
    );
    assert!(started);
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
}

#[tokio::test]
async fn false_exact_snapshot_without_any_bundle_refuses_then_separate_submission_resends() {
    let mut f = Fixture::new();
    f.seed(&f.tips.clone());
    let (first, _, bytes, terminal, started) = f.submit().await;
    assert!(bytes.is_empty());
    assert!(!started);
    assert_eq!(terminal["ran"], "none");
    assert_eq!(terminal["outcome"]["reason"], "transfer_interrupted");
    assert!(f.held(&first.jti).is_empty());
    let (_, have, bytes, terminal, started) = f.submit().await;
    assert!(have["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["commits"] == json!([])));
    assert_eq!(bytes.len(), 2);
    assert!(started);
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
}

#[tokio::test]
async fn warm_disk_uses_verified_import_record_for_next_jobs_thin_bundles() {
    let mut f = Fixture::new();
    let (first, have, _, terminal, started) = f.submit().await;
    assert_eq!(
        have["git_dependencies"],
        json!([{"url":"https://example.com/dependency","commits":[]}])
    );
    assert!(
        have["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["commits"] == json!([])),
        "client candidates must not add possession"
    );
    assert!(started);
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
    assert!(!f.held(&first.jti).is_empty());
    let old = f.tips.clone();
    for (i, path) in f.repositories.iter().enumerate() {
        f.tips[i] = commit(path, "next");
    }
    let (_, have, bytes, terminal, started) = f.submit().await;
    for (index, data) in bytes.iter().enumerate() {
        assert_eq!(have["repositories"][index]["commits"], json!([old[index]]));
        assert!(String::from_utf8_lossy(data).contains(&format!("-{}", old[index])));
    }
    assert_eq!(bytes.len(), 2);
    assert!(started);
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
}

#[tokio::test]
async fn false_retire_report_only_resends_objects_that_remain_on_disk() {
    let mut f = Fixture::new();
    let first = f.claims();
    let (mut client, task) = f.connect(&first).await;
    let have = f.have(&mut client).await;
    let (request, bytes) = f.request(&first, &have);
    let (terminal, _) = f.finish(&first, client, task, &request, &bytes).await;
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
    f.report(&first.jti, 1000, false, f.refs(&f.tips));
    assert!(f.held(&first.jti).is_empty());
    f.stop(&first);
    let (_, have, bytes, terminal, started) = f.submit().await;
    assert!(have["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["commits"] == json!([])));
    assert_eq!(bytes.len(), 2);
    assert!(started);
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
}

#[tokio::test]
async fn rejected_bundle_clears_the_whole_record_and_never_starts() {
    let mut f = Fixture::new();
    f.seed(&f.base.clone());
    let c = f.claims();
    let (mut client, task) = f.connect(&c).await;
    let have = f.have(&mut client).await;
    let (request, mut bytes) = f.request(&c, &have);
    bytes[0][0] ^= 1;
    let (terminal, started) = f.finish(&c, client, task, &request, &bytes).await;
    assert!(!started);
    assert_eq!(terminal["ran"], "none");
    assert_eq!(terminal["outcome"]["reason"], "bundle_rejected");
    assert!(f.held(&c.jti).is_empty());
    f.stop(&c);
    let (_, _, bytes, terminal, started) = f.submit().await;
    assert_eq!(bytes.len(), 2);
    assert!(started);
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
}

#[tokio::test]
async fn interrupted_upload_at_each_bundle_boundary_adds_no_have_entries() {
    for boundary in [
        "before_bytes",
        "partial_first",
        "between_bundles",
        "partial_last",
    ] {
        let mut f = Fixture::new();
        let c = f.claims();
        let (mut client, task) = f.connect(&c).await;
        let have = f.have(&mut client).await;
        let (request, bytes) = f.request(&c, &have);
        write_frame(&mut client, &request).await.unwrap();
        next(&mut client, "accepted").await;
        // The host acknowledges its queued Run before the guest accepts it.
        // The guest creates record.json before writing Accepted and can remove
        // it if admission fails. Wait for this job's durable admitted event so
        // every cut below interrupts an admitted upload.
        let events = f.root.path().join("disk/runner/tenant-events.json");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let admitted = std::fs::read(&events)
                    .ok()
                    .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
                    .is_some_and(|state| {
                        state["pending"].as_array().is_some_and(|pending| {
                            pending
                                .iter()
                                .any(|event| event["event"] == "admitted" && event["jti"] == c.jti)
                        })
                    });
                if admitted {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("runner durably admits the relayed Run before interruption");
        let lengths = match boundary {
            "before_bytes" => [0, 0],
            "partial_first" => [bytes[0].len() / 2, 0],
            "between_bundles" => [bytes[0].len(), 0],
            "partial_last" => [bytes[0].len(), bytes[1].len() - 1],
            _ => unreachable!(),
        };
        for (index, length) in lengths.into_iter().enumerate() {
            if length != 0 {
                let frame = Frame::BundleChunk {
                    index: index as u32,
                    offset: 0,
                    data: motor_protocol::BytePayload(bytes[index][..length].into()),
                };
                write_frame(&mut client, &serde_json::to_value(frame).unwrap())
                    .await
                    .unwrap();
            }
        }
        drop(client);
        task.await.unwrap().unwrap();
        // The runner persists its no-start outcome after seeing EOF, independently
        // of the disconnected host relay. Wait on that outcome, not on timing.
        let terminal = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let record = f
                    .root
                    .path()
                    .join("disk/runner/jobs")
                    .join(&c.jti)
                    .join("record.json");
                let terminal = std::fs::read(record)
                    .ok()
                    .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
                    .and_then(|job| {
                        (job["terminal"]["outcome"]["reason"] == "transfer_interrupted")
                            .then(|| job["terminal"].clone())
                    });
                if let Some(terminal) = terminal {
                    break terminal;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("runner records interrupted upload");
        assert_eq!(terminal["outcome"]["type"], "refused_before_start");
        assert_eq!(terminal["ran"], "none");
        let events = f.harvest(&c.jti);
        assert!(
            events.iter().all(|event| event.mirror_refs.is_none()),
            "{boundary}"
        );
        assert!(f.held(&c.jti).is_empty(), "{boundary}");
        state_tests::reopen(&f);
        assert!(f.held(&c.jti).is_empty(), "restart at {boundary}");
    }
}

#[tokio::test]
async fn interrupted_upload_before_delivery_replays_no_start_and_keeps_have_empty() {
    let mut f = Fixture::new();
    let c = f.claims();
    let (entered, preparing) = tokio::sync::oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    *f.disk.prepare_hold.lock().unwrap() = Some(PrepareHold {
        entered,
        release: resume,
    });
    let (mut client, task) = f.connect(&c).await;
    tokio::time::timeout(Duration::from_secs(10), preparing)
        .await
        .expect("host begins preparing the job")
        .unwrap();
    let have = f.have(&mut client).await;
    let (request, bytes) = f.request(&c, &have);
    write_frame(&mut client, &request).await.unwrap();
    next(&mut client, "accepted").await;
    write_frame(
        &mut client,
        &serde_json::to_value(Frame::BundleChunk {
            index: 0,
            offset: 0,
            data: motor_protocol::BytePayload(bytes[0][..bytes[0].len() / 2].into()),
        })
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(!f.runtime.host.lock().unwrap().snapshot.leases[&c.jti].delivered);
    drop(client);
    assert!(tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("host settles the disconnected queued upload")
        .unwrap()
        .is_err());

    let terminal = {
        let host = f.runtime.host.lock().unwrap();
        let terminal = host.snapshot.billing[&c.jti]
            .terminal
            .clone()
            .expect("host durably records the interrupted upload's no-start terminal");
        assert_eq!(terminal["outcome"]["type"], "refused_before_start");
        assert_eq!(terminal["outcome"]["reason"], "transfer_interrupted");
        assert_eq!(terminal["ran"], "none");
        let usage = &host.snapshot.zero_usage[&c.jti];
        assert_eq!(usage.started_at_ms, usage.ended_at_ms);
        assert!(!host.snapshot.leases[&c.jti].delivered);
        terminal
    };
    // Finish the cancelled preparation before replaying the retained result.
    drop(release);
    tokio::time::timeout(Duration::from_secs(10), f.disk.released.notified())
        .await
        .expect("cancelled preparation releases its resources");
    assert!(!f
        .root
        .path()
        .join("disk/runner/tenant-events.json")
        .exists());
    assert!(f.held(&c.jti).is_empty());
    assert_eq!(f.reattach_terminal(&c).await, terminal);
    state_tests::reopen(&f);
    assert!(f.held(&c.jti).is_empty());
    assert_eq!(f.reattach_terminal(&c).await, terminal);
    let host = f.runtime.host.lock().unwrap();
    assert_eq!(
        host.snapshot.zero_usage[&c.jti].started_at_ms,
        host.snapshot.zero_usage[&c.jti].ended_at_ms
    );
}

#[tokio::test]
async fn delivered_and_verified_bundles_without_a_received_pin_report_never_populate_have() {
    let mut f = Fixture::new();
    // Withhold lifecycle delivery to model a host crash before report receipt.
    // The real runner still imports and pins both bundles before running.
    f.relay_reports = false;
    let (first, _, bytes, terminal, started) = f.submit().await;
    assert_eq!(bytes.len(), 2);
    assert!(started);
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
    assert!(f.held(&first.jti).is_empty());
    state_tests::reopen(&f);
    assert!(f.held(&first.jti).is_empty());
    f.relay_reports = true;
    let (_, have, bytes, terminal, started) = f.submit().await;
    assert!(have["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["commits"] == json!([])));
    assert_eq!(bytes.len(), 2);
    assert!(started);
    assert_eq!(terminal["outcome"], json!({"type":"exit","code":0}));
}

mod state_tests;
