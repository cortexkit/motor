use super::*;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Barrier, Mutex,
    },
};

#[derive(Clone, Copy)]
struct Node {
    attributes: FsXattr,
    blocks: u64,
}
#[derive(Default)]
struct KernelState {
    nodes: BTreeMap<(u64, u64), Node>,
    limits: BTreeMap<u32, (u64, u64)>,
}
struct FakeSystem {
    mount: PathBuf,
    state: Mutex<KernelState>,
    free: AtomicU64,
    fail_monitor: AtomicBool,
    release_barrier: Mutex<Option<Arc<Barrier>>>,
}
impl FakeSystem {
    fn new(mount: &Path) -> Self {
        Self {
            mount: mount.to_owned(),
            state: Mutex::new(KernelState::default()),
            free: AtomicU64::new(128 << 30),
            fail_monitor: AtomicBool::new(false),
            release_barrier: Mutex::new(None),
        }
    }
    fn project(&self, path: &Path) -> u32 {
        self.attributes(&File::open(path).unwrap()).unwrap().project
    }
    fn forget_project(&self, project: u32) {
        let mut state = self.state.lock().unwrap();
        state.limits.remove(&project);
        for node in state.nodes.values_mut() {
            if node.attributes.project == project {
                node.attributes.project = 0;
            }
        }
    }
}
impl QuotaSystem for FakeSystem {
    fn mount(&self) -> &Path {
        &self.mount
    }
    fn quota(&self, id: u32, limits: Option<(u64, u64)>) -> io::Result<DiskQuota> {
        let mut state = self.state.lock().unwrap();
        if let Some(limits) = limits {
            if limits == (0, 0) {
                state.limits.remove(&id);
            } else {
                state.limits.insert(id, limits);
            }
        }
        let (blocks, inodes) = state
            .nodes
            .values()
            .filter(|node| node.attributes.project == id)
            .fold((0, 0), |(b, i), node| (b + node.blocks, i + 1));
        let configured = state.limits.get(&id).copied();
        if limits.is_none() && configured.is_none() && inodes == 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        let (block_hard, inode_hard) = configured.unwrap_or_default();
        Ok(DiskQuota {
            id,
            blocks,
            inodes,
            block_hard,
            inode_hard,
            ..Default::default()
        })
    }
    fn attributes(&self, file: &File) -> io::Result<FsXattr> {
        let metadata = file.metadata()?;
        let mut state = self.state.lock().unwrap();
        Ok(state
            .nodes
            .entry((metadata.dev(), metadata.ino()))
            .or_insert(Node {
                attributes: FsXattr::default(),
                blocks: metadata.blocks(),
            })
            .attributes)
    }
    fn set_attributes(&self, file: &File, attr: &FsXattr) -> io::Result<()> {
        let metadata = file.metadata()?;
        self.state.lock().unwrap().nodes.insert(
            (metadata.dev(), metadata.ino()),
            Node {
                attributes: *attr,
                blocks: metadata.blocks(),
            },
        );
        Ok(())
    }
    fn available(&self, _: &Path) -> io::Result<(u64, u64)> {
        Ok((self.free.load(Ordering::Relaxed), 1 << 24))
    }
    fn monitor(&self, _: &Path, _: u32, _: u64) -> io::Result<()> {
        if self.fail_monitor.load(Ordering::Relaxed) {
            Err(io::Error::other("injected netlink failure"))
        } else {
            Ok(())
        }
    }
    fn finish_monitor(&self, _: &Path) -> bool {
        false
    }
    fn before_release_lock(&self) {
        let barrier = self.release_barrier.lock().unwrap().clone();
        if let Some(barrier) = barrier {
            barrier.wait();
        }
    }
}

