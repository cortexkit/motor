use super::*;
use serde_json::json;
use std::os::unix::fs::{symlink, PermissionsExt};

fn values() -> serde_json::Value {
    json!({
        "windows_vm_bytes": 17179869184_u64,
        "host_reserve_bytes": 6442450944_u64,
        "jobs_min_bytes": 25769803776_u64,
        "storage_margin_bytes": 8589934592_u64,
        "growth_allowance_bytes": 68719476736_u64,
        "tenant_memory_reserve_bytes": 2147483648_u64
    })
}

fn policy() -> Capacity {
    Capacity::parse(&serde_json::to_vec(&values()).unwrap()).unwrap()
}

#[test]
fn post_grow_requires_operating_space_not_the_latch_release_point() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut r = crate::tenant::tests::registry("a");
    r.guest_ram_mib = 78 * 1024;
    r.idle_suspend_s = 0;
    r.host_floor_bytes = 128 * GIB;
    for available in [160 * GIB, 208 * GIB - 1] {
        let error = policy()
            .check_post_grow(&[r.clone()], available)
            .unwrap_err();
        assert!(error.0.contains("operating_requirement_bytes=223338299392"));
    }
    policy().check_post_grow(&[r.clone()], 208 * GIB).unwrap();
    r.host_floor_bytes = 223875170304;
    policy()
        .check_post_grow(&[r.clone()], 309774516224)
        .unwrap();
    assert!(policy()
        .check_post_grow(&[r.clone()], 309774516223)
        .is_err());
    let mut other = r.clone();
    other.host_floor_bytes = 150 * GIB;
    other.idle_suspend_s = 600;
    // The larger term is 150 GiB host floor + 78 GiB suspend reserve, followed
    // by 16 GiB in storage margins and the 64 GiB growth allowance.
    policy()
        .check_post_grow(&[r.clone(), other.clone()], 308 * GIB)
        .unwrap();
    assert!(policy()
        .check_post_grow(&[r, other.clone()], 308 * GIB - 1)
        .is_err());
    other.guest_ram_mib = u64::MAX;
    assert!(policy()
        .check_post_grow(&[other.clone()], u64::MAX)
        .is_err());
    other.idle_suspend_s = 0;
    other.host_floor_bytes = u64::MAX;
    assert!(policy().check_post_grow(&[other], u64::MAX).is_err());
    assert!(policy().check_post_grow(&[], u64::MAX).is_err());
}

#[test]
fn capacity_json_is_strict_and_requires_every_input() {
    let valid = values();
    for key in valid.as_object().unwrap().keys() {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert_eq!(
            Capacity::parse(&serde_json::to_vec(&missing).unwrap())
                .unwrap_err()
                .case,
            CapacityJson
        );
        for bad in [json!(-1), json!(1.5), json!("1"), json!(null), json!(true)] {
            let mut changed = valid.clone();
            changed[key] = bad;
            assert_eq!(
                Capacity::parse(&serde_json::to_vec(&changed).unwrap())
                    .unwrap_err()
                    .case,
                CapacityJson
            );
        }
    }
    for bad in [
        "{}".to_string(),
        "{".into(),
        format!(
            "{},\"unknown\":0}}",
            valid.to_string().trim_end_matches('}')
        ),
        valid
            .to_string()
            .replace("17179869184", "18446744073709551616"),
        valid
            .to_string()
            .replace("\"jobs_min_bytes\":25769803776", "\"jobs_min_bytes\":0"),
    ] {
        assert_eq!(
            Capacity::parse(bad.as_bytes()).unwrap_err().case,
            CapacityJson,
            "{bad}"
        );
    }
    // The Windows VM's allowance sits on top of jobs_min_bytes, so a Windows
    // allowance above the Linux floor is a valid configuration.
    let larger_windows = valid.to_string().replace("17179869184", "25769803777");
    Capacity::parse(larger_windows.as_bytes()).unwrap();
    let mut overflow = valid;
    overflow["windows_vm_bytes"] = json!(1);
    overflow["jobs_min_bytes"] = json!(u64::MAX);
    let refused = Capacity::parse(&serde_json::to_vec(&overflow).unwrap()).unwrap_err();
    assert_eq!(refused.case, CapacityJson);
    assert!(refused.message.contains("overflows u64"));
}

#[test]
fn capacity_file_refuses_missing_unreadable_or_untrusted_inputs() {
    let scratch = cortexkit_test_support::ScratchDir::new("capacity-file");
    let path = scratch.join("capacity.json");
    assert_eq!(read_capacity(&path).unwrap_err().case, CapacityJson);
    std::fs::create_dir(&path).unwrap();
    assert_eq!(read_capacity(&path).unwrap_err().case, CapacityJson);
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, serde_json::to_vec(&values()).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(read_capacity(&path).unwrap_err().case, CapacityJson);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(read_capacity(&path).unwrap_err().case, CapacityJson);
    }
}

