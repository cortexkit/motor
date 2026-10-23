use super::*;
use std::os::fd::{AsRawFd, FromRawFd};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Barrier, Mutex,
    },
};

pub(crate) struct DeepTree {
    leaf: File,
    depth: usize,
    owned: BTreeSet<u64>,
}
impl DeepTree {
    pub(crate) fn unlabelled(root: &Path, depth: usize, width: usize) -> Self {
        Self::new(root, depth, width, None)
    }
    fn new(root: &Path, depth: usize, width: usize, owner: Option<(&FakeSystem, u32)>) -> Self {
        let mut leaf = File::open(root).unwrap();
        let mut owned = BTreeSet::new();
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
            owned.insert(leaf.metadata().unwrap().ino());
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
        Self { leaf, depth, owned }
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
            let Ok(current) = self.leaf.metadata() else {
                return;
            };
            if current.nlink() == 0 || !self.owned.contains(&current.ino()) {
                return;
            }
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
            // GC can lift this inode under a fresh private name. Match only our
            // pinned inode, never an unrelated sibling in its new parent.
            if let Ok(entries) = fs::read_dir(format!("/proc/self/fd/{}", parent.as_raw_fd())) {
                use std::os::unix::ffi::OsStrExt;
                for entry in entries.flatten() {
                    let Ok(name) = std::ffi::CString::new(entry.file_name().as_bytes()) else {
                        continue;
                    };
                    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                    if unsafe {
                        libc::fstatat(
                            parent.as_raw_fd(),
                            name.as_ptr(),
                            stat.as_mut_ptr(),
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    } != 0
                    {
                        continue;
                    }
                    let stat = unsafe { stat.assume_init() };
                    if stat.st_dev == current.dev() && stat.st_ino == current.ino() {
                        unsafe {
                            libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                        }
                        break;
                    }
                }
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
#[derive(Clone, Default)]
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
    fn restarted(&self) -> Self {
        Self {
            mount: self.mount.clone(),
            state: Mutex::new(self.state.lock().unwrap().clone()),
            free: AtomicU64::new(self.free.load(Ordering::Relaxed)),
            fail_monitor: AtomicBool::new(false),
            release_barrier: Mutex::new(None),
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
    minimum_growth: AtomicU64,
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
            minimum_growth: AtomicU64::new(MIN_COLD_CAP_BYTES),
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
        prepare_with(
            self.system.as_ref(),
            &self.state,
            id,
            self.roots(id),
            false,
            self.minimum_growth.load(Ordering::Relaxed),
        )
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
fn release_before_successor_clears_owned_roots_and_preserves_new_assignment() {
    let fixture = Fixture::new();
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    fixture.prepare(a).unwrap();
    let first: Lease = serde_json::from_slice(&fs::read(fixture.manifest(a)).unwrap()).unwrap();
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)).unwrap();
    assert_eq!(fixture.system.project(&fixture.workspace), 0);
    assert_eq!(
        fixture
            .system
            .project(&fixture.runtime(a).join("output/stdout")),
        0
    );
    assert!(usage(fixture.system.as_ref(), first.project)
        .unwrap()
        .is_none());
    fixture.prepare(b).unwrap();
    let second: Lease = serde_json::from_slice(&fs::read(fixture.manifest(b)).unwrap()).unwrap();
    validate_for_launch_with(fixture.system.as_ref(), &fixture.state, b, &second.roots).unwrap();
}

#[test]
fn restart_with_partial_legacy_lease_cannot_unquota_the_successor() {
    let mut fixture = Fixture::new();
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    fixture.prepare(a).unwrap();
    let first: Lease = serde_json::from_slice(&fs::read(fixture.manifest(a)).unwrap()).unwrap();
    let mut pending = serde_json::to_value(&first).unwrap();
    pending.as_object_mut().unwrap().remove("phase");
    pending["blocks"] = 0.into();
    pending["inodes"] = 0.into();
    crate::storage::atomic_json_private(&fixture.manifest(a), &pending).unwrap();
    fixture
        .system
        .state
        .lock()
        .unwrap()
        .limits
        .remove(&first.project);
    let restarted = fixture.system.restarted();
    fixture.system = Arc::new(restarted);
    fixture.prepare(b).unwrap();
    let second: Lease = serde_json::from_slice(&fs::read(fixture.manifest(b)).unwrap()).unwrap();
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)).unwrap();
    validate_for_launch_with(fixture.system.as_ref(), &fixture.state, b, &second.roots).unwrap();
    assert_eq!(
        fixture
            .system
            .project(&fixture.workspace.join("target/reused")),
        second.project
    );
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
            prepare_with(
                fixture.system.as_ref(),
                &fixture.state,
                id,
                roots,
                false,
                MIN_COLD_CAP_BYTES
            )
            .is_err(),
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

#[test]
fn reused_baseline_consumes_the_total_physical_allowance() {
    let fixture = Fixture::new();
    const CAP: u64 = 16 << 20;
    fixture.minimum_growth.store(4 << 20, Ordering::Relaxed);
    fixture.system.free.store(
        PROVISIONAL_RESERVE + crate::storage::REPLAY_BYTES + CAP,
        Ordering::Relaxed,
    );
    fs::write(fixture.workspace.join("target/baseline"), vec![7; 4 << 20]).unwrap();
    let id = Uuid::now_v7();
    fixture.prepare(id).unwrap();
    let lease: Lease = serde_json::from_slice(&fs::read(fixture.manifest(id)).unwrap()).unwrap();
    let initial = fixture.system.quota(lease.project, None).unwrap().blocks * 512;
    assert!(initial >= 4 << 20);
    assert_eq!(lease.blocks * 512, CAP);
    assert_eq!(lease.growth_bytes, CAP - initial);
}

#[test]
fn reflink_baseline_cannot_turn_released_logical_charge_into_extra_physical_growth() {
    use std::io::Write;
    let fixture = Fixture::new();
    const CAP: u64 = 16 << 20;
    fixture.minimum_growth.store(4 << 20, Ordering::Relaxed);
    fixture.system.free.store(
        PROVISIONAL_RESERVE + crate::storage::REPLAY_BYTES + CAP,
        Ordering::Relaxed,
    );
    let a = Uuid::now_v7();
    fixture.prepare(a).unwrap();
    let lease_a: Lease = serde_json::from_slice(&fs::read(fixture.manifest(a)).unwrap()).unwrap();
    let source = fixture.workspace.join("target/source");
    fs::write(&source, vec![9; 1 << 20]).unwrap();
    let input = File::open(&source).unwrap();
    fixture
        .system
        .set_attributes(
            &input,
            &assignment(FsXattr::default(), lease_a.project, false),
        )
        .unwrap();
    let mut copies = Vec::new();
    for index in 0..7 {
        let path = fixture.workspace.join(format!("target/shared-{index}"));
        let output = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe { libc::ioctl(output.as_raw_fd(), 0x40049409_u64, input.as_raw_fd()) },
            0,
            "this regression requires reflink-capable scratch"
        );
        fixture
            .system
            .set_attributes(
                &output,
                &assignment(FsXattr::default(), lease_a.project, false),
            )
            .unwrap();
        copies.push(path);
    }
    assert!(fixture.system.quota(lease_a.project, None).unwrap().blocks * 512 >= 8 << 20);
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(a)).unwrap();
    let b = Uuid::now_v7();
    fixture.prepare(b).unwrap();
    let lease_b: Lease = serde_json::from_slice(&fs::read(fixture.manifest(b)).unwrap()).unwrap();
    // Unlinking these reflinks drops logical quota charges but does not free
    // their shared data: source still holds every extent. Model that kernel
    // charge change while using actual FICLONE and unique-data writes on XFS.
    for path in copies {
        let metadata = fs::metadata(&path).unwrap();
        fs::remove_file(path).unwrap();
        fixture
            .system
            .state
            .lock()
            .unwrap()
            .nodes
            .remove(&(metadata.dev(), metadata.ino()));
    }
    let used = fixture.system.quota(lease_b.project, None).unwrap().blocks;
    let maximum_new_data = lease_b.blocks.saturating_sub(used) * 512;
    let unique = fixture.workspace.join("target/unique");
    let mut output = File::create(&unique).unwrap();
    let data = vec![3; 1 << 20];
    let mut remaining = maximum_new_data;
    while remaining != 0 {
        let count = remaining.min(data.len() as u64) as usize;
        output.write_all(&data[..count]).unwrap();
        remaining -= count as u64;
    }
    output.sync_all().unwrap();
    drop(output);
    let physical_new_data = fs::metadata(unique).unwrap().blocks() * 512;
    assert!(physical_new_data <= CAP, "reflink baseline permitted {physical_new_data} bytes of new physical data beyond cap {CAP}");
}

#[test]
fn bounded_private_gc_can_finish_a_quarantined_deep_tree() {
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
    let container = fixture.state.join("boat-quarantine");
    for _ in 0..32 {
        let roots: Vec<_> = fs::read_dir(&container)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        if roots.is_empty() {
            return;
        }
        for root in roots {
            remove_retired_tree(&root).unwrap();
        }
    }
    panic!("bounded GC made no progress through a legal deep tree");
}

#[test]
fn failed_quarantine_keeps_retirement_durable_until_detachment() {
    let fixture = Fixture::new();
    let id = Uuid::now_v7();
    fixture.prepare(id).unwrap();
    let lease: Lease = serde_json::from_slice(&fs::read(fixture.manifest(id)).unwrap()).unwrap();
    let _tree = DeepTree::new(&fixture.runtime(id).join("tmp"), 2200, 1, Some((fixture.system.as_ref(), lease.project)));
    let blocked = fixture.state.join("boat-quarantine");
    fs::write(&blocked, b"block the rename destination").unwrap();
    assert!(release_runtime_with(fixture.system.as_ref(), &fixture.runtime(id)).is_err());
    assert!(fixture.manifest(id).exists(), "retirement state vanished before runtime detachment");
    fs::remove_file(blocked).unwrap();
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(id)).unwrap();
    assert!(!fixture.runtime(id).exists());
    assert!(!fixture.manifest(id).exists());
}

