use super::*;
#[path = "tests/tenant.rs"]
mod tenant;
use crate::spool::Upload;
use cortexkit_test_support::ScratchDir;
use rand::{rngs::StdRng, Rng, SeedableRng};
use sha2::{Digest, Sha256};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::time::Instant;
use tokio::net::{UnixListener, UnixStream};

#[path = "tests/cancellation.rs"]
mod cancellation;
#[path = "tests/dependency_writer.rs"]
mod dependency_writer;
#[path = "tests/disk_pressure.rs"]
mod disk_pressure;
#[path = "tests/fixture_cleanup.rs"]
mod fixture_cleanup;
#[path = "tests/gate_priority.rs"]
mod gate_priority;
#[path = "tests/git_dependencies.rs"]
mod git_dependencies;
#[path = "tests/health.rs"]
mod health;
#[path = "tests/migration.rs"]
mod migration;
#[path = "tests/monitoring.rs"]
mod monitoring;
#[path = "tests/nested_workspaces.rs"]
mod nested_workspaces;
#[path = "tests/platform.rs"]
mod platform_refusal;
#[path = "tests/refusals.rs"]
mod refusals;
#[path = "tests/return_paths.rs"]
mod return_paths;
#[path = "tests/started.rs"]
mod started;
#[path = "tests/status_transitions.rs"]
mod status_transitions;
#[path = "tests/tip_capability.rs"]
mod tip_capability;
#[path = "tests/vcpu.rs"]
mod vcpu;
#[path = "tests/windows_backend.rs"]
mod windows_backend;
#[path = "tests/windows_environment.rs"]
mod windows_environment;
#[path = "tests/windows_instance.rs"]
mod windows_instance;
#[cfg(target_os = "linux")]
#[path = "tests/windows_operator.rs"]
mod windows_operator;
#[path = "tests/workspace_lifetime.rs"]
mod workspace_lifetime;
#[path = "tests/workspace_snapshots.rs"]
mod workspace_snapshots;
#[path = "tests/writer_command_selection.rs"]
mod writer_command_selection;
#[path = "tests/writer_storage.rs"]
mod writer_storage;

pub(super) fn register_runner(runner: &Runner) {
    let Some(processes) = runner
        .config()
        .test_processes
        .clone()
        .or_else(|| crate::test_cleanup::processes_for(&runner.config().state_dir()))
    else {
        return;
    };
    let weak = Arc::downgrade(&runner.core);
    processes.lock().unwrap().quiesce(move || {
        let Some(core) = weak.upgrade() else {
            return true;
        };
        let runner = Runner { core };
        // Drain freezes new dispatch, but restart-recoverable units can outlive
        // a drain reply. Wait for actual completion rather than that reply.
        runner.begin_drain(Duration::ZERO, false);
        let started = std::time::Instant::now();
        loop {
            let _ = runner.tick();
            let (quiet, cancel) = {
                let inner = runner.core.inner.lock().unwrap();
                let live: Vec<_> = inner
                    .jobs
                    .values()
                    .filter(|j| matches!(j.state, JobState::Queued | JobState::Running))
                    .collect();
                let quiet = live.is_empty()
                    && inner.writer.is_none()
                    && inner.starting.is_empty()
                    && inner.reserved.is_empty()
                    && inner.preparing.is_empty()
                    && inner.admitting.is_empty()
                    && inner.finalizing.is_empty();
                #[cfg(target_os = "linux")]
                let quiet = quiet && inner.cache_fills.is_empty();
                let cancel = live
                    .into_iter()
                    .filter(|j| !j.base || started.elapsed() >= Duration::from_secs(30))
                    .map(|j| j.id)
                    .collect::<Vec<_>>();
                (quiet, cancel)
            };
            if quiet && runner.core.background_deletes.load(Ordering::SeqCst) == 0 {
                return true;
            }
            for id in cancel {
                let _ = runner.cancel(id);
            }
            if started.elapsed() >= Duration::from_secs(60) {
                eprintln!("fixture runner did not quiesce; retaining its scratch tree");
                return false;
            }
            std::thread::park_timeout(Duration::from_millis(10));
        }
    });
}

struct Fixture {
    cleanup: crate::test_cleanup::Cleanup,
    scratch: std::mem::ManuallyDrop<ScratchDir>,
    main: PathBuf,
    sibling: PathBuf,
    root: String,
    canonical: String,
    key: String,
    clock: Arc<AtomicU64>,
}

struct ScopedEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl ScopedEnv {
    fn set(entries: &[(&'static str, &str)]) -> Self {
        let previous = entries
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in entries {
            std::env::set_var(key, value);
        }
        Self(previous)
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    workspace::git(path, args).unwrap()
}
fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}
fn commit(path: &Path, content: &str) -> (String, String) {
    fs::write(path.join("source"), content).unwrap();
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "snapshot",
        ],
    );
    (
        git(path, &["rev-parse", "HEAD"]),
        git(path, &["rev-parse", "HEAD^{tree}"]),
    )
}

impl Fixture {
    fn new() -> Self {
        let scratch = ScratchDir::new("motor-runner");
        #[cfg(target_os = "linux")]
        require_reflink_scratch(&scratch);
        let main = scratch.join("local/main");
        let sibling = scratch.join("local/commons");
        for path in [&main, &sibling] {
            fs::create_dir_all(path).unwrap();
            git(path, &["init"]);
            commit(path, "first\n");
        }
        let root = scratch
            .join("remote/source/main")
            .to_string_lossy()
            .to_string();
        let canonical = scratch
            .join("remote/source/commons")
            .to_string_lossy()
            .to_string();
        let key = scratch
            .join("remote/worktrees/a")
            .to_string_lossy()
            .to_string();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut cleanup = crate::test_cleanup::Cleanup::default();
        cleanup.capture(&scratch);
        Self {
            cleanup,
            scratch: std::mem::ManuallyDrop::new(scratch),
            main,
            sibling,
            root,
            canonical,
            key,
            clock: Arc::new(AtomicU64::new(now)),
        }
    }
    fn config(&self) -> Config {
        let mut c = Config::new(self.scratch.join("state"), "e2e");
        c.free_space_floor_bytes = 0;
        let cargo_home = self.scratch.join("cargo-home");
        fs::create_dir_all(&cargo_home).unwrap();
        c.fixture_cargo_home = Some(cargo_home);
        c.test_processes = Some(self.cleanup.0.clone());
        c.cpu_topology =
            Some(CpuTopology::new((0..16).map(|cpu| vec![cpu, cpu + 16]).collect()).unwrap());
        c.workspace_roots = vec![self.scratch.join("remote/worktrees")];
        c.owned_workspace_roots = vec![self.scratch.join("remote")];
        c.base_commands = vec!["printf warm > \"$CARGO_TARGET_DIR/artifact\"".into()];
        let clock = self.clock.clone();
        c.clock = Arc::new(move || clock.load(Ordering::SeqCst));
        c
    }
    fn bundle(&self, source: &Path, repository: &str, index: u32) -> (BundleManifest, Vec<u8>) {
        let id = Uuid::now_v7();
        let reference = format!("refs/ck-motor/{id}/snapshot");
        git(source, &["update-ref", &reference, "HEAD"]);
        let path = self.scratch.join(format!("{id}.bundle"));
        git(source, &["bundle", "create", text(&path), &reference]);
        git(source, &["update-ref", "-d", &reference]);
        let bytes = fs::read(path).unwrap();
        (
            BundleManifest {
                index,
                repository: repository.to_string(),
                length: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            },
            bytes,
        )
    }
    fn run(&self, command: &str) -> (Run, Vec<Vec<u8>>) {
        let (a, first) = self.bundle(&self.main, &self.root, 0);
        let (b, second) = self.bundle(&self.sibling, &self.canonical, 1);
        (
            Run {
                return_paths: Vec::new(),
                return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
                gate_job: false,
                network: None,
                platform: None,
                workspace_kind: None,
                job_id: Uuid::now_v7(),
                workspace_key: self.key.clone(),
                repository_root: self.root.clone(),
                cwd: self.key.clone(),
                command: command.to_string(),
                env: BTreeMap::new(),
                weight_hint: None,
                timeout: None,
                queue_wait_limit_s: 300,
                snapshot_commit: git(&self.main, &["rev-parse", "HEAD"]),
                tree_hash: git(&self.main, &["rev-parse", "HEAD^{tree}"]),
                integration_tip: git(&self.main, &["rev-parse", "HEAD"]),
                siblings: vec![SiblingSnapshot {
                    canonical_path: self.canonical.clone(),
                    commit: git(&self.sibling, &["rev-parse", "HEAD"]),
                    tree_hash: git(&self.sibling, &["rev-parse", "HEAD^{tree}"]),
                    base_commit: Some(git(&self.sibling, &["rev-parse", "HEAD"])),
                }],
                bundles: vec![a, b],
                git_dependencies: vec![],
                base_commit: Some(git(&self.main, &["rev-parse", "HEAD"])),
            },
            vec![first, second],
        )
    }
    fn spool(&self, runner: &Runner, request: &Run, bytes: &[Vec<u8>]) -> Upload {
        let manifests: Vec<_> = request
            .bundles
            .iter()
            .chain(
                request
                    .git_dependencies
                    .iter()
                    .filter_map(|d| d.bundle.as_ref()),
            )
            .cloned()
            .collect();
        let mut upload = Upload::new(runner.spool_dir(request.job_id), &manifests).unwrap();
        for (i, data) in bytes.iter().enumerate() {
            upload
                .chunk(Frame::BundleChunk {
                    index: i as u32,
                    offset: 0,
                    data: BytePayload(data.clone()),
                })
                .unwrap();
        }
        upload
    }
    fn submit(&self, runner: &Runner, request: &Run, bytes: &[Vec<u8>]) {
        assert_run_accepted(runner, request);
        let spool = self.spool(runner, request, bytes);
        runner.fetch_run(request, &spool.directory, true).unwrap();
    }
    fn run_worktree(&self, command: &str) -> (Run, Vec<Vec<u8>>) {
        let head = git(&self.main, &["rev-parse", "HEAD"]);
        let spool = self.scratch.join(format!("index-{}", Uuid::now_v7()));
        let mut cmd = Command::new("git");
        cmd.arg("--no-optional-locks").arg("-C").arg(&self.main);
        cmd.env("GIT_INDEX_FILE", &spool)
            .args(["read-tree", "HEAD"]);
        assert!(cmd.status().unwrap().success());
        let mut cmd = Command::new("git");
        cmd.arg("--no-optional-locks").arg("-C").arg(&self.main);
        cmd.env("GIT_INDEX_FILE", &spool)
            .args(["add", "-A", "--", "."]);
        assert!(cmd.status().unwrap().success());
        let mut cmd = Command::new("git");
        cmd.arg("--no-optional-locks").arg("-C").arg(&self.main);
        cmd.env("GIT_INDEX_FILE", &spool).arg("write-tree");
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        let tree = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let _ = fs::remove_file(spool);

        let head_tree = git(&self.main, &["rev-parse", "HEAD^{tree}"]);
        let (snap_commit, bundle_commit) = if head_tree == tree {
            (head.clone(), head.clone())
        } else {
            let snap = git(
                &self.main,
                &[
                    "-c",
                    "user.name=ck-motor",
                    "-c",
                    "user.email=motor@localhost",
                    "commit-tree",
                    &tree,
                    "-p",
                    &head,
                    "-m",
                    "ck-motor snapshot",
                ],
            );
            (snap.clone(), snap)
        };

        let id = Uuid::now_v7();
        let reference = format!("refs/ck-motor/{id}/snapshot");
        git(&self.main, &["update-ref", &reference, &bundle_commit]);
        let path = self.scratch.join(format!("{id}.bundle"));
        git(&self.main, &["bundle", "create", text(&path), &reference]);
        git(&self.main, &["update-ref", "-d", &reference]);
        let first = fs::read(path).unwrap();
        let a = BundleManifest {
            index: 0,
            repository: self.root.clone(),
            length: first.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&first)),
        };

        let (b, second) = self.bundle(&self.sibling, &self.canonical, 1);
        (
            Run {
                return_paths: Vec::new(),
                return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
                gate_job: false,
                network: None,
                platform: None,
                workspace_kind: None,
                job_id: Uuid::now_v7(),
                workspace_key: self.key.clone(),
                repository_root: self.root.clone(),
                cwd: self.key.clone(),
                command: command.to_string(),
                env: BTreeMap::new(),
                weight_hint: None,
                timeout: None,
                queue_wait_limit_s: 300,
                snapshot_commit: snap_commit,
                tree_hash: tree,
                integration_tip: head.clone(),
                siblings: vec![SiblingSnapshot {
                    canonical_path: self.canonical.clone(),
                    commit: git(&self.sibling, &["rev-parse", "HEAD"]),
                    tree_hash: git(&self.sibling, &["rev-parse", "HEAD^{tree}"]),
                    base_commit: Some(git(&self.sibling, &["rev-parse", "HEAD"])),
                }],
                bundles: vec![a, b],
                git_dependencies: vec![],
                base_commit: Some(head),
            },
            vec![first, second],
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A restart helper deliberately exits without running its destructors.
        // Its parent fixture still owns cleanup of the recorded surviving unit.
        if let Ok(entries) = fs::read_dir(self.scratch.join("state/e2e/jobs")) {
            for entry in entries.flatten() {
                if let Ok(job) = read_json::<Job>(&entry.path().join("record.json")) {
                    if job.terminal.is_none() {
                        if let Some(process) = job.process.filter(|p| p.pid > 0) {
                            self.cleanup
                                .0
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .register(process);
                        }
                    }
                }
            }
        }
        let quiet = self.cleanup.finish();
        if quiet {
            // A stuck fill must keep its input paths; deleting them would turn
            // a teardown failure into another failing, orphaned writer unit.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.scratch) };
        } else if !std::thread::panicking() {
            panic!("fixture runner did not quiesce before scratch cleanup");
        }
    }
}

#[test]
fn ignored_target_is_not_hashed_when_materializing_workspace() {
    let f = Fixture::new();
    fs::write(f.main.join(".gitignore"), "target/\n").unwrap();
    commit(&f.main, "first\n");
    let mut config = f.config();
    config.max_weight = 1; // Hold the queue while checking materialization directly.
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    let path = Path::new(&request.workspace_key);
    let mut inner = runner.core.inner.lock().unwrap();
    runner
        .core
        .trees
        .materialize(
            &request.workspace_key,
            &request.repository_root,
            &request.snapshot_commit,
            &request.siblings,
            runner.now(),
            &mut inner.catalog,
        )
        .unwrap();
    fs::write(path.join("target/runner-artifact"), "not source\n").unwrap();
    assert_eq!(
        runner.core.trees.tree_hash(path).unwrap(),
        request.tree_hash
    );
    assert_eq!(
        fs::read_to_string(path.join("target/runner-artifact")).unwrap(),
        "not source\n"
    );
}

#[test]
fn dissociated_workspace_checks_out_a_commit_fetched_into_the_mirror_later() {
    // Job isolation repacks each checkout and removes its alternates before the
    // job runs. The next run on the same workspace brings a commit that exists
    // only in the mirror, so checkout must re-attach the mirror first or Git
    // fails with "unable to read tree".
    let f = Fixture::new();
    commit(&f.main, "dissociation base\n");
    let mut config = f.config();
    config.max_weight = 1; // Hold the queue while checking materialization directly.
    let runner = Runner::open(config).unwrap();
    let (first, bytes) = f.run("true");
    f.submit(&runner, &first, &bytes);
    let path = Path::new(&first.workspace_key);
    {
        let mut inner = runner.core.inner.lock().unwrap();
        runner
            .core
            .trees
            .materialize(
                &first.workspace_key,
                &first.repository_root,
                &first.snapshot_commit,
                &first.siblings,
                runner.now(),
                &mut inner.catalog,
            )
            .unwrap();
    }
    let alternates = path.join(".git/objects/info/alternates");
    assert!(
        alternates.is_file(),
        "a fresh workspace borrows the mirror's objects"
    );
    workspace::git(path, &["repack", "-a", "-d"]).unwrap();
    fs::remove_file(&alternates).unwrap();

    commit(&f.main, "fetched after dissociation\n");
    let (second, bytes) = f.run("true");
    assert_ne!(second.snapshot_commit, first.snapshot_commit);
    f.submit(&runner, &second, &bytes);
    let mut inner = runner.core.inner.lock().unwrap();
    runner
        .core
        .trees
        .materialize(
            &first.workspace_key,
            &second.repository_root,
            &second.snapshot_commit,
            &second.siblings,
            runner.now(),
            &mut inner.catalog,
        )
        .expect("a dissociated workspace still checks out a commit fetched into the mirror");
    assert_eq!(runner.core.trees.tree_hash(path).unwrap(), second.tree_hash);
}

#[test]
fn standalone_build_slot_wrapper_compiles_with_its_cargo_edition() {
    let scratch = ScratchDir::new("motor-wrapper-edition");
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../prefrontal-build-slot");
    // Keep the explicit standalone compiler flag in step with the real crate.
    assert!(fs::read_to_string(crate_root.join("Cargo.toml"))
        .unwrap()
        .lines()
        .any(|line| line.trim() == "edition = \"2021\""));
    let binary = scratch.join("prefrontal-build-slot");
    platform::build_wrapper(
        &crate_root.join("src/main.rs"),
        &binary,
        &scratch.join("slots"),
        2,
    )
    .expect("compile the real standalone wrapper source, not a fixture");
    let output = std::process::Command::new(&binary)
        .args(["rustc", "--version"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("rustc "));
}

#[test]
fn sibling_catalog_rematerializes_deleted_or_moved_checkouts() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 1;
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    let mut inner = runner.core.inner.lock().unwrap();
    let materialize = |catalog: &mut Catalog| {
        runner
            .core
            .trees
            .materialize(
                &request.workspace_key,
                &request.repository_root,
                &request.snapshot_commit,
                &request.siblings,
                runner.now(),
                catalog,
            )
            .unwrap();
    };
    materialize(&mut inner.catalog);
    let sibling = Path::new(&f.canonical);
    let expected = request.siblings[0].commit.clone();
    assert_eq!(inner.catalog.sibling_commits[&f.canonical], expected);
    fs::remove_dir_all(sibling).unwrap();
    materialize(&mut inner.catalog);
    assert!(
        sibling.is_dir(),
        "a catalog hit must not skip a deleted checkout"
    );
    assert_eq!(git(sibling, &["rev-parse", "HEAD"]), expected);
    let (foreign, _) = commit(sibling, "wrong checkout\n");
    assert_ne!(foreign, expected);
    materialize(&mut inner.catalog);
    assert_eq!(
        git(sibling, &["rev-parse", "HEAD"]),
        expected,
        "a catalog hit must not skip a moved HEAD"
    );
    assert_eq!(
        fs::read_to_string(sibling.join("source")).unwrap(),
        "first\n"
    );
}