#[test]
fn fit_counts_headroom_all_tenants_and_checked_memtotal() {
    let p = policy();
    let mut tenant = crate::tenant::tests::registry("a");
    tenant.guest_ram_mib = 78 * 1024;
    tenant.memory_headroom_mib = 2397;
    // 78 GiB RAM + 2397 MiB headroom + 8 GiB page cache, plus host reservations.
    assert!(p
        .check_fit(&[tenant.clone()], "MemTotal: 140866560 kB\n")
        .is_ok());
    assert_eq!(
        p.check_fit(&[tenant.clone()], "MemTotal: 140866559 kB\n")
            .unwrap_err()
            .case,
        Fit
    );
    let refused = p
        .check_fit(&[tenant.clone()], "MemTotal: 131491984 kB\n")
        .unwrap_err();
    assert_eq!(refused.case, Fit);
    assert!(refused.message.contains("reserved_bytes=144247357440"));
    assert!(refused.message.contains("MemTotal_bytes=134647791616"));
    tenant.guest_ram_mib += 1024;
    tenant.memory_headroom_mib = 2427;
    assert_eq!(
        p.check_fit(&[tenant.clone()], "MemTotal: 131491984 kB\n")
            .unwrap_err()
            .case,
        Fit
    );
    tenant.guest_ram_mib = 29 * 1024;
    tenant.memory_headroom_mib = 2048;
    assert!(p
        .check_fit(
            &[tenant.clone(), tenant.clone()],
            "MemTotal: 131491984 kB\n"
        )
        .is_ok());
    assert_eq!(
        p.check_fit(
            &[tenant.clone(), tenant.clone(), tenant],
            "MemTotal: 131491984 kB\n"
        )
        .unwrap_err()
        .case,
        Fit
    );
    for bad in [
        "",
        "MemTotal: nope kB",
        "MemTotal: 18446744073709551615 kB",
        "MemTotal: 1 MB",
        "MemTotal: 1 kB\nMemTotal: 2 kB",
    ] {
        assert_eq!(p.check_fit(&[], bad).unwrap_err().case, Fit);
    }
}

#[test]
fn fit_refuses_old_headroom_with_the_rewrite_remedy() {
    let mut tenant = crate::tenant::tests::registry("a");
    tenant.guest_ram_mib = 78 * 1024;
    tenant.memory_headroom_mib = 512;
    let refused = policy()
        .check_fit(&[tenant], "MemTotal: 131491984 kB\n")
        .unwrap_err();
    assert_eq!(refused.case, Fit);
    assert!(refused
        .message
        .contains("512 MiB is below minimum 2397 MiB"));
    assert!(refused.message.contains("guest_ram_mib 79872 MiB"));
    assert!(refused
        .message
        .contains("run provision.py switch-image or resize"));
}

#[test]
fn live_cap_and_exact_drop_in_are_read_only() {
    let p = policy();
    let expected = include_bytes!("../../../../tests/vectors/jobs-slice-capacity.conf");
    assert_eq!(p.drop_in().unwrap().as_bytes(), expected);
    let scratch = cortexkit_test_support::ScratchDir::new("jobs-drop-in");
    let path = scratch.join("50-capacity.conf");
    let read = || std::fs::read(&path).map_err(Error::from);
    for max in ["42949672960", "42949672959"] {
        p.check_live_cap(&format!("LoadState=loaded\nMemoryMax={max}\n"), read)
            .unwrap();
        assert!(!path.exists(), "a start must not create the drop-in");
    }
    for output in [
        "LoadState=loaded\nMemoryMax=42949672961\n",
        "LoadState=loaded\nMemoryMax=infinity\n",
        "LoadState=error\nMemoryMax=1\n",
        "LoadState=loaded\n",
        "LoadState=loaded\nMemoryMax=1\nMemoryMax=2\n",
    ] {
        assert_eq!(p.check_live_cap(output, read).unwrap_err().case, LiveCap);
        assert!(!path.exists());
    }
    assert_eq!(
        p.check_live_cap("LoadState=not-found\nMemoryMax=infinity\n", read)
            .unwrap_err()
            .case,
        DropIn
    );
    std::fs::write(&path, b"[Slice]\nMemoryMax=85%\n").unwrap();
    assert_eq!(
        p.check_live_cap("LoadState=not-found\n", read)
            .unwrap_err()
            .case,
        DropIn
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"[Slice]\nMemoryMax=85%\n");
    std::fs::write(&path, expected).unwrap();
    p.check_live_cap("LoadState=not-found\n", read).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), expected);
    for (jobs, high) in [(6, 5), (5, 5), (1, 1)] {
        let mut v = values();
        v["windows_vm_bytes"] = json!(0);
        v["jobs_min_bytes"] = json!(jobs);
        let p = Capacity::parse(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(
            p.drop_in().unwrap(),
            format!("[Slice]\nMemoryMax={jobs}\nMemoryHigh={high}\n")
        );
    }
}

#[test]
fn any_resize_journal_blocks_start_without_touching_saved_files() {
    let scratch = cortexkit_test_support::ScratchDir::new("resize-journal");
    let home = scratch.path();
    for file in [
        "suspend.vmstate",
        "suspend.memory",
        "key.json",
        "generation",
    ] {
        std::fs::write(home.join(file), file.as_bytes()).unwrap();
    }
    check_journal(home).unwrap();
    for content in [b"".as_slice(), b"{truncated", b"{}"] {
        std::fs::write(home.join("resize.json"), content).unwrap();
        assert_eq!(check_journal(home).unwrap_err().case, ResizeJournal);
    }
    std::fs::remove_file(home.join("resize.json")).unwrap();
    symlink(home.join("absent"), home.join("resize.json")).unwrap();
    assert_eq!(check_journal(home).unwrap_err().case, ResizeJournal);
    for file in [
        "suspend.vmstate",
        "suspend.memory",
        "key.json",
        "generation",
    ] {
        assert_eq!(std::fs::read(home.join(file)).unwrap(), file.as_bytes());
    }
}
