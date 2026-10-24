use super::*;
use crate::remote_cache::gateway::Tool;
use crate::remote_cache::provisioning::HostRole;

#[test]
fn ccache_rollout_refuses_unprovisioned_roles_and_unpinned_binaries() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../../script/runner/toolchain.json")).unwrap();
    let pin = &manifest["ccache"];
    assert_eq!(pin["version"], VERSION);
    assert_eq!(pin["print_stats"]["remote_hit"], REMOTE_HIT);
    assert_eq!(pin["print_stats"]["remote_error"], REMOTE_ERROR);
    assert_eq!(pin["print_stats"]["remote_timeout"], REMOTE_TIMEOUT);
    for (name, role) in [
        ("build-server", HostRole::BuildServer),
        ("boat", HostRole::Boat),
        ("tenant", HostRole::TenantVm),
    ] {
        assert_eq!(
            usable(role, "ccache version 4.12.3\nFeatures: http-storage"),
            pin["provisioned_by_role"][name].as_bool().unwrap()
        );
        assert!(!usable(role, "ccache version 4.9.1"));
        assert!(!usable(role, ""));
    }
    assert!(usable(HostRole::BuildServer, "ccache version 4.12.3"));
    assert!(!usable(HostRole::Boat, "ccache version 4.12.3"));
    assert!(!usable(HostRole::TenantVm, "ccache version 4.12.3"));
}

#[test]
fn all_roles_use_private_remote_only_ccache_without_compiler_wrappers() {
    let forwarder = Forwarder::new(Tool::Ccache, 4182);
    for role in [HostRole::BuildServer, HostRole::Boat, HostRole::TenantVm] {
        let mut env = super::super::environment::delta(role, &[forwarder.clone()], "b", "r");
        env.extend(environment(&forwarder, false, "/caller/project"));
        assert_eq!(
            env,
            BTreeMap::from([
                (
                    "CCACHE_REMOTE_STORAGE".into(),
                    "http://127.0.0.1:4182|read-only".into()
                ),
                ("CCACHE_REMOTE_ONLY".into(), "true".into()),
                ("CCACHE_DIR".into(), "/tmp/motor-ccache".into()),
                ("CCACHE_CONFIGPATH".into(), "/dev/null".into()),
                ("CCACHE_COMPILERCHECK".into(), "content".into()),
                ("CCACHE_NOHASHDIR".into(), "true".into()),
                ("CCACHE_BASEDIR".into(), "/caller/project".into()),
            ])
        );
    }
    let fill = environment(&forwarder, true, "/fill/project");
    assert_eq!(fill["CCACHE_REMOTE_STORAGE"], "http://127.0.0.1:4182");
    assert_eq!(fill["CCACHE_RECACHE"], "true");
    assert_eq!(fill["CCACHE_REMOTE_ONLY"], "true");
    assert_eq!(fill["CCACHE_BASEDIR"], "/fill/project");
}

#[test]
fn pinned_statistics_require_complete_unambiguous_remote_counters() {
    let good = b"remote_storage_hit\t2\nremote_storage_error\t0\nremote_storage_timeout\t0\nremote_storage_write\t3\n";
    let stats = statistics(good).unwrap();
    assert_eq!(stats[REMOTE_HIT], 2);
    assert_eq!(stats[REMOTE_ERROR], 0);
    assert_eq!(stats[REMOTE_TIMEOUT], 0);
    assert_eq!(stats["remote_storage_write"], 3);
    for bad in [
        "",
        "not statistics",
        "remote_storage_hit 1\nremote_storage_error 0\n",
        "remote_storage_hit 1\nremote_storage_error 0\nremote_storage_error 0\n",
        "remote_storage_hit 1\nremote_storage_error -1\nremote_storage_timeout 0\n",
        "remote_storage_hit 1 extra\nremote_storage_error 0\nremote_storage_timeout 0\n",
    ] {
        assert!(statistics(bad.as_bytes()).is_err(), "accepted {bad:?}");
    }
}

#[test]
fn ccache_statistics_reader_rejects_symlinks_and_oversized_output() {
    use std::fs;
    let scratch = cortexkit_test_support::ScratchDir::new("ccache-statistics");
    let id = motor_protocol::Uuid::now_v7();
    let output = scratch
        .join("sandboxes")
        .join(id.to_string())
        .join("output");
    fs::create_dir_all(&output).unwrap();
    let read = || crate::writer::fill::ccache_statistics(&scratch, id);
    assert!(read().is_err());
    fs::write(output.join(STATS_FILE), b"").unwrap();
    assert!(read().is_err());
    fs::write(output.join(STATS_FILE), vec![b' '; 64 * 1024 + 1]).unwrap();
    assert!(read().is_err());
    fs::remove_file(output.join(STATS_FILE)).unwrap();
    let outside = scratch.join("other-stats");
    fs::write(
        &outside,
        "remote_storage_hit 1\nremote_storage_error 0\nremote_storage_timeout 0\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, output.join(STATS_FILE)).unwrap();
    assert!(read().is_err());
    fs::remove_file(output.join(STATS_FILE)).unwrap();
    fs::copy(&outside, output.join(STATS_FILE)).unwrap();
    assert_eq!(read().unwrap()[REMOTE_HIT], 1);
    assert_eq!(
        crate::writer::fill::ccache_write_failures(&scratch, id).unwrap(),
        0
    );
    fs::write(
        output.join(STATS_FILE),
        "remote_storage_hit 0\nremote_storage_error 2\nremote_storage_timeout 3\n",
    )
    .unwrap();
    assert_eq!(
        crate::writer::fill::ccache_write_failures(&scratch, id).unwrap(),
        5
    );
}
