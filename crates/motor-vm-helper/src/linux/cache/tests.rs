use super::*;
use serde_json::json;

pub(crate) fn set(account: &str, generation: &str) -> RemoteCacheRequest {
    RemoteCacheRequest {
        op: "set_remote_cache".into(),
        config: json!({"generation":generation,"tenant":account,"endpoint":"https://cache.example","region":"region","bucket":"cache","prefix":format!("tenant/{account}/"),"storage_class":"STANDARD","enabled":true}).to_string().into(),
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
