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

#[derive(Debug, PartialEq, Eq)]
struct InventoryEntry {
    kind: &'static str,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: (i64, i64),
    content: Option<String>,
    acl: String,
}

type Inventory = BTreeMap<PathBuf, InventoryEntry>;

fn inventory(root: &Path) -> Inventory {
    fn visit(root: &Path, path: &Path, entries: &mut Inventory) {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => panic!("inventory {path:?}: {error}"),
        };
        let kind = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else if metadata.file_type().is_symlink() {
            "symlink"
        } else {
            "special"
        };
        let content = if metadata.is_file() {
            Some(format!("{:x}", Sha256::digest(fs::read(path).unwrap())))
        } else {
            None
        };
        let acl = Command::new("/usr/bin/getfacl")
            .args(["-c", "-n", "-p", "-P", "--"])
            .arg(path)
            .output()
            .unwrap();
        assert!(acl.status.success());
        entries.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            InventoryEntry {
                kind,
                size: metadata.len(),
                mode: metadata.mode(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                mtime: (metadata.mtime(), metadata.mtime_nsec()),
                content,
                acl: format!("{:x}", Sha256::digest(acl.stdout)),
            },
        );
        if metadata.is_dir() {
            let mut children: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            children.sort();
            for child in children {
                visit(root, &child, entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn inventory_diff(before: &Inventory, after: &Inventory) -> String {
    fn describe(entry: Option<&InventoryEntry>) -> String {
        match entry {
            None => "<absent>".into(),
            Some(entry) => format!(
                "kind={} size={} mode={:06o} owner={}:{} mtime={}.{:09} content={} acl={}",
                entry.kind,
                entry.size,
                entry.mode,
                entry.uid,
                entry.gid,
                entry.mtime.0,
                entry.mtime.1,
                entry.content.as_deref().unwrap_or("-"),
                entry.acl,
            ),
        }
    }
    let paths: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let changed: Vec<_> = paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    if changed.is_empty() {
        return String::new();
    }
    // One header, at most 48 path records, and one truncation line stay within
    // 50 lines. Debug paths escape newlines in unexpected cache filenames.
    let mut lines = vec![format!(
        "shared-cache inventory differences: {} changed paths",
        changed.len()
    )];
    for path in changed.iter().take(48) {
        lines.push(format!(
            "{path:?}: before [{}] -> after [{}]",
            describe(before.get(*path)),
            describe(after.get(*path))
        ));
    }
    if changed.len() > 48 {
        lines.push(format!(
            "... {} more changed paths omitted",
            changed.len() - 48
        ));
    }
    lines.join("\n")
}

struct CacheSnapshot {
    fingerprint: String,
    inventory: Inventory,
}

impl CacheSnapshot {
    fn capture(root: &Path) -> Self {
        Self {
            fingerprint: fingerprint(root),
            inventory: inventory(root),
        }
    }

    fn assert_unchanged(&self, root: &Path) {
        let after = inventory(root);
        let differences = inventory_diff(&self.inventory, &after);
        if !differences.is_empty() {
            eprintln!("{differences}");
        }
        // Retain the original byte/ACL/metadata fingerprint and exact equality;
        // the inventory is diagnostic only and excludes nothing from the check.
        let actual = fingerprint(root);
        if actual != self.fingerprint && differences.is_empty() {
            eprintln!("shared-cache digest changed without a recorded per-path difference; cache changed during inventory capture");
        }
        assert_eq!(actual, self.fingerprint);
    }
}

#[test]
fn shared_cache_inventory_diff_reports_fields_and_is_bounded() {
    let scratch = cortexkit_test_support::ScratchDir::new("shared-cache-diff");
    let before = inventory(scratch.path());
    for number in 0..60 {
        fs::write(scratch.path().join(format!("file-{number:02}")), "changed").unwrap();
    }
    let after = inventory(scratch.path());
    let differences = inventory_diff(&before, &after);
    assert!(differences.lines().count() <= 50);
    for field in [
        "file-00",
        "kind=file",
        "size=7",
        "mode=",
        "owner=",
        "mtime=",
        "content=",
        "acl=",
        "omitted",
    ] {
        assert!(
            differences.contains(field),
            "missing {field}: {differences}"
        );
    }
    assert!(!inventory_diff(&after, &after).contains("changed"));
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
    let private = sandbox.runtime.0.join("cargo-overlay/private");
    if !private.exists() {
        return;
    }
    restore_runner_access(&private).unwrap();
    let upper = sandbox.runtime.0.join("cargo-overlay/archives/upper");
    if upper.exists() {
        restore_runner_access(&upper).unwrap();
    }
    let mut inspected = Vec::new();
    let mut paths = vec![private.join("src"), private.join("cache"), upper.clone()];
    if sandbox.locked_git_commits.is_empty() {
        paths.extend([private.join("git/db"), private.join("git/checkouts")]);
    }
    for path in paths {
        let empty = match fs::read_dir(&path) {
            Ok(mut entries) => Ok(entries.next().is_none()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error),
        };
        inspected.push((path, empty));
    }
    prepare_job_home(&private).unwrap();
    if upper.exists() {
        prepare_job_home(&upper).unwrap();
    }
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
    let offline_before = CacheSnapshot::capture(&shared);
    cargo_overlay::ownership::validate(&shared.join("registry/cache"));
    cargo_overlay::ownership::validated_owner(&shared.join("registry/cache")).unwrap();

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
    offline_before.assert_unchanged(&shared);

    let mut sandbox = f.sandbox().in_tenant_vm(true);
    shared_cache(&mut sandbox, &shared);
    let mut sandbox = host_inputs::fixture::with(
        host_inputs::fixture::Inputs {
            environment: BTreeMap::from([("CARGO_HOME", shared.clone().into_os_string())]),
            existing: BTreeMap::new(),
            directories: BTreeMap::new(),
        },
        || sandbox.with_network().unwrap(),
    );
    // Trusted seed preparation creates Cargo's two package-cache lock files and
    // changes their parent's size/mtime. Snapshot after that authorized setup,
    // before namespace setup or job code, without excluding any cache path.
    let before = CacheSnapshot::capture(&shared);
    let runtime = sandbox.runtime.0.clone();
    no_seeded_files(&sandbox);
    let overlays = sandbox.cargo_overlays.take();
    let mut launch = sandbox
        .launch("python3 cargo-network-probe.py", &f.workspace, &[])
        .unwrap();
    if let Some(overlays) = overlays {
        let decoy_fixture = Fixture::new();
        let decoy = decoy_fixture.sandbox().in_tenant_vm(true);
        let mut decoy_launch = decoy.launch("true", &decoy_fixture.workspace, &[]).unwrap();
        let decoy_pid =
            systemctl(&["show", "--property=MainPID", "--value", &unit(decoy.id)]).unwrap();
        let decoy_pid = String::from_utf8_lossy(&decoy_pid.stdout)
            .trim()
            .parse()
            .unwrap();
        let refused = overlays
            .setup_with_pid_for_test(sandbox.id, decoy_pid)
            .unwrap_err();
        assert!(
            refused.to_string().contains("cargo_overlay_target_refused"),
            "{refused}"
        );
        assert!(refused.to_string().contains("cgroup"), "{refused}");
        decoy_launch.release().unwrap();
        assert!(decoy_launch.wait().unwrap().success());
        decoy_launch.reap();
        drop(decoy_launch);
        drop(decoy);
        overlays.setup(sandbox.id).unwrap();
    }
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
    before.assert_unchanged(&shared);
    drop(sandbox);
    assert!(
        !runtime.exists(),
        "private Cargo cache survived job cleanup"
    );
    before.assert_unchanged(&shared);
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
    // Launch already uses sudo; isolated mode also delegates stop/kill/reset
    // so cleanup can stop the system-managed helper after recording its exit.
    config.isolate_jobs = true;
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
        "CARGO_CACHE_INVENTORY entries={entries} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    let archive_started = Instant::now();
    let (archive_entries, _) = cargo_overlay::ownership::validate(&home.join("registry/cache"));
    cargo_overlay::ownership::validated_owner(&home.join("registry/cache")).unwrap();
    println!(
        "CARGO_ARCHIVE_OWNERSHIP_STARTUP entries={archive_entries} elapsed_ms={}",
        archive_started.elapsed().as_millis()
    );
    assert!(
        entries >= 200_000,
        "use a production-sized shared cache, found {entries} entries"
    );
    let f = Fixture::new();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let archive = javascript_git(repository, &["archive".as_ref(), "HEAD".as_ref()]).unwrap();
    tar::Archive::new(std::io::Cursor::new(archive))
        .unpack(&f.workspace)
        .unwrap();
    fs::write(
        f.workspace.join("cargo-source-bench.py"),
        include_str!("../../test-fixtures/cargo-source-bench.py"),
    )
    .unwrap();
    let head = javascript_git(repository, &["rev-parse".as_ref(), "HEAD".as_ref()]).unwrap();
    let commits = known_git_commits(repository, String::from_utf8_lossy(&head).trim()).unwrap();
    let started = Instant::now();
    let mut sandbox = f.sandbox().in_tenant_vm(true);
    sandbox.locked_git_commits = commits;
    println!(
        "CARGO_SANDBOX_BASE_PREPARE_MS={}",
        started.elapsed().as_millis()
    );
    let network_started = Instant::now();
    let mut sandbox = sandbox.with_network().unwrap();
    println!(
        "CARGO_NETWORK_PREPARE_MS={}",
        network_started.elapsed().as_millis()
    );
    println!("CARGO_OVERLAY_PREPARE_MS={}", started.elapsed().as_millis());
    let host_home = sandbox.runtime.0.join("home");
    restore_runner_access(&host_home).unwrap();
    for name in ["registry", "git"] {
        fs::create_dir_all(host_home.join("reference-cargo").join(name)).unwrap();
        sandbox.read_only.push(
            bind(
                &home.join(name),
                &sandbox.home.join("reference-cargo").join(name),
            )
            .unwrap(),
        );
    }
    prepare_job_home(&host_home).unwrap();
    no_seeded_files(&sandbox);
    let mut launch = sandbox
        .launch("python3 cargo-source-bench.py", &f.workspace, &[])
        .unwrap();
    no_seeded_files(&sandbox);
    launch.release().unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    launch.stdout.read_to_string(&mut stdout).unwrap();
    launch.stderr.read_to_string(&mut stderr).unwrap();
    let status = launch.wait().unwrap();
    assert!(status.success(), "{stdout}\n{stderr}");
    println!("{stdout}");
    launch.reap();
    drop(launch);
    let started = Instant::now();
    drop(sandbox);
    println!("CARGO_OVERLAY_CLEANUP_MS={}", started.elapsed().as_millis());
}
