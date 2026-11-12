use super::*;

#[test]
fn isolated_workspace_binding_is_created_from_verified_grant_and_never_backfilled() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = super::super::tests::scratch();
    let settings = Some(
        serde_json::from_value(json!({
            "uid_base":500000, "uid_count":1, "memory_max_bytes":1048576,
            "workspace_disk_max_bytes":4096
        }))
        .unwrap(),
    );
    let id = job::test_id(97);
    let account = crate::cache_write::tests::ACCOUNT;
    crate::cache_write::tests::with_fresh_token(&id, account, |config, token| {
        let mut jobs = super::super::tests::machine_with_config(&base, config);
        jobs.settings = settings;
        jobs.cache_write_grants.admit(token).unwrap();
        let request = |disk: &str, selector: Option<String>| job::CreateWorkspace {
            account: account.into(),
            workspace_disk_id: disk.into(),
            size_bytes: 4096,
            cache_write_job_id: selector,
        };
        jobs.create_workspace(request("bound", Some(id.clone())))
            .unwrap();
        let (bound, _) = open_disk(&jobs.workspaces, account, "bound").unwrap();
        assert_eq!(
            bound.workspace_key_hash.as_deref(),
            Some(
                jobs.cache_write_grants
                    .get(&id)
                    .unwrap()
                    .workspace_key_hash
                    .as_str()
            )
        );
        jobs.create_workspace(request("legacy", None)).unwrap();
        let registry_path = jobs.workspaces.join("legacy/registry.json");
        let before = std::fs::read(&registry_path).unwrap();
        assert!(!String::from_utf8_lossy(&before).contains("workspace_key_hash"));
        jobs.handle(
            Operation::EnsureWorkspace {
                account: account.into(),
                disk_id: "legacy".into(),
                size_bytes: 4096,
                cache_write_job_id: Some(id.clone()),
            },
            Peer { uid: 0, gid: 0 },
        )
        .unwrap();
        assert_eq!(std::fs::read(&registry_path).unwrap(), before);
        let (legacy, _) = open_disk(&jobs.workspaces, account, "legacy").unwrap();
        assert!(legacy.workspace_key_hash.is_none());
        jobs.create_workspace(request("unknown", Some(job::test_id(98))))
            .unwrap();
        assert!(open_disk(&jobs.workspaces, account, "unknown")
            .unwrap()
            .0
            .workspace_key_hash
            .is_none());
        let mut other = request("other-account", Some(id.clone()));
        other.account = "account-2".into();
        jobs.create_workspace(other).unwrap();
        assert!(open_disk(&jobs.workspaces, "account-2", "other-account")
            .unwrap()
            .0
            .workspace_key_hash
            .is_none());
        jobs.cache_write_grants.consume(&id);
        jobs.create_workspace(request("consumed", Some(id.clone())))
            .unwrap();
        assert!(open_disk(&jobs.workspaces, account, "consumed")
            .unwrap()
            .0
            .workspace_key_hash
            .is_none());
        assert_eq!(
            open_disk(&jobs.workspaces, account, "bound")
                .unwrap()
                .0
                .workspace_key_hash,
            bound.workspace_key_hash
        );
    });
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn isolated_pre_intent_retry_stages_write_key_and_read_only_credential_drive() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = super::super::tests::scratch();
    let start = Start {
        kind: job::VmKind::Job,
        job_id: job::test_id(99),
        account: crate::cache_write::tests::ACCOUNT.into(),
        workspace_disk_id: "bound".into(),
        image_generation_id: "image".into(),
        vcpus: 1,
        memory_mib: 128,
        burst_mib: None,
        cache_write_blocked: false,
    };
    crate::cache_write::tests::with_fresh_token(&start.job_id, &start.account, |config, token| {
        let mut jobs = super::super::tests::machine_with_config(&base, config);
        jobs.cache_write_grants.admit(token).unwrap();
        assert!(jobs.start(start.clone(), Peer { uid: 0, gid: 0 }).is_err());
        let grant = jobs.disk_grant(Some(&start.job_id)).unwrap();
        jobs.settings = Some(
            serde_json::from_value(json!({
                "uid_base":500000, "uid_count":1, "memory_max_bytes":1048576,
                "workspace_disk_max_bytes":4096
            }))
            .unwrap(),
        );
        jobs.create_workspace(job::CreateWorkspace {
            account: start.account.clone(),
            workspace_disk_id: start.workspace_disk_id.clone(),
            size_bytes: 4096,
            cache_write_job_id: Some(start.job_id.clone()),
        })
        .unwrap();
        let (registry, _) =
            open_disk(&jobs.workspaces, &start.account, &start.workspace_disk_id).unwrap();
        cache::tests::install_fixture(&jobs.cache_store, &start.account);
        let boundary = base.join("credential-boundary");
        trusted_directory(&boundary).unwrap();
        let (nonce, job_write) = cache::stage_with_write(
            &jobs.cache_store,
            &start.account,
            start.kind,
            registry.role,
            &File::open(&boundary).unwrap(),
            600001,
            grant.permits(&start, &registry),
        )
        .unwrap();
        let nonce = nonce.unwrap();
        assert!(job_write);
        let image = boundary.join(cache::IMAGE);
        let bytes = std::fs::read(&image).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("WRITE-SECRET-new"));
        assert!(String::from_utf8_lossy(&bytes).contains("READ-SECRET-new"));
        assert_eq!(std::fs::metadata(&image).unwrap().uid(), 600001);
        assert_eq!(std::fs::metadata(&image).unwrap().mode() & 0o777, 0o600);
        let mut requests = recorded_boot_requests(
            &start,
            Some(1),
            Some(&nonce),
            4096,
            &registry,
            jobs.settings.as_ref().unwrap(),
        )
        .unwrap();
        authorize_job_write_boot(&mut requests, &start, Some(&nonce), job_write).unwrap();
        let drive = &requests
            .iter()
            .find(|(path, _)| *path == "/drives/remote_cache")
            .unwrap()
            .1;
        assert_eq!(drive["is_read_only"], true);
        let boot = requests
            .iter()
            .find(|(path, _)| *path == "/boot-source")
            .unwrap()
            .1["boot_args"]
            .as_str()
            .unwrap();
        assert!(boot.contains("motor.cache_write=1"));
        assert!(!boot.contains("motor.fill=1"));
    });
    std::fs::remove_dir_all(base).unwrap();
}
