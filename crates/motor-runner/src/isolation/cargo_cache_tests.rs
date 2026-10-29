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
        let acl = Command::new("/usr/bin/getfacl")
            .args(["-c", "-n", "-p", "--"])
            .arg(path)
            .output()
            .unwrap();
        assert!(acl.status.success());
        hash.update(acl.stdout);
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

fn print_kernel() {
    let output = Command::new("/usr/bin/uname").arg("-r").output().unwrap();
    assert!(output.status.success());
    println!(
        "CARGO_OVERLAY_KERNEL={}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
}

fn no_seeded_files(sandbox: &Sandbox) {
    // The operator inspects only private, otherwise job-owned directories before
    // job execution. Revoke this delegated access before the job command starts.
    let home = sandbox.runtime.0.join("home");
    restore_runner_access(&home).unwrap();
    let mut inspected = Vec::new();
    for name in ["registry", "git"] {
        for path in [
            sandbox.runtime.0.join("home/cargo").join(name),
            sandbox
                .runtime
                .0
                .join("cargo-overlay")
                .join(name)
                .join("upper"),
        ] {
            let empty = match fs::read_dir(&path) {
                Ok(mut entries) => Ok(entries.next().is_none()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error),
            };
            inspected.push((path, empty));
        }
    }
    prepare_job_home(&home).unwrap();
    for (path, empty) in inspected {
        assert!(
            empty.unwrap(),
            "cache files copied into {} before Cargo ran",
            path.display()
        );
    }
}

#[test]
fn overlay_backing_path_is_reserved_for_tenant_network_jobs() {
    let f = Fixture::new();
    let mut config = crate::Config::new(&f.state, "reserved-overlay");
    let reserved = |config: &crate::Config| {
        reserved_destinations(config).contains(&PathBuf::from(cargo_overlay::LAYERS))
    };
    assert!(!reserved(&config));
    config.tenant_vm = true;
    config.job_network = Some(crate::network::JobNetwork::Veth);
    assert!(reserved(&config));
    config.boat_profile = true;
    assert!(!reserved(&config));
}

#[test]
#[ignore = "operator only: non-root account with sudo -n, provisioned motor_net and trusted host isolation"]
fn isolated_tenant_net_fetches_missing_crate_without_writing_shared_cache() {
    print_kernel();
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
    for name in ["registry", "git"] {
        // Production defaults name both reader accounts. Exercise ACL copy-up
        // as well as file data, and leave the shared ACLs untouched.
        grant_read_only_tree_as(&shared.join(name), true, USER).unwrap();
        grant_read_only_tree_as(&shared.join(name), true, WRITER_USER).unwrap();
    }
    fs::write(
        f.workspace.join("cargo-network-probe.py"),
        include_str!("../../test-fixtures/cargo-network-probe.py"),
    )
    .unwrap();
    let before = fingerprint(&shared);
    for name in ["registry", "git"] {
        cargo_overlay::ownership::validate(&shared.join(name));
        cargo_overlay::ownership::validated_owner(&shared.join(name)).unwrap();
    }

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
    no_seeded_files(&sandbox);
    let mut launch = sandbox
        .launch("python3 cargo-network-probe.py", &f.workspace, &[])
        .unwrap();
    // Namespace setup has finished, but the start gate still prevents Cargo
    // from running. Even a populated lower must leave both uppers empty.
    no_seeded_files(&sandbox);
    launch.release().unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    launch.stdout.read_to_string(&mut stdout).unwrap();
    launch.stderr.read_to_string(&mut stderr).unwrap();
    let status = launch.wait().unwrap();
    launch.reap();
    drop(launch);
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

#[test]
#[ignore = "operator only: non-root account with sudo -n and trusted host isolation"]
fn isolated_tenant_overlay_refuses_mixed_cache_owners() {
    print_kernel();
    let f = Fixture::new();
    let lower = f.scratch.join("lower");
    fs::create_dir(&lower).unwrap();
    fs::write(lower.join("foreign-owner"), "cache entry").unwrap();
    checked(
        crate::privilege::command("/usr/bin/chown")
            .args(["--", "motor-job:motor-job"])
            .arg(lower.join("foreign-owner")),
    )
    .unwrap();
    cargo_overlay::ownership::validate(&lower);
    let error = cargo_overlay::ownership::validated_owner(&lower).unwrap_err();
    assert!(
        error.to_string().contains("cargo_overlay_mixed_ownership"),
        "{error}"
    );
}

#[test]
#[ignore = "operator only: non-root account with sudo -n and online trusted snapshot helpers"]
fn isolated_tenant_prefetch_writes_as_validated_cache_owner() {
    print_kernel();
    let f = Fixture::new();
    let scratch = f.scratch.join("prefetch");
    assert!(crate::privilege::uses_sudo());
    let rustup = PathBuf::from(std::env::var_os("RUSTUP_HOME").unwrap());
    let mut config = crate::Config::new(&scratch, "state");
    config.tenant_vm = true;
    // The operator host is not an attached tenant. Give the helper a private
    // copy of its usable DNS addresses without writing the fixed /run paths.
    let directory = f.scratch.join("tenant-resolver");
    let resolvers: Vec<String> = fs::read_to_string("/etc/resolv.conf")
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != "nameserver" {
                return None;
            }
            fields.next()?.parse::<std::net::Ipv4Addr>().ok()
        })
        .take(3)
        .map(|address| address.to_string())
        .collect();
    assert!(
        !resolvers.is_empty(),
        "operator host needs an IPv4 DNS resolver"
    );
    crate::tenant_agent::dns::install(&directory, &resolvers, unsafe { libc::geteuid() }).unwrap();
    let marker = f.scratch.join("tenant-resolver-marker");
    fs::write(&marker, Uuid::now_v7().to_string()).unwrap();
    use crate::sibling_snapshots::helper::{self, Helper, TenantResolver};
    let resolver = TenantResolver::from_test_attachment(&config, &marker, &directory);
    assert!(matches!(&resolver, TenantResolver::Attached { .. }));
    // check-tenant-cargo-fetch is a root-only CLI. Use its cold fetch and
    // snapshot helpers without changing the operator's delegated sudo mode.
    cargo_fetch::check_online_via(&scratch, &rustup, |command, limit| {
        let view =
            crate::sibling_snapshots::helper::View::prefetch(&scratch.join("workspace"), &[])?
                .keep(&scratch.join("cargo-home"), true)
                .keep(&rustup, false);
        helper::run_with_test_resolver(
            &config,
            Helper::CargoPrefetch,
            limit.min(Helper::CargoPrefetch.limit(&config)),
            command,
            &view,
            resolver.clone(),
        )
    })
    .unwrap();
    assert!(crate::privilege::uses_sudo());
    let registry = scratch.join("cargo-home/registry");
    let (entries, _) = cargo_overlay::ownership::validate(&registry);
    assert!(entries > 0, "the cold prefetch must create cache files");
    let owner = cargo_overlay::ownership::validated_owner(&registry).unwrap();
    assert_eq!(owner.uid, unsafe { libc::geteuid() });
}