struct Fixture {
    _scratch: cortexkit_test_support::ScratchDir,
    system: Arc<FakeSystem>,
    state: PathBuf,
    workspace: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let scratch = cortexkit_test_support::ScratchDir::new("boat-quota-lifecycle");
        let state = scratch.path().join("state");
        let workspace = scratch.path().join("workspace");
        fs::create_dir_all(workspace.join("target")).unwrap();
        fs::write(workspace.join("target/reused"), b"cached bytes").unwrap();
        Self {
            system: Arc::new(FakeSystem::new(scratch.path())),
            _scratch: scratch,
            state,
            workspace,
        }
    }
    fn roots(&self, id: Uuid) -> Vec<PathBuf> {
        let runtime = self.runtime(id);
        let spool = self.state.join("spools").join(id.to_string());
        for directory in [
            runtime.join("home"),
            runtime.join("tmp"),
            runtime.join("output"),
            spool.clone(),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        for file in ["stdout", "stderr", "exit", "cache-stats"] {
            fs::write(runtime.join("output").join(file), [b' '; 32]).unwrap();
        }
        job_roots(
            &self.workspace,
            &self.workspace.join("target"),
            &runtime,
            &spool,
        )
    }
    fn runtime(&self, id: Uuid) -> PathBuf {
        self.state.join("sandboxes").join(id.to_string())
    }
    fn manifest(&self, id: Uuid) -> PathBuf {
        self.state.join("boat-quotas").join(format!("{id}.json"))
    }
    fn prepare(&self, id: Uuid) -> io::Result<()> {
        prepare_with(self.system.as_ref(), &self.state, id, self.roots(id))
    }
}

#[test]
fn stale_release_after_new_prepare_preserves_new_workspace_project() {
    let fixture = Fixture::new();
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    fixture.prepare(a).unwrap();
    fixture.prepare(b).unwrap();
    let lease: Lease = serde_json::from_slice(&fs::read(fixture.manifest(b)).unwrap()).unwrap();
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)).unwrap();
    for path in [
        &fixture.workspace,
        &fixture.workspace.join("target"),
        &fixture.workspace.join("target/reused"),
        &fixture.runtime(b).join("home"),
        &fixture.runtime(b).join("output/stdout"),
    ] {
        assert_eq!(
            fixture.system.project(path),
            lease.project,
            "stale cleanup changed {}",
            path.display()
        );
    }
    assert!(!fixture.manifest(a).exists());
    assert!(fixture.manifest(b).exists());
}

#[test]
fn pending_monitor_failure_with_no_dquot_does_not_poison_next_prepare() {
    let fixture = Fixture::new();
    let a = Uuid::now_v7();
    fixture.system.fail_monitor.store(true, Ordering::Relaxed);
    assert!(fixture.prepare(a).is_err());
    fixture.system.fail_monitor.store(false, Ordering::Relaxed);
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)).unwrap();
    assert!(!fixture.manifest(a).exists());
    fixture.prepare(Uuid::now_v7()).unwrap();
}

#[test]
fn cleared_dquot_before_manifest_removal_is_recoverable() {
    let fixture = Fixture::new();
    let a = Uuid::now_v7();
    fixture.prepare(a).unwrap();
    let lease: Lease = serde_json::from_slice(&fs::read(fixture.manifest(a)).unwrap()).unwrap();
    fixture.system.forget_project(lease.project);
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)).unwrap();
    assert!(!fixture.manifest(a).exists());
    fixture.prepare(Uuid::now_v7()).unwrap();
}

#[test]
fn simultaneous_runtime_release_is_idempotent() {
    let fixture = Fixture::new();
    let a = Uuid::now_v7();
    fixture.prepare(a).unwrap();
    *fixture.system.release_barrier.lock().unwrap() = Some(Arc::new(Barrier::new(2)));
    let results = std::thread::scope(|scope| {
        let first =
            scope.spawn(|| release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)));
        let second =
            scope.spawn(|| release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)));
        [first.join().unwrap(), second.join().unwrap()]
    });
    for result in results {
        result.unwrap();
    }
    assert!(!fixture.manifest(a).exists());
}
