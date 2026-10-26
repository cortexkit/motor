//! Linux-only veth allocation and namespace attachment.
use fs2::FileExt;
use motor_protocol::Uuid;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

const IP: &str = "/usr/sbin/ip";
const SYSTEMCTL: &str = "/usr/bin/systemctl";

#[cfg(test)]
static OPERATOR_GENERATION: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) struct OperatorGenerationGuard(Option<PathBuf>);
#[cfg(test)]
impl Drop for OperatorGenerationGuard {
    fn drop(&mut self) {
        *OPERATOR_GENERATION.lock().unwrap() = self.0.take();
    }
}
#[cfg(test)]
pub(crate) fn operator_generation_path(path: PathBuf) -> OperatorGenerationGuard {
    OperatorGenerationGuard(OPERATOR_GENERATION.lock().unwrap().replace(path))
}

#[cfg(test)]
#[path = "network_linux/operator.rs"]
mod operator;
#[cfg(test)]
pub(crate) use operator::validated_generation;

fn generation_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = OPERATOR_GENERATION.lock().unwrap().clone() {
        return path;
    }
    PathBuf::from("/run/motor-net-ready")
}

/// A failed readiness check rejects only this admission, never future jobs.
/// Full permissions/kernel checks are repeated by attachment before it succeeds.
pub(crate) fn admission_probe() -> io::Result<()> {
    read_success_generation(&generation_path(), 0).map(|_| ())
}

fn sudo(path: &str, args: &[&str]) -> io::Result<String> {
    let output = crate::privilege::command(path).args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "network {path} {}: {} ({})",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim(),
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(crate) fn probe() -> io::Result<()> {
    probe_policy(
        sudo,
        || read_success_generation(&generation_path(), 0),
        || fs::read_to_string("/proc/sys/net/ipv4/ip_forward"),
    )?;
    // Permission queries never execute the mutating probe commands. Keep the
    // shared inventory in sync with attach and cleanup, including namespace entry.
    // A root runner that runs its helpers directly has no grants to query.
    let commands: Vec<Vec<String>> = if crate::privilege::uses_sudo() {
        serde_json::from_str(include_str!(
            "../../../script/runner/network-sudo-probes.json"
        ))?
    } else {
        Vec::new()
    };
    for command in commands {
        let output = Command::new("/usr/bin/sudo")
            .args(["-n", "-l"])
            .args(&command)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "network sudo -n permission missing for {}: {}",
                command.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(())
}

fn probe_policy(
    mut command: impl FnMut(&str, &[&str]) -> io::Result<String>,
    generation: impl FnOnce() -> io::Result<Uuid>,
    forwarding: impl FnOnce() -> io::Result<String>,
) -> io::Result<()> {
    command("/usr/sbin/nft", &["list", "table", "inet", "motor_net"])?;
    // A failed or interrupted reload can leave the previous table in the kernel.
    // Only a root-published completed load, not table existence, authorizes attach.
    generation()?;
    if forwarding()?.trim() != "1" {
        return Err(io::Error::other("network requires net.ipv4.ip_forward=1"));
    }
    Ok(())
}

fn read_success_generation(path: &Path, owner: u32) -> io::Result<Uuid> {
    read_success_generation_record(path, owner).map_err(|error| generation_error(path, error))
}

fn generation_error(path: &Path, error: io::Error) -> io::Error {
    let problem = if error.kind() == io::ErrorKind::NotFound {
        "is missing (the host's motor-net rules have not published a success generation)".into()
    } else {
        format!(
            "cannot be used: {error} (a readable, safe motor-net success generation is required)"
        )
    };
    io::Error::new(
        error.kind(),
        format!("job network not ready: {} {problem}", path.display()),
    )
}

fn read_success_generation_record(path: &Path, owner: u32) -> io::Result<Uuid> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
        || metadata.len() > 1024
    {
        return Err(io::Error::other(format!(
            "unsafe network success generation (requires a single-link regular file owned by uid {owner}, not group/world-writable, at most 1024 bytes)"
        )));
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Ready {
        schema: u8,
        generation: String,
    }
    let mut bytes = Vec::new();
    file.by_ref().take(1025).read_to_end(&mut bytes)?;
    let ready: Ready = serde_json::from_slice(&bytes)?;
    let generation = Uuid::parse_str(&ready.generation).map_err(io::Error::other)?;
    if ready.schema != 1
        || generation.get_version_num() != 4
        || generation.simple().to_string() != ready.generation
    {
        return Err(io::Error::other("invalid network success generation"));
    }
    Ok(generation)
}

/// The host-wide lock that keeps attach and startup reconciliation from
/// seeing each other's half-created links.
pub(crate) fn lock() -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/run/lock/motor-net.lock")?;
    let metadata = lock.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || !metadata.is_file()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "network allocation lock has unsafe owner or permissions",
        ));
    }
    lock.lock_exclusive()?;
    Ok(lock)
}