#[test]
#[ignore = "operator only: non-root account with sudo -n, a large shared Cargo cache and provisioned motor_net"]
fn isolated_tenant_net_overlay_setup_on_large_cache() {
    print_kernel();
    let _guard = net_operator_lock();
    let home = PathBuf::from(std::env::var_os("CARGO_HOME").unwrap());
    let mut entries = 0;
    let started = Instant::now();
    for name in ["registry", "git"] {
        let path = home.join(name);
        entries += cargo_overlay::ownership::validate(&path).0;
        cargo_overlay::ownership::validated_owner(&path).unwrap();
    }
    println!(
        "CARGO_OVERLAY_STARTUP entries={entries} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert!(
        entries >= 200_000,
        "use a production-sized shared cache, found {entries} entries"
    );
    let f = Fixture::new();
    let started = Instant::now();
    let sandbox = f.sandbox().in_tenant_vm(true);
    println!(
        "CARGO_SANDBOX_BASE_PREPARE_MS={}",
        started.elapsed().as_millis()
    );
    let network_started = Instant::now();
    let sandbox = sandbox.with_network().unwrap();
    println!(
        "CARGO_NETWORK_PREPARE_MS={}",
        network_started.elapsed().as_millis()
    );
    println!("CARGO_OVERLAY_PREPARE_MS={}", started.elapsed().as_millis());
    no_seeded_files(&sandbox);
    let mut launch = sandbox.launch("true", &f.workspace, &[]).unwrap();
    no_seeded_files(&sandbox);
    launch.release().unwrap();
    assert!(launch.wait().unwrap().success());
    launch.reap();
    drop(launch);
    let started = Instant::now();
    drop(sandbox);
    println!("CARGO_OVERLAY_CLEANUP_MS={}", started.elapsed().as_millis());
}
