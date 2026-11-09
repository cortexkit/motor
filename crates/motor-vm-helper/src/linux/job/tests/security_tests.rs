use super::*;

#[test]
fn failed_record_does_not_block_other_exit_delivery_stop_start_or_relay() {
    let mut vmm = Vmm::new();
    let mut jobs = machine(&vmm.base);
    std::fs::create_dir_all(&jobs.records_path).unwrap();
    let first = record();
    jobs.adopt_record(first.clone()).unwrap();
    // A malformed cgroup control file keeps failing without erasing evidence.
    std::fs::create_dir_all(jobs.group(&first.start.job_id)).unwrap();
    std::fs::create_dir(jobs.group(&first.start.job_id).join("cgroup.procs")).unwrap();
    let mut second = record();
    second.start.job_id = job::test_id(2);
    second.released = true;
    second.oom_kill = Some(9);
    jobs.adopt_record(second.clone()).unwrap();
    let peer = Peer {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    };
    let response = jobs.handle(Operation::Exits, peer).unwrap();
    assert!(
        matches!(response.reply, Reply::JobExits { exits } if exits == vec![second.pending_exit().unwrap()])
    );
    let failures = jobs.collection_failures.clone();
    let first_group = jobs.group(&first.start.job_id);
    std::fs::remove_dir(first_group.join("cgroup.procs")).unwrap();
    std::fs::write(first_group.join("cgroup.procs"), "").unwrap();
    std::fs::write(first_group.join("memory.events"), "oom_kill 5\n").unwrap();
    jobs.handle(Operation::Stop(second.start.job_id.clone()), peer)
        .unwrap();
    assert_eq!(jobs.collection_failures, failures);
    assert_eq!(
        jobs.records[&first.start.job_id].oom_kill, None,
        "stop must not collect a different record"
    );
    std::fs::remove_file(first_group.join("cgroup.procs")).unwrap();
    std::fs::create_dir(first_group.join("cgroup.procs")).unwrap();
    assert!(!jobs.log_collection_failure(
        &first.start.job_id,
        CollectionFailure::Cgroup,
        &"same failure"
    ));
    // Reaching the new start's own configuration check proves that another
    // record's collection failure did not short-circuit start dispatch.
    let mut next = start();
    next.job_id = job::test_id(3);
    assert!(jobs
        .handle(Operation::Start(next), peer)
        .err()
        .unwrap()
        .0
        .contains("settings are required"));

    let mut live = record();
    live.start.job_id = job::test_id(4);
    let id = live.start.job_id.clone();
    let group = jobs.group(&id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    jobs.adopt_record(live).unwrap();
    let root = jobs.root(&id);
    std::fs::create_dir_all(&root).unwrap();
    // Keep the socket path below sockaddr_un's length limit.
    let dir = File::open(&root).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(format!(
        "/proc/self/fd/{}/vsock.sock",
        dir.as_raw_fd()
    ))
    .unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 11];
        socket.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"CONNECT 52\n");
        socket.write_all(b"OK 52\n").unwrap();
    });
    assert!(matches!(
        jobs.handle(Operation::Relay(id, 52), peer).unwrap().reply,
        Reply::Relay { fd_count: 1 }
    ));
    server.join().unwrap();
    assert!(vmm.child.try_wait().unwrap().is_none());
    assert_eq!(jobs.collection_failures[&first.start.job_id].len(), 1);
}

#[cfg(feature = "linux-root-tests")]
mod root {
    use super::*;

    fn settings(jobs: &mut LinuxJobs) {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        jobs.settings = Some(JobVms {
            workspace_disk_max_bytes: Some(1073741824),
            uid_base: 600001,
            uid_count: 1,
            memory_max_bytes: 1048576,
        });
    }