// Linux warm-target fixtures use the production FICLONE path. Probe a real,
// nonempty file once per test process so an unsuitable scratch filesystem fails
// at setup with its requirement, rather than as a later job-outcome mismatch.
#[cfg(target_os = "linux")]
fn require_reflink_scratch(scratch: &Path) {
    static SUPPORT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let result = SUPPORT.get_or_init(|| {
        let probe = scratch.join("reflink-probe");
        let source = probe.join("source");
        let target = probe.join("target");
        let result = (|| -> io::Result<()> {
            fs::create_dir_all(&source)?;
            fs::write(source.join("artifact"), b"nonempty reflink probe")?;
            platform::clone_target(&source, &target)?;
            if fs::read(target.join("artifact"))? != b"nonempty reflink probe" {
                return Err(io::Error::other("reflink probe did not clone its contents"));
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(probe);
        result.map_err(|error| {
            format!(
                "motor-runner fixtures require FICLONE reflink support on the TMPDIR scratch filesystem ({}); set TMPDIR to a reflink-capable filesystem, for example TMPDIR=/srv/build/tmp: {error}",
                scratch.display()
            )
        })
    });
    if let Err(message) = result {
        panic!("{message}");
    }
}

fn runner_diagnostics(runner: &Runner) -> String {
    let config = format!("allowed_roots={:?}", runner.config().owned_workspace_roots);
    // Diagnostics may run while the caller holds the state lock. Keep live
    // state best-effort, but never hide configuration behind that lock.
    let Ok(inner) = runner.core.inner.try_lock() else {
        return format!("{config}; runner state lock unavailable");
    };
    let jobs: Vec<_> = inner.jobs.values().map(|job| {
        format!("id={} base={} state={:?} key={} cwd={:?} process={:?} started_ms={:?} monitoring_error={:?} terminal={:?}",
            job.id, job.base, job.state, job.key(), job.request.as_ref().map(|r| &r.cwd), job.process, job.started_ms, job.monitoring_error, job.terminal)
    }).collect();
    format!(
        "{config}; jobs={jobs:#?}; starting={:?}; finalizing={:?}; reserved={:?}; dropping={:?}",
        inner.starting,
        inner.finalizing,
        inner.reserved.keys().collect::<Vec<_>>(),
        inner.dropping
    )
}

fn assert_run_accepted(runner: &Runner, request: &Run) {
    let admission = runner.accept(request.clone()).unwrap_or_else(|error| {
        panic!(
            "Run admission errored for {}: {error}; {}",
            request.job_id,
            runner_diagnostics(runner)
        )
    });
    let refusal = match admission {
        Admission::Accepted(_) => return,
        Admission::Refused(reason) => format!("{reason:?}"),
        Admission::Duplicate => "Duplicate".into(),
        Admission::DiskFull(full) => format!("DiskFull: {full:?}"),
    };
    panic!(
        "Run admission failed for {} at {}: {refusal}; {}",
        request.job_id,
        request.workspace_key,
        runner_diagnostics(runner)
    );
}

async fn until_with_diagnostics(
    mut predicate: impl FnMut() -> bool,
    diagnostics: impl FnOnce() -> String,
) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("test condition timed out: {}", diagnostics()));
}

async fn until(predicate: impl FnMut() -> bool) {
    until_with_diagnostics(predicate, || "predicate did not become true".into()).await;
}
async fn terminal(runner: &Runner, id: Uuid) -> TerminalRecord {
    until_with_diagnostics(
        || {
            runner
                .core
                .inner
                .lock()
                .unwrap()
                .jobs
                .get(&id)
                .is_some_and(|j| j.terminal.is_some())
        },
        || {
            format!(
                "waiting for terminal of {id}; {}",
                runner_diagnostics(runner)
            )
        },
    )
    .await;
    runner.core.inner.lock().unwrap().jobs[&id]
        .terminal
        .clone()
        .unwrap()
}
async fn base_done(runner: &Runner) {
    until_with_diagnostics(
        || {
            !runner
                .core
                .inner
                .lock()
                .unwrap()
                .jobs
                .values()
                .any(|j| j.base && j.terminal.is_none())
        },
        || {
            format!(
                "waiting for background fills; {}",
                runner_diagnostics(runner)
            )
        },
    )
    .await;
}

#[test]
fn runner_full_counts_64_awaiting_uploads_and_tombstone_is_durable() {
    let f = Fixture::new();
    let c = f.config();
    let runner = Runner::open(c.clone()).unwrap();
    let (r, _) = f.run("touch marker");
    for _ in 0..64 {
        let mut r = r.clone();
        r.job_id = Uuid::now_v7();
        assert!(matches!(runner.accept(r).unwrap(), Admission::Accepted(_)));
    }
    assert!(matches!(
        runner.accept(r.clone()).unwrap(),
        Admission::Refused(RefusalReason::RunnerFull)
    ));
    assert_eq!(runner.status().queue_depth, 64);
    let before = fs::read(runner.record_path(r.job_id)).unwrap();
    assert!(matches!(
        runner.accept(r.clone()).unwrap(),
        Admission::Duplicate
    ));
    assert_eq!(fs::read(runner.record_path(r.job_id)).unwrap(), before);
    drop(runner);
    // The scheduler holds a short-lived Arc while ticking. Acquire after it yields.
    let runner = reopen(c);
    let frames = runner.replay(r.job_id, 0).unwrap();
    assert!(
        matches!(&frames[0], Frame::Terminal(t) if t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::RunnerFull })
    );
    assert_eq!(runner.status().queue_depth, 0);
}

fn reopen(config: Config) -> Runner {
    let end = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match Runner::open(config.clone()) {
            Ok(r) => return r,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && std::time::Instant::now() < end => {
                std::thread::park_timeout(Duration::from_millis(5))
            }
            Err(e) => panic!("reopen: {e}"),
        }
    }
}

#[test]
fn unknown_cancel_and_interrupted_attach_are_permanent_tombstones() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (r, _) = f.run("touch marker");
    runner.cancel(r.job_id).unwrap();
    runner.cancel(r.job_id).unwrap();
    assert!(
        matches!(runner.replay(r.job_id,0).unwrap().last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::Cancelled)
    );
    assert!(matches!(runner.accept(r).unwrap(), Admission::Duplicate));
    let id = Uuid::now_v7();
    assert!(
        matches!(runner.replay(id,0).unwrap().last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::TransferInterrupted })
    );
    assert_eq!(runner.status().queue_depth, 0);
}

#[test]
fn startup_marks_awaiting_and_queued_lost_and_retains_terminals() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let (r, _) = f.run("touch marker");
    runner.accept(r.clone()).unwrap();
    let mut q = r.clone();
    q.job_id = Uuid::now_v7();
    runner.accept(q.clone()).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&q.job_id).unwrap();
        job.state = JobState::Queued;
        // Weight over the cap leaves this record queued until recovery.
        job.weight = 100;
        runner.save(job).unwrap();
    }
    let cancel = Uuid::now_v7();
    runner.cancel(cancel).unwrap();
    drop(runner);
    let runner = reopen(config);
    for id in [r.job_id, q.job_id] {
        assert!(
            matches!(runner.replay(id,0).unwrap().last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::LostToRestart } && t.ran == Some(Ran::None))
        );
        let replay = runner.replay(id, 0).unwrap();
        let Some(Frame::Terminal(t)) = replay.last() else {
            panic!("missing recovery terminal")
        };
        assert_eq!(
            t.refusal_detail(),
            Some("runner restarted before the job could start")
        );
        assert_eq!(t.retry_after_ms(), None);
    }
    assert!(
        matches!(runner.replay(cancel,0).unwrap().last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::Cancelled)
    );
    assert_eq!(runner.status().queue_depth, 0);
    assert!(!Path::new(&f.key).join("marker").exists());
}

#[tokio::test]
async fn drain_refuses_new_and_queued_jobs_without_starting_queued_commands() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let marker = f.scratch.join("drain-marker");
    let (first, a) = f.run(&format!(
        "while [ ! -f release ]; do sleep 0.01; done; echo first >> '{}'",
        marker.display()
    ));
    let (second, b) = f.run(&format!("echo second >> '{}'", marker.display()));
    f.submit(&runner, &first, &a);
    wait_for(|| {
        runner.core.inner.lock().unwrap().jobs[&first.job_id]
            .started
            .is_some()
    })
    .await;
    f.submit(&runner, &second, &b);
    assert_eq!(runner.job_status(second.job_id).unwrap().state, "queued");
    runner.begin_drain(Duration::from_secs(30), true);
    let (new, _) = f.run("echo forbidden >> marker");
    assert!(
        matches!(runner.accept(new.clone()).unwrap(), Admission::Refused(reason) if String::from(reason.clone()) == "runner_draining")
    );
    let refused = terminal(&runner, new.job_id).await;
    assert_eq!(refused.ran, Some(Ran::None));
    assert_eq!(runner.drain_result(), None);
    let refused = terminal(&runner, second.job_id).await;
    assert_eq!(refused.ran, Some(Ran::None));
    assert_eq!(
        refused.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDraining
        }
    );
    let persisted: Job = read_json(&runner.record_path(second.job_id)).unwrap();
    assert_eq!(persisted.terminal, Some(refused));
    fs::write(Path::new(&f.key).join("release"), "").unwrap();
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    wait_for(|| runner.drain_result() == Some(false)).await;
    assert!(runner.drain_should_exit());
    assert_eq!(fs::read_to_string(marker).unwrap(), "first\n");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn drain_completes_promptly_with_a_started_unit_backed_job() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = start_long_running_job(&f, &runner).await;
    let process = {
        // Model an isolated job's persisted unit identity without requiring
        // systemd privileges. The operator restart test covers real reattach.
        let mut inner = runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&id).unwrap();
        let process = job.process;
        job.process.as_mut().unwrap().unit = Some(id);
        runner.save(job).unwrap();
        process
    };
    let persisted: Job = read_json(&runner.record_path(id)).unwrap();
    assert!(persisted.started.is_some());
    assert!(persisted.started_ms.is_some());
    assert_eq!(persisted.process.unwrap().unit, Some(id));
    let (socket, server) = listener(&f, &runner);
    let mut channel = open_drain(&socket, 1200, false).await;
    let reply = tokio::time::timeout(Duration::from_secs(2), read_next(&mut channel)).await;
    server.abort();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&id).unwrap();
        assert!(
            job.terminal.is_none(),
            "drain must not wait for the command to exit"
        );
        job.process = process;
        runner.save(job).unwrap();
    }
    runner.cancel(id).unwrap();
    assert_eq!(
        reply.expect("recoverable jobs must not hold the drain"),
        Frame::DrainReply {
            timed_out: false,
            restart_recoverable_jobs: Some(1),
        }
    );
    assert!(runner.is_draining());
    assert!(!runner.drain_should_exit());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_waits_for_a_job_still_preparing() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    hold.arm();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    reached(&hold).await;
    runner.begin_drain(Duration::from_secs(30), false);
    settle().await;
    assert_eq!(
        runner.drain_result(),
        None,
        "setup cannot reattach after a restart"
    );
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .starting
        .contains(&request.job_id));
    hold.release();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    wait_for(|| runner.drain_result() == Some(false)).await;
}

#[test]
fn drain_deadline_bounds_an_incomplete_upload_and_hold_keeps_admission_closed() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (request, _) = f.run("true");
    runner.accept(request).unwrap();
    runner.begin_drain(Duration::ZERO, false);
    {
        let _held = runner.core.inner.lock().unwrap();
        assert_eq!(
            runner.drain_result(),
            Some(true),
            "deadline must remain observable while state persistence holds the lock"
        );
    }
    assert!(!runner.drain_should_exit());
    let (request, _) = f.run("true");
    assert!(
        matches!(runner.accept(request).unwrap(), Admission::Refused(reason) if String::from(reason.clone()) == "runner_draining")
    );
}

#[test]
fn exit_after_drain_observes_deadline_without_a_client_request() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    runner.begin_drain(Duration::ZERO, true);

    assert!(runner.drain_should_exit());
}

async fn wait_for(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("lifecycle condition timed out");
}

async fn start_long_running_job(f: &Fixture, runner: &Runner) -> Uuid {
    let (request, bytes) = f.run("sleep 30");
    let job_id = request.job_id;
    f.submit(runner, &request, &bytes);
    wait_for(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|job| job.job_id == job_id)
    })
    .await;
    job_id
}

async fn open_drain(socket: &Path, deadline_s: u64, exit_after: bool) -> UnixStream {
    let mut channel = connect(socket).await;
    write_frame_async(
        &mut channel,
        &Frame::Drain {
            deadline_s,
            exit_after,
        },
    )
    .await
    .unwrap();
    channel
}

#[tokio::test]
async fn resume_reopens_admission_on_the_owner_admin_path() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    runner.begin_drain(Duration::ZERO, false);
    let (socket, server) = listener(&f, &runner);
    let mut channel = connect(&socket).await;
    write_frame_async(&mut channel, &Frame::Resume)
        .await
        .unwrap();
    assert_eq!(
        read_next(&mut channel).await,
        Frame::ResumeReply { resumed: true }
    );
    assert!(!runner.is_draining());
    assert_eq!(runner.drain_result(), None);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(_)
    ));
    server.abort();
}

#[tokio::test]
async fn concurrent_drains_answer_at_the_earliest_requested_deadline() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    start_long_running_job(&f, &runner).await;
    let (socket, server) = listener(&f, &runner);
    let started = std::time::Instant::now();
    let mut first = open_drain(&socket, 1200, false).await;
    wait_for(|| runner.drain_retry_after_ms().is_some()).await;
    let mut second = open_drain(&socket, 2, true).await;

    let replies = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(read_next(&mut first), read_next(&mut second))
    })
    .await;
    server.abort();

    let (first, second) = replies.expect("both drain requests must be answered");
    assert_eq!(
        first,
        Frame::DrainReply {
            timed_out: true,
            restart_recoverable_jobs: Some(0)
        }
    );
    assert_eq!(
        second,
        Frame::DrainReply {
            timed_out: true,
            restart_recoverable_jobs: Some(0)
        }
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(runner.is_draining());
    // The first request in a drain episode decides exit behavior; later requests
    // can shorten its deadline but cannot change whether the daemon exits.
    assert!(!runner.drain_should_exit());
}

#[tokio::test]
async fn shorter_first_drain_still_answers_later_longer_request() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    start_long_running_job(&f, &runner).await;
    let (socket, server) = listener(&f, &runner);
    let mut first = open_drain(&socket, 2, true).await;
    wait_for(|| runner.drain_retry_after_ms().is_some()).await;
    let mut second = open_drain(&socket, 1200, false).await;

    let replies = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(read_next(&mut first), read_next(&mut second))
    })
    .await;
    server.abort();

    let (first, second) = replies.expect("both drain requests must be answered");
    assert_eq!(
        first,
        Frame::DrainReply {
            timed_out: true,
            restart_recoverable_jobs: Some(0)
        }
    );
    assert_eq!(
        second,
        Frame::DrainReply {
            timed_out: true,
            restart_recoverable_jobs: Some(0)
        }
    );
    assert!(runner.drain_should_exit());
}

#[tokio::test]
async fn resume_answers_a_pending_drain_request() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    start_long_running_job(&f, &runner).await;
    let (socket, server) = listener(&f, &runner);
    let mut drain = open_drain(&socket, 1200, false).await;
    wait_for(|| runner.drain_retry_after_ms().is_some()).await;
    // The second request asks for a shorter deadline, which the runner adopts
    // only once it has registered that request. Waiting for it keeps Resume
    // from arriving first, which would leave the second request to start a
    // fresh drain that nothing ever answers.
    let mut second_drain = open_drain(&socket, 600, true).await;
    wait_for(|| {
        runner
            .drain_retry_after_ms()
            .is_some_and(|ms| ms <= 600_000)
    })
    .await;

    let mut resume = connect(&socket).await;
    write_frame_async(&mut resume, &Frame::Resume)
        .await
        .unwrap();
    assert_eq!(
        read_next(&mut resume).await,
        Frame::ResumeReply { resumed: true }
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), read_next(&mut drain))
            .await
            .expect("resume must release the pending drain request"),
        Frame::DrainReply {
            timed_out: false,
            restart_recoverable_jobs: Some(0)
        }
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), read_next(&mut second_drain))
            .await
            .expect("resume must release every pending drain request"),
        Frame::DrainReply {
            timed_out: false,
            restart_recoverable_jobs: Some(0)
        }
    );
    assert!(!runner.is_draining());

    server.abort();
}