#[test]
fn manifest_free_crash_recovery_never_uses_unbounded_permission_repair() {
    let fixture = Fixture::new();
    let id = Uuid::now_v7();
    fixture.prepare(id).unwrap();
    let lease: Lease = serde_json::from_slice(&fs::read(fixture.manifest(id)).unwrap()).unwrap();
    let _tree = DeepTree::new(&fixture.runtime(id).join("tmp"), 2200, 1, Some((fixture.system.as_ref(), lease.project)));
    fixture.system.quota(lease.project, Some((0, 0))).unwrap();
    fs::remove_file(fixture.manifest(id)).unwrap(); // Crash at the old unlink-before-detach window.
    crate::systemd::with_process_hook(Box::new(|_| panic!("ordinary recursive permission helper reached during Boat recovery")), || {
        crate::isolation::reclaim_runtime(&fixture.runtime(id)).unwrap();
    });
    assert!(!fixture.runtime(id).exists());
}

#[test]
fn long_named_boat_runtime_reclamation_is_bounded_after_lease_removal() {
    let fixture = Fixture::new();
    let id = Uuid::now_v7();
    fixture.prepare(id).unwrap();
    let lease: Lease = serde_json::from_slice(&fs::read(fixture.manifest(id)).unwrap()).unwrap();
    let _tree = DeepTree::new(&fixture.runtime(id).join("tmp"), 30, 200, Some((fixture.system.as_ref(), lease.project)));
    release_runtime_with(fixture.system.as_ref(), &fixture.runtime(id)).unwrap();
    crate::systemd::with_process_hook(Box::new(|_| panic!("PATH_MAX-sensitive permission helper reached for Boat runtime")), || {
        crate::isolation::reclaim_runtime(&fixture.runtime(id)).unwrap();
    });
    assert!(!fixture.runtime(id).exists());
}