    #[test]
    fn isolated_job_relay_refuses_symlink_and_wrong_owner_without_connecting() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let vmm = Vmm::new();
        let mut jobs = machine(&vmm.base);
        let record = record();
        let id = record.start.job_id.clone();
        let group = jobs.group(&id);
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
        jobs.adopt_record(record).unwrap();
        let root = jobs.root(&id);
        std::fs::create_dir_all(&root).unwrap();
        let dir = File::open(&root).unwrap();
        let alias = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        let target = alias.join("host.sock");
        let listener = std::os::unix::net::UnixListener::bind(&target).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::os::unix::fs::symlink(&target, alias.join("vsock.sock")).unwrap();
        let peer = Peer { uid: 0, gid: 0 };
        assert!(jobs.handle(Operation::Relay(id.clone(), 52), peer).is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        std::fs::remove_file(alias.join("vsock.sock")).unwrap();
        std::fs::rename(&target, alias.join("vsock.sock")).unwrap();
        let socket = fs::beneath(dir.as_raw_fd(), "vsock.sock", libc::O_PATH, 0).unwrap();
        let name = std::ffi::CString::new(alias.join("vsock.sock").as_os_str().as_encoded_bytes())
            .unwrap();
        assert_eq!(unsafe { libc::chown(name.as_ptr(), 600001, 600001) }, 0);
        assert_eq!(socket.metadata().unwrap().uid(), 600001);
        assert!(jobs.handle(Operation::Relay(id, 52), peer).is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn isolated_start_enforces_workspace_role_before_opening_disk_or_booting() {
        let base = scratch();
        let mut jobs = machine(&base);
        settings(&mut jobs);
        for role in [
            job::WorkspaceRole::Job,
            job::WorkspaceRole::Fill,
            job::WorkspaceRole::Base,
        ] {
            let disk_id = format!("disk-{role:?}");
            super::super::super::workspace::create(
                &jobs.workspaces,
                job::CreateWorkspace {
                    account: "account-1".into(),
                    workspace_disk_id: disk_id.clone(),
                    size_bytes: 4096,
                },
                role,
            )
            .unwrap();
            // A denial must precede even opening this now-missing disk.
            std::fs::remove_file(jobs.workspaces.join(&disk_id).join("workspace.img")).unwrap();
            for kind in [
                job::VmKind::Job,
                job::VmKind::Fill,
                job::VmKind::BaseBuild,
                job::VmKind::Recovery,
            ] {
                let mut request = start();
                request.workspace_disk_id = disk_id.clone();
                request.kind = kind;
                let response = jobs.start(request, Peer { uid: 0, gid: 0 });
                let allowed = matches!(
                    (role, kind),
                    (
                        job::WorkspaceRole::Job,
                        job::VmKind::Job | job::VmKind::Recovery
                    ) | (job::WorkspaceRole::Fill, job::VmKind::Fill)
                        | (job::WorkspaceRole::Base, job::VmKind::BaseBuild)
                );
                if allowed {
                    assert!(
                        response.is_err(),
                        "permitted kind reaches missing disk, not role denial"
                    );
                } else {
                    assert!(matches!(
                        response.unwrap().reply,
                        Reply::StartRefused {
                            case: crate::tenant::StartRefusalCase::WorkspaceRole,
                            ..
                        }
                    ));
                }
                assert!(jobs.records.is_empty());
                assert!(!jobs.jail.exists());
                assert!(!jobs.groups.exists());
            }
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_recovery_refuses_fill_and_base_before_opening_disk() {
        let base = scratch();
        let mut jobs = machine(&base);
        settings(&mut jobs);
        for role in [job::WorkspaceRole::Fill, job::WorkspaceRole::Base] {
            let mut request = start();
            request.kind = job::VmKind::Recovery;
            request.workspace_disk_id = format!("recovery-{role:?}");
            super::super::super::workspace::create(
                &jobs.workspaces,
                job::CreateWorkspace {
                    account: request.account.clone(),
                    workspace_disk_id: request.workspace_disk_id.clone(),
                    size_bytes: 4096,
                },
                role,
            )
            .unwrap();
            std::fs::remove_file(
                jobs.workspaces
                    .join(&request.workspace_disk_id)
                    .join("workspace.img"),
            )
            .unwrap();
            let response = jobs
                .handle(
                    Operation::Start(request),
                    Peer {
                        uid: 600001,
                        gid: 600001,
                    },
                )
                .unwrap();
            assert!(matches!(
                response.reply,
                Reply::StartRefused {
                    case: crate::tenant::StartRefusalCase::WorkspaceRole,
                    ..
                }
            ));
            assert!(jobs.records.is_empty());
            assert!(!jobs.jail.exists());
            assert!(!jobs.groups.exists());
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_start_does_not_take_a_short_lived_workspace_flock() {
        let base = scratch();
        let mut jobs = machine(&base);
        settings(&mut jobs);
        jobs.create_workspace(job::CreateWorkspace {
            account: "account-1".into(),
            workspace_disk_id: "disk-1".into(),
            size_bytes: 4096,
        })
        .unwrap();
        let disk = File::open(jobs.workspaces.join("disk-1/workspace.img")).unwrap();
        assert_eq!(
            unsafe { libc::flock(disk.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let error = jobs.start(start(), Peer { uid: 0, gid: 0 }).err().unwrap();
        assert!(
            error.0.contains("job-images"),
            "must reach image lookup, not transient flock: {error}"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_operator_installed_account_subject_is_found_by_job_start_staging() {
        let base = scratch();
        let jobs = machine(&base);
        let subject = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let mut set = cache::tests::set(subject, "1");
        set.config = include_str!("../../../../fixtures/cache-subject.json")
            .to_string()
            .into();
        let source = base.join("source");
        std::fs::create_dir(&source).unwrap();
        let mut args = vec!["--account".into(), subject.into()];
        for (flag, name, bytes) in [
            ("--config", "config", &set.config),
            ("--read-key", "read", &set.read_key),
            ("--write-key", "write", set.write_key.as_ref().unwrap()),
        ] {
            let path = source.join(name);
            std::fs::write(&path, bytes.as_bytes()).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            args.extend([flag.into(), path.into_os_string()]);
        }
        cache::install_command_at(args, &jobs.cache_store).unwrap();
        assert!(jobs.cache_store.join(subject).is_dir());
        assert!(!jobs.cache_store.join(subject.replace(':', "-")).exists());
        let jail = base.join("stage");
        std::fs::create_dir(&jail).unwrap();
        let mut request = start();
        request.account = subject.into();
        request.validate().unwrap();
        let nonce = jobs
            .stage_start_cache(&request, &File::open(&jail).unwrap(), 600001)
            .unwrap()
            .unwrap();
        let drive = std::fs::read(jail.join(cache::IMAGE)).unwrap();
        let header = motor_protocol::REMOTE_CACHE_DRIVE_MAGIC.len()
            + motor_protocol::REMOTE_CACHE_DRIVE_NONCE_BYTES;
        let len = u32::from_le_bytes(drive[header..header + 4].try_into().unwrap()) as usize;
        let value: Value = serde_json::from_slice(&drive[header + 4..header + 4 + len]).unwrap();
        let config: Value = serde_json::from_str(value["config"].as_str().unwrap()).unwrap();
        assert_eq!(config["tenant"], subject);
        assert_eq!(config["prefix"], "tenant/acct-01ARZ3NDEKTSV4RRFFQ69G5FAV/");
        assert!(value.get("write_key").is_none());
        assert!(
            boot_requests(&request, Some(17), Some(&nonce), 4 << 30).unwrap()[1].1["boot_args"]
                .as_str()
                .unwrap()
                .ends_with(&format!("motor.cache_nonce={nonce}"))
        );
        std::fs::remove_dir_all(base).unwrap();
    }
}