#[cfg(target_os = "linux")]
#[test]
fn surviving_unit_child() {
    let Some(root) = std::env::var_os("MOTOR_REATTACH_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let request: Run = read_json(&root.join("request.json")).unwrap();
    let mut config = Config::new(root.join("state"), "e2e");
    config.owned_workspace_roots = vec![root.clone()];
    config.free_space_floor_bytes = 0;
    config.isolate_jobs = true;
    config.disable_warm_builds = true;
    let runner = Runner::open(config).unwrap();
    runner.accept(request.clone()).unwrap();
    runner
        .fetch_run(&request, &root.join("bundles"), true)
        .unwrap();
    let started = Instant::now();
    while !Path::new(&request.workspace_key).join("marker").exists() {
        let inner = runner.core.inner.lock().unwrap();
        assert!(
            inner.jobs[&request.job_id].terminal.is_none(),
            "launch failed: {:?}",
            inner.jobs[&request.job_id].terminal
        );
        drop(inner);
        assert!(started.elapsed() < Duration::from_secs(30));
        std::thread::park_timeout(Duration::from_millis(25));
    }
    let process = runner.core.inner.lock().unwrap().jobs[&request.job_id]
        .process
        .unwrap();
    // A user-service restart also kills the pipe bridge, not the system job unit.
    unsafe {
        libc::kill(-process.pid, libc::SIGKILL);
    }
    std::process::exit(86);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires host system manager, sudo and motor-job account"]
async fn isolated_restart_reattaches_surviving_unit_and_replays_output() {
    let f = Fixture::new();
    let (request, bytes) = f.run("echo once >> marker; echo before; while [ ! -f release ]; do sleep 0.01; done; echo after; echo error-after >&2; exit 7");
    atomic_json(&f.scratch.join("request.json"), &request).unwrap();
    fs::create_dir_all(f.scratch.join("bundles")).unwrap();
    for (i, bytes) in bytes.iter().enumerate() {
        fs::write(f.scratch.join("bundles").join(i.to_string()), bytes).unwrap();
    }
    let status = crate::test_process::ChildGuard::spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::tests::surviving_unit_child",
                "--nocapture",
            ])
            .env("MOTOR_REATTACH_FIXTURE", f.scratch.path()),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert_eq!(status.code(), Some(86));
    let old: Job = read_json(
        &f.config()
            .state_dir()
            .join("jobs")
            .join(request.job_id.to_string())
            .join("record.json"),
    )
    .unwrap();
    assert_eq!(platform::unit_alive(old.process).unwrap(), Some(true));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.disable_warm_builds = true;
    let runner = reopen(config);
    let recovered = runner.job_status(request.job_id).unwrap();
    assert_eq!(recovered.state, "running");
    assert_eq!(old.cpus.len(), 2);
    {
        let inner = runner.core.inner.lock().unwrap();
        assert_eq!(inner.jobs[&request.job_id].claimed_cpus(), old.cpus);
        let next = runner
            .core
            .cpus
            .place(2, inner.jobs.values().map(Job::claimed_cpus));
        assert!(next.iter().all(|cpu| !old.cpus.contains(cpu)));
    }
    assert_eq!(recovered.unit_alive, Some(true));
    assert_eq!(
        recovered.unit,
        Some(format!(
            "motor-job-{}.service",
            old.process.unwrap().unit.unwrap()
        ))
    );
    // Isolation transfers workspace write access to motor-job, not the operator.
    assert!(Command::new("/usr/bin/sudo")
        .args(["-n", "-u", "motor-job", "/usr/bin/touch", "--"])
        .arg(Path::new(&f.key).join("release"))
        .status()
        .unwrap()
        .success());
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 7 }
    );
    let frames = runner.replay(request.job_id, 0).unwrap();
    let output = |stream| {
        frames
            .iter()
            .filter_map(|f| match f {
                Frame::Output(o) if o.stream == stream => Some(o.bytes.0.clone()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>()
    };
    assert_eq!(output(OutputStream::Stdout), b"before\nafter\n");
    assert_eq!(output(OutputStream::Stderr), b"error-after\n");
    assert_eq!(
        fs::read_to_string(Path::new(&f.key).join("marker")).unwrap(),
        "once\n"
    );
}

#[tokio::test]
async fn retention_uses_terminal_and_mint_time_and_expiry_never_reruns() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (r, bytes) = f.run("echo once >> marker");
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    base_done(&runner).await;
    let finish = runner.core.inner.lock().unwrap().jobs[&r.job_id]
        .finished_ms
        .unwrap()
        .max(mint_ms(r.job_id));
    // Keep the workspace active so its marker remains observable after history expires.
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.catalog.workspaces.get_mut(&f.key).unwrap().used_ms = finish + RETENTION_MS;
    }
    f.clock.store(finish + RETENTION_MS - 1, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(runner.record_path(r.job_id).exists());
    f.clock.store(finish + RETENTION_MS, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(
        matches!(runner.replay(r.job_id,0).unwrap().last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::HistoryExpired && t.ran.is_none())
    );
    assert!(!runner.record_path(r.job_id).exists());
    assert_eq!(
        fs::read_to_string(Path::new(&f.key).join("marker")).unwrap(),
        "once\n"
    );
}

#[test]
fn replay_is_inclusive_durable_and_truncates_at_64_mib() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    // Exercise the real chunk store, not an expected value derived from its eviction predicate.
    let megabyte = vec![b'x'; 1024 * 1024];
    for _ in 0..65 {
        runner.output(id, OutputStream::Stdout, &megabyte).unwrap();
    }
    let mut replay = Vec::new();
    let mut cursor = 0;
    loop {
        let page = runner.replay(id, cursor).unwrap();
        let terminal = page.iter().any(|frame| matches!(frame, Frame::Terminal(_)));
        let next = page
            .iter()
            .filter_map(|frame| match frame {
                Frame::Output(output) => Some(output.seq + 1),
                _ => None,
            })
            .max()
            .unwrap_or(cursor);
        replay.extend(page);
        if terminal {
            break;
        }
        assert!(
            next > cursor,
            "retained pages must advance the inclusive cursor"
        );
        cursor = next;
    }
    assert_eq!(replay.len(), 65);
    assert!(
        matches!(&replay[0], Frame::Output(o) if o.seq == 1 && o.truncated_before_seq == Some(1))
    );
    let replay = runner.replay(id, 7).unwrap();
    assert!(
        matches!(&replay[0], Frame::Output(o) if o.seq == 7 && o.truncated_before_seq.is_none())
    );
    let disk: u64 = fs::read_dir(runner.record_path(id).parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name().to_string_lossy().starts_with("chunk-"))
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert_eq!(disk, 64 * 1024 * 1024);
    drop(runner);
    let runner = reopen(config);
    assert!(
        matches!(runner.replay(id,0).unwrap().first(), Some(Frame::Output(o)) if o.seq == 1 && o.truncated_before_seq == Some(1))
    );
}

#[tokio::test]
async fn inline_run_preserves_paths_targets_siblings_env_and_changes() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut r, bytes) = f.run("cat ../commons/source; echo own > target/own; echo changed > source; echo new > extra; printf '%s/%s' \"$RUST_BACKTRACE\" \"${ANTHROPIC_API_KEY-unset}\"");
    r.env.insert("RUST_BACKTRACE".into(), "1".into());
    r.env.insert("ANTHROPIC_API_KEY".into(), "bad".into());
    r.env.insert("PATH".into(), "/nope".into());
    r.env.insert("CARGO_TARGET_DIR".into(), "/nope".into());
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 0 });
    assert_eq!(t.ran, Some(Ran::Remote));
    assert_eq!(t.tree_hash, Some(r.tree_hash.clone()));
    assert!(t
        .workspace_changes
        .as_ref()
        .unwrap()
        .contains(&"extra".to_string()));
    let output: Vec<_> = runner
        .replay(r.job_id, 0)
        .unwrap()
        .into_iter()
        .filter_map(|f| {
            if let Frame::Output(o) = f {
                Some(o.bytes.0)
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert_eq!(String::from_utf8(output).unwrap(), "first\n1/unset");
    assert_eq!(
        fs::canonicalize(Path::new(&f.key).parent().unwrap().join("commons")).unwrap(),
        fs::canonicalize(&f.canonical).unwrap()
    );
    base_done(&runner).await;
    let (mut second, bytes) = f.run("test ! -e extra && test -e target/own && cat source");
    second.workspace_key = f
        .scratch
        .join("remote/worktrees/b")
        .to_string_lossy()
        .to_string();
    second.cwd = second.workspace_key.clone();
    // A fresh workspace starts empty, never with another workspace's outputs.
    second.command = "test ! -e target/own && test ! -e target/artifact && cat source".into();
    f.submit(&runner, &second, &bytes);
    assert_eq!(
        terminal(&runner, second.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let (third, bytes) = f.run("test ! -e extra && test -e target/own && cat source");
    f.submit(&runner, &third, &bytes);
    assert_eq!(
        terminal(&runner, third.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert!(runner.drop_workspace(&f.key).await.unwrap().dropped);
    assert!(!Path::new(&f.key).exists());
    assert!(!runner.drop_workspace(&f.key).await.unwrap().dropped);
}

#[tokio::test]
async fn remote_jobs_preserve_explicit_test_caps() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut r, bytes) =
        f.run("printf '%s/%s' \"${RUST_TEST_THREADS-unset}\" \"${NEXTEST_TEST_THREADS-unset}\"");
    r.env.insert("RUST_TEST_THREADS".into(), "4".into());
    r.env.insert("NEXTEST_TEST_THREADS".into(), "4".into());
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let output: Vec<_> = runner
        .replay(r.job_id, 0)
        .unwrap()
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) => Some(output.bytes.0),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(String::from_utf8(output).unwrap(), "4/4");
    assert_eq!(runner.core.inner.lock().unwrap().jobs[&r.job_id].weight, 2);
}

#[tokio::test]
async fn remote_jobs_default_test_caps_to_granted_permits_not_daemon_caps() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let _daemon_env = ScopedEnv::set(&[
        ("RUST_TEST_THREADS", "111"),
        ("NEXTEST_TEST_THREADS", "222"),
    ]);
    let (mut r, bytes) = f.run("printf '%s/%s' \"$RUST_TEST_THREADS\" \"$NEXTEST_TEST_THREADS\"");
    r.weight_hint = Some(6);
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let output: Vec<_> = runner
        .replay(r.job_id, 0)
        .unwrap()
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) => Some(output.bytes.0),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(String::from_utf8(output).unwrap(), "6/6");
}

#[tokio::test]
async fn cargo_build_jobs_defaults_to_permits_and_preserves_explicit_values() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let _daemon_env = ScopedEnv::set(&[("CARGO_BUILD_JOBS", "99")]);
    for (explicit, expected, weight) in [(None, "6", 6), (Some("3"), "3", 6), (Some("11"), "11", 6)]
    {
        let (mut r, bytes) = f.run("printf '%s' \"$CARGO_BUILD_JOBS\"");
        r.weight_hint = Some(6);
        if let Some(value) = explicit {
            r.env.insert("CARGO_BUILD_JOBS".into(), value.into());
        }
        f.submit(&runner, &r, &bytes);
        assert_eq!(
            terminal(&runner, r.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        let output: Vec<_> = runner
            .replay(r.job_id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|frame| match frame {
                Frame::Output(output) => Some(output.bytes.0),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(String::from_utf8(output).unwrap(), expected);
        assert_eq!(
            runner.core.inner.lock().unwrap().jobs[&r.job_id].weight,
            weight
        );
    }
}

#[tokio::test]
async fn jobs_receive_only_allowlisted_daemon_environment() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (r, bytes) = f.run(
        r#"printf '%s\n' "${MOTOR_TEST_DAEMON_SECRET-unset}" "$PATH" "$HOME" "${SUBC_LAUNCH_NONCE-unset}" "${SUBC_LAUNCH_NONCE_FD-unset}" "${SUBC_MODULE_ID-unset}" "${SSH_AUTH_SOCK-unset}""#,
    );
    let daemon_path = std::env::var("PATH").unwrap();
    let daemon_home = std::env::var("HOME").unwrap();
    let _daemon_env = ScopedEnv::set(&[
        ("MOTOR_TEST_DAEMON_SECRET", "x"),
        ("SUBC_LAUNCH_NONCE", "bad"),
        ("SUBC_LAUNCH_NONCE_FD", "bad"),
        ("SUBC_MODULE_ID", "bad"),
        ("SSH_AUTH_SOCK", "bad"),
    ]);
    f.submit(&runner, &r, &bytes);
    let terminal = terminal(&runner, r.job_id).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
    let output: Vec<_> = runner
        .replay(r.job_id, 0)
        .unwrap()
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) => Some(output.bytes.0),
            _ => None,
        })
        .flatten()
        .collect();
    let output = String::from_utf8(output).unwrap();
    assert_eq!(
        output,
        format!("unset\n{daemon_path}\n{daemon_home}\nunset\nunset\nunset\nunset\n")
    );
}

#[tokio::test]
async fn hash_mismatch_and_materialization_failure_never_start() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut r, bytes) = f.run("touch marker");
    let computed = r.tree_hash.clone();
    r.tree_hash = "bad".into();
    f.submit(&runner, &r, &bytes);
    let record = terminal(&runner, r.job_id).await;
    assert_eq!(
        record.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TreeHashMismatch
        }
    );
    let detail = record.refusal_detail().expect("missing mismatch detail");
    assert!(detail.contains(&computed), "{detail}");
    assert!(detail.contains("unavailable"), "{detail}");
    assert!(!Path::new(&f.key).join("marker").exists());
    base_done(&runner).await;
    drop(runner);
    let mut c = f.config();
    c.fail_materialize = true;
    let runner = reopen(c);
    let (r, bytes) = f.run("touch marker");
    f.submit(&runner, &r, &bytes);
    let record = terminal(&runner, r.job_id).await;
    assert_eq!(
        record.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceSetupFailed
        }
    );
    let detail = record
        .refusal_detail()
        .expect("missing materialization detail");
    assert!(detail.starts_with("prepare job workspace: "), "{detail}");
    assert!(detail.contains("materialize workspace"), "{detail}");
    assert!(
        detail.contains("injected materialization failure"),
        "{detail}"
    );
    assert_eq!(runner.status().queue_depth, 0);
    assert!(!Path::new(&f.key).join("marker").exists());
}

async fn connect(socket: &Path) -> UnixStream {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    write_frame_async(
        &mut stream,
        &Frame::Hello {
            return_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        read_next(&mut stream).await,
        Frame::Hello {
            return_paths: true,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        }
    );
    stream
}
async fn read_next(stream: &mut UnixStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let frame = read_frame_async(stream)
                .await
                .unwrap()
                .expect("channel ended");
            if !matches!(frame, Frame::Heartbeat { .. }) {
                return frame;
            }
        }
    })
    .await
    .unwrap()
}
async fn wire_terminal(stream: &mut UnixStream) -> TerminalRecord {
    loop {
        if let Frame::Terminal(t) = read_next(stream).await {
            return t;
        }
    }
}
struct SocketPath(PathBuf);
impl std::ops::Deref for SocketPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn listener(
    _f: &Fixture,
    runner: &Runner,
) -> (SocketPath, tokio::task::JoinHandle<io::Result<()>>) {
    // The process scratch directory includes a long kernel start-time identity.
    // Keep Unix socket names directly under the shared root to fit sun_path.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let socket = cortexkit_test_support::scratch_root()
        .parent()
        .unwrap()
        .join(format!(
            "m{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    let listener = UnixListener::bind(&socket).unwrap();
    (
        SocketPath(socket),
        tokio::spawn(crate::serve(runner.clone(), listener)),
    )
}
async fn chunks(stream: &mut UnixStream, bytes: &[Vec<u8>]) {
    let mut random = StdRng::seed_from_u64(200);
    for (index, data) in bytes.iter().enumerate() {
        let mut offset = 0;
        while offset < data.len() {
            let length = random.gen_range(1..=97).min(data.len() - offset);
            write_frame_async(
                stream,
                &Frame::Heartbeat {
                    queue_wait_ms: None,
                },
            )
            .await
            .unwrap();
            write_frame_async(
                stream,
                &Frame::BundleChunk {
                    index: index as u32,
                    offset: offset as u64,
                    data: BytePayload(data[offset..offset + length].to_vec()),
                },
            )
            .await
            .unwrap();
            offset += length;
        }
    }
}

#[tokio::test]
async fn two_bundle_wire_spools_before_any_fetch_and_verified_detach_runs() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (socket, server) = listener(&f, &runner);
    let (r, bytes) = f.run("echo once >> marker");
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Run(r.clone()))
        .await
        .unwrap();
    assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
    write_frame_async(
        &mut stream,
        &Frame::BundleChunk {
            index: 0,
            offset: 0,
            data: BytePayload(bytes[0].clone()),
        },
    )
    .await
    .unwrap();
    until(|| {
        fs::metadata(runner.spool_dir(r.job_id).join("0"))
            .is_ok_and(|m| m.len() == bytes[0].len() as u64)
            || runner.core.inner.lock().unwrap().jobs[&r.job_id]
                .terminal
                .is_some()
    })
    .await;
    // The first spool is present, but the second bundle is deliberately held
    // back. Other channels must remain serviceable during the upload.
    let mut status = connect(&socket).await;
    write_frame_async(&mut status, &Frame::Status)
        .await
        .unwrap();
    assert!(matches!(
        read_next(&mut status).await,
        Frame::StatusReply(_)
    ));
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&r.job_id].state,
        JobState::AwaitingBundles
    );
    assert!(!runner.core.trees.mirror(&f.root).exists());
    assert!(!runner.core.trees.mirror(&f.canonical).exists());
    write_frame_async(
        &mut stream,
        &Frame::BundleChunk {
            index: 1,
            offset: 0,
            data: BytePayload(bytes[1].clone()),
        },
    )
    .await
    .unwrap();
    drop(stream);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Attach(AttachRequest::new(r.job_id, 0)))
        .await
        .unwrap();
    assert_eq!(
        wire_terminal(&mut stream).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        fs::read_to_string(Path::new(&f.key).join("marker")).unwrap(),
        "once\n"
    );
    base_done(&runner).await;
    server.abort();
}

