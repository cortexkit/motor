use super::*;
use crate::linux::root_test_support::ReflinkStore;

fn fixture() -> (PathBuf, LinuxJobs, job::Fill) {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = super::super::tests::scratch();
    let mut jobs = super::super::tests::machine(&base);
    jobs.settings = Some(JobVms {
        uid_base: 500000,
        uid_count: 2,
        memory_max_bytes: 1048576,
        workspace_disk_max_bytes: Some(4096),
        fixed_path_repositories: Default::default(),
    });
    let fill = job::Fill {
        account: "account".into(),
        repository_key: "repo".into(),
        fill_id: job::test_id(700),
        size_bytes: 4096,
    };
    (base, jobs, fill)
}

fn exited(jobs: &mut LinuxJobs, fill: &job::Fill, success: Option<bool>, oom: Option<u64>) {
    let mut record = super::super::tests::record();
    record.start.kind = job::VmKind::Fill;
    record.start.job_id = fill.fill_id.clone();
    record.start.account = fill.account.clone();
    record.start.workspace_disk_id = fill.disk_id();
    record.released = true;
    record.fill_exit_success = success;
    record.oom_kill = oom;
    jobs.records.insert(fill.fill_id.clone(), record);
}

#[test]
fn isolated_fill_provisioning_owns_provenance_and_cannot_relabel_job_disks() {
    let (base, mut jobs, fill) = fixture();
    let peer = Peer { uid: 0, gid: 0 };
    jobs.handle(Operation::PrepareFill(fill.clone()), peer)
        .unwrap();
    let (registry, disk) = open_disk(&jobs.workspaces, &fill.account, &fill.disk_id()).unwrap();
    assert_eq!(registry.role, job::WorkspaceRole::Fill);
    assert_eq!(disk.metadata().unwrap().uid(), 0);
    assert_eq!(disk.metadata().unwrap().blocks(), 0);
    jobs.handle(Operation::PrepareFill(fill.clone()), peer)
        .unwrap();
    let mut other = fill.clone();
    other.repository_key = "foreign".into();
    assert!(jobs.handle(Operation::PrepareFill(other), peer).is_err());
    assert!(jobs
        .handle(
            Operation::EnsureWorkspace {
                account: fill.account.clone(),
                disk_id: fill.disk_id(),
                size_bytes: 4096
            },
            peer
        )
        .is_err());
    assert!(jobs
        .handle(
            Operation::Start(Start {
                kind: job::VmKind::Job,
                job_id: job::test_id(701),
                account: fill.account.clone(),
                workspace_disk_id: fill.disk_id(),
                image_generation_id: "image".into(),
                vcpus: 1,
                memory_mib: 128,
                burst_mib: None
            }),
            peer
        )
        .map(|r| matches!(r.reply, Reply::StartRefused { .. }))
        .unwrap());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_fill_promotion_requires_publication_clean_exit_evidence_and_detachment() {
    for (published, success, oom, promoted) in [
        (true, Some(true), Some(0), true),
        (false, Some(true), Some(0), false),
        (true, Some(false), Some(0), false),
        (true, None, Some(0), false),
        (true, Some(true), Some(1), false),
        (true, Some(true), None, false),
    ] {
        let (base, mut jobs, fill) = fixture();
        prepare_fill(&jobs.workspaces, &fill).unwrap();
        exited(&mut jobs, &fill, success, oom);
        assert_eq!(jobs.finish_fill(&fill, published).unwrap(), promoted);
        if promoted {
            assert_eq!(
                open_disk(&jobs.workspaces, &fill.account, &fill.disk_id())
                    .unwrap()
                    .0
                    .role,
                job::WorkspaceRole::Base
            );
            assert!(jobs.finish_fill(&fill, true).unwrap());
        } else {
            assert!(!jobs.workspaces.join(fill.disk_id()).exists());
        }
        std::fs::remove_dir_all(base).unwrap();
    }
    let (base, mut jobs, fill) = fixture();
    prepare_fill(&jobs.workspaces, &fill).unwrap();
    // A Fill role alone never proves that any VM ran or cleanly exited.
    assert!(!jobs.finish_fill(&fill, true).unwrap());
    assert!(!jobs.workspaces.join(fill.disk_id()).exists());
    std::fs::remove_dir_all(base).unwrap();

    let (base, mut jobs, fill) = fixture();
    prepare_fill(&jobs.workspaces, &fill).unwrap();
    exited(&mut jobs, &fill, Some(true), Some(0));
    let mut attached = super::super::tests::record();
    attached.start.job_id = job::test_id(704);
    attached.start.workspace_disk_id = fill.disk_id();
    attached.released = false;
    jobs.records.insert(attached.start.job_id.clone(), attached);
    assert!(jobs.finish_fill(&fill, true).is_err());
    assert_eq!(
        open_disk(&jobs.workspaces, &fill.account, &fill.disk_id())
            .unwrap()
            .0
            .role,
        job::WorkspaceRole::Fill
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_base_seed_is_same_account_repository_reflink_and_never_a_writable_base() {
    let (base, mut jobs, fill) = fixture();
    // Root scratch may be ext4; exercise real reflinks on a disposable XFS store.
    let store = ReflinkStore::new(&base);
    jobs.workspaces = store.0.join("workspaces");
    prepare_fill(&jobs.workspaces, &fill).unwrap();
    let source = jobs.workspaces.join(fill.disk_id()).join("workspace.img");
    std::fs::write(&source, vec![42; 4096]).unwrap();
    exited(&mut jobs, &fill, Some(true), Some(0));
    assert!(jobs.finish_fill(&fill, true).unwrap());
    let request = job::Seed {
        account: fill.account.clone(),
        repository_key: fill.repository_key.clone(),
        base_id: fill.disk_id(),
        disk_id: "new-job".into(),
    };
    for (account, repository_key) in [("foreign", "repo"), ("account", "foreign")] {
        let mut wrong = request.clone();
        wrong.account = account.into();
        wrong.repository_key = repository_key.into();
        assert!(seed(&jobs.workspaces, &wrong).is_err());
        assert!(!jobs.workspaces.join("new-job").exists());
    }
    let peer = Peer { uid: 0, gid: 0 };
    jobs.handle(Operation::SeedWorkspace(request.clone()), peer)
        .unwrap();
    jobs.handle(Operation::SeedWorkspace(request), peer)
        .unwrap();
    let (registry, clone) = open_disk(&jobs.workspaces, &fill.account, "new-job").unwrap();
    let original = File::open(&source).unwrap();
    assert_eq!(registry.role, job::WorkspaceRole::Job);
    assert_ne!(
        clone.metadata().unwrap().ino(),
        original.metadata().unwrap().ino()
    );
    assert_eq!(exclusive_bytes(&clone).unwrap(), 0);
    let destination = jobs.workspaces.join("new-job/workspace.img");
    assert_eq!(std::fs::read(&destination).unwrap(), vec![42; 4096]);
    std::fs::write(destination, vec![7; 4096]).unwrap();
    assert_eq!(std::fs::read(&source).unwrap(), vec![42; 4096]);
    assert!(!job::WorkspaceRole::Base.permits(job::VmKind::Job));
    assert!(!job::WorkspaceRole::Base.permits(job::VmKind::Fill));
    let denied = jobs
        .handle(
            Operation::Start(Start {
                kind: job::VmKind::BaseBuild,
                job_id: job::test_id(702),
                account: fill.account.clone(),
                workspace_disk_id: fill.disk_id(),
                image_generation_id: "image".into(),
                vcpus: 1,
                memory_mib: 128,
                burst_mib: None,
            }),
            peer,
        )
        .unwrap();
    assert!(matches!(denied.reply, Reply::StartRefused { .. }));
    drop((clone, original));
    drop(store);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_restart_discards_abandoned_fill_but_keeps_published_base() {
    let (base, mut jobs, fill) = fixture();
    prepare_fill(&jobs.workspaces, &fill).unwrap();
    jobs.reconcile().unwrap();
    assert!(!jobs.workspaces.join(fill.disk_id()).exists());
    prepare_fill(&jobs.workspaces, &fill).unwrap();
    exited(&mut jobs, &fill, Some(true), Some(0));
    assert!(jobs.finish_fill(&fill, true).unwrap());
    jobs.records.clear();
    let partial = jobs.workspaces.join(format!("fill-{}", job::test_id(703)));
    std::fs::create_dir(&partial).unwrap();
    jobs.reconcile().unwrap();
    assert!(!partial.exists());
    assert_eq!(
        open_disk(&jobs.workspaces, &fill.account, &fill.disk_id())
            .unwrap()
            .0
            .role,
        job::WorkspaceRole::Base
    );
    std::fs::remove_dir_all(base).unwrap();
}
