use super::*;

struct Scratch {
    _dir: TempRoot,
    paths: Paths,
}
struct TempRoot(PathBuf);
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Two provisioned tenants, `a` (tap 1) and `b` (tap 2), under a scratch root.
fn scratch() -> Scratch {
    let base = cortexkit_test_support::scratch_root();
    fs::create_dir_all(&base).unwrap();
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = base.join(format!(
        "removal-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    let paths = Paths {
        root: dir.join("var/lib/motor-vm"),
        registry: dir.join("etc/motor-vm-helper/tenants"),
        bindings: dir.join("etc/motor-host/tenants.json"),
        provision: dir.join("etc/motor-host/provision"),
    };
    for path in [&paths.root, &paths.registry, &paths.provision] {
        fs::create_dir_all(path).unwrap();
    }
    for (tenant, tap) in [("a", 1), ("b", 2)] {
        let home = paths.home(tenant);
        fs::create_dir_all(home.join("suspend")).unwrap();
        fs::create_dir_all(home.join("suspend.stale-v0-1")).unwrap();
        fs::write(home.join("disk.img"), tenant).unwrap();
        fs::write(home.join("suspend/suspend.memory"), tenant).unwrap();
        let mut registry = crate::tenant::tests::registry(tenant);
        registry.tap_index = tap;
        fs::write(
            paths.registry.join(format!("{tenant}.json")),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::write(paths.provision.join(format!("{tenant}.json")), tenant).unwrap();
    }
    fs::write(
        &paths.bindings,
        r#"{"vm-a":{"tenant":"a","sub":"acct:A"},"vm-b":{"tenant":"b","sub":"acct:B"}}"#,
    )
    .unwrap();
    Scratch {
        _dir: TempRoot(dir),
        paths,
    }
}

#[test]
fn helper_startup_and_offline_removal_are_mutually_exclusive() {
    let s = scratch();
    let p = &s.paths;
    let offline = lock(p).unwrap();
    let mut started = false;
    let result = startup_with(p, || {
        started = true;
        Ok(())
    });
    assert!(result
        .unwrap_err()
        .0
        .contains("provisioning is in progress"));
    assert!(
        !started,
        "no construction, reconcile or bind while offline removal owns the lock"
    );
    drop(offline);
    startup_with(p, || {
        let result = offline_with(
            p,
            "a",
            false,
            1000,
            |_| panic!("must not read tenant state during helper startup"),
            |_| panic!("must not check stopped state during helper startup"),
            |_| panic!("must not mutate network during helper startup"),
        );
        assert!(result
            .unwrap_err()
            .0
            .contains("provisioning is in progress"));
        Ok(())
    })
    .unwrap();
    assert!(
        lock(p).is_ok(),
        "startup guard must not remain held while serving requests"
    );
}

#[test]
fn removal_rejects_symlinked_home_parent_before_records_or_quarantine_mutation() {
    use std::os::unix::fs::symlink;
    let s = scratch();
    let p = &s.paths;
    let homes = p.root.join("tenants");
    let real = p.root.join("real-tenants");
    fs::rename(&homes, &real).unwrap();
    symlink(&real, &homes).unwrap();
    assert!(begin(p, "a", 1000).is_err());
    assert!(!p.removed().exists());
    fs::remove_file(&homes).unwrap();
    fs::rename(&real, &homes).unwrap();
    let quarantine = begin(p, "a", 1000).unwrap();
    fs::rename(&homes, &real).unwrap();
    symlink(&real, &homes).unwrap();
    let bindings = fs::read(&p.bindings).unwrap();
    assert!(finish(p, "a", &quarantine).is_err());
    assert!(p.registry_file("a").exists());
    assert_eq!(fs::read(&p.bindings).unwrap(), bindings);
    assert!(real.join("a/disk.img").exists());
    assert!(!quarantine.join("home").exists());
}

#[test]
fn offline_resume_refuses_reassigned_index_before_any_tenant_mutation() {
    for resume_only in [false, true] {
        let s = scratch();
        let p = &s.paths;
        let quarantine = begin(p, "a", 1000).unwrap();
        fs::remove_file(p.registry_file("a")).unwrap();
        let mut new_owner = crate::tenant::tests::registry("b");
        new_owner.tap_index = 1;
        let new_record = serde_json::to_vec(&new_owner).unwrap();
        fs::write(p.registry_file("b"), &new_record).unwrap();
        let before_binding = fs::read(&p.bindings).unwrap();
        let mut stopped = false;
        let result = offline_with(
            p,
            "a",
            resume_only,
            1100,
            |path| serde_json::from_slice(&fs::read(path)?).map_err(|e| Error(e.to_string())),
            |_| {
                stopped = true;
                Ok(())
            },
            |_| panic!("must not delete B's chains, membership, UFW grant or TAP"),
        );
        assert!(result.unwrap_err().0.contains("reassigned to b"));
        assert!(
            !stopped,
            "reassignment must be detected before liveness checks or mutation"
        );
        assert_eq!(fs::read(p.registry_file("b")).unwrap(), new_record);
        assert_eq!(fs::read(&p.bindings).unwrap(), before_binding);
        assert!(p.home("a").exists());
        assert!(p.home("b").exists());
        assert!(!quarantine.join("home").exists());
    }
}

#[test]
#[cfg(target_os = "linux")]
fn offline_cli_removes_and_resumes_without_loading_broken_uplink_config() {
    for resume in [false, true] {
        let s = scratch();
        let p = &s.paths;
        let registry = crate::tenant::tests::registry("a");
        fs::write(p.registry_file("a"), serde_json::to_vec(&registry).unwrap()).unwrap();
        if resume {
            begin(p, "a", 1200).unwrap();
            fs::remove_file(p.registry_file("a")).unwrap();
        }
        let mut cleaned = false;
        crate::linux::server::dispatch(
            [
                std::ffi::OsString::from(if resume {
                    "--resume-removal"
                } else {
                    "--remove-tenant"
                }),
                "a".into(),
            ],
            |_| {
                Err(Error(
                    "uplink invalid: helper cannot reconcile or serve".into(),
                ))
            },
            |tenant, resume_only| {
                offline_with(
                    p,
                    tenant,
                    resume_only,
                    1300,
                    |path| {
                        serde_json::from_slice(&fs::read(path)?).map_err(|e| Error(e.to_string()))
                    },
                    |_| Ok(()),
                    |saved| {
                        assert_eq!(saved.tap_index, 1);
                        cleaned = true;
                        Ok(())
                    },
                )?;
                Ok(())
            },
        )
        .unwrap();
        assert!(cleaned);
        assert!(!p.home("a").exists());
        assert!(p.home("b").exists());
    }
}

#[test]
fn offline_removal_refuses_live_service_or_process_before_any_quarantine() {
    let s = scratch();
    let p = &s.paths;
    fs::write(
        p.registry_file("a"),
        serde_json::to_vec(&crate::tenant::tests::registry("a")).unwrap(),
    )
    .unwrap();
    let result = offline_with(
        p,
        "a",
        false,
        1300,
        |path| serde_json::from_slice(&fs::read(path)?).map_err(|e| Error(e.to_string())),
        |_| Err(Error("tenant still has a live process".into())),
        |_| panic!("must not clean a live VM"),
    );
    assert!(result.unwrap_err().0.contains("live process"));
    assert!(!p.removed().exists());
    assert!(p.registry_file("a").exists());
    assert!(p.home("a").exists());
}

#[test]
fn interrupted_removal_replays_network_before_finishing_saved_records() {
    let s = scratch();
    let p = &s.paths;
    let registry = crate::tenant::tests::registry("a");
    fs::write(p.registry_file("a"), serde_json::to_vec(&registry).unwrap()).unwrap();
    let quarantine = begin(p, "a", 1234).unwrap();
    fs::remove_file(p.registry_file("a")).unwrap();
    assert_eq!(pending(p, "a").unwrap(), Some(quarantine.clone()));
    let mut cleaned = false;
    finish_with_network(p, "a", &quarantine, |saved| {
        assert_eq!(saved.tap_index, 1);
        assert!(p.home("a").exists());
        cleaned = true;
        Ok(())
    })
    .unwrap();
    assert!(
        cleaned,
        "resume must not finish records while owned network resources survive"
    );
    assert!(!p.home("a").exists());
    assert!(quarantine.join("home").exists());
    assert!(p.home("b").exists());
}

#[test]
fn layout_matches_the_list_the_provisioning_script_removes() {
    let pinned: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../script/tenant-provision/removal-layout.json"
    ))
    .unwrap();
    assert_eq!(layout(&Paths::live()), pinned);
}