/// Blocking client half of the wire, so the test itself never depends on the
/// runner's async runtime making progress.
fn blocking_frame(stream: &mut std::os::unix::net::UnixStream) -> io::Result<Frame> {
    loop {
        match read_frame(stream)? {
            Some(Frame::Heartbeat { .. }) => continue,
            Some(frame) => return Ok(frame),
            None => return Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
}
fn blocking_hello(socket: &Path) -> io::Result<std::os::unix::net::UnixStream> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    write_frame(
        &mut stream,
        &Frame::Hello {
            return_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        },
    )?;
    match blocking_frame(&mut stream)? {
        Frame::Hello { .. } => Ok(stream),
        other => Err(io::Error::other(format!("expected hello, got {other:?}"))),
    }
}

/// The runner's state lock is a blocking mutex, held however briefly by every
/// request. A session that waits for that lock on an async worker parks the
/// worker, and once the workers are parked the runner stops
/// answering new connections: ck-motor then refuses the job as unreachable
/// after its 15-second handshake bound. Every request kind that needs the lock
/// must therefore wait for it off the async workers, and a new channel's Hello
/// must be answered while the lock is still held.
///
/// The runtime here has a single worker so that one parked worker is enough
/// to show the stall; production has one worker per core, which only moves
/// the threshold.
#[test]
fn hello_is_answered_while_a_locked_request_waits_for_the_state_lock() {
    let f = Fixture::new();
    let (run, _) = f.run("true");
    let unknown = Uuid::now_v7();
    let requests = [
        ("status", Frame::Status),
        ("cancel", Frame::Cancel(CancelRequest::new(unknown))),
        (
            "withdraw",
            Frame::Withdraw {
                job_id: unknown,
                reason: None,
            },
        ),
        ("drop", Frame::Drop(DropRequest::new(f.key.clone()))),
        ("run", Frame::Run(run)),
    ];
    let mut stalled = Vec::new();
    for (name, request) in requests {
        let runner = reopen(f.config());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (socket, _server) = {
            let _context = runtime.enter();
            listener(&f, &runner)
        };
        let mut waiting = blocking_hello(&socket).unwrap();
        let held = runner.core.inner.lock().unwrap();
        write_frame(&mut waiting, &request).unwrap();
        // Give the session time to reach the lock before probing.
        std::thread::sleep(Duration::from_millis(300));
        if let Err(error) = blocking_hello(&socket) {
            stalled.push(format!("{name}: {error}"));
        }
        drop(held);
        drop(waiting);
        runtime.shutdown_timeout(Duration::from_secs(5));
        drop(runner);
    }
    assert!(
        stalled.is_empty(),
        "a new channel's hello waited behind the state lock for: {stalled:?}"
    );
}

#[tokio::test]
async fn truncated_upload_tombstones_without_fetch_and_replayed_run_is_ignored() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (socket, server) = listener(&f, &runner);
    let (r, bytes) = f.run("touch marker");
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Run(r.clone()))
        .await
        .unwrap();
    assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
    write_frame_async(
        &mut stream,
        &Frame::BundleChunk {
            index: 0,
            offset: 0,
            data: BytePayload(bytes[0].clone()),
        },
    )
    .await
    .unwrap();
    write_frame_async(
        &mut stream,
        &Frame::BundleChunk {
            index: 1,
            offset: 0,
            data: BytePayload(bytes[1][..bytes[1].len() / 2].to_vec()),
        },
    )
    .await
    .unwrap();
    drop(stream);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TransferInterrupted
        }
    );
    assert!(!runner.core.trees.mirror(&f.root).exists());
    assert!(!runner.core.trees.mirror(&f.canonical).exists());
    assert_eq!(runner.status().queue_depth, 0);
    let before = fs::read(runner.record_path(r.job_id)).unwrap();
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Run(r.clone()))
        .await
        .unwrap();
    while matches!(
        read_frame_async(&mut stream).await.unwrap(),
        Some(Frame::Heartbeat {
            queue_wait_ms: None
        })
    ) {}
    assert_eq!(before, fs::read(runner.record_path(r.job_id)).unwrap());
    server.abort();
}

#[tokio::test]
async fn overrun_gap_and_digest_mismatch_reject_run_and_prepare() {
    for mode in ["overrun", "gap", "digest"] {
        let f = Fixture::new();
        let runner = Runner::open(f.config()).unwrap();
        let (socket, server) = listener(&f, &runner);
        let (mut r, mut bytes) = f.run("touch marker");
        if mode == "overrun" {
            bytes[0].push(1);
        } else if mode == "digest" {
            r.bundles[0].sha256 = "0".repeat(64);
        }
        let mut stream = connect(&socket).await;
        write_frame_async(&mut stream, &Frame::Run(r.clone()))
            .await
            .unwrap();
        assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
        write_frame_async(
            &mut stream,
            &Frame::BundleChunk {
                index: 0,
                offset: if mode == "gap" { 1 } else { 0 },
                data: BytePayload(bytes[0].clone()),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            wire_terminal(&mut stream).await.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::BundleRejected
            }
        );
        assert!(!runner.core.trees.mirror(&f.root).exists());
        let prepare = Prepare {
            workspace_kind: None,
            transfer_id: Uuid::now_v7(),
            workspace_key: r.workspace_key,
            repository_root: r.repository_root,
            base_commit: r.snapshot_commit,
            integration_tip: r.integration_tip,
            siblings: r.siblings,
            bundles: r.bundles,
            git_dependencies: vec![],
        };
        let mut stream = connect(&socket).await;
        write_frame_async(&mut stream, &Frame::Prepare(prepare))
            .await
            .unwrap();
        assert!(matches!(
            read_next(&mut stream).await,
            Frame::PrepareAccepted { .. }
        ));
        write_frame_async(
            &mut stream,
            &Frame::BundleChunk {
                index: 0,
                offset: if mode == "gap" { 1 } else { 0 },
                data: BytePayload(bytes[0].clone()),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_next(&mut stream).await,Frame::Refusal(r) if r.reason==RefusalReason::BundleRejected)
        );
        assert!(!runner.core.trees.mirror(&f.root).exists());
        assert_eq!(runner.status().queue_depth, 0);
        server.abort();
    }
}

#[tokio::test]
async fn randomized_prepare_transcript_and_have_only_advertise_held_commits() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (socket, server) = listener(&f, &runner);
    let (r, bytes) = f.run("true");
    let prepare = Prepare {
        workspace_kind: None,
        transfer_id: Uuid::now_v7(),
        workspace_key: r.workspace_key.clone(),
        repository_root: r.repository_root.clone(),
        base_commit: r.snapshot_commit.clone(),
        integration_tip: r.integration_tip.clone(),
        siblings: r.siblings.clone(),
        bundles: r.bundles.clone(),
        git_dependencies: vec![],
    };
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Prepare(prepare.clone()))
        .await
        .unwrap();
    assert_eq!(
        read_next(&mut stream).await,
        Frame::PrepareAccepted {
            transfer_id: prepare.transfer_id
        }
    );
    chunks(&mut stream, &bytes).await;
    assert_eq!(
        read_next(&mut stream).await,
        Frame::Prepared {
            transfer_id: prepare.transfer_id
        }
    );
    let held = runner.have(vec![RepositoryCandidates {
        repository: f.root.clone(),
        commits: vec![r.snapshot_commit.clone(), "0".repeat(40)],
    }]);
    assert_eq!(held[0].commits, vec![r.snapshot_commit]);
    base_done(&runner).await;
    assert_eq!(fs::read_dir(&runner.core.trees.mirrors).unwrap().count(), 2);
    server.abort();
}

#[tokio::test]
async fn failed_fetch_or_absent_snapshot_rejects_without_held_refs() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut r, bytes) = f.run("touch marker");
    r.snapshot_commit = "0".repeat(40);
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::BundleRejected
        }
    );
    assert!(runner.have(vec![RepositoryCandidates {
        repository: f.root.clone(),
        commits: vec![r.integration_tip]
    }])[0]
        .commits
        .is_empty());
    assert!(!Path::new(&f.key).join("marker").exists());
}

async fn warm(f: &Fixture, runner: &Runner) {
    let (r, bytes) = f.run("true");
    f.submit(runner, &r, &bytes);
    let result = terminal(runner, r.job_id).await;
    assert_eq!(
        result.outcome,
        Outcome::Exit { code: 0 },
        "warmup job failed: {result:?}; {}",
        runner_diagnostics(runner)
    );
    base_done(runner).await;
}

#[tokio::test]
async fn three_default_rust_jobs_run_concurrently_within_28_permits() {
    let f = Fixture::new();
    let release = f.scratch.join("release-default-jobs");
    // Real, dependency-free Cargo jobs keep their test processes alive until
    // the scheduler has admitted all three, rather than measuring setup alone.
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname = \"default-job\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = \"default-job\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), format!(
        "#[test]\nfn waits_for_release() {{ while !std::path::Path::new({:?}).exists() {{ std::thread::sleep(std::time::Duration::from_millis(10)); }} }}\n", release.to_str().unwrap())).unwrap();
    commit(&f.main, "default Cargo jobs\n");
    let runner = Runner::open(f.config()).unwrap();
    assert_eq!(runner.config().max_weight, 28);
    warm(&f, &runner).await;
    let mut ids = Vec::new();
    for name in ["default-one", "default-two", "default-three"] {
        let (r, bytes) = job_at(&f, name, "cargo test --locked");
        assert!(r.weight_hint.is_none());
        f.submit(&runner, &r, &bytes);
        ids.push(r.job_id);
    }
    let concurrent = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let running = runner.status().running_jobs;
            if ids
                .iter()
                .all(|id| running.iter().any(|job| job.job_id == *id))
            {
                break running;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    // Release and reap even when admission regresses, so a red test leaves no
    // held Cargo processes behind on the shared runner.
    fs::write(release, "").unwrap();
    for id in &ids {
        assert_eq!(
            terminal(&runner, *id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
    let running = concurrent.expect("three default Rust jobs did not run concurrently");
    assert_eq!(running.len(), 3);
    assert!(running.iter().all(|job| job.weight == 2));
    assert_eq!(running.iter().map(|job| job.weight).sum::<u32>(), 6);
}

#[tokio::test]
async fn fairness_rotates_never_dispatched_keys_before_a_backlog() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let release = f.scratch.join("release");
    let log = f.scratch.join("dispatch.log");
    let (mut first, bytes) = f.run(&format!(
        "echo A1 >> '{}'; while test ! -e '{}'; do sleep .02; done",
        log.display(),
        release.display()
    ));
    first.weight_hint = Some(16);
    f.submit(&runner, &first, &bytes);
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|j| j.job_id == first.job_id)
    })
    .await;
    let mut a_ids = Vec::new();
    for _ in 0..3 {
        let mut a = first.clone();
        a.job_id = Uuid::now_v7();
        a.bundles.clear();
        a.command = format!("echo A >> '{}'", log.display());
        f.submit(&runner, &a, &[]);
        a_ids.push(a.job_id);
    }
    let mut b = first.clone();
    b.job_id = Uuid::now_v7();
    b.workspace_key = f
        .scratch
        .join("remote/worktrees/b")
        .to_string_lossy()
        .into();
    b.cwd = b.workspace_key.clone();
    b.bundles.clear();
    b.command = format!("echo B >> '{}'", log.display());
    f.submit(&runner, &b, &[]);
    assert_eq!(runner.status().queue_depth, 4);
    fs::write(release, "").unwrap();
    for id in std::iter::once(first.job_id)
        .chain(std::iter::once(b.job_id))
        .chain(a_ids)
    {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
    assert_eq!(fs::read_to_string(log).unwrap(), "A1\nB\nA\nA\nA\n");
}

#[tokio::test]
async fn capped_head_allows_backfill_only_until_its_grace_expires() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let release = f.scratch.join("release");
    let cheap_marker = f.scratch.join("cheap");
    let (mut a, bytes) = f.run(&format!(
        "while test ! -e '{}'; do sleep .02; done",
        release.display()
    ));
    a.weight_hint = Some(20);
    f.submit(&runner, &a, &bytes);
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|j| j.job_id == a.job_id)
    })
    .await;
    let mut b = a.clone();
    b.job_id = Uuid::now_v7();
    b.workspace_key = f
        .scratch
        .join("remote/worktrees/b")
        .to_string_lossy()
        .into();
    b.cwd = b.workspace_key.clone();
    b.bundles.clear();
    b.command = "true".into();
    b.weight_hint = Some(16);
    f.submit(&runner, &b, &[]);
    let mut c = b.clone();
    c.job_id = Uuid::now_v7();
    c.workspace_key = f
        .scratch
        .join("remote/worktrees/c")
        .to_string_lossy()
        .into();
    c.cwd = c.workspace_key.clone();
    c.weight_hint = None;
    c.command = format!("touch '{}'", cheap_marker.display());
    f.submit(&runner, &c, &[]);
    assert_eq!(
        terminal(&runner, c.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert!(
        cheap_marker.exists(),
        "fresh heavy head must not idle spare permits"
    );
    f.clock.fetch_add(BACKFILL_GRACE_MS, Ordering::SeqCst);
    let mut d = c.clone();
    d.job_id = Uuid::now_v7();
    d.workspace_key = f
        .scratch
        .join("remote/worktrees/d")
        .to_string_lossy()
        .into();
    d.cwd = d.workspace_key.clone();
    let late_marker = f.scratch.join("late");
    d.command = format!("touch '{}'", late_marker.display());
    f.submit(&runner, &d, &[]);
    runner.tick().unwrap();
    settle().await;
    assert!(
        !begun(&runner, d.job_id),
        "aged head must drain permits, not starve behind backfill"
    );
    assert!(!late_marker.exists());
    assert_eq!(runner.status().running_jobs.len(), 1);
    fs::write(release, "").unwrap();
    for id in [a.job_id, b.job_id, d.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
}

#[tokio::test]
async fn aged_workspace_head_precedes_never_dispatched_workspaces() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let release_holder = f.scratch.join("holder-release");
    let release_waiters = f.scratch.join("waiters-release");
    let (mut holder, bytes) = job_at(
        &f,
        "holder",
        &format!(
            "while test ! -e '{}'; do sleep .02; done",
            release_holder.display()
        ),
    );
    holder.weight_hint = Some(20);
    f.submit(&runner, &holder, &bytes);
    until(|| begun(&runner, holder.job_id)).await;
    let (mut old, bytes) = f.run(&format!(
        "while test ! -e '{}'; do sleep .02; done",
        release_waiters.display()
    ));
    old.weight_hint = Some(16);
    f.submit(&runner, &old, &bytes);
    f.clock.fetch_add(BACKFILL_GRACE_MS, Ordering::SeqCst);
    let (mut newcomer, bytes) = job_at(&f, "newcomer", &old.command);
    newcomer.weight_hint = Some(16);
    f.submit(&runner, &newcomer, &bytes);
    fs::write(release_holder, "").unwrap();
    until(|| begun(&runner, old.job_id) || begun(&runner, newcomer.job_id)).await;
    let old_won = begun(&runner, old.job_id);
    let new_won = begun(&runner, newcomer.job_id);
    fs::write(release_waiters, "").unwrap();
    for id in [holder.job_id, old.job_id, newcomer.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
    assert!(
        old_won && !new_won,
        "rotation starved the aged workspace head behind a new workspace"
    );
}

#[tokio::test]
async fn impossible_demand_does_not_fence_other_workspaces() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let (mut impossible, bytes) = f.run("touch impossible");
    impossible.weight_hint = Some(30);
    impossible.queue_wait_limit_s = BACKFILL_GRACE_MS / 1000;
    f.submit(&runner, &impossible, &bytes);
    f.clock.fetch_add(BACKFILL_GRACE_MS, Ordering::SeqCst);
    let (cheap, bytes) = job_at(&f, "cheap", "true");
    f.submit(&runner, &cheap, &bytes);
    assert_eq!(
        terminal(&runner, cheap.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    runner.withdraw(impossible.job_id).unwrap();
    let record = terminal(&runner, impossible.job_id).await;
    assert_eq!(
        record.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::QueueWaitExceeded
        }
    );
    let detail = record
        .refusal_detail()
        .expect("missing capacity diagnostic");
    assert!(
        detail.contains("waiting_on=demand exceeds capacity"),
        "{detail}"
    );
    assert!(detail.contains("requested_permits=30"), "{detail}");
    assert!(!Path::new(&impossible.workspace_key)
        .join("impossible")
        .exists());
}

#[tokio::test]
async fn queue_wait_refusal_names_permit_holders_and_their_start_time() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let release = f.scratch.join("release");
    let (mut holder, bytes) = job_at(
        &f,
        "holder",
        &format!(
            "while test ! -e '{}'; do sleep .02; done",
            release.display()
        ),
    );
    holder.weight_hint = Some(20);
    f.submit(&runner, &holder, &bytes);
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|j| j.job_id == holder.job_id)
    })
    .await;
    let started = runner.core.inner.lock().unwrap().jobs[&holder.job_id]
        .started_ms
        .unwrap();
    let (mut waiting, bytes) = f.run("touch must-not-run");
    waiting.weight_hint = Some(16);
    f.submit(&runner, &waiting, &bytes);
    f.clock.fetch_add(300_000, Ordering::SeqCst);
    assert_eq!(
        runner.withdraw(waiting.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    let record = terminal(&runner, waiting.job_id).await;
    fs::write(release, "").unwrap();
    assert_eq!(
        terminal(&runner, holder.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        record.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::QueueWaitExceeded
        }
    );
    let detail = record.refusal_detail().expect("missing queue diagnostic");
    assert_eq!(record.ran, Some(Ran::None));
    assert_eq!(record.retry_after_ms(), None);
    assert!(detail.starts_with("waiting_on=permits"), "{detail}");
    assert!(
        detail.contains("requested_permits=16; held_permits=20/28"),
        "{detail}"
    );
    // Refusal details redact runner-internal paths, and a fixture's scratch can
    // live under the runner's state root, so match the workspace by its
    // fixture-relative tail rather than its full path.
    let holder_workspace = Path::new(&holder.workspace_key);
    let tail = holder_workspace
        .strip_prefix(holder_workspace.ancestors().nth(3).unwrap())
        .unwrap()
        .display()
        .to_string();
    assert!(
        detail.contains(&format!("job={} workspace=", holder.job_id)),
        "{detail}"
    );
    assert!(
        detail.contains(&format!(
            "{tail} permits=20 phase=running since_ms={started}"
        )),
        "{detail}"
    );
    assert_eq!(record.queue_wait_ms, 300_000);
    assert!(!Path::new(&waiting.workspace_key)
        .join("must-not-run")
        .exists());
}

/// A job that would fit, held behind an older head that is past its backfill
/// grace and still waiting for enough free permits, names that head as the
/// reason instead of a generic queue label.
#[tokio::test]
async fn queue_wait_refusal_names_the_aged_head_that_blocks_a_fitting_job() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let release = f.scratch.join("release");
    let (mut holder, bytes) = job_at(
        &f,
        "holder",
        &format!(
            "while test ! -e '{}'; do sleep .02; done",
            release.display()
        ),
    );
    holder.weight_hint = Some(20);
    f.submit(&runner, &holder, &bytes);
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|j| j.job_id == holder.job_id)
    })
    .await;
    let (mut head, bytes) = f.run("true");
    head.weight_hint = Some(16);
    f.submit(&runner, &head, &bytes);
    f.clock.fetch_add(61_000, Ordering::SeqCst);
    let (mut small, bytes) = job_at(&f, "small", "touch must-not-run");
    small.weight_hint = Some(4);
    f.submit(&runner, &small, &bytes);
    f.clock.fetch_add(300_000, Ordering::SeqCst);
    assert_eq!(
        runner.withdraw(small.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    let record = terminal(&runner, small.job_id).await;
    assert_eq!(
        runner.withdraw(head.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    fs::write(release, "").unwrap();
    terminal(&runner, holder.job_id).await;
    let detail = record.refusal_detail().expect("missing queue diagnostic");
    assert!(
        detail.starts_with(&format!(
            "waiting_on=older job ahead needs more free permits; blocked_by=job={} permits=16;",
            head.job_id
        )),
        "{detail}"
    );
    assert!(
        detail.contains("requested_permits=4; held_permits=20/28"),
        "{detail}"
    );
    assert!(!Path::new(&small.workspace_key)
        .join("must-not-run")
        .exists());
}

#[tokio::test]
async fn conflicting_sibling_snapshots_serialize_and_stay_immutable() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let release = f.scratch.join("release");
    let seen = f.scratch.join("seen");
    let other = f.scratch.join("other");
    let (a,bytes)=f.run(&format!("cat ../commons/source >> '{}'; while test ! -e '{}'; do sleep .02; done; cat ../commons/source >> '{}'",seen.display(),release.display(),seen.display()));
    f.submit(&runner, &a, &bytes);
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|j| j.job_id == a.job_id)
    })
    .await;
    commit(&f.sibling, "second\n");
    let (mut b, bytes) = f.run(&format!("cat ../commons/source > '{}'", other.display()));
    b.workspace_key = f
        .scratch
        .join("remote/worktrees/b")
        .to_string_lossy()
        .into();
    b.cwd = b.workspace_key.clone();
    f.submit(&runner, &b, &bytes);
    runner.tick().unwrap();
    assert!(!other.exists());
    assert_eq!(
        fs::read_to_string(Path::new(&f.canonical).join("source")).unwrap(),
        "first\n"
    );
    fs::write(release, "").unwrap();
    for id in [a.job_id, b.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
    assert_eq!(fs::read_to_string(seen).unwrap(), "first\nfirst\n");
    assert_eq!(fs::read_to_string(other).unwrap(), "second\n");
}

