use super::*;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn fingerprint(root: &Path) -> String {
    fn visit(path: &Path, hash: &mut Sha256) {
        let metadata = fs::symlink_metadata(path).unwrap();
        hash.update(format!(
            "{}:{}:{}:{}:{}:{}:{}\n",
            path.display(),
            metadata.mode(),
            metadata.uid(),
            metadata.gid(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        ));
        if metadata.is_dir() {
            let mut children: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            children.sort();
            for child in children {
                visit(&child, hash);
            }
        } else {
            assert!(metadata.is_file());
            hash.update(fs::read(path).unwrap());
        }
    }
    let mut hash = Sha256::new();
    visit(root, &mut hash);
    format!("{:x}", hash.finalize())
}

fn shared_cache(sandbox: &mut Sandbox, shared: &Path) {
    for name in ["registry", "git"] {
        let destination = sandbox.cargo.join(name);
        let suffix = format!(":{}", word(&destination).unwrap());
        sandbox.read_only.retain(|entry| !entry.ends_with(&suffix));
        sandbox
            .read_only
            .push(bind(&shared.join(name), &destination).unwrap());
    }
}

#[test]
#[ignore = "requires root, reflink scratch, motor-job, systemd and provisioned motor_net"]
fn isolated_tenant_net_fetches_missing_crate_without_writing_shared_cache() {
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "run as root on the operator host"
    );
    let _guard = net_operator_lock();
    let f = Fixture::new();
    let shared = f.scratch.join("shared-cargo");
    for name in ["registry/index/seed", "registry/cache/seed", "git/db/seed"] {
        let directory = shared.join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("canary"), "shared\n").unwrap();
        fs::set_permissions(directory.join("canary"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();
    }
    fs::write(
        f.workspace.join("cargo-network-probe.py"),
        include_str!("../../test-fixtures/cargo-network-probe.py"),
    )
    .unwrap();
    let before = fingerprint(&shared);

    // An offline job still reads the shared trees and cannot write to them.
    let mut offline = f.sandbox().in_tenant_vm(true);
    shared_cache(&mut offline, &shared);
    let (status, stdout, stderr) = output(
        &offline,
        "set -eu; test \"$CARGO_NET_OFFLINE\" = true; for p in registry/index registry/cache git/db; do test \"$(cat \"$CARGO_HOME/$p/seed/canary\")\" = shared; if touch \"$CARGO_HOME/$p/seed/forbidden\"; then exit 41; fi; done",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    drop(offline);
    assert_eq!(fingerprint(&shared), before);

    let mut sandbox = f.sandbox().in_tenant_vm(true);
    shared_cache(&mut sandbox, &shared);
    let sandbox = host_inputs::fixture::with(
        host_inputs::fixture::Inputs {
            environment: BTreeMap::from([("CARGO_HOME", shared.clone().into_os_string())]),
            existing: BTreeMap::new(),
            directories: BTreeMap::new(),
        },
        || sandbox.with_network().unwrap(),
    );
    let runtime = sandbox.runtime.0.clone();
    let (status, stdout, stderr) = output(&sandbox, "python3 cargo-network-probe.py", &f.workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("downloaded private crate"), "{stdout}");
    assert_eq!(fingerprint(&shared), before);
    drop(sandbox);
    assert!(
        !runtime.exists(),
        "private Cargo cache survived job cleanup"
    );
    assert_eq!(fingerprint(&shared), before);
}