#[test]
fn removal_quarantines_one_tenant_and_leaves_the_other_untouched() {
    let s = scratch();
    let p = &s.paths;
    let quarantine = begin(p, "a", 1_000).unwrap();
    assert_eq!(quarantine, p.removed().join("a-1000"));
    // Nothing is deleted before `finish`.
    assert!(p.home("a").join("disk.img").exists());
    finish(p, "a", &quarantine).unwrap();
    assert!(!p.home("a").exists());
    assert!(!p.registry.join("a.json").exists());
    assert!(!p.provision.join("a.json").exists());
    assert_eq!(fs::read(quarantine.join("home/disk.img")).unwrap(), b"a");
    assert!(quarantine.join("home/suspend/suspend.memory").exists());
    assert!(quarantine.join("home/suspend.stale-v0-1").is_dir());
    assert_eq!(
        fs::read(quarantine.join("records/provision.json")).unwrap(),
        b"a"
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(quarantine.join("records/binding.json")).unwrap())
            .unwrap();
    assert_eq!(saved["vm-a"]["sub"], "acct:A");
    let bindings: serde_json::Value =
        serde_json::from_slice(&fs::read(&p.bindings).unwrap()).unwrap();
    assert_eq!(
        bindings,
        serde_json::json!({"vm-b":{"tenant":"b","sub":"acct:B"}})
    );
    assert_eq!(
        fs::metadata(&p.bindings).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(
        fs::metadata(p.removed()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // Tenant b keeps everything.
    assert_eq!(fs::read(p.home("b").join("disk.img")).unwrap(), b"b");
    assert!(p.registry.join("b.json").exists());
    assert!(p.provision.join("b.json").exists());
}

#[test]
fn an_interrupted_move_is_found_and_completed_by_a_retry() {
    let s = scratch();
    let p = &s.paths;
    let quarantine = begin(p, "a", 1_000).unwrap();
    assert_eq!(pending(p, "a").unwrap(), None, "registry still present");
    // The records went, then the process stopped before the rename.
    fs::remove_file(p.registry.join("a.json")).unwrap();
    assert_eq!(pending(p, "a").unwrap(), Some(quarantine.clone()));
    // Such a folder is never purged: its saved records are the only record.
    assert!(purge(p, 1_000 + QUARANTINE_S).unwrap().is_empty());
    finish(p, "a", &quarantine).unwrap();
    assert!(quarantine.join("home/disk.img").exists());
    assert_eq!(pending(p, "a").unwrap(), None);
    assert!(pending(p, "b").unwrap().is_none());
}

#[test]
fn purge_deletes_only_quarantine_folders_at_least_seven_days_old() {
    let s = scratch();
    let p = &s.paths;
    let old = begin(p, "a", 1_000).unwrap();
    finish(p, "a", &old).unwrap();
    let young = begin(p, "b", 1_000 + 3600).unwrap();
    finish(p, "b", &young).unwrap();
    // Not quarantine folders: a stray file and a name with no timestamp.
    fs::write(p.removed().join("notes-1"), "keep").unwrap();
    fs::create_dir(p.removed().join("manual")).unwrap();
    assert!(purge(p, 1_000 + QUARANTINE_S - 1).unwrap().is_empty());
    assert_eq!(purge(p, 1_000 + QUARANTINE_S).unwrap(), vec![old.clone()]);
    assert!(!old.exists());
    assert!(young.join("home/disk.img").exists());
    assert!(p.removed().join("notes-1").exists());
    assert!(p.removed().join("manual").exists());
}