#[tokio::test]
async fn writer_failure_keeps_success_status_without_publishing_target_generations() {
    let f = Fixture::new();
    let release = f.scratch.join("release");
    let mut config = f.config();
    config.base_commands = vec![format!("if grep -q fail source; then while test ! -e '{}'; do sleep .02; done; exit 1; fi; printf warm > \"$CARGO_TARGET_DIR/artifact\"", release.display())];
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    let first = runner.status().repositories[0].clone();
    fs::write(f.main.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    commit(&f.main, "fail\n");
    let (r, bytes) = f.run("true");
    f.submit(&runner, &r, &bytes);
    terminal(&runner, r.job_id).await;
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|j| j.workspace_key.starts_with("base:"))
    })
    .await;
    let building = runner.status().repositories[0].clone();
    assert_eq!(building.published_commit, first.published_commit);
    assert_eq!(building.last_rebuild_result, Some(RebuildResult::Building));
    let mut during = prepare_for(&r);
    during.workspace_key = text(&f.scratch.join("remote/worktrees/during")).into();
    runner.prepare(&during, true).unwrap();
    assert!(!Path::new(&during.workspace_key)
        .join("target/artifact")
        .exists());
    fs::write(release, "").unwrap();
    base_done(&runner).await;
    let failed = runner.status().repositories[0].clone();
    assert_eq!(failed.published_commit, first.published_commit);
    assert_eq!(failed.last_rebuild_result, Some(RebuildResult::Failed));
    assert!(!runner.config().state_dir().join("generations").exists());
    assert_eq!(
        fs::read_dir(runner.config().writer_scratch_dir())
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn queued_withdraw_is_atomic_and_running_withdraw_reports_started() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut r, bytes) = f.run("touch marker");
    r.weight_hint = Some(30);
    r.queue_wait_limit_s = 0;
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        runner.withdraw(r.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    let record = terminal(&runner, r.job_id).await;
    assert_eq!(
        record.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::QueueWaitExceeded
        }
    );
    assert!(record.refusal_detail().unwrap().starts_with("waiting_on="));
    assert!(record
        .refusal_detail()
        .unwrap()
        .contains("requested_permits=30"));
    assert!(!Path::new(&f.key).join("marker").exists());
    base_done(&runner).await;
    let release = f.scratch.join("release");
    let (r, bytes) = f.run(&format!(
        "while test ! -e '{}'; do sleep .02; done",
        release.display()
    ));
    f.submit(&runner, &r, &bytes);
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|j| j.job_id == r.job_id)
    })
    .await;
    assert_eq!(runner.withdraw(r.job_id).unwrap(), WithdrawOutcome::Started);
    fs::write(release, "").unwrap();
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
}

// Linux owns the crash-safe fork/gate and process-group kill implementation.
#[cfg(target_os = "linux")]
#[test]
fn launch_gate_child() {
    let Ok(path) = std::env::var("MOTOR_GATE_FIXTURE") else {
        return;
    };
    let path = PathBuf::from(path);
    let r: Run = read_json(&path.join("request.json")).unwrap();
    let mut c = Config::new(path.join("state"), "e2e");
    c.owned_workspace_roots = vec![path.clone()];
    c.free_space_floor_bytes = 0;
    c.base_commands = vec!["true".into()];
    c.before_gate = Some(Arc::new(|| unsafe { libc::_exit(86) }));
    let runner = Runner::open(c).unwrap();
    runner.accept(r.clone()).unwrap();
    runner.fetch_run(&r, &path.join("bundles"), true).unwrap();
    std::thread::park_timeout(Duration::from_secs(10));
    panic!("launch hook not reached");
}

// macOS cannot exercise Linux's EOF launch gate or /proc start-time recovery.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn crash_before_gate_byte_runs_nothing_and_startup_loses_the_record() {
    let f = Fixture::new();
    let (r, bytes) = f.run("echo once >> marker");
    atomic_json(&f.scratch.join("request.json"), &r).unwrap();
    fs::create_dir_all(f.scratch.join("bundles")).unwrap();
    for (i, bytes) in bytes.iter().enumerate() {
        fs::write(f.scratch.join("bundles").join(i.to_string()), bytes).unwrap();
    }
    let status = crate::test_process::ChildGuard::spawn(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "engine::tests::launch_gate_child", "--nocapture"])
            .env("MOTOR_GATE_FIXTURE", f.scratch.path())
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0"),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert_eq!(status.code(), Some(86));
    assert!(!Path::new(&f.key).join("marker").exists());
    // The gated child briefly inherits the daemon lock until EOF makes it exit.
    // Reopen only after that descriptor is closed, rather than racing its exit.
    let runner = reopen(f.config());
    base_done(&runner).await;
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::OutcomeUnknown
    );
    // Restart may request a new trusted fill, but cannot replay foreground work.
    assert!(runner
        .status()
        .running_jobs
        .iter()
        .all(|j| j.workspace_key.starts_with("base:")));
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .all(|j| j.base || !j.pending()));
    let (r, bytes) = f.run("echo once >> marker");
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        fs::read_to_string(Path::new(&f.key).join("marker")).unwrap(),
        "once\n"
    );
    base_done(&runner).await;
}

#[tokio::test]
async fn workspace_setup_refusal_preserves_step_and_error_in_fallback_terminal() {
    let f = Fixture::new();
    let mut config = f.config();
    config.fail_materialize = true;
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);

    let terminal = terminal(&runner, request.job_id).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceSetupFailed
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.retry_after_ms(), None);
    let detail = terminal.refusal_detail().expect("missing setup detail");
    assert!(detail.starts_with("prepare job workspace: "), "{detail}");
    assert!(detail.contains("prepare job workspace"), "{detail}");
    assert!(detail.contains("materialize workspace"), "{detail}");
    assert!(
        detail.contains("injected materialization failure"),
        "{detail}"
    );
    let notice = serde_json::to_string(&Frame::Terminal(terminal.clone())).unwrap();
    assert!(
        notice.contains(detail),
        "caller terminal lost setup detail: {notice}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_serve_job_receives_private_tmp_and_expands_shell_variables() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    let runner = Runner::open(config).unwrap();
    let (socket, server) = listener(&f, &runner);
    // Send shell variables literally over the wire. systemd must not rewrite
    // shell source before Bash can expand it in the job's own environment.
    // Braced ${X} is expanded even inside a systemd argument; X is assigned only
    // by this shell, so pre-expansion would erase it before that assignment.
    let (mut request, bytes) = f.run(
        r#"set -eu
printf 'shell:%s:%s:%s\n' "$TMPDIR" "$HOME" "${X:-y}"
printf "TMPDIR=%s\n" "${TMPDIR-}"
printf 'defaults:%s:%s\n' "${TMPDIR:-x}" "${HOME-}"
X=job-shell
printf 'assigned:%s:%s\n' "$X" "${X}"
printenv TMPDIR TMP TEMP
printf 'scratch write\n' > "$TMPDIR/serve-write-probe"
cat "$TMPDIR/serve-write-probe"
"#,
    );
    for key in ["TMPDIR", "TMP", "TEMP", "HOME"] {
        request.env.insert(key.into(), "/tmp/caller-shared".into());
    }
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Run(request.clone()))
        .await
        .unwrap();
    assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
    chunks(&mut stream, &bytes).await;
    let mut output = Vec::new();
    let mut started = false;
    let terminal = loop {
        match read_next(&mut stream).await {
            Frame::Started { job_id, seq, .. } => {
                assert_eq!(job_id, request.job_id);
                assert_eq!(seq, 0);
                assert!(!started, "one Started per job");
                started = true;
            }
            Frame::Output(chunk) => {
                assert!(started, "Started before Output");
                assert!(chunk.seq > 0);
                output.extend(chunk.bytes.0);
            }
            Frame::Terminal(record) => break record,
            frame => panic!("unexpected job frame: {frame:?}"),
        }
    };
    assert!(started, "Started before the running job's Terminal");
    let output = String::from_utf8(output).unwrap();
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 }, "{output}");
    assert_eq!(terminal.ran, Some(Ran::Remote));
    assert_eq!(
        output,
        "shell:/motor-home/tmp:/motor-home:y\nTMPDIR=/motor-home/tmp\ndefaults:/motor-home/tmp:/motor-home\nassigned:job-shell:job-shell\n/motor-home/tmp\n/motor-home/tmp\n/motor-home/tmp\nscratch write\n"
    );
    base_done(&runner).await;
    server.abort();
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires motor-cache, motor-job, patched sccache, sudo, ACLs and systemd"]
async fn isolated_dependency_writer_fills_normal_dev_and_host_graphs_from_tip_only() {
    dependency_writer::isolated_graph_fixture().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_wrapper_never_compiles_uploaded_supervisor_as_daemon() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    fs::create_dir_all(config.state_dir()).unwrap();
    let secret = config.state_dir().join("supervisor-secret");
    fs::write(&secret, "runner-secret-must-not-be-embedded").unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname = 'isolation-probe'\nversion = '0.0.0'\nedition = '2021'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = 'isolation-probe'\nversion = '0.0.0'\n",
    )
    .unwrap();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn probe() {}\n").unwrap();
    let source = f.main.join("crates/prefrontal-build-slot/src/main.rs");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(
        &source,
        format!(
            "fn main() {{ panic!(\"{{}}\", include_str!({:?})); }}",
            secret
        ),
    )
    .unwrap();
    commit(&f.main, "first\n");
    let runner = Runner::open(config).unwrap();
    let (r, bytes) = f.run("cargo check --locked");
    f.submit(&runner, &r, &bytes);
    let terminal = terminal(&runner, r.job_id).await;
    let mut output = Vec::new();
    for frame in runner.replay(r.job_id, 0).unwrap() {
        if let Frame::Output(chunk) = frame {
            output.extend(chunk.bytes.0);
        }
    }
    assert_eq!(
        terminal.outcome,
        Outcome::Exit { code: 0 },
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert!(!String::from_utf8_lossy(&output).contains("runner-secret-must-not-be-embedded"));
    base_done(&runner).await;
}

// Process-group signalling is deliberately a Linux-only production primitive.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted host process-group semantics; run with the build-server isolation gate"]
async fn cancellation_deadline_and_drop_kill_even_term_ignoring_groups() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let mut surviving_descendants = Vec::new();
    for action in ["cancel", "deadline", "drop"] {
        let pid_file = f.scratch.join(format!("{action}-descendant.pid"));
        // The descendant ignores TERM and owns no output pipe. Killing just the
        // leader can therefore produce a terminal record while leaving this
        // workload alive; the PID assertion below must detect that leak.
        let (mut r, bytes) = f.run(&format!(
            "trap '' TERM; (trap '' TERM; exec sleep 600) </dev/null >/dev/null 2>&1 & echo $! > '{}'; echo start; wait",
            pid_file.display()
        ));
        if action == "deadline" {
            r.timeout = Some(1);
        }
        f.submit(&runner, &r, &bytes);
        until(|| {
            runner
                .status()
                .running_jobs
                .iter()
                .any(|j| j.job_id == r.job_id)
        })
        .await;
        until(|| {
            fs::read_to_string(&pid_file)
                .ok()
                .and_then(|pid| pid.trim().parse::<i32>().ok())
                .is_some_and(|pid| pid > 0)
        })
        .await;
        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let descendant =
            DescendantCleanup(platform::identity(pid).expect("descendant has a kernel start time"));
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "{action}: descendant must be alive before cancellation"
        );
        if action == "cancel" {
            runner.cancel(r.job_id).unwrap();
            runner.cancel(r.job_id).unwrap();
        }
        if action == "deadline" {
            f.clock.fetch_add(1000, Ordering::SeqCst);
            runner.tick().unwrap();
        }
        f.clock.fetch_add(2001, Ordering::SeqCst);
        if action == "drop" {
            let r2 = runner.clone();
            let key = f.key.clone();
            let task = tokio::spawn(async move { r2.drop_workspace(&key).await.unwrap() });
            until(|| {
                runner.core.inner.lock().unwrap().jobs[&r.job_id]
                    .kill
                    .is_some()
            })
            .await;
            f.clock.fetch_add(2001, Ordering::SeqCst);
            runner.tick().unwrap();
            assert!(task.await.unwrap().cancelled_jobs.contains(&r.job_id));
        } else {
            runner.tick().unwrap();
        }
        let t = terminal(&runner, r.job_id).await;
        assert_eq!(
            t.killed,
            Some(if action == "deadline" {
                Killed::Deadline
            } else {
                Killed::Cancel
            })
        );
        assert_eq!(t.ran, Some(Ran::Remote));
        if !descendant_gone(pid).await {
            surviving_descendants.push(format!(
                "{action}: descendant PID {pid} survived the terminal record"
            ));
        }
        // An assertion failure must not leave a ten-minute workload behind.
        drop(descendant);
    }
    assert!(
        surviving_descendants.is_empty(),
        "process-group kill failed: {}",
        surviving_descendants.join("; ")
    );
}

#[cfg(target_os = "linux")]
struct DescendantCleanup(platform::ProcessIdentity);

