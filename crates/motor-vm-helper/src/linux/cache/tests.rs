use super::*;
use serde_json::json;

pub(crate) fn set(account: &str, generation: &str) -> RemoteCacheRequest {
    RemoteCacheRequest {
        op: "set_remote_cache".into(),
        config: json!({"generation":generation,"tenant":account,"endpoint":"https://cache.example","region":"region","bucket":"cache","prefix":format!("tenant/{}/", account.replace(':', "-")),"storage_class":"STANDARD","enabled":true}).to_string().into(),
        read_key: json!({"generation":generation,"access_key_id":"read-id","secret_access_key":format!("READ-SECRET-{generation}")}).to_string().into(),
        write_key: Some(json!({"generation":generation,"access_key_id":"write-id","secret_access_key":format!("WRITE-SECRET-{generation}")}).to_string().into()),
    }
}

#[cfg(feature = "linux-root-tests")]
pub(crate) fn install_fixture(path: &Path, account: &str) {
    store::install(path, account, &set(account, "new")).unwrap();
}

#[test]
fn validation_matches_operator_subject_prefix_schema_and_generation_checks() {
    let valid = set("account-1", "new");
    validate("account-1", &valid).unwrap();
    assert!(validate("account-2", &valid).is_err());
    let mut wrong = set("account-1", "new");
    wrong.write_key = set("account-1", "old").write_key;
    assert!(validate("account-1", &wrong).is_err());
    for change in ["missing", "extra", "prefix", "generation"] {
        let mut wrong = set("account-1", "new");
        let mut config: serde_json::Value = serde_json::from_str(&wrong.config).unwrap();
        match change {
            "missing" => {
                config.as_object_mut().unwrap().remove("enabled");
            }
            "extra" => config["unknown"] = json!(true),
            "prefix" => config["prefix"] = json!("tenant/another/"),
            _ => config["generation"] = json!(""),
        }
        wrong.config = config.to_string().into();
        assert!(validate("account-1", &wrong).is_err(), "{change}");
    }
}

#[test]
fn invalid_credential_errors_never_include_secret_input() {
    for text in ["READ-SECRET", "{\"generation\":\"WRITE-SECRET\"}"] {
        let mut wrong = set("account-1", "new");
        wrong.read_key = text.to_owned().into();
        let error = validate("account-1", &wrong).unwrap_err();
        assert_eq!(error.0, "invalid remote-cache credentials");
    }
    let mut oversized = set("account-1", "new");
    oversized.read_key = "SECRET".repeat(12000).into();
    assert!(validate("account-1", &oversized).is_err());
}

