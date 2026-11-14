use super::*;
use crate::linux::root_test_support::ReflinkStore;

const ACCOUNT: &str = crate::cache_write::tests::ACCOUNT;

fn fixture(config: Option<&Config>) -> (PathBuf, LinuxJobs) {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = crate::linux::job::tests::scratch();
    let mut jobs = match config {
        Some(config) => crate::linux::job::tests::machine_with_config(&base, config),
        None => crate::linux::job::tests::machine(&base),
    };
    jobs.settings = Some(JobVms {
        uid_base: 500000,
        uid_count: 2,
        memory_max_bytes: 1048576,
        workspace_disk_max_bytes: Some(1 << 20),
        fixed_path_repositories: [(ACCOUNT.into(), vec!["/repo".into()])].into(),
    });
    (base, jobs)
}

fn request(ids: &[&str]) -> job::Fork {
    job::Fork {
        account: ACCOUNT.into(),
        disk_id: "new".into(),
        repository_root: "/repo".into(),
        donor_ids: ids.iter().map(|id| (*id).into()).collect(),
        cache_write_job_id: None,
    }
}

fn donor(jobs: &LinuxJobs, account: &str, id: &str, role: job::WorkspaceRole) {
    create(
        &jobs.workspaces,
        job::CreateWorkspace {
            account: account.into(),
            workspace_disk_id: id.into(),
            size_bytes: 65536,
            cache_write_job_id: None,
        },
        role,
    )
    .unwrap();
}

fn fork(jobs: &LinuxJobs, request: job::Fork) -> Result<Response> {
    jobs.handle(Operation::ForkWorkspace(request), Peer { uid: 0, gid: 0 })
}

#[test]
#[ignore = "requires operator root and trusted scratch"]
fn isolated_fork_refuses_foreign_non_job_and_existing_destination() {
    let (base, jobs) = fixture(None);
    donor(&jobs, ACCOUNT, "valid", job::WorkspaceRole::Job);
    for (account, role, id, error) in [
        (
            "other",
            job::WorkspaceRole::Job,
            "foreign",
            "identity mismatch",
        ),
        (ACCOUNT, job::WorkspaceRole::Fill, "fill", "root-owned Job"),
        (ACCOUNT, job::WorkspaceRole::Base, "base", "root-owned Job"),
    ] {
        donor(&jobs, account, id, role);
        // Even a later invalid donor refuses the request; it is not skipped.
        let err = fork(&jobs, request(&["valid", id])).err().unwrap();
        assert!(err.0.contains(error), "{}", err.0);
        assert!(!jobs.workspaces.join("new").exists());
    }
    let destination = jobs.workspaces.join("new");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("sentinel"), b"unchanged").unwrap();
    assert!(fork(&jobs, request(&["valid"]))
        .err()
        .unwrap()
        .0
        .contains("destination already exists"));
    assert_eq!(
        std::fs::read(destination.join("sentinel")).unwrap(),
        b"unchanged"
    );
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
    std::fs::remove_dir_all(&destination).unwrap();
    std::os::unix::fs::symlink("absent", &destination).unwrap();
    assert!(fork(&jobs, request(&["valid"])).is_err());
    assert!(std::fs::symlink_metadata(&destination)
        .unwrap()
        .file_type()
        .is_symlink());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