#[cfg(target_os = "linux")]
impl Drop for DescendantCleanup {
    fn drop(&mut self) {
        if platform::identity(self.0.pid).is_some_and(|process| process.start == self.0.start) {
            unsafe {
                libc::kill(self.0.pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn descendant_gone(pid: i32) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if unsafe { libc::kill(pid, 0) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return;
                }
                panic!("checking descendant PID {pid}: {error}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

/// Holds the first long operation with a given label open until released, so
/// a test can observe what the rest of the runner does meanwhile. Dropping it
/// releases the hold, so a failed assertion never leaves a thread parked.
struct Hold {
    label: &'static str,
    armed: AtomicBool,
    reached: AtomicBool,
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
}
impl Hold {
    fn new(label: &'static str) -> Arc<Self> {
        Arc::new(Self {
            label,
            armed: AtomicBool::new(false),
            reached: AtomicBool::new(false),
            released: std::sync::Mutex::new(false),
            wake: std::sync::Condvar::new(),
        })
    }
    fn hook(self: &Arc<Self>) -> Arc<dyn Fn(&'static str) + Send + Sync> {
        let hold = self.clone();
        Arc::new(move |label| {
            if label == hold.label && hold.armed.swap(false, Ordering::SeqCst) {
                hold.reached.store(true, Ordering::SeqCst);
                let mut released = hold.released.lock().unwrap();
                while !*released {
                    released = hold.wake.wait(released).unwrap();
                }
            }
        })
    }
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
    fn held(&self) -> bool {
        self.reached.load(Ordering::SeqCst) && !*self.released.lock().unwrap()
    }
}
struct Release(Arc<Hold>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Poll without ever blocking on the state lock: a test of lock contention
/// must not itself hang on the lock it is probing.
fn poll(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < end {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn upload_and_fetch(runner: &Runner, request: &Run, bytes: &[Vec<u8>]) {
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    let mut upload = Upload::new(runner.spool_dir(request.job_id), &request.bundles).unwrap();
    for (i, data) in bytes.iter().enumerate() {
        upload
            .chunk(Frame::BundleChunk {
                index: i as u32,
                offset: 0,
                data: BytePayload(data.clone()),
            })
            .unwrap();
    }
    runner.fetch_run(request, &upload.directory, true).unwrap();
}

/// A job accepted by an idle runner must start promptly while workspace removal,
/// materialization, setup or bundle indexing is in flight. These long operations
/// must not hold the runner's state lock and block another job's fetch and start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_runner_starts_an_accepted_job_while_long_work_is_in_flight() {
    let mut stalled = Vec::new();
    for kind in ["remove", "materialize", "start", "fetch"] {
        let f = Fixture::new();
        let hold = Hold::new(if kind == "start" { "materialize" } else { kind });
        let _release = Release(hold.clone());
        let mut config = f.config();
        config.long_work = Some(hold.hook());
        let runner = Runner::open(config).unwrap();
        warm(&f, &runner).await;
        let workspace = |name: &str| {
            f.scratch
                .join(format!("remote/worktrees/{name}"))
                .to_string_lossy()
                .to_string()
        };
        hold.arm();
        let background: tokio::task::JoinHandle<()> = match kind {
            "remove" => {
                let r = runner.clone();
                let key = f.key.clone();
                tokio::spawn(async move {
                    let _ = r.drop_workspace(&key).await;
                })
            }
            "materialize" => {
                let (r, _) = f.run("true");
                let prepare = Prepare {
                    workspace_kind: None,
                    transfer_id: Uuid::now_v7(),
                    workspace_key: workspace("prepared"),
                    repository_root: r.repository_root,
                    base_commit: r.snapshot_commit,
                    integration_tip: r.integration_tip,
                    siblings: r.siblings,
                    bundles: vec![],
                    git_dependencies: vec![],
                };
                let r = runner.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = r.prepare(&prepare, true);
                })
            }
            // "start" holds the other job's workspace setup, "fetch" its
            // bundle fetch.
            _ => {
                let (mut other, bytes) = f.run("true");
                other.workspace_key = workspace("other");
                other.cwd = other.workspace_key.clone();
                let r = runner.clone();
                tokio::task::spawn_blocking(move || upload_and_fetch(&r, &other, &bytes))
            }
        };
        let reached = {
            let hold = hold.clone();
            tokio::task::spawn_blocking(move || {
                poll(Duration::from_secs(10), || {
                    hold.reached.load(Ordering::SeqCst)
                })
            })
            .await
            .unwrap()
        };
        assert!(reached, "{kind}: the long operation never began");
        let (mut job, bytes) = f.run("echo prompt");
        job.workspace_key = workspace("b");
        job.cwd = job.workspace_key.clone();
        let id = job.job_id;
        let submitting = {
            let r = runner.clone();
            tokio::task::spawn_blocking(move || upload_and_fetch(&r, &job, &bytes))
        };
        let started = {
            let r = runner.clone();
            tokio::task::spawn_blocking(move || {
                poll(Duration::from_secs(10), || {
                    r.core.inner.try_lock().is_ok_and(|inner| {
                        inner.jobs.get(&id).is_some_and(|j| j.started_ms.is_some())
                    })
                })
            })
            .await
            .unwrap()
        };
        if !started {
            stalled.push(kind);
        } else {
            assert!(hold.held(), "{kind}: the long operation ended early");
        }
        hold.release();
        submitting.await.unwrap();
        background.await.unwrap();
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        base_done(&runner).await;
    }
    assert!(
        stalled.is_empty(),
        "an idle runner's accepted job waited behind: {stalled:?}"
    );
}

/// Write `Cargo.toml` with `body` into the fixture's main repository and
/// commit it, returning the new commit.
fn declare(f: &Fixture, body: &str) -> String {
    fs::write(f.main.join("Cargo.toml"), body).unwrap();
    commit(&f.main, body).0
}

#[test]
fn warm_command_is_derived_from_the_repository_tree() {
    let f = Fixture::new();
    let bare = git(&f.main, &["rev-parse", "HEAD"]);
    assert_eq!(workspace::warm_command(&f.main, &bare), "true");
    let plain = declare(&f, "[workspace]\nmembers = [\"a\"]\n");
    assert_eq!(
        workspace::warm_command(&f.main, &plain),
        "cargo build --workspace --all-targets"
    );
    let declared = declare(
        &f,
        "[workspace]\nmembers = [\"a\"]\n[workspace.metadata.motor]\nwarm-commands = [\"cargo nextest run --workspace --no-run\", \"cargo nextest run -p a --no-run\"]\n",
    );
    assert_eq!(
        workspace::warm_command(&f.main, &declared),
        "cargo nextest run --workspace --no-run && cargo nextest run -p a --no-run"
    );
    let package = declare(
        &f,
        "[package]\nname = \"a\"\n[package.metadata.motor]\nwarm-commands = [\"cargo build\"]\n",
    );
    assert_eq!(workspace::warm_command(&f.main, &package), "cargo build");
    let malformed = declare(
        &f,
        "[workspace]\n[workspace.metadata.motor]\nwarm-commands = \"cargo build\"\n",
    );
    assert_eq!(
        workspace::warm_command(&f.main, &malformed),
        "cargo build --workspace --all-targets"
    );
    // The original commits keep their own commands: derivation reads the
    // commit being warmed, not whatever the repository holds now.
    assert_eq!(workspace::warm_command(&f.main, &bare), "true");
    assert_eq!(
        workspace::warm_command(&f.main, &plain),
        "cargo build --workspace --all-targets"
    );
}

/// Motor declares the workspace and package-scoped commands that precompile
/// its test binaries; the runner reads them instead of hardcoding package names.
#[test]
fn this_repository_declares_its_warm_build() {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml")).unwrap();
    let command = workspace::manifest_warm_command(&manifest, "HEAD");
    assert_eq!(
        command,
        concat!(
            "cargo nextest run --workspace --no-run && ",
            "cargo nextest run -p motor-module --lib --bins --no-run && ",
            "cargo nextest run -p motor-runner --lib --bins --no-run"
        )
    );
}

#[tokio::test]
async fn disabled_warm_builds_still_dispatch_accepted_jobs() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let inner = runner.core.inner.lock().unwrap();
    assert!(inner.jobs[&request.job_id].started.is_some());
    assert!(
        inner.jobs.values().all(|job| !job.base),
        "foreground-only fixtures must not queue warm jobs"
    );
    assert!(!runner.is_draining());
}

fn base_commands(runner: &Runner) -> Vec<String> {
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|j| j.base)
        .map(|j| j.request.as_ref().unwrap().command.clone())
        .collect()
}

/// With no runner-wide override the warm build runs the command the warmed
/// repository declares, never one repository's hardcoded package selection.
#[tokio::test]
async fn warm_build_runs_the_repositorys_own_command() {
    let f = Fixture::new();
    declare(
        &f,
        "[workspace]\n[workspace.metadata.motor]\nwarm-commands = [\"printf declared > \\\"$CARGO_TARGET_DIR/artifact\\\"\"]\n",
    );
    let mut config = f.config();
    config.base_commands.clear();
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    assert_eq!(
        base_commands(&runner),
        vec!["printf declared > \"$CARGO_TARGET_DIR/artifact\"".to_string()]
    );
    let status = runner.status();
    assert_eq!(
        status.repositories[0].last_rebuild_result,
        Some(RebuildResult::Ok)
    );
    assert!(
        runner.core.inner.lock().unwrap().catalog.repositories[&f.root]
            .filled_descriptor
            .is_some()
    );
    assert!(!runner.config().state_dir().join("generations").exists());
}

/// A failed warm build is the warm build's own result: a real job on the same
/// commit still runs, and the failure only suppresses retrying the same
/// command. Once the warm command changes (a runner deployed with a better
/// derivation, or an operator override) the commit is warmed again.
#[tokio::test]
async fn failed_warm_build_does_not_fail_the_commit_for_jobs_or_other_commands() {
    let f = Fixture::new();
    let mut config = f.config();
    config.base_commands = vec!["exit 101".into()];
    let runner = Runner::open(config.clone()).unwrap();
    warm(&f, &runner).await;
    let tip = git(&f.main, &["rev-parse", "HEAD"]);
    let status = runner.status();
    assert_eq!(
        status.repositories[0].last_rebuild_result,
        Some(RebuildResult::Failed)
    );
    assert_eq!(status.repositories[0].published_commit, None);
    let (job, bytes) = f.run("echo real");
    f.submit(&runner, &job, &bytes);
    let record = terminal(&runner, job.job_id).await;
    assert_eq!(record.outcome, Outcome::Exit { code: 0 });
    assert_eq!(record.ran, Some(Ran::Remote));
    base_done(&runner).await;
    assert_eq!(
        base_commands(&runner).len(),
        1,
        "the same command is not retried for the same commit"
    );
    drop(runner);
    config.base_commands = vec!["printf warm > \"$CARGO_TARGET_DIR/artifact\"".into()];
    let runner = reopen(config);
    let (job, bytes) = f.run("true");
    f.submit(&runner, &job, &bytes);
    assert_eq!(
        terminal(&runner, job.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    base_done(&runner).await;
    let status = runner.status();
    assert_eq!(status.repositories[0].published_commit, Some(tip));
    assert_eq!(
        status.repositories[0].last_rebuild_result,
        Some(RebuildResult::Ok)
    );
}

/// A have reply carries, besides the offered candidates the mirror holds, the
/// commits its pinned refs point at. Candidates are the sender's new work,
/// which the runner rarely has; its earlier integration tips are what make a
/// thin bundle possible.
#[tokio::test]
async fn have_reply_reports_the_mirrors_pinned_commits() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let first = git(&f.main, &["rev-parse", "HEAD"]);
    let (next, _) = commit(&f.main, "second\n");
    let held = runner.have(vec![RepositoryCandidates {
        repository: f.root.clone(),
        commits: vec![next.clone()],
    }]);
    assert_eq!(held[0].commits, vec![first.clone()]);
    let unknown = runner.have(vec![RepositoryCandidates {
        repository: f.scratch.join("remote/never").to_string_lossy().into(),
        commits: vec![next],
    }]);
    assert!(unknown[0].commits.is_empty());
}

/// Every bundle fetch with a new tip writes a pack, even when the mirror
/// already holds almost every object in it. Repeated full transfers of one
/// history must still leave the mirror with about one copy of it, not one per
/// transfer.
#[tokio::test]
async fn repeated_transfers_keep_one_object_store_per_mirror() {
    use rand::RngCore;
    let f = Fixture::new();
    let mut payload = vec![0; 1 << 20];
    StdRng::seed_from_u64(7).fill_bytes(&mut payload);
    fs::write(f.main.join("payload"), &payload).unwrap();
    commit(&f.main, "payload\n");
    let runner = Runner::open(f.config()).unwrap();
    let mirror = runner.core.trees.mirror(&f.root);
    let size = |mirror: &Path| -> u64 {
        fs::read_dir(mirror.join("objects/pack"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .map(|e| e.metadata().unwrap().len())
            .sum()
    };
    let mut single = 0;
    for round in 0..8 {
        // A new tip each round, as in real use: fetch skips a bundle whose
        // tips the mirror already has, so identical bundles would prove nothing.
        commit(&f.main, &format!("round {round}\n"));
        let (job, bytes) = f.run("true");
        single = bytes[0].len() as u64;
        f.submit(&runner, &job, &bytes);
        assert_eq!(
            terminal(&runner, job.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
    base_done(&runner).await;
    let settled = tokio::task::spawn_blocking({
        let mirror = mirror.clone();
        move || poll(Duration::from_secs(15), || workspace::packs(&mirror) <= 3)
    })
    .await
    .unwrap();
    assert!(
        settled,
        "eight transfers left {} packs",
        workspace::packs(&mirror)
    );
    assert!(single >= payload.len() as u64);
    assert!(
        size(&mirror) < 4 * single,
        "mirror holds {} bytes of packs for one {single}-byte history",
        size(&mirror)
    );
    // Compaction keeps everything a pinned ref reaches.
    git(&mirror, &["fsck", "--connectivity-only", "--no-dangling"]);
}

// Reservations: a prepare or a job setup works on a workspace without the
// state lock held. Every reader of that reservation (retention, drop,
// prepare, the scheduler, sibling and weight checks, ref pruning and warm
// workspace reservations) must treat the setup as in progress. Each test below holds
// one setup open, exercises one reader, then lets the setup finish.

fn held_runner(f: &Fixture) -> (Runner, Arc<Hold>, Release) {
    let hold = Hold::new("materialize");
    let mut config = f.config();
    config.long_work = Some(hold.hook());
    (Runner::open(config).unwrap(), hold.clone(), Release(hold))
}
fn job_at(f: &Fixture, name: &str, command: &str) -> (Run, Vec<Vec<u8>>) {
    let (mut run, bytes) = f.run(command);
    run.workspace_key = f
        .scratch
        .join(format!("remote/worktrees/{name}"))
        .to_string_lossy()
        .to_string();
    run.cwd = run.workspace_key.clone();
    (run, bytes)
}
fn prepare_for(run: &Run) -> Prepare {
    Prepare {
        workspace_kind: run.workspace_kind,
        transfer_id: Uuid::now_v7(),
        workspace_key: run.workspace_key.clone(),
        repository_root: run.repository_root.clone(),
        base_commit: run.snapshot_commit.clone(),
        integration_tip: run.integration_tip.clone(),
        siblings: run.siblings.clone(),
        bundles: vec![],
        git_dependencies: vec![],
    }
}
fn spawn_prepare(runner: &Runner, prepare: &Prepare) -> tokio::task::JoinHandle<io::Result<()>> {
    let runner = runner.clone();
    let prepare = prepare.clone();
    tokio::task::spawn_blocking(move || runner.prepare(&prepare, true))
}
async fn reached(hold: &Hold) {
    until(|| hold.reached.load(Ordering::SeqCst)).await;
}
/// The job has begun setting up or has started.
fn begun(runner: &Runner, id: Uuid) -> bool {
    let inner = runner.core.inner.lock().unwrap();
    inner.starting.contains(&id) || inner.jobs.get(&id).is_some_and(|j| j.started_ms.is_some())
}
/// Long enough for several scheduler ticks (one every 25 ms).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// The retention sweep deletes workspaces idle for a day. A workspace whose
/// setup is in flight is in use even when its catalog entry is old: deleting
/// it would pull the checkout out from under that setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_sweep_spares_a_workspace_being_set_up() {
    for setup in ["prepare", "job"] {
        let f = Fixture::new();
        let (runner, hold, _release) = held_runner(&f);
        warm(&f, &runner).await;
        hold.arm();
        let (run, bytes) = f.run("cat source");
        let preparing = if setup == "prepare" {
            Some(spawn_prepare(&runner, &prepare_for(&run)))
        } else {
            f.submit(&runner, &run, &bytes);
            None
        };
        reached(&hold).await;
        // Only now age the workspace, so the scheduler's own periodic sweep
        // cannot run before the setup holds its reservation.
        f.clock.fetch_add(RETENTION_MS + 1, Ordering::SeqCst);
        runner.maintenance().unwrap();
        assert!(
            Path::new(&f.key).join(".git").exists(),
            "{setup}: the sweep discarded a workspace being set up"
        );
        assert!(
            runner
                .core
                .inner
                .lock()
                .unwrap()
                .catalog
                .workspaces
                .contains_key(&f.key),
            "{setup}: the sweep forgot a workspace being set up"
        );
        hold.release();
        let job = match preparing {
            Some(preparing) => {
                preparing.await.unwrap().unwrap();
                let (job, bytes) = f.run("cat source");
                f.submit(&runner, &job, &bytes);
                job
            }
            None => run,
        };
        let record = terminal(&runner, job.job_id).await;
        assert_eq!(record.outcome, Outcome::Exit { code: 0 }, "{setup}");
        assert_eq!(record.ran, Some(Ran::Remote), "{setup}");
        base_done(&runner).await;
    }
}

/// Drop waits for a setup in flight on its workspace, cancels the job, and
/// only then deletes the workspace; the cancelled job never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_waits_for_a_setup_in_flight() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    warm(&f, &runner).await;
    hold.arm();
    let (job, bytes) = job_at(&f, "fresh", "touch marker");
    f.submit(&runner, &job, &bytes);
    reached(&hold).await;
    let dropping = {
        let runner = runner.clone();
        let key = job.workspace_key.clone();
        tokio::spawn(async move { runner.drop_workspace(&key).await.unwrap() })
    };
    settle().await;
    assert!(
        !dropping.is_finished(),
        "drop returned while the workspace was still being set up"
    );
    hold.release();
    let reply = dropping.await.unwrap();
    assert!(reply.dropped);
    assert!(reply.cancelled_jobs.contains(&job.job_id));
    assert_eq!(
        terminal(&runner, job.job_id).await.outcome,
        Outcome::Cancelled
    );
    assert!(!Path::new(&job.workspace_key).exists());
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(&job.workspace_key));
}

/// A prepare on a workspace whose job is still being set up is refused as
/// busy (the session retries it) rather than materializing concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_waits_for_a_job_setup_on_the_same_workspace() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    warm(&f, &runner).await;
    hold.arm();
    let (job, bytes) = f.run("true");
    f.submit(&runner, &job, &bytes);
    reached(&hold).await;
    let refused = spawn_prepare(&runner, &prepare_for(&job)).await.unwrap();
    assert_eq!(
        refused.map_err(|e| e.kind()),
        Err(io::ErrorKind::WouldBlock),
        "prepare ran during the job's setup"
    );
    hold.release();
    assert_eq!(
        terminal(&runner, job.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    spawn_prepare(&runner, &prepare_for(&job))
        .await
        .unwrap()
        .unwrap();
}

/// A second job on a workspace waits for the first job's setup, and the two
/// run in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_job_waits_for_the_first_jobs_setup() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    warm(&f, &runner).await;
    let log = f.scratch.join("order.log");
    hold.arm();
    let (first, bytes) = f.run(&format!("echo one >> '{}'", log.display()));
    f.submit(&runner, &first, &bytes);
    reached(&hold).await;
    let (second, bytes) = f.run(&format!("echo two >> '{}'", log.display()));
    f.submit(&runner, &second, &bytes);
    settle().await;
    assert!(
        !begun(&runner, second.job_id),
        "a second setup began on a workspace already being set up"
    );
    hold.release();
    for id in [first.job_id, second.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
    assert_eq!(fs::read_to_string(log).unwrap(), "one\ntwo\n");
}

/// A job being set up already counts toward the weight cap, so a job that
/// would not fit beside it waits instead of starting concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_being_set_up_counts_toward_the_weight_cap() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    warm(&f, &runner).await;
    hold.arm();
    let (mut heavy, bytes) = job_at(&f, "heavy", "true");
    heavy.weight_hint = Some(20);
    f.submit(&runner, &heavy, &bytes);
    reached(&hold).await;
    let (mut other, bytes) = job_at(&f, "other", "true");
    other.weight_hint = Some(16);
    f.submit(&runner, &other, &bytes);
    settle().await;
    assert!(
        !begun(&runner, other.job_id),
        "20 + 16 exceeds the cap of 28, yet the second job began"
    );
    hold.release();
    for id in [heavy.job_id, other.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
}

/// A setup checking a sibling out at a commit the catalog does not record
/// yet owns that checkout; another setup using the sibling waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sibling_checkout_in_flight_blocks_other_setups_using_it() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    warm(&f, &runner).await;
    commit(&f.sibling, "second\n");
    hold.arm();
    let (first, bytes) = job_at(&f, "first", "cat ../commons/source");
    f.submit(&runner, &first, &bytes);
    reached(&hold).await;
    let (second, bytes) = job_at(&f, "second", "cat ../commons/source");
    f.submit(&runner, &second, &bytes);
    settle().await;
    assert!(
        !begun(&runner, second.job_id),
        "a second setup began while the sibling was being checked out"
    );
    hold.release();
    for id in [first.job_id, second.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
}

/// Ref pruning keeps the snapshot a prepare in flight is materializing,
/// although no job or recorded workspace holds it yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_pruning_keeps_the_snapshot_of_a_prepare_in_flight() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    warm(&f, &runner).await;
    let (next, _) = commit(&f.main, "next\n");
    let (run, bytes) = job_at(&f, "prepared", "true");
    let mut prepare = prepare_for(&run);
    prepare.bundles = run.bundles.clone();
    let upload = f.spool(&runner, &run, &bytes);
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    drop(upload);
    hold.arm();
    let preparing = spawn_prepare(&runner, &prepare);
    reached(&hold).await;
    runner.maintenance().unwrap();
    let mirror = runner.core.trees.mirror(&f.root);
    assert!(
        workspace::git(&mirror, &["for-each-ref", "refs/snapshots"])
            .unwrap()
            .contains(&format!("refs/snapshots/{next}")),
        "pruning dropped the snapshot of a prepare in flight"
    );
    hold.release();
    preparing.await.unwrap().unwrap();
    base_done(&runner).await;
}

/// Cache eviction and writer setup reserve the same fence under the state lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_fence_protects_writer_setup_and_queues_writers_during_eviction() {
    let f = Fixture::new();
    let hold = Hold::new("materialize");
    let _release = Release(hold.clone());
    let pressure = Arc::new(AtomicBool::new(false));
    let mut config = f.config();
    config.long_work = Some(hold.hook());
    config.free_space_floor_bytes = 100;
    let pressured = pressure.clone();
    let fleet = config.sccache_dir();
    let entry = fleet.join("entry");
    let probe_entry = entry.clone();
    config.free_space_probe = Arc::new(move |_| {
        Ok(
            if pressured.load(Ordering::SeqCst) && probe_entry.exists() {
                10
            } else {
                300
            },
        )
    });
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    fs::create_dir_all(&fleet).unwrap();
    fs::write(&entry, "cached").unwrap();
    fs::write(f.main.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    commit(&f.main, "new descriptor\n");
    let (r, bytes) = f.run("true");
    let upload = f.spool(&runner, &r, &bytes);
    let mut prepare = prepare_for(&r);
    prepare.bundles = r.bundles.clone();
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.evicting_caches.insert(fleet.clone());
        runner
            .queue_base(&mut inner, &f.root, &r.integration_tip, &f.key, &[], &[])
            .unwrap();
    }
    settle().await;
    assert!(runner.core.inner.lock().unwrap().writer.is_none());
    {
        let inner = runner.core.inner.lock().unwrap();
        let queued = inner
            .jobs
            .values()
            .find(|j| {
                j.base
                    && j.request
                        .as_ref()
                        .is_some_and(|r| r.integration_tip == prepare.integration_tip)
            })
            .unwrap();
        assert!(
            queued.setup_ms.is_none(),
            "writer started while eviction owned the fence"
        );
    }
    hold.arm();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .evicting_caches
        .remove(&fleet);
    reached(&hold).await;
    assert!(runner.core.inner.lock().unwrap().writer.is_some());
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_some());
    assert_eq!(fs::read_to_string(&entry).unwrap(), "cached");
    hold.release();
    base_done(&runner).await;
    assert!(runner.core.inner.lock().unwrap().writer.is_none());
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(fleet.is_dir());
    assert!(!entry.exists());
}

/// Exercises the real daemon over its wire protocol without modifying the Mac
/// client or using an in-process Runner. The operator supplies the daemon socket
/// and an existing host-only file whose absence inside the job proves isolation.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the trusted isolation gate, MOTOR_SMOKE_SOCKET and MOTOR_SMOKE_CANARY"]
async fn live_isolated_runner_wire_smoke() {
    let f = Fixture::new();
    let socket = PathBuf::from(std::env::var_os("MOTOR_SMOKE_SOCKET").unwrap());
    let canary =
        PathBuf::from(std::env::var_os("MOTOR_SMOKE_CANARY").expect("host-only canary path"));
    assert!(
        canary.is_absolute() && canary.is_file(),
        "canary must be an existing absolute host file"
    );
    let canary = format!(
        "'{}'",
        canary
            .to_str()
            .expect("UTF-8 canary path")
            .replace('\'', "'\\''")
    );
    let (r, bytes) = f.run(&format!("set -eu; test \"$(id -un)\" = motor-job; test ! -e {canary}; cat source; cat ../commons/source; touch target/smoke; git rev-parse HEAD; if sudo -n true; then exit 44; fi; echo isolated-wire-smoke-ok"));
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Run(r.clone()))
        .await
        .unwrap();
    assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
    chunks(&mut stream, &bytes).await;
    let mut output = String::new();
    loop {
        match read_next(&mut stream).await {
            Frame::Output(chunk) => output.push_str(&String::from_utf8_lossy(&chunk.bytes.0)),
            Frame::Terminal(terminal) => {
                assert_eq!(terminal.outcome, Outcome::Exit { code: 0 }, "{output}");
                assert_eq!(terminal.ran, Some(Ran::Remote));
                break;
            }
            _ => {}
        }
    }
    assert!(output.contains("isolated-wire-smoke-ok"), "{output}");
    println!("live isolated job {}: {output}", r.job_id);
    // Ask the daemon to remove the materialized workspace before Fixture drops
    // the source trees; this also proves cleanup of job-owned target files.
    let mut stream = connect(&socket).await;
    write_frame_async(
        &mut stream,
        &Frame::Drop(DropRequest::new(r.workspace_key.clone())),
    )
    .await
    .unwrap();
    assert!(matches!(read_next(&mut stream).await, Frame::DropReply(_)));
}

#[cfg(target_os = "linux")]
#[path = "tests/isolated_shm.rs"]
mod isolated_shm;
#[cfg(target_os = "linux")]
#[path = "tests/isolated_targets.rs"]
mod isolated_targets;
fn job_output(runner: &Runner, id: Uuid) -> String {
    let bytes: Vec<u8> = runner
        .replay(id, 0)
        .unwrap()
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) => Some(output.bytes.0),
            _ => None,
        })
        .flatten()
        .collect();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn remote_copy_reports_caller_commit_for_head_when_clean_and_dirty() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();

    // 1. Clean run: HEAD inside the job must be the caller's exact commit
    let caller_commit = git(&f.main, &["rev-parse", "HEAD"]);
    let (r, bytes) = f.run_worktree(
        "printf 'HEAD=%s\nENV=%s\n' \"$(git rev-parse HEAD)\" \"${CK_REMOTE_SOURCE_COMMIT-unset}\"",
    );
    f.submit(&runner, &r, &bytes);
    let term = terminal(&runner, r.job_id).await;
    assert_eq!(term.outcome, Outcome::Exit { code: 0 });
    let output = job_output(&runner, r.job_id);
    assert!(
        output.contains(&format!("HEAD={caller_commit}")),
        "clean remote copy HEAD must match caller commit: {output}"
    );
    assert!(
        output.contains(&format!("ENV={caller_commit}")),
        "clean remote copy CK_REMOTE_SOURCE_COMMIT must match caller commit: {output}"
    );

    // 2. Dirty run: dirt applied on top, HEAD must still be caller's exact commit
    fs::write(f.main.join("source"), "uncommitted caller dirt\n").unwrap();
    let (r, bytes) = f.run_worktree(
        "printf 'HEAD=%s\nENV=%s\nSTATUS=%s\nCONTENT=%s\n' \"$(git rev-parse HEAD)\" \"${CK_REMOTE_SOURCE_COMMIT-unset}\" \"$(git status --porcelain)\" \"$(cat source)\"",
    );
    f.submit(&runner, &r, &bytes);
    let term = terminal(&runner, r.job_id).await;
    assert_eq!(term.outcome, Outcome::Exit { code: 0 });
    let output = job_output(&runner, r.job_id);
    assert!(
        output.contains(&format!("HEAD={caller_commit}")),
        "dirty remote copy HEAD must match caller commit: {output}"
    );
    assert!(
        output.contains(&format!("ENV={caller_commit}")),
        "dirty remote copy CK_REMOTE_SOURCE_COMMIT must match caller commit: {output}"
    );
    assert!(
        output.contains("CONTENT=uncommitted caller dirt"),
        "dirt must be present in remote copy: {output}"
    );
    assert!(
        output.contains("STATUS= M source") || output.contains("STATUS=M  source"),
        "dirt must appear as working-tree changes: {output}"
    );
}

#[tokio::test]
async fn noop_remote_build_stays_fresh_and_one_changed_file_rebuilds_only_its_crate() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();

