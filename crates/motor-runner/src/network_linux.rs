//! Linux-only veth allocation and namespace attachment.
use fs2::FileExt;
use motor_protocol::Uuid;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io,
    process::Command,
    time::Instant,
};

const IP: &str = "/usr/sbin/ip";
const SYSTEMCTL: &str = "/usr/bin/systemctl";

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
    sudo("/usr/sbin/nft", &["list", "table", "inet", "motor_net"])?;
    if fs::read_to_string("/proc/sys/net/ipv4/ip_forward")?.trim() != "1" {
        return Err(io::Error::other("network requires net.ipv4.ip_forward=1"));
    }
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
        namespace(
            pid,
            "/usr/sbin/sysctl",
            &["-q", "-w", "net.ipv6.conf.eth0.disable_ipv6=1"],
        )?;
        namespace(pid, IP, &["addr", "add", &job, "dev", "eth0"])?;
        namespace(pid, IP, &["link", "set", "dev", "eth0", "up"])?;
        sudo(IP, &["link", "set", "dev", &name, "up"])?;
        namespace(pid, IP, &["route", "add", "default", "via", &gateway])?;
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
}