#[ignore = "requires operator root and trusted scratch"]
fn isolated_fork_fixed_paths_are_account_scoped_and_default_off() {
    let (base, mut jobs) = fixture(None);
    for map in [
        BTreeMap::new(),
        [(ACCOUNT.into(), Vec::new())].into(),
        [(ACCOUNT.into(), vec!["/other".into()])].into(),
        [("other".into(), vec!["*".into()])].into(),
    ] {
        jobs.settings.as_mut().unwrap().fixed_path_repositories = map;
        assert!(fork(&jobs, request(&["absent"]))
            .err()
            .unwrap()
            .0
            .contains("account fixed-path repository"));
        assert!(!jobs.workspaces.join("new").exists());
    }
    for root in ["/repo", "*"] {
        jobs.settings.as_mut().unwrap().fixed_path_repositories =
            [(ACCOUNT.into(), vec![root.into()])].into();
        assert!(fork(&jobs, request(&["absent"]))
            .err()
            .unwrap()
            .0
            .contains("no fork donors exist"));
    }
    jobs.settings = None;
    assert!(fork(&jobs, request(&["absent"]))
        .err()
        .unwrap()
        .0
        .contains("account fixed-path repository"));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
#[ignore = "requires operator root and trusted scratch"]
fn isolated_fork_refuses_booting_running_and_inode_alias_donors() {
    let (base, jobs) = fixture(None);
    donor(&jobs, ACCOUNT, "donor", job::WorkspaceRole::Job);
    let meta = open_disk(&jobs.workspaces, ACCOUNT, "donor")
        .unwrap()
        .1
        .metadata()
        .unwrap();
    for state in [State::Booting, State::Running, State::Exited] {
        for alias in [false, true] {
            let mut record = crate::linux::job::tests::record();
            record.start.account = if alias { "other" } else { ACCOUNT }.into();
            record.start.workspace_disk_id = if alias { "alias" } else { "donor" }.into();
            record.disk_device = meta.dev();
            record.disk_inode = meta.ino();
            record.released = false;
            let cell = std::sync::Arc::new(JobCell::new(JobState::new(record, state, false)));
            let guard = cell.0.lock();
            jobs.insert_cell(cell.clone(), &guard);
            drop(guard);
            let err = fork(&jobs, request(&["donor"])).err().unwrap();
            assert_eq!(
                err.0,
                if alias {
                    "fork donor is attached or awaiting cleanup"
                } else {
                    "workspace disk still held by a VM"
                }
            );
            assert!(!jobs.workspaces.join("new").exists());
        }
    }
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
#[ignore = "requires operator root, trusted scratch and loop-mounted XFS"]
fn isolated_fork_reflinks_size_choice_and_records_verified_binding_and_provenance() {
    let job_id = job::test_id(801);
    crate::cache_write::tests::with_fresh_token(&job_id, ACCOUNT, |config, token| {
        let (base, mut jobs) = fixture(Some(config));
        let store = ReflinkStore::new(&base);
        jobs.workspaces = store.0.join("workspaces");
        jobs.cache_write_grants.admit(token).unwrap();
        for (id, bytes) in [("recent", 4096), ("large", 16384), ("tie", 16384)] {
            donor(&jobs, ACCOUNT, id, job::WorkspaceRole::Job);
            std::fs::write(
                jobs.workspaces.join(id).join("workspace.img"),
                vec![42; bytes],
            )
            .unwrap();
        }
        let sizes: Vec<_> = ["recent", "large", "tie"]
            .into_iter()
            .map(|id| {
                open_disk(&jobs.workspaces, ACCOUNT, id)
                    .unwrap()
                    .1
                    .metadata()
                    .unwrap()
                    .blocks()
            })
            .collect();
        assert!(5 * sizes[0] < 4 * sizes[1]);
        assert_eq!(sizes[1], sizes[2]);
        // A released record is a tombstone, not a disk hold.
        let mut released = crate::linux::job::tests::record();
        released.start.account = ACCOUNT.into();
        released.start.workspace_disk_id = "large".into();
        let source = open_disk(&jobs.workspaces, ACCOUNT, "large").unwrap().1;
        let meta = source.metadata().unwrap();
        released.disk_device = meta.dev();
        released.disk_inode = meta.ino();
        released.released = true;
        jobs.insert_record(released);
        let mut bound = request(&["absent", "recent", "large", "tie"]);
        bound.cache_write_job_id = Some(job_id.clone());
        let response = fork(&jobs, bound.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(response.reply).unwrap(),
            json!({
                "result": "workspace_forked", "donor_id": "large"
            })
        );
        let (registry, clone) = open_disk(&jobs.workspaces, ACCOUNT, "new").unwrap();
        assert_eq!(registry.account, ACCOUNT);
        assert_eq!(registry.workspace_disk_id, "new");
        assert_eq!(registry.role, job::WorkspaceRole::Job);
        assert_eq!(
            registry.workspace_key_hash.as_ref(),
            Some(
                &jobs
                    .cache_write_grants
                    .get(&job_id)
                    .unwrap()
                    .workspace_key_hash
            )
        );
        assert_ne!(clone.metadata().unwrap().ino(), meta.ino());
        assert_eq!(exclusive_bytes(&clone).unwrap(), 0);
        let provenance: job::Forked = serde_json::from_reader(
            fs::trusted_file(&jobs.workspaces.join("new/fork.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(provenance.donor_id, "large");
        let destination = jobs.workspaces.join("new/workspace.img");
        assert_eq!(std::fs::read(&destination).unwrap(), vec![42; 16384]);
        std::fs::write(&destination, vec![7; 16384]).unwrap();
        assert_eq!(
            std::fs::read(jobs.workspaces.join("large/workspace.img")).unwrap(),
            vec![42; 16384]
        );
        let before = std::fs::read(jobs.workspaces.join("new/registry.json")).unwrap();
        assert!(fork(&jobs, bound).is_err());
        assert_eq!(
            std::fs::read(jobs.workspaces.join("new/registry.json")).unwrap(),
            before
        );
        assert_eq!(std::fs::read(&destination).unwrap(), vec![7; 16384]);
        // Wildcard policy allows the same operation without a grant selector.
        jobs.settings.as_mut().unwrap().fixed_path_repositories =
            [(ACCOUNT.into(), vec!["*".into()])].into();
        let mut unbound = request(&["recent", "large"]);
        unbound.disk_id = "unbound".into();
        fork(&jobs, unbound).unwrap();
        assert!(open_disk(&jobs.workspaces, ACCOUNT, "unbound")
            .unwrap()
            .0
            .workspace_key_hash
            .is_none());
        drop((source, clone));
        drop(store);
        std::fs::remove_dir_all(base).unwrap();
    });
}

#[test]
#[ignore = "requires operator root, trusted scratch and loop-mounted XFS"]
fn isolated_fork_staging_is_unpublished_until_rename_and_removed_on_restart() {
    let (base, mut jobs) = fixture(None);
    let store = ReflinkStore::new(&base);
    jobs.workspaces = store.0.join("workspaces");
    donor(&jobs, ACCOUNT, "donor", job::WorkspaceRole::Job);
    let source = open_disk(&jobs.workspaces, ACCOUNT, "donor").unwrap().1;
    let request = request(&["donor"]);
    let forked = job::Forked {
        donor_id: "donor".into(),
    };
    // Stop at the production staging boundary, as if the process exited
    // after syncing the clone and registry but before the publication rename.
    let staging = stage(&jobs.workspaces, &request, &forked, &source, None).unwrap();
    assert!(jobs
        .workspaces
        .join(&staging)
        .join("registry.json")
        .exists());
    assert!(!jobs.workspaces.join("new").exists());
    jobs.reconcile().unwrap();
    assert!(!jobs.workspaces.join(&staging).exists());
    assert!(!jobs.workspaces.join("new").exists());
    assert!(jobs.workspaces.join("donor").exists());

    let staging = stage(&jobs.workspaces, &request, &forked, &source, None).unwrap();
    std::fs::create_dir(jobs.workspaces.join("new")).unwrap();
    assert!(publish(&jobs.workspaces, &staging, "new").is_err());
    assert_eq!(
        std::fs::read_dir(jobs.workspaces.join("new"))
            .unwrap()
            .count(),
        0
    );
    jobs.reconcile().unwrap();
    assert!(!jobs.workspaces.join(&staging).exists());
    drop(source);
    drop(store);
    std::fs::remove_dir_all(base).unwrap();
}