fn links() -> io::Result<Vec<Value>> {
    serde_json::from_str(&sudo(IP, &["-j", "link", "show"])?).map_err(io::Error::other)
}

pub(crate) fn interface(id: Uuid) -> String {
    let simple = id.simple().to_string();
    // UUIDv7's leading bytes share a timestamp, so use its random trailing
    // characters to distinguish simultaneous jobs within the 15-byte name limit.
    format!("mnet-{}", &simple[22..])
}

fn cleanup_unit(name: &str) -> String {
    format!("motor-net-{}.service", name.trim_start_matches("mnet-"))
}

fn owner(link: &Value) -> Option<(Uuid, u16)> {
    let mut parts = link.get("ifalias")?.as_str()?.split(':');
    if parts.next()? != "motor-net" {
        return None;
    }
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn live(id: Uuid) -> io::Result<bool> {
    let state = sudo(
        SYSTEMCTL,
        &[
            "show",
            "--property=ActiveState",
            "--value",
            &crate::backend::host_unit_name(id),
        ],
    )?;
    Ok(matches!(
        state.as_str(),
        "active" | "activating" | "deactivating"
    ))
}

pub(crate) fn reconcile() -> io::Result<()> {
    let _lock = lock()?;
    for link in links()? {
        let Some(name) = link.get("ifname").and_then(Value::as_str) else {
            continue;
        };
        if !name.starts_with("mnet-") {
            continue;
        }
        let id = match owner(&link) {
            Some((id, _)) => Some(id),
            None => {
                // A daemon can die between creating the link and setting its
                // alias. The cleanup unit is installed first and records its owner.
                let binds = sudo(
                    SYSTEMCTL,
                    &["show", "--property=BindsTo", "--value", &cleanup_unit(name)],
                )?;
                binds.split_whitespace().find_map(|unit| {
                    unit.strip_prefix("motor-job-")?
                        .strip_suffix(".service")?
                        .parse()
                        .ok()
                })
            }
        };
        if let Some(id) = id {
            if live(id)? {
                continue;
            }
        }
        sudo(IP, &["link", "delete", "dev", name])?;
        let _ = sudo(SYSTEMCTL, &["stop", &cleanup_unit(name)]);
    }
    Ok(())
}

pub(crate) fn gateway(slot: u16) -> String {
    let offset = u32::from(slot) * 4 + 1;
    format!("10.88.{}.{}", offset / 256, offset % 256)
}

fn job_address(slot: u16) -> String {
    let offset = u32::from(slot) * 4 + 2;
    format!("10.88.{}.{}/30", offset / 256, offset % 256)
}

fn namespace(pid: u32, path: &str, args: &[&str]) -> io::Result<()> {
    let pid = pid.to_string();
    let mut command = vec!["--target", &pid, "--net", path];
    command.extend(args);
    sudo("/usr/bin/nsenter", &command).map(|_| ())
}

const SYSCTL: &str = "/usr/sbin/sysctl";
const TEST: &str = "/usr/bin/test";
const DISABLE_ETH0_IPV6: [&str; 3] = ["-q", "-w", "net.ipv6.conf.eth0.disable_ipv6=1"];
/// Succeeds only when the network namespace it runs in has no IPv6 sysctl
/// tree. The kernel shows each process the `/proc/sys/net` of its own network
/// namespace, so running it under `nsenter --net` checks the job's namespace.
const IPV6_ABSENT: [&str; 3] = ["!", "-e", "/proc/sys/net/ipv6"];

/// Turn off IPv6 on the job's `eth0`, which the job firewall and routes only
/// cover for IPv4. A kernel booted with `ipv6.disable=1` or built without IPv6
/// has no `/proc/sys/net/ipv6` in any namespace: IPv6
/// cannot be enabled there, so a failed write is accepted only when
/// `ipv6_absent` proves that. Every other failure, including a failed write
/// while IPv6 exists, still fails the attach with the write's own error.
fn disable_eth0_ipv6(
    disable: impl FnOnce() -> io::Result<()>,
    ipv6_absent: impl FnOnce() -> bool,
) -> io::Result<()> {
    let Err(error) = disable() else {
        return Ok(());
    };
    if ipv6_absent() {
        tracing::debug!(%error, "job namespace has no IPv6 support; skipping the eth0 IPv6 disable");
        return Ok(());
    }
    Err(error)
}

pub(crate) fn attach(id: Uuid) -> io::Result<()> {
    let started = Instant::now();
    // Recheck inet motor_net and IPv4 forwarding in case an operator removed
    // them since startup. The runner releases the job's command gate only after
    // attachment succeeds, so no user command can run on a partially set-up link.
    probe()?;
    let pid: u32 = sudo(
        SYSTEMCTL,
        &[
            "show",
            "--property=MainPID",
            "--value",
            &crate::backend::host_unit_name(id),
        ],
    )?
    .parse()
    .map_err(io::Error::other)?;
    if pid == 0 {
        return Err(io::Error::other("network: job unit has no MainPID"));
    }
    let _lock = lock()?;
    let existing = links()?;
    let name = interface(id);
    if existing.iter().any(|link| link["ifname"] == name) {
        return Err(io::Error::other(format!(
            "network interface collision: {name}"
        )));
    }
    let occupied: BTreeSet<_> = existing
        .iter()
        .filter_map(owner)
        .map(|(_, slot)| slot)
        .collect();
    // Interface aliases record /30 slots before addresses are assigned. Count
    // those slots as occupied until the kernel link is gone, so overlapping
    // jobs can never be allocated the same addresses.
    let slot = (0..16384u16)
        .find(|slot| !occupied.contains(slot))
        .ok_or_else(|| io::Error::other("network /30 pool exhausted"))?;
    let gateway = gateway(slot);
    let job = job_address(slot);
    let peer = format!("mj-{}", name.trim_start_matches("mnet-"));
    let cleanup = cleanup_unit(&name);
    let unit = crate::backend::host_unit_name(id);
    // A separate host-namespace service is bound to the job's unit lifetime.
    // ExecStop runs even when the daemon is replaced or dies during attachment.
    // Its delete usually finds the link already gone: ending the job destroys
    // its network namespace, which removes the peer and with it the whole veth
    // pair. The leading `-` keeps that expected miss from leaving a failed
    // unit; a link that does survive is removed by startup reconciliation.
    sudo(
        "/usr/bin/systemd-run",
        &[
            "--quiet",
            "--collect",
            "--service-type=oneshot",
            crate::systemd::JOB_MEMORY_PROPERTIES[0],
            crate::systemd::JOB_MEMORY_PROPERTIES[1],
            crate::systemd::JOB_MEMORY_PROPERTIES[2],
            &format!("--unit={cleanup}"),
            &format!("--property=BindsTo={unit}"),
            &format!("--property=After={unit}"),
            "--property=RemainAfterExit=yes",
            &format!("--property=ExecStop=-{IP} link delete dev {name}"),
            "/usr/bin/true",
        ],
    )?;
    let result = (|| {
        sudo(
            IP,
            &["link", "add", &name, "type", "veth", "peer", "name", &peer],
        )?;
        sudo(
            IP,
            &[
                "link",
                "set",
                "dev",
                &name,
                "alias",
                &format!("motor-net:{id}:{slot}"),
            ],
        )?;
        sudo(IP, &["addr", "add", &format!("{gateway}/30"), "dev", &name])?;
        sudo(
            IP,
            &["link", "set", "dev", &peer, "netns", &pid.to_string()],
        )?;
        namespace(pid, IP, &["link", "set", "dev", &peer, "name", "eth0"])?;
        disable_eth0_ipv6(
            || namespace(pid, SYSCTL, &DISABLE_ETH0_IPV6),
            // Only a root runner may run this check. A sudo runner has no
            // grant for it, and attempting an ungranted command would only add
            // a sudo policy failure to an attach that is failing anyway.
            || !crate::privilege::uses_sudo() && namespace(pid, TEST, &IPV6_ABSENT).is_ok(),
        )?;
        namespace(pid, IP, &["addr", "add", &job, "dev", "eth0"])?;
        namespace(pid, IP, &["link", "set", "dev", "eth0", "up"])?;
        sudo(IP, &["link", "set", "dev", &name, "up"])?;
        namespace(pid, IP, &["route", "add", "default", "via", &gateway])?;
        // Attachment is authorized under the generation this final probe reads.
        // A later withdrawal affects new jobs and tenant gates, not this accepted
        // attachment; command-gate release is intentionally not another probe.
        probe()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = sudo(SYSTEMCTL, &["stop", &cleanup]);
        // The namespace may have vanished while setup was in flight. A surviving
        // veth still reserves its /30 and startup reconciliation retries removal.
        if links()?.iter().any(|link| link["ifname"] == name) {
            sudo(IP, &["link", "delete", "dev", &name])?;
        }
    }
    tracing::info!(job_id = %id, elapsed_us = started.elapsed().as_micros(), "network attach finished");
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn readiness_refusal_names_missing_unsafe_and_unreadable_generation() {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("motor-ready-diagnostic-{}", Uuid::now_v7()));
        fs::create_dir(&root).unwrap();
        let marker = root.join("motor-net-ready");
        let owner = unsafe { libc::geteuid() };
        let missing = read_success_generation(&marker, owner).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert_eq!(missing.to_string(), format!(
            "job network not ready: {} is missing (the host's motor-net rules have not published a success generation)", marker.display()
        ));
        fs::write(
            &marker,
            r#"{"schema":1,"generation":"c61f406a764b4df3ae8552b43aa520b9"}"#,
        )
        .unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o666)).unwrap();
        let unsafe_marker = read_success_generation(&marker, owner)
            .unwrap_err()
            .to_string();
        assert!(unsafe_marker.contains(&format!("job network not ready: {}", marker.display())));
        assert!(unsafe_marker.contains("unsafe network success generation"));
        // Format permission failures without relying on the test account being
        // unable to bypass file permissions (the operator may run as root).
        let unreadable =
            generation_error(&marker, io::Error::from(io::ErrorKind::PermissionDenied));
        assert_eq!(unreadable.kind(), io::ErrorKind::PermissionDenied);
        assert!(unreadable
            .to_string()
            .contains(&marker.display().to_string()));
        assert!(unreadable
            .to_string()
            .contains("readable, safe motor-net success generation is required"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operator_generation_override_is_unprivileged_and_restores_the_previous_path() {
        let previous = super::generation_path();
        let private = std::env::temp_dir().join("operator-private-generation");
        let guard = super::operator_generation_path(private.clone());
        assert_eq!(super::generation_path(), private);
        drop(guard);
        assert_eq!(super::generation_path(), previous);
    }

    #[test]
    fn existing_table_after_failed_reload_does_not_authorize_attach() {
        use super::*;
        let root = std::env::temp_dir().join(format!("motor-policy-ready-{}", Uuid::now_v7()));
        fs::create_dir(&root).unwrap();
        let marker = root.join("ready");
        let generation = Uuid::parse_str("c61f406a-764b-4df3-ae85-52b43aa520b9").unwrap();
        fs::write(
            &marker,
            serde_json::json!({"schema":1,"generation":generation.simple().to_string()})
                .to_string(),
        )
        .unwrap();
        let owner = unsafe { libc::geteuid() };
        let probe = || {
            probe_policy(
                |_, _| Ok("table inet motor_net { old policy }".into()),
                || read_success_generation(&marker, owner),
                || Ok("1".into()),
            )
        };
        probe().unwrap();
        // The loader invalidates its generation before attempting a reload;
        // the old kernel table can survive a failed nft --check unchanged.
        fs::remove_file(&marker).unwrap();
        assert!(
            probe().is_err(),
            "an existing table must not bypass an unfinished or failed load"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn policy_success_generation_requires_safe_owner_mode_and_canonical_record() {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = std::env::temp_dir().join(format!("motor-policy-marker-{}", Uuid::now_v7()));
        fs::create_dir(&root).unwrap();
        let marker = root.join("ready");
        let owner = unsafe { libc::geteuid() };
        fs::write(
            &marker,
            serde_json::json!({"schema":1,"generation":"c61f406a764b4df3ae8552b43aa520b9"})
                .to_string(),
        )
        .unwrap();
        read_success_generation(&marker, owner).unwrap();
        assert!(read_success_generation(&marker, owner.wrapping_add(1)).is_err());
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_success_generation(&marker, owner).is_err());
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        let alias = root.join("alias");
        symlink(&marker, &alias).unwrap();
        assert!(read_success_generation(&alias, owner).is_err());
        for invalid in ["{}", "{\"schema\":1,\"generation\":\"not-a-generation\"}"] {
            fs::write(&marker, invalid).unwrap();
            assert!(read_success_generation(&marker, owner).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    use super::*;
    #[test]
    fn pool_and_names_are_distinct_and_bounded() {
        assert_eq!(gateway(0), "10.88.0.1");
        assert_eq!(job_address(16383), "10.88.255.254/30");
        let mut addresses = BTreeSet::new();
        for slot in 0..16384 {
            assert!(addresses.insert(job_address(slot)));
        }
        let name = interface(Uuid::now_v7());
        assert_eq!(name.len(), 15);
    }

    #[test]
    fn eth0_ipv6_disable_is_skipped_only_when_the_namespace_has_no_ipv6() {
        use std::cell::Cell;
        // IPv6 present and the write succeeds (every current host type): no
        // check runs.
        let checked = Cell::new(false);
        disable_eth0_ipv6(
            || Ok(()),
            || {
                checked.set(true);
                false
            },
        )
        .unwrap();
        assert!(!checked.get());

        let missing = || {
            Err(io::Error::other(
                "sysctl: cannot stat /proc/sys/net/ipv6/conf/eth0/disable_ipv6: No such file or directory",
            ))
        };
        // A kernel built or booted without IPv6: nothing to disable, so the
        // step is skipped.
        disable_eth0_ipv6(missing, || true).unwrap();
        // IPv6 exists (or its absence is unproven) but the write failed: the
        // attach still fails closed with the write's error.
        let error = disable_eth0_ipv6(missing, || false).unwrap_err();
        assert!(error.to_string().contains("disable_ipv6"), "{error}");
    }

    #[test]
    fn ipv6_absence_check_proves_absence_by_succeeding() {
        assert_eq!(IPV6_ABSENT, ["!", "-e", "/proc/sys/net/ipv6"]);
        let status = Command::new(TEST).args(IPV6_ABSENT).status().unwrap();
        assert_eq!(
            status.success(),
            !std::path::Path::new("/proc/sys/net/ipv6").exists()
        );
    }
}
