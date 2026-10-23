use super::*;
use std::os::fd::{AsRawFd, FromRawFd};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Barrier, Mutex,
    },
};

pub(crate) struct DeepTree {
    leaf: File,
    depth: usize,
    name: std::ffi::CString,
}
impl DeepTree {
    pub(crate) fn unlabelled(root: &Path, depth: usize, width: usize) -> Self {
        Self::new(root, depth, width, None)
    }
    fn new(root: &Path, depth: usize, width: usize, owner: Option<(&FakeSystem, u32)>) -> Self {
        let mut leaf = File::open(root).unwrap();
        let name = std::ffi::CString::new(vec![b'd'; width]).unwrap();
        for _ in 0..depth {
            assert_eq!(
                unsafe { libc::mkdirat(leaf.as_raw_fd(), name.as_ptr(), 0o755) },
                0
            );
            let fd = unsafe {
                libc::openat(
                    leaf.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            assert!(fd >= 0);
            leaf = unsafe { File::from_raw_fd(fd) };
            if let Some((system, project)) = owner {
                system
                    .set_attributes(&leaf, &assignment(FsXattr::default(), project, true))
                    .unwrap();
            }
        }
        let fd = unsafe {
            libc::openat(
                leaf.as_raw_fd(),
                c"file".as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        };
        assert!(fd >= 0);
        let file = unsafe { File::from_raw_fd(fd) };
        if let Some((system, project)) = owner {
            system
                .set_attributes(&file, &assignment(FsXattr::default(), project, false))
                .unwrap();
        }
        Self { leaf, depth, name }
    }
}
impl Drop for DeepTree {
    fn drop(&mut self) {
        // Teardown is also descriptor-relative, even when an assertion failed or
        // the runtime root was renamed into quarantine during the test.
        unsafe {
            libc::unlinkat(self.leaf.as_raw_fd(), c"file".as_ptr(), 0);
        }
        for _ in 0..self.depth {
            let fd = unsafe {
                libc::openat(
                    self.leaf.as_raw_fd(),
                    c"..".as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return;
            }
            let parent = unsafe { File::from_raw_fd(fd) };
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), self.name.as_ptr(), libc::AT_REMOVEDIR);
            }
            self.leaf = parent;
        }
    }
}

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

#[test]
fn descriptor_walk_handles_descendants_longer_than_path_max() {
    let fixture = Fixture::new();
    let _tree = DeepTree::new(&fixture.workspace, 30, 200, None);
    let mut visited = 0;
    visit(
        &fixture.workspace,
        42,
        fs::metadata(&fixture.workspace).unwrap().dev(),
        &mut |_, _, _| {
            visited += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(visited, 34); // root, target, cached file, 30 directories and leaf
}

#[test]
fn hardlink_aliases_are_updated_once() {
    let fixture = Fixture::new();
    let root = fixture._scratch.path().join("hardlinks");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("original"), b"one inode").unwrap();
    for index in 0..64 {
        fs::hard_link(root.join("original"), root.join(format!("link-{index}"))).unwrap();
    }
    let mut files = 0;
    visit(
        &root,
        42,
        fs::metadata(&root).unwrap().dev(),
        &mut |_, _, directory| {
            if !directory {
                files += 1;
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        files, 1,
        "hardlinks must not amplify privileged inode operations"
    );
}

#[test]
fn symlink_and_special_lease_roots_are_refused_before_any_assignment() {
    for special in [false, true] {
        let fixture = Fixture::new();
        let id = Uuid::now_v7();
        let roots = fixture.roots(id);
        let bad = roots.last().unwrap();
        fs::remove_dir(bad).unwrap();
        if special {
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(bad.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        } else {
            std::os::unix::fs::symlink(&fixture.workspace, bad).unwrap();
        }
        assert!(
            prepare_with(fixture.system.as_ref(), &fixture.state, id, roots).is_err(),
            "nonphysical lease root was accepted"
        );
        assert_eq!(
            fixture.system.project(&fixture.workspace),
            0,
            "roots must be validated before relabeling any tree"
        );
    }
}

#[test]
fn hostile_private_depth_releases_capacity_and_quarantines_without_recursion() {
    let fixture = Fixture::new();
    let id = Uuid::now_v7();
    fixture.prepare(id).unwrap();
    let lease: Lease = serde_json::from_slice(&fs::read(fixture.manifest(id)).unwrap()).unwrap();
    let _tree = DeepTree::new(
        &fixture.runtime(id).join("tmp"),
        2200,
        1,
        Some((fixture.system.as_ref(), lease.project)),
    );
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(id)).unwrap();
    assert!(
        !fixture.manifest(id).exists(),
        "failed relabel must not retain a capacity reservation"
    );
    assert!(
        !fixture.runtime(id).exists(),
        "hostile private data must be detached into quarantine"
    );
    fixture.prepare(Uuid::now_v7()).unwrap();
}

#[test]
fn hardlink_walk_has_entry_and_elapsed_budgets() {
    let fixture = Fixture::new();
    let root = fixture._scratch.path().join("bounded-links");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file"), b"one inode").unwrap();
    for index in 0..64 {
        fs::hard_link(root.join("file"), root.join(format!("link-{index}"))).unwrap();
    }
    let device = fs::metadata(&root).unwrap().dev();
    for limits in [
        walk::Limits {
            entries: 8,
            ..walk::Limits::default()
        },
        walk::Limits {
            elapsed: std::time::Duration::ZERO,
            ..walk::Limits::default()
        },
    ] {
        let mut budget = walk::Budget::new(limits);
        let mut updates = 0;
        let result = walk::physical_root(&root, &budget).and_then(|root| {
            walk::visit(root, device, &mut budget, &mut |_, _| {
                updates += 1;
                Ok(true)
            })
        });
        assert!(result.as_ref().is_err_and(walk::is_limit));
        assert!(
            updates <= 2,
            "hardlinks or a deadline amplified inode operations: {updates}"
        );
    }
}

#[test]
fn quarantined_project_ids_are_not_reused_for_new_jobs() {
    let fixture = Fixture::new();
    let a = Uuid::now_v7();
    fixture.prepare(a).unwrap();
    let first: Lease = serde_json::from_slice(&fs::read(fixture.manifest(a)).unwrap()).unwrap();
    let _tree = DeepTree::new(
        &fixture.runtime(a).join("tmp"),
        2200,
        1,
        Some((fixture.system.as_ref(), first.project)),
    );
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)).unwrap();
    let b = Uuid::now_v7();
    fixture.prepare(b).unwrap();
    let second: Lease = serde_json::from_slice(&fs::read(fixture.manifest(b)).unwrap()).unwrap();
    assert!(
        second.project > first.project,
        "quarantined inodes must not become another job's quota usage"
    );
}

#[test]
fn launch_requires_active_matching_limits_and_root_ownership() {
    for fault in ["pending", "missing-kernel", "wrong-root"] {
        let fixture = Fixture::new();
        let id = Uuid::now_v7();
        fixture.prepare(id).unwrap();
        let mut lease: Lease =
            serde_json::from_slice(&fs::read(fixture.manifest(id)).unwrap()).unwrap();
        validate_for_launch_with(fixture.system.as_ref(), &fixture.state, id, &lease.roots)
            .unwrap();
        match fault {
            "pending" => {
                lease.phase = Phase::Pending;
                crate::storage::atomic_json_private(&fixture.manifest(id), &lease).unwrap();
            }
            "missing-kernel" => fixture.system.forget_project(lease.project),
            "wrong-root" => fixture
                .system
                .set_attributes(
                    &File::open(&fixture.workspace).unwrap(),
                    &FsXattr::default(),
                )
                .unwrap(),
            _ => unreachable!(),
        }
        assert!(
            validate_for_launch_with(fixture.system.as_ref(), &fixture.state, id, &lease.roots)
                .is_err(),
            "launch accepted {fault}"
        );
    }
}

#[test]
fn consumed_upload_spool_is_not_a_required_job_mount_at_launch() {
    let fixture = Fixture::new();
    let id = Uuid::now_v7();
    fixture.prepare(id).unwrap();
    let lease: Lease = serde_json::from_slice(&fs::read(fixture.manifest(id)).unwrap()).unwrap();
    fs::remove_dir(lease.roots.last().unwrap()).unwrap();
    validate_for_launch_with(fixture.system.as_ref(), &fixture.state, id, &lease.roots).unwrap();
}