    fs::write(
        f.main.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/crate-a\", \"crates/crate-b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::create_dir_all(f.main.join("crates/crate-a/src")).unwrap();
    fs::write(
        f.main.join("crates/crate-a/Cargo.toml"),
        "[package]\nname = \"crate-a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        f.main.join("crates/crate-a/src/lib.rs"),
        "pub fn a() -> i32 { 1 }\n",
    )
    .unwrap();

    fs::create_dir_all(f.main.join("crates/crate-b/src")).unwrap();
    fs::write(
        f.main.join("crates/crate-b/Cargo.toml"),
        "[package]\nname = \"crate-b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ncrate-a = { path = \"../crate-a\" }\n",
    )
    .unwrap();
    fs::write(
        f.main.join("crates/crate-b/src/lib.rs"),
        "pub fn b() -> i32 { crate_a::a() + 1 }\n",
    )
    .unwrap();

    git(&f.main, &["add", "."]);
    git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "workspace init",
        ],
    );

    // Job 1: Initial build
    let (r, bytes) = f.run_worktree("cargo build -vv");
    f.submit(&runner, &r, &bytes);
    let term = terminal(&runner, r.job_id).await;
    assert_eq!(term.outcome, Outcome::Exit { code: 0 });

    // Job 2: Second build on unchanged tree (no Dirty lines, both Fresh)
    let (r, bytes) = f.run_worktree("cargo build -vv");
    f.submit(&runner, &r, &bytes);
    let term = terminal(&runner, r.job_id).await;
    assert_eq!(term.outcome, Outcome::Exit { code: 0 });
    let output2 = job_output(&runner, r.job_id);
    let dirty_lines: Vec<&str> = output2.lines().filter(|l| l.contains("Dirty")).collect();
    assert!(
        dirty_lines.is_empty(),
        "second job on unchanged tree must report no Dirty lines, got: {dirty_lines:?}\n{output2}"
    );
    assert!(
        output2.contains("Fresh crate-a"),
        "crate-a must report Fresh on second build: {output2}"
    );
    assert!(
        output2.contains("Fresh crate-b"),
        "crate-b must report Fresh on second build: {output2}"
    );

    // Job 3: One changed file in crate-b (crate-a unchanged)
    fs::write(
        f.main.join("crates/crate-b/src/lib.rs"),
        "pub fn b() -> i32 { crate_a::a() + 2 }\n",
    )
    .unwrap();
    let (r, bytes) = f.run_worktree("cargo build -vv");
    f.submit(&runner, &r, &bytes);
    let term = terminal(&runner, r.job_id).await;
    assert_eq!(term.outcome, Outcome::Exit { code: 0 });
    let output3 = job_output(&runner, r.job_id);
    assert!(
        output3.contains("Fresh crate-a"),
        "unchanged crate-a must stay Fresh: {output3}"
    );
    assert!(
        output3.contains("Dirty crate-b"),
        "changed crate-b must rebuild: {output3}"
    );
}

#[tokio::test]
async fn upload_fetch_and_setup_do_not_spend_queue_wait_budget() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    // Warm first: otherwise queueing the job also queues a base build, and the
    // hold below can catch that build's setup while this job is still queued.
    warm(&f, &runner).await;
    hold.arm();
    let (mut request, bytes) = f.run("true");
    request.queue_wait_limit_s = 1;
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    let progress = runner.queue_progress(request.job_id);
    assert_eq!(progress.elapsed_ms(), None);
    f.clock.fetch_add(400_000, Ordering::SeqCst);
    assert_eq!(
        runner.job_status(request.job_id).unwrap().queue_wait_ms,
        None
    );
    assert_eq!(
        runner.withdraw(request.job_id).unwrap(),
        WithdrawOutcome::Started
    );
    let upload = f.spool(&runner, &request, &bytes);
    f.clock.fetch_add(100_000, Ordering::SeqCst);
    // A temporary dropping marker prevents the scheduler reserving setup
    // before we observe the queued state. Remove it before exercising setup;
    // the injected clock does not advance while that marker is present.
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .dropping
        .insert(request.workspace_key.clone());
    runner.fetch_run(&request, &upload.directory, true).unwrap();
    assert_eq!(progress.elapsed_ms(), Some(0));
    assert_eq!(
        runner.job_status(request.job_id).unwrap().queue_wait_ms,
        Some(0)
    );
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .dropping
        .remove(&request.workspace_key);
    runner.tick().unwrap();
    reached(&hold).await;
    f.clock.fetch_add(500_000, Ordering::SeqCst);
    assert_eq!(progress.elapsed_ms(), None);
    assert_eq!(
        runner.job_status(request.job_id).unwrap().queue_wait_ms,
        None
    );
    assert_eq!(
        runner.withdraw(request.job_id).unwrap(),
        WithdrawOutcome::Started
    );
    hold.release();
    let result = terminal(&runner, request.job_id).await;
    assert_eq!(result.outcome, Outcome::Exit { code: 0 });
    assert_eq!(result.ran, Some(Ran::Remote));
    assert_eq!(result.queue_wait_ms, 0);
}