#[cfg(feature = "linux-root-tests")]
mod root {
    use super::*;
    use std::{
        os::unix::{
            fs::{DirBuilderExt, PermissionsExt},
            process::CommandExt,
        },
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn scratch() -> PathBuf {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = cortexkit_test_support::scratch_root().join(format!(
            "helper-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    }

    fn decode(path: &Path, nonce: &str) -> serde_json::Value {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(
            &bytes[..REMOTE_CACHE_DRIVE_MAGIC.len()],
            b"MOTORCACHE\0\x01"
        );
        let offset = REMOTE_CACHE_DRIVE_MAGIC.len();
        assert_eq!(hex(&bytes[offset..offset + 16]), nonce);
        let offset = offset + 16;
        let size = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        assert_eq!(bytes.len() % 512, 0);
        assert!(bytes[offset + 4 + size..].iter().all(|byte| *byte == 0));
        serde_json::from_slice(&bytes[offset + 4..offset + 4 + size]).unwrap()
    }

    #[test]
    fn isolated_rotate_then_rollback_delivers_current_read_and_write_sets() {
        use std::os::unix::net::UnixStream;
        let base = scratch();
        let path = base.join("store");
        for (tenant, account, write) in [
            ("a", "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", true),
            ("b", "org:01ARZ3NDEKTSV4RRFFQ69G5FAV", false),
        ] {
            let old = set(account, "superseded");
            store::install(&path, account, &old).unwrap();
            let mut rotated = set(account, "rotated");
            if !write {
                rotated.write_key = None;
            }
            store::install(&path, account, &rotated).unwrap();
            let mut registry = crate::tenant::tests::registry(tenant);
            registry.account = account.into();
            let (helper, mut guest) = UnixStream::pair().unwrap();
            let installed = std::thread::spawn(move || {
                let mut header = [0; 4];
                guest.read_exact(&mut header).unwrap();
                let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
                guest.read_exact(&mut bytes).unwrap();
                assert!(!String::from_utf8_lossy(&bytes).contains("superseded"));
                let current: RemoteCacheRequest = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(current.op, "set_remote_cache");
                assert_eq!(&*current.config, &*rotated.config);
                assert_eq!(&*current.read_key, &*rotated.read_key);
                assert_eq!(current.write_key.as_deref(), rotated.write_key.as_deref());
                // A whole-set replacement withdraws the old writer credential
                // when this account's current set is read-only.
                let mut kept_tenant = old;
                assert!(kept_tenant.write_key.is_some());
                kept_tenant = current;
                assert_eq!(kept_tenant.write_key.is_some(), write);
                let reply = br#"{"ok":true}"#;
                guest
                    .write_all(&(reply.len() as u32).to_be_bytes())
                    .unwrap();
                guest.write_all(reply).unwrap();
            });
            install_tenant(&path, &registry, || Ok(helper)).unwrap();
            installed.join().unwrap();
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_tenant_cache_install_redacts_untrusted_guest_errors_and_missing_sets() {
        use std::os::unix::net::UnixStream;
        let base = scratch();
        let path = base.join("store");
        let mut registry = crate::tenant::tests::registry("a");
        registry.account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
        assert_eq!(
            install_tenant(&path, &registry, || panic!("missing set must not connect"))
                .unwrap_err()
                .0,
            "tenant_cache_set_missing"
        );
        store::install(&path, &registry.account, &set(&registry.account, "rotated")).unwrap();
        for malformed in [false, true] {
            let (helper, mut guest) = UnixStream::pair().unwrap();
            let worker = std::thread::spawn(move || {
                let mut header = [0; 4];
                guest.read_exact(&mut header).unwrap();
                let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
                guest.read_exact(&mut bytes).unwrap();
                let reply = if malformed {
                    b"READ-SECRET-rotated".to_vec()
                } else {
                    br#"{"ok":false,"error":"WRITE-SECRET-rotated"}"#.to_vec()
                };
                guest
                    .write_all(&(reply.len() as u32).to_be_bytes())
                    .unwrap();
                guest.write_all(&reply).unwrap();
            });
            let error = install_tenant(&path, &registry, || Ok(helper)).unwrap_err();
            assert_eq!(error.0, "tenant_cache_install_failed");
            let visible = serde_json::to_string(&crate::Reply::Error { message: error.0 }).unwrap();
            assert!(!visible.contains("SECRET"));
            worker.join().unwrap();
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_account_drives_rotate_at_next_start_and_never_reuse_nonces() {
        let base = scratch();
        let path = base.join("store");
        store::install(&path, "account-1", &set("account-1", "old")).unwrap();
        store::install(&path, "account-2", &set("account-2", "other")).unwrap();
        let mut previous = None;
        for (index, kind) in [
            VmKind::Job,
            VmKind::Fill,
            VmKind::BaseBuild,
            VmKind::Recovery,
        ]
        .into_iter()
        .enumerate()
        {
            let jail = base.join(format!("jail-{index}"));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&jail)
                .unwrap();
            let nonce = stage(
                &path,
                "account-1",
                kind,
                &File::open(&jail).unwrap(),
                600001,
            )
            .unwrap();
            if kind == VmKind::Recovery {
                assert!(nonce.is_none());
                assert!(!jail.join(IMAGE).exists());
                continue;
            }
            let nonce = nonce.unwrap();
            let payload = decode(&jail.join(IMAGE), &nonce);
            assert_eq!(payload["op"], "set_remote_cache");
            assert!(payload["read_key"]
                .as_str()
                .unwrap()
                .contains(if index == 0 {
                    "READ-SECRET-old"
                } else {
                    "READ-SECRET-new"
                }));
            let config: serde_json::Value =
                serde_json::from_str(payload["config"].as_str().unwrap()).unwrap();
            assert_eq!(config["tenant"], "account-1");
            if kind == VmKind::Job {
                assert!(payload.get("write_key").is_none());
                assert!(
                    !String::from_utf8_lossy(&std::fs::read(jail.join(IMAGE)).unwrap())
                        .contains("WRITE-SECRET")
                );
            } else {
                assert!(payload["write_key"]
                    .as_str()
                    .unwrap()
                    .contains("WRITE-SECRET-new"));
            }
            let metadata = std::fs::metadata(jail.join(IMAGE)).unwrap();
            assert_eq!(metadata.uid(), 600001);
            assert_eq!(metadata.mode() & 0o777, 0o600);
            if let Some(old) = previous {
                assert_ne!(nonce, old);
            }
            previous = Some(nonce);
            if index == 0 {
                store::install(&path, "account-1", &set("account-1", "new")).unwrap();
                assert!(
                    decode(&jail.join(IMAGE), previous.as_ref().unwrap())["read_key"]
                        .as_str()
                        .unwrap()
                        .contains("READ-SECRET-old")
                );
            }
        }
        let jail = base.join("missing");
        std::fs::create_dir(&jail).unwrap();
        assert!(stage(
            &path,
            "no-account",
            VmKind::Job,
            &File::open(&jail).unwrap(),
            600001
        )
        .unwrap()
        .is_none());
        assert!(!jail.join(IMAGE).exists());
        assert!(store::load(&path, "account-2")
            .unwrap()
            .unwrap()
            .read_key
            .contains("READ-SECRET-other"));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_store_and_drive_refuse_host_and_other_jail_uids() {
        let base = scratch();
        let path = base.join("store");
        store::install(&path, "account-1", &set("account-1", "new")).unwrap();
        for directory in [&path, &path.join("account-1")] {
            let meta = std::fs::metadata(directory).unwrap();
            assert_eq!(meta.uid(), 0);
            assert_eq!(meta.mode() & 0o777, 0o700);
        }
        for name in NAMES {
            let meta = std::fs::metadata(path.join("account-1").join(name)).unwrap();
            assert_eq!(meta.uid(), 0);
            assert_eq!(meta.mode() & 0o777, 0o600);
        }
        let jail = base.join("jail");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&jail)
            .unwrap();
        stage(
            &path,
            "account-1",
            VmKind::Job,
            &File::open(&jail).unwrap(),
            600001,
        )
        .unwrap();
        // Make the fixture's parent traversable so access is refused by the
        // actual store/jail boundary, not by the test scratch directory.
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (uid, target) in [
            (1000, path.join("account-1/read-key")),
            (600002, jail.join(IMAGE)),
        ] {
            let output = Command::new("/bin/cat")
                .arg(target)
                .uid(uid)
                .gid(uid)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("Permission denied"));
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn isolated_source_and_store_never_follow_links_or_accept_unsafe_keys() {
        let base = scratch();
        let path = base.join("store");
        let set = set("account-1", "new");
        store::install(&path, "account-1", &set).unwrap();
        let file = base.join("key");
        std::fs::write(&file, set.read_key.as_bytes()).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        source(&file, true).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(source(&file, true).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let handle = File::open(&file).unwrap();
        super::super::super::fs::chown(&handle, 600001).unwrap();
        assert!(source(&file, true).is_err());
        std::os::unix::fs::symlink(&file, base.join("key-link")).unwrap();
        assert!(source(&base.join("key-link"), true).is_err());
        std::os::unix::fs::symlink(&base, base.join("parent-link")).unwrap();
        assert!(source(&base.join("parent-link/key"), true).is_err());
        let stored = path.join("account-1/read-key");
        std::fs::remove_file(&stored).unwrap();
        std::os::unix::fs::symlink(&file, stored).unwrap();
        assert!(store::load(&path, "account-1").is_err());
        std::fs::remove_dir_all(base).unwrap();
    }
}

#[test]
fn job_credential_staging_requires_matching_grant_and_keeps_read_key() {
    use crate::job::{Start, Workspace, WorkspaceRole};
    let start = Start {
        kind: VmKind::Job,
        job_id: crate::job::test_id(90),
        account: "account-1".into(),
        workspace_disk_id: "disk-1".into(),
        image_generation_id: "gen-1".into(),
        vcpus: 1,
        memory_mib: 1024,
        burst_mib: None,
        cache_write_blocked: false,
    };
    let disk = Workspace {
        account: start.account.clone(),
        workspace_disk_id: start.workspace_disk_id.clone(),
        role: WorkspaceRole::Job,
        workspace_key_hash: Some("bound-hash".into()),
    };
    let grant = crate::cache_write::Grant {
        sub: start.account.clone(),
        jti: start.job_id.clone(),
        workspace_key_hash: "bound-hash".into(),
    };
    for case in [
        "matching",
        "job-a-for-b",
        "other-workspace",
        "legacy",
        "other-account",
        "blocked",
        "missing-write-key",
    ] {
        let mut start = start.clone();
        let mut disk = disk.clone();
        let mut keys = set("account-1", "new");
        match case {
            "job-a-for-b" => start.job_id = crate::job::test_id(91),
            "other-workspace" => disk.workspace_key_hash = Some("other-hash".into()),
            "legacy" => disk.workspace_key_hash = None,
            "other-account" => {
                disk.account = "account-2".into();
                start.account = disk.account.clone();
            }
            "blocked" => start.cache_write_blocked = true,
            "missing-write-key" => keys.write_key = None,
            _ => {}
        }
        let (staged, write_boot) =
            credentials_for_boot(keys, start.kind, grant.permits(&start, &disk));
        let payload = serde_json::to_string(&staged).unwrap();
        assert!(payload.contains("READ-SECRET-new"), "{case}");
        assert_eq!(write_boot, case == "matching", "{case}");
        assert_eq!(
            payload.contains("WRITE-SECRET-new"),
            case == "matching",
            "{case}"
        );
        assert_eq!(staged.write_key.is_some(), case == "matching", "{case}");
    }
    let (staged, write_boot) = credentials_for_boot(set("account-1", "new"), VmKind::Job, false);
    assert!(staged.write_key.is_none());
    assert!(!write_boot);
    for kind in [VmKind::Fill, VmKind::BaseBuild] {
        let (staged, write_boot) = credentials_for_boot(set("account-1", "new"), kind, false);
        assert!(staged.write_key.is_some());
        assert!(!write_boot);
    }
}
