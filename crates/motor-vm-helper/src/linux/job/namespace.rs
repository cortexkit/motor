//! Per-job network namespaces. Host policy still sees the guest's original IP
//! on mtapN, now the host end of a routed veth rather than the guest-facing TAP.
use super::*;
use std::io::Write;

const PREFIX: &str = "motor-job-";
const ROOT: &str = "/run/netns";
const UPLINK: &str = "uplink";

fn name(id: &str) -> Result<String> {
    job::validate_id(id)?;
    Ok(format!("{PREFIX}{id}"))
}

pub(super) fn path(id: &str) -> Result<PathBuf> {
    Ok(Path::new(ROOT).join(name(id)?))
}

fn checked(mut command: Command) -> Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(Error(format!(
            "job network command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn ip(args: &[&str]) -> Result<()> {
    let mut command = Command::new("/usr/sbin/ip");
    command.args(args);
    checked(command)
}

fn inside(id: &str, program: &str) -> Result<Command> {
    let mut command = Command::new("/usr/bin/nsenter");
    command.arg(format!("--net={}", path(id)?.display()));
    command.args(["--", program]);
    Ok(command)
}

fn ns_ip(id: &str, args: &[&str]) -> Result<()> {
    let mut command = inside(id, "/usr/sbin/ip")?;
    command.args(args);
    checked(command)
}

fn sysctl(id: &str, settings: &[&str]) -> Result<()> {
    let mut command = inside(id, "/usr/sbin/sysctl")?;
    command.args(["-q", "-w"]).args(settings);
    checked(command)
}

fn policy(tap: Option<&Tap>) -> String {
    let mut text = String::from(
        "add table inet motor_job\n\
         add chain inet motor_job input { type filter hook input priority 0; policy drop; }\n\
         add chain inet motor_job output { type filter hook output priority 0; policy drop; }\n\
         add chain inet motor_job forward { type filter hook forward priority 0; policy drop; }\n",
    );
    if let Some(tap) = tap {
        let dev = tap.tap();
        let guest = format!("10.200.{}.2", tap.tap_index);
        text.push_str(&format!(
            "add rule inet motor_job forward iifname \"{dev}\" oifname \"{UPLINK}\" meta nfproto ipv4 ip saddr {guest} ct state new,established,related accept\n\
             add rule inet motor_job forward iifname \"{UPLINK}\" oifname \"{dev}\" meta nfproto ipv4 ip daddr {guest} ct state established,related accept\n"
        ));
    }
    text
}

fn apply(id: &str, text: &str) -> Result<()> {
    let mut child = inside(id, "/usr/sbin/nft")?
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let write = child.stdin.take().unwrap().write_all(text.as_bytes());
    let output = child.wait_with_output()?;
    write?;
    if !output.status.success() {
        return Err(Error(format!(
            "job namespace policy failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

pub(super) fn create(id: &str) -> Result<PathBuf> {
    for directory in ["/", "/run"] {
        fs::trusted(Path::new(directory), &std::fs::symlink_metadata(directory)?)?;
    }
    std::fs::create_dir_all(ROOT)?;
    fs::trusted(Path::new(ROOT), &std::fs::symlink_metadata(ROOT)?)?;
    let path = path(id)?;
    if path.try_exists()? {
        return Err(Error(
            "job network namespace requires reconciliation".into(),
        ));
    }
    ip(&["netns", "add", &name(id)?])?;
    // Firecracker needs no loopback interface. Keep it down even for recovery
    // VMs, which get a namespace but no TAP, veth or forwarding permission.
    ns_ip(id, &["link", "set", "lo", "down"])?;
    sysctl(
        id,
        &[
            "net.ipv6.conf.all.disable_ipv6=1",
            "net.ipv6.conf.default.disable_ipv6=1",
        ],
    )?;
    apply(id, &policy(None))?;
    Ok(path)
}

pub(super) fn connect(id: &str, tap: &Tap) -> Result<()> {
    let index = tap.tap_index;
    if !(1..=254).contains(&index) {
        return Err(Error("invalid job network index".into()));
    }
    let dev = tap.tap();
    // iproute2 can ignore add-time aliases. Keep both peers inside the job
    // namespace until the alias is set, then move the tagged peer to the host.
    // A crash before the move leaves no unidentifiable host interface; deleting
    // the namespace reclaims both peers even if the alias was never installed.
    ns_ip(
        id,
        &["link", "add", &dev, "type", "veth", "peer", "name", UPLINK],
    )?;
    ns_ip(id, &["link", "set", &dev, "alias", &name(id)?])?;
    let host_namespace = File::open("/proc/thread-self/ns/net")?;
    let host_path = format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        host_namespace.as_raw_fd()
    );
    ns_ip(id, &["link", "set", &dev, "netns", &host_path])?;
    std::fs::write(format!("/proc/sys/net/ipv6/conf/{dev}/disable_ipv6"), "1")?;
    ns_ip(
        id,
        &[
            "tuntap",
            "add",
            "dev",
            &dev,
            "mode",
            "tap",
            "user",
            &tap.uid.to_string(),
        ],
    )?;
    ns_ip(
        id,
        &["addr", "add", &format!("10.200.{index}.1/30"), "dev", &dev],
    )?;
    ns_ip(
        id,
        &[
            "addr",
            "add",
            &format!("10.201.{index}.2/30"),
            "dev",
            UPLINK,
        ],
    )?;
    // No NAT in the job namespace: the host's source guard, UFW route rule,
    // named counter and masquerade must all see 10.200.N.2, not the transit IP.
    apply(
        id,
        &format!("delete table inet motor_job\n{}", policy(Some(tap))),
    )?;
    sysctl(
        id,
        &[
            "net.ipv4.ip_forward=1",
            "net.ipv4.conf.all.send_redirects=0",
            "net.ipv4.conf.default.send_redirects=0",
        ],
    )?;
    ip(&["addr", "add", &format!("10.201.{index}.1/30"), "dev", &dev])?;
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")?;
    for link in [&dev, UPLINK] {
        ns_ip(id, &["link", "set", link, "mtu", "1500", "up"])?;
    }
    ip(&["link", "set", &dev, "mtu", "1500", "up"])?;
    // Connected transit routes appear only after both veth ends are up. Add
    // gateway routes afterwards so the kernel can validate each next hop.
    ns_ip(
        id,
        &[
            "route",
            "add",
            "default",
            "via",
            &format!("10.201.{index}.1"),
        ],
    )?;
    ip(&[
        "route",
        "add",
        &format!("10.200.{index}.2/32"),
        "via",
        &format!("10.201.{index}.2"),
        "dev",
        &dev,
    ])
}

fn links() -> Result<Vec<Value>> {
    let output = Command::new("/usr/sbin/ip")
        .args(["-j", "-d", "link", "show"])
        .output()?;
    if !output.status.success() {
        return Err(Error("cannot inspect job veth ownership".into()));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| Error(format!("invalid link inventory: {e}")))
}

fn owned(link: &Value, id: &str, tap: &Tap) -> Result<bool> {
    Ok(link["ifname"] == tap.tap()
        && link["linkinfo"]["info_kind"] == "veth"
        && link["ifalias"] == name(id)?)
}

fn flush_inside(id: &str, index: u8) -> Result<()> {
    in_namespace(id, move || super::conntrack::flush(index))
}

fn mounted(id: &str) -> Result<bool> {
    let path = path(id)?;
    if !path.try_exists()? {
        return Ok(false);
    }
    let file = fs::trusted_file(&path)?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // ip netns add creates an empty file before bind-mounting nsfs onto it.
    // A crash in that gap leaves a mountpoint, not a namespace to enter.
    if stat.f_type == 0x6e736673 {
        return Ok(true);
    } // NSFS_MAGIC
    if file.metadata()?.len() != 0 {
        return Err(Error("foreign job namespace mountpoint".into()));
    }
    Ok(false)
}

pub(super) fn in_namespace<T: Send + 'static>(
    id: &str,
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let file = fs::trusted_file(&path(id)?)?;
    // setns affects only this short-lived thread, never the helper's request
    // thread or an unrelated child spawned by it.
    std::thread::spawn(move || {
        if unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        operation()
    })
    .join()
    .map_err(|_| Error("job conntrack worker panicked".into()))?
}

pub(super) fn disconnect(id: &str, tap: &Tap) -> Result<()> {
    if let Some(link) = links()?.iter().find(|link| link["ifname"] == tap.tap()) {
        if owned(link, id, tap)? {
            ip(&["link", "delete", &tap.tap()])?;
        } else if link["linkinfo"]["info_kind"] != "tun" {
            return Err(Error("refusing to delete foreign job veth".into()));
        }
        // Legacy host TAPs are checked and removed by tenant_network below.
    }
    if mounted(id)? {
        // A dead VMM cannot inject packets. Lower the persistent TAP before
        // retiring namespace state, and retain the mount on any flush failure.
        let mut command = inside(id, "/usr/sbin/ip")?;
        command.args(["link", "show", "dev", &tap.tap()]);
        if command.output()?.status.success() {
            ns_ip(id, &["link", "set", &tap.tap(), "down"])?;
        }
        flush_inside(id, tap.tap_index)?;
    }
    Ok(())
}

pub(super) fn remove(id: &str) -> Result<()> {
    if mounted(id)? {
        ip(&["netns", "delete", &name(id)?])?;
    } else if path(id)?.try_exists()? {
        std::fs::remove_file(path(id)?)?;
    }
    Ok(())
}

pub(super) fn reconcile(retained: &BTreeSet<String>) -> Result<()> {
    // An alias survives a crash even if the namespace mount was lost. Do not
    // infer ownership from mtapN alone: that name also belongs to tenant TAPs.
    for link in links()? {
        let Some(id) = link["ifalias"]
            .as_str()
            .and_then(|a| a.strip_prefix(PREFIX))
        else {
            continue;
        };
        job::validate_id(id)?;
        if retained.contains(id) {
            continue;
        }
        let dev = link["ifname"]
            .as_str()
            .ok_or_else(|| Error("missing veth name".into()))?;
        let index = dev
            .strip_prefix("mtap")
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|n| (1..=254).contains(n))
            .ok_or_else(|| Error("invalid orphan job veth".into()))?;
        let tap = Tap {
            tap_index: index,
            uid: 0,
        };
        if !owned(&link, id, &tap)? {
            return Err(Error("foreign orphan job veth".into()));
        }
        disconnect(id, &tap)?;
        // No record owns this allocation. Leave its fail-closed host policy
        // for the ordinary policy reconciler, which runs after job cleanup.
        super::conntrack::flush(index)?;
        remove(id)?;
    }
    let entries = match std::fs::read_dir(ROOT) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(id) = name.to_str().and_then(|n| n.strip_prefix(PREFIX)) {
            job::validate_id(id)?;
            if !retained.contains(id) {
                if !mounted(id)? {
                    remove(id)?;
                    continue;
                }
                let mut command = inside(id, "/usr/sbin/ip")?;
                let output = command.args(["-j", "link"]).output()?;
                if !output.status.success() {
                    return Err(Error("cannot inspect orphan namespace".into()));
                }
                let links: Vec<Value> = serde_json::from_slice(&output.stdout)
                    .map_err(|e| Error(format!("invalid orphan namespace links: {e}")))?;
                for link in links {
                    if let Some(index) = link["ifname"]
                        .as_str()
                        .and_then(|n| n.strip_prefix("mtap"))
                        .and_then(|n| n.parse::<u8>().ok())
                        .filter(|n| (1..=254).contains(n))
                    {
                        flush_inside(id, index)?;
                        super::conntrack::flush(index)?;
                    }
                }
                remove(id)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "linux-root-tests"))]
pub(super) mod root_tests;