#[tokio::test]
async fn workspace_owner_wait_does_not_spend_queue_wait_budget() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let release = f.scratch.join("release-workspace-owner");
    struct FinishHolder(PathBuf);
    impl Drop for FinishHolder {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let _finish = FinishHolder(release.clone());
    let (holder, bytes) = f.run(&format!(
        "while test ! -e '{}'; do sleep .02; done",
        release.display()
    ));
    f.submit(&runner, &holder, &bytes);
    until(|| runner.job_status(holder.job_id).unwrap().state == "running").await;

    let (mut waiting, bytes) = f.run("true");
    waiting.queue_wait_limit_s = 1;
    f.submit(&runner, &waiting, &bytes);
    let progress = runner.queue_progress(waiting.job_id);
    let status = runner.job_status(waiting.job_id).unwrap();
    assert_eq!(status.state, "queued");
    assert_eq!(status.waiting_on.as_deref(), Some("workspace owner"));
    assert_eq!(status.queue_wait_ms, Some(0));

    f.clock.fetch_add(10_000, Ordering::SeqCst);
    let status = runner.job_status(waiting.job_id).unwrap();
    assert_eq!(status.waiting_on.as_deref(), Some("workspace owner"));
    assert_eq!(status.queue_wait_ms, Some(0));
    assert_eq!(progress.elapsed_ms(), Some(0));
    assert_eq!(
        runner.withdraw(waiting.job_id).unwrap(),
        WithdrawOutcome::Started
    );

    fs::write(release, "").unwrap();
    assert_eq!(
        terminal(&runner, holder.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        terminal(&runner, waiting.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let wait = runner
        .runner_status()
        .health
        .unwrap()
        .queue_wait_1h
        .unwrap();
    assert_eq!(wait.p50_ms, 0);
    assert_eq!(wait.p90_ms, 0);
    assert_eq!(wait.refusals, 0);
}

#[test]
fn queue_wait_clock_resumes_for_capacity_after_workspace_owner_finishes() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 4;
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let now = runner.now();
    let (mut owner, _) = f.run("true");
    owner.siblings.clear();
    let (mut capacity_owner, _) = job_at(&f, "capacity-owner", "true");
    capacity_owner.siblings.clear();
    let (mut waiting, _) = f.run("true");
    waiting.siblings.clear();
    waiting.queue_wait_limit_s = 1;

    {
        let mut inner = runner.core.inner.lock().unwrap();
        let mut owner_job = Job::new(owner.job_id, Some(owner.clone()), now, 1, 2);
        owner_job.state = JobState::Running;
        inner.jobs.insert(owner.job_id, owner_job);
        let mut capacity_job = Job::new(
            capacity_owner.job_id,
            Some(capacity_owner.clone()),
            now,
            2,
            1,
        );
        capacity_job.state = JobState::Running;
        inner.jobs.insert(capacity_job.id, capacity_job);
        let mut waiting_job = Job::new(waiting.job_id, Some(waiting.clone()), now, 3, 2);
        waiting_job.state = JobState::Queued;
        waiting_job.queued_ms = Some(now);
        waiting_job.start_queue_wait(now);
        inner.jobs.insert(waiting.job_id, waiting_job);
    }

    let status = runner.job_status(waiting.job_id).unwrap();
    assert_eq!(status.waiting_on.as_deref(), Some("workspace owner"));
    f.clock.fetch_add(5_000, Ordering::SeqCst);
    assert_eq!(
        runner.job_status(waiting.job_id).unwrap().queue_wait_ms,
        Some(0)
    );

    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.jobs.remove(&owner.job_id);
        inner.jobs.get_mut(&capacity_owner.job_id).unwrap().weight = 3;
    }
    let status = runner.job_status(waiting.job_id).unwrap();
    assert_eq!(status.waiting_on.as_deref(), Some("permits"));
    assert_eq!(status.queue_wait_ms, Some(0));
    f.clock.fetch_add(400, Ordering::SeqCst);
    assert_eq!(
        runner.job_status(waiting.job_id).unwrap().queue_wait_ms,
        Some(400)
    );
    f.clock.fetch_add(600, Ordering::SeqCst);
    assert_eq!(
        runner.withdraw(waiting.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    let terminal = runner.core.inner.lock().unwrap().jobs[&waiting.job_id]
        .terminal
        .clone()
        .unwrap();
    assert_eq!(terminal.queue_wait_ms, 1_000);
    assert!(terminal
        .refusal_detail()
        .unwrap()
        .starts_with("waiting_on=permits"));
}

#[test]
fn cancelled_workspace_waiter_is_removed_from_the_queue() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let now = runner.now();
    let (mut owner, _) = f.run("true");
    owner.siblings.clear();
    let (mut waiting, _) = f.run("touch must-not-run");
    waiting.siblings.clear();

    {
        let mut inner = runner.core.inner.lock().unwrap();
        let mut owner_job = Job::new(owner.job_id, Some(owner), now, 1, 2);
        owner_job.state = JobState::Running;
        inner.jobs.insert(owner_job.id, owner_job);
        let mut waiting_job = Job::new(waiting.job_id, Some(waiting.clone()), now, 2, 2);
        waiting_job.state = JobState::Queued;
        waiting_job.queued_ms = Some(now);
        waiting_job.start_queue_wait(now);
        inner.jobs.insert(waiting.job_id, waiting_job);
    }
    assert_eq!(
        runner
            .job_status(waiting.job_id)
            .unwrap()
            .waiting_on
            .as_deref(),
        Some("workspace owner")
    );
    assert_eq!(runner.queue_progress(waiting.job_id).elapsed_ms(), Some(0));

    runner.cancel(waiting.job_id).unwrap();
    assert_eq!(
        runner.job_status(waiting.job_id).unwrap().state,
        "tombstone"
    );
    assert_eq!(runner.queue_progress(waiting.job_id).elapsed_ms(), None);
    assert!(!runner.core.inner.lock().unwrap().jobs[&waiting.job_id].pending());
}

#[tokio::test]
async fn jobs_waiting_on_a_workspace_keep_fifo_order() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let release = f.scratch.join("release-workspace-fifo");
    let log = f.scratch.join("workspace-fifo.log");
    struct FinishHolder(PathBuf);
    impl Drop for FinishHolder {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let _finish = FinishHolder(release.clone());
    let (holder, bytes) = f.run(&format!(
        "while test ! -e '{}'; do sleep .02; done; echo first >> '{}'",
        release.display(),
        log.display()
    ));
    f.submit(&runner, &holder, &bytes);
    until(|| runner.job_status(holder.job_id).unwrap().state == "running").await;

    let (first, bytes) = f.run(&format!("echo second >> '{}'", log.display()));
    f.submit(&runner, &first, &bytes);
    let (second, bytes) = f.run(&format!("echo third >> '{}'", log.display()));
    f.submit(&runner, &second, &bytes);
    assert_eq!(
        runner
            .job_status(first.job_id)
            .unwrap()
            .waiting_on
            .as_deref(),
        Some("workspace owner")
    );
    assert_eq!(
        runner
            .job_status(second.job_id)
            .unwrap()
            .waiting_on
            .as_deref(),
        Some("workspace owner")
    );

    fs::write(release, "").unwrap();
    for id in [holder.job_id, first.job_id, second.job_id] {
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
    }
    assert_eq!(fs::read_to_string(log).unwrap(), "first\nsecond\nthird\n");
}

#[tokio::test]
async fn real_permit_wait_still_expires_and_names_the_other_holder() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 4;
    let runner = Runner::open(config).unwrap();
    let release = f.scratch.join("release-permit-holder");
    struct FinishHolder(PathBuf);
    impl Drop for FinishHolder {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let _finish = FinishHolder(release.clone());
    let (mut holder, bytes) = f.run(&format!(
        "while test ! -e '{}'; do sleep .02; done",
        release.display()
    ));
    holder.weight_hint = Some(4);
    f.submit(&runner, &holder, &bytes);
    until(|| runner.job_status(holder.job_id).unwrap().state == "running").await;
    let (mut waiting, bytes) = job_at(&f, "waiting", "true");
    waiting.queue_wait_limit_s = 1;
    f.submit(&runner, &waiting, &bytes);
    f.clock.fetch_add(1_001, Ordering::SeqCst);
    assert_eq!(
        runner.withdraw(waiting.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    let result = terminal(&runner, waiting.job_id).await;
    assert_eq!(
        result.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::QueueWaitExceeded
        }
    );
    let detail = result.refusal_detail().unwrap();
    assert!(
        detail.contains(&holder.job_id.to_string())
            && !detail.contains(&format!(
                "job={} workspace={}",
                waiting.job_id, waiting.workspace_key
            ))
    );
    assert_eq!(result.ran, Some(Ran::None));
    fs::write(release, "").unwrap();
    terminal(&runner, holder.job_id).await;
}

#[tokio::test]
async fn withdraw_racing_dispatch_never_refuses_a_started_job() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, bytes) = f.run("true");
    request.queue_wait_limit_s = 0;
    f.submit(&runner, &request, &bytes);
    let other = runner.clone();
    let id = request.job_id;
    let withdraw = std::thread::spawn(move || other.withdraw(id).unwrap());
    runner.tick().unwrap();
    let reply = withdraw.join().unwrap();
    let result = terminal(&runner, id).await;
    if result.ran == Some(Ran::Remote) {
        assert_eq!(reply, WithdrawOutcome::Started);
        assert!(!matches!(
            result.outcome,
            Outcome::RefusedBeforeStart { .. }
        ));
    } else {
        assert_eq!(reply, WithdrawOutcome::Withdrawn);
        assert!(matches!(result.outcome, Outcome::RefusedBeforeStart { .. }));
    }
}

#[tokio::test]
async fn drain_retry_hint_is_bounded_by_the_deadline_on_the_private_frame() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    runner.begin_drain(Duration::from_secs(60), false);
    let (request, _) = f.run("true");
    let hint = runner.drain_retry_after_ms().unwrap();
    assert!(hint <= 60_000 && hint > 0);
    let (socket, server) = listener(&f, &runner);
    let mut channel = connect(&socket).await;
    write_frame_async(&mut channel, &Frame::Run(request.clone()))
        .await
        .unwrap();
    let Frame::Refusal(refusal) = read_next(&mut channel).await else {
        panic!("expected refusal")
    };
    assert!(refusal.retry_after_ms.unwrap() <= hint);
    let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    server.abort();
}

#[test]
fn refused_transfer_survives_maintenance_and_restart_until_retention_expires() {
    let f = Fixture::new();
    fs::write(f.main.join("source"), "dirty snapshot\n").unwrap();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, bytes) = f.run_worktree("true");
    request.weight_hint = Some(30);
    request.queue_wait_limit_s = 0;
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        runner.withdraw(request.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    runner.maintenance().unwrap();
    let offered = vec![RepositoryCandidates {
        repository: f.root.clone(),
        commits: vec![request.snapshot_commit.clone()],
    }];
    assert!(runner.have(offered.clone())[0]
        .commits
        .contains(&request.snapshot_commit));
    drop(runner);
    let runner = reopen(f.config());
    runner.maintenance().unwrap();
    assert!(runner.have(offered.clone())[0]
        .commits
        .contains(&request.snapshot_commit));
    f.clock.fetch_add(RETENTION_MS + 60_000, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(!runner.have(offered)[0]
        .commits
        .contains(&request.snapshot_commit));
}

#[test]
fn failed_prepare_keeps_its_completed_transfer_after_guard_and_restart() {
    let f = Fixture::new();
    fs::write(f.main.join("source"), "prepared dirty snapshot\n").unwrap();
    let runner = Runner::open(f.config()).unwrap();
    let (request, bytes) = f.run_worktree("true");
    let mut prepare = prepare_for(&request);
    prepare.transfer_id = request.job_id;
    prepare.bundles = request.bundles.clone();
    let guard = runner.admit_prepare(&prepare).unwrap().unwrap();
    let upload = f.spool(&runner, &request, &bytes);
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    drop(guard);
    runner.maintenance().unwrap();
    drop(runner);
    let runner = reopen(f.config());
    let offered = vec![RepositoryCandidates {
        repository: f.root.clone(),
        commits: vec![request.snapshot_commit.clone()],
    }];
    assert!(runner.have(offered.clone())[0]
        .commits
        .contains(&request.snapshot_commit));
    f.clock.fetch_add(RETENTION_MS + 60_000, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(!runner.have(offered)[0]
        .commits
        .contains(&request.snapshot_commit));
}

#[tokio::test]
async fn queued_heartbeat_does_not_wait_for_the_global_state_lock() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, bytes) = f.run("true");
    request.weight_hint = Some(30);
    f.submit(&runner, &request, &bytes);
    let (socket, server) = listener(&f, &runner);
    let mut stream = connect(&socket).await;
    write_frame_async(
        &mut stream,
        &Frame::Attach(AttachRequest::new(request.job_id, 0)),
    )
    .await
    .unwrap();
    async fn queued_heartbeat(stream: &mut UnixStream) -> u64 {
        loop {
            if let Some(Frame::Heartbeat {
                queue_wait_ms: Some(ms),
            }) = read_frame_async(stream).await.unwrap()
            {
                return ms;
            }
        }
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), queued_heartbeat(&mut stream))
            .await
            .unwrap(),
        0
    );
    let hold = Hold::new("state");
    hold.arm();
    let _release = Release(hold.clone());
    let other = runner.clone();
    let hook = hold.hook();
    let blocked = std::thread::spawn(move || {
        let _state = other.core.inner.lock().unwrap();
        hook("state");
    });
    reached(&hold).await;
    f.clock.fetch_add(1_000, Ordering::SeqCst);
    let elapsed = tokio::time::timeout(Duration::from_secs(3), queued_heartbeat(&mut stream))
        .await
        .expect(
            "a queued heartbeat must remain writable while another operation owns the state lock",
        );
    assert_eq!(elapsed, 1_000);
    hold.release();
    blocked.join().unwrap();
    server.abort();
}

#[test]
fn terminal_transition_clears_published_queue_entry() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, bytes) = f.run("true");
    request.weight_hint = Some(30);
    request.queue_wait_limit_s = 0;
    f.submit(&runner, &request, &bytes);
    let progress = runner.queue_progress(request.job_id);
    assert_eq!(progress.elapsed_ms(), Some(0));
    assert_eq!(
        runner.withdraw(request.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    assert_eq!(progress.elapsed_ms(), None);
}

/// Exercise the engine through an injected implementation, rather than testing
/// only the host adapter (which would also pass if the engine bypassed it).
struct RecordingBackend(Arc<Mutex<Vec<(Uuid, &'static str)>>>);

impl Backend for RecordingBackend {
    fn materialize(&self, config: &Config, trees: &Trees, setup: &Setup) -> io::Result<Prepared> {
        self.0.lock().unwrap().push((setup.id, "materialize"));
        backend::HostBackend.materialize(config, trees, setup)
    }
    fn launch(
        &self,
        config: &Config,
        trees: &Trees,
        setup: &Setup,
        prepared: &Prepared,
        weight: u32,
    ) -> io::Result<backend::Launch> {
        self.0.lock().unwrap().push((setup.id, "launch"));
        backend::HostBackend.launch(config, trees, setup, prepared, weight)
    }
    fn cancel(&self, config: &Config, job: &Job, hard: bool) -> io::Result<()> {
        self.0.lock().unwrap().push((job.id, "cancel"));
        backend::HostBackend.cancel(config, job, hard)
    }
    fn unit_alive(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        backend::HostBackend.unit_alive(config, process)
    }
    fn collect_changes(&self, _trees: &Trees, job: &Job) -> io::Result<Option<Vec<String>>> {
        self.0.lock().unwrap().push((job.id, "collect_changes"));
        Ok((!job.base).then(|| vec!["backend-collected".into()]))
    }
    fn reconcile(&self, config: &Config, interrupted: &[Job]) -> io::Result<Vec<Uuid>> {
        for job in interrupted {
            self.0.lock().unwrap().push((job.id, "reconcile"));
        }
        backend::HostBackend.reconcile(config, interrupted)
    }
}

#[tokio::test]
async fn engine_routes_all_five_operations_through_the_backend() {
    let f = Fixture::new();
    let config = f.config();
    let mut interrupted = Job::new(Uuid::now_v7(), None, (config.clock)(), 0, 0);
    interrupted.state = JobState::Running;
    interrupted.started_ms = Some((config.clock)());
    let directory = config
        .state_dir()
        .join("jobs")
        .join(interrupted.id.to_string());
    fs::create_dir_all(&directory).unwrap();
    atomic_json(&directory.join("record.json"), &interrupted).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let runner =
        Runner::open_with_backend(config, Box::new(RecordingBackend(events.clone()))).unwrap();
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[(interrupted.id, "reconcile")]
    );
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&interrupted.id].state,
        JobState::Lost
    );
    let (request, bytes) = f.run("touch backend-ready; sleep 1");
    f.submit(&runner, &request, &bytes);
    until(|| Path::new(&f.key).join("backend-ready").exists()).await;
    runner.cancel(request.job_id).unwrap();
    let record = terminal(&runner, request.job_id).await;
    assert_eq!(
        record.workspace_changes,
        Some(vec!["backend-collected".into()])
    );
    let mut operations: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _)| *id == request.job_id)
        .map(|(_, operation)| *operation)
        .collect();
    // The scheduler may retry while the job is running, but collecting its
    // changes must fence off all further backend cancellation.
    let collected = operations
        .iter()
        .position(|operation| *operation == "collect_changes")
        .unwrap();
    assert!(!operations[collected + 1..].contains(&"cancel"));
    operations.dedup();
    assert_eq!(
        operations,
        ["materialize", "launch", "cancel", "collect_changes"]
    );
    base_done(&runner).await;
}

#[test]
fn vm_selection_fails_before_creating_host_state_and_never_falls_back() {
    let scratch = ScratchDir::new("motor-backend-selection");
    let mut config = Config::new(scratch.join("state"), "main");
    assert_eq!(config.backend, BackendKind::Host);
    config.backend = BackendKind::Vm;
    let error = Runner::open(config.clone())
        .err()
        .expect("missing VM config must fail");
    assert!(error
        .to_string()
        .contains("--backend vm requires --vm-config"));
    assert!(!config.state_dir().exists());
    config.vm_config = Some(scratch.join("vm.toml"));
    let error = Runner::open(config.clone())
        .err()
        .expect("unavailable VM backend must fail");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(!config.state_dir().exists());
}

// The privileged isolation tests reuse these engine fixtures to exercise real
// Run admission, materialization and recovery rather than launching only a sandbox.
#[path = "tests/network.rs"]
pub(super) mod network;

#[test]
fn runner_socket_parent_is_private_before_binding() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let config = f.config();
    fs::create_dir_all(config.state_dir()).unwrap();
    fs::set_permissions(config.state_dir(), fs::Permissions::from_mode(0o777)).unwrap();
    let runner = Runner::open(config).unwrap();
    assert_eq!(
        fs::metadata(runner.config().socket_path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn admission_failure_diagnostics_include_config_while_state_is_locked() {
    let f = Fixture::new();
    let config = f.config();
    let roots = format!("allowed_roots={:?}", config.owned_workspace_roots);
    let runner = Runner::open(config).unwrap();
    let state = runner.core.inner.lock().unwrap();
    let message = runner_diagnostics(&runner);
    drop(state);
    assert!(message.contains(&roots), "missing {roots:?} in {message}");
    assert!(
        message.contains("runner state lock unavailable"),
        "{message}"
    );
}

#[test]
fn admission_failure_diagnostics_include_refusal_and_destination() {
    let f = Fixture::new();
    let mut config = f.config();
    config.owned_workspace_roots = vec![f.scratch.join("other-destinations")];
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_run_accepted(&runner, &request);
    }))
    .unwrap_err();
    let message = panic.downcast_ref::<String>().unwrap();
    for detail in [
        "WorkspaceKeyRejected",
        "allowed_roots=",
        &f.key,
        &request.job_id.to_string(),
    ] {
        assert!(message.contains(detail), "missing {detail:?} in {message}");
    }
}

#[test]
fn setup_refusal_detail_is_bounded_redacted_and_not_part_of_the_reason() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, _) = f.run("true");
    let secret = "private-error-canary";
    request.env.insert("RUSTFLAGS".into(), secret.into());
    runner.accept(request.clone()).unwrap();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&request.job_id)
        .unwrap()
        .state = JobState::Queued;
    let error = io::Error::other(format!("{secret}: {}", "é".repeat(1024)));
    runner
        .launch(
            Setup {
                id: request.job_id,
                cpus: vec![1, 17],
                request: request.clone(),
                base: false,
                recorded: BTreeMap::new(),
            },
            Err(error),
        )
        .unwrap();
    let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceSetupFailed
        }
    );
    let detail = terminal.refusal_detail().unwrap();
    assert!(
        detail.starts_with("prepare job workspace: runner environment"),
        "{detail}"
    );
    assert!(detail.len() <= REFUSAL_DETAIL_MAX_BYTES);
    assert!(detail.len() >= REFUSAL_DETAIL_MAX_BYTES - 1);
    assert!(!serde_json::to_string(&terminal).unwrap().contains(secret));
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.retry_after_ms(), None);
}
