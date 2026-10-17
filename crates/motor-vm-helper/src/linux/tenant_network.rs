use crate::{tenant::Registry, Error, Result};
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn run(args: &[&str]) -> Result<()> {
    let output = Command::new("/usr/sbin/ip").args(args).output()?;
    if !output.status.success() {
        return Err(Error(format!(
            "tenant ip: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}
pub fn ensure(r: &Registry, uid: u32) -> Result<()> {
    let tap = r.tap();
    let exists = Command::new("/usr/sbin/ip")
        .args(["link", "show", "dev", &tap])
        .output()?
        .status
        .success();
    if !exists {
        run(&[
            "tuntap",
            "add",
            "dev",
            &tap,
            "mode",
            "tap",
            "user",
            &uid.to_string(),
        ])?;
    }
    run(&[
        "address",
        "replace",
        &format!("10.200.{}.1/30", r.tap_index),
        "dev",
        &tap,
    ])?;
    run(&["link", "set", "dev", &tap, "up"])?;
    // IPv6 has no guest identity or egress contract and is blocked independently
    // of the guest firewall. Never leave an automatically configured link-local.
    std::fs::write(format!("/proc/sys/net/ipv6/conf/{tap}/disable_ipv6"), "1")?;
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")?;
    Ok(())
}

/// A single atomic transaction replaces only our table. Rebuild from the entire
/// registry so enabling one tenant cannot remove another tenant's protection.
pub fn rules(registries: &[Registry], exists: bool) -> String {
    let mut text = String::new();
    if exists {
        text.push_str("delete table inet motor_tenant\n");
    }
    text.push_str("table inet motor_tenant {\n set taps { type ifname; ");
    if !registries.is_empty() {
        text.push_str("elements = { ");
        text.push_str(
            &registries
                .iter()
                .map(|r| format!("\"{}\"", r.tap()))
                .collect::<Vec<_>>()
                .join(", "),
        );
        text.push_str(" }; ");
    }
    text.push_str("}\n chain input { type filter hook input priority -10; policy accept; iifname @taps counter drop comment \"tenant_host\"; }\n");
    text.push_str(" chain forward { type filter hook forward priority -10; policy accept;\n iifname @taps oifname @taps counter drop comment \"tenant_taps\"\n iifname @taps meta nfproto ipv6 counter drop comment \"tenant_ipv6\"\n iifname @taps fib daddr type local counter drop comment \"tenant_host\"\n");
    text.push_str(" iifname @taps ip daddr { 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.0.0.0/24, 192.0.2.0/24, 192.168.0.0/16, 198.18.0.0/15, 198.51.100.0/24, 203.0.113.0/24, 224.0.0.0/4, 240.0.0.0/4 } counter drop comment \"tenant_private\"\n");
    for r in registries {
        text.push_str(&format!(
            " iifname \"{}\" ip saddr != 10.200.{}.2 counter drop comment \"tenant_source\"\n",
            r.tap(),
            r.tap_index
        ));
    }
    text.push_str(" oifname @taps ct state established,related counter accept\n oifname @taps counter drop comment \"tenant_inbound\"\n }\n chain postrouting { type nat hook postrouting priority srcnat; policy accept;\n");
    for r in registries {
        text.push_str(&format!(
            " iifname \"{}\" ip saddr 10.200.{}.2 counter masquerade\n",
            r.tap(),
            r.tap_index
        ));
    }
    text.push_str(" }\n}\n");
    text
}
pub fn install(registries: &[Registry]) -> Result<()> {
    let exists = Command::new("/usr/sbin/nft")
        .args(["list", "table", "inet", "motor_tenant"])
        .output()?
        .status
        .success();
    let mut process = Command::new("/usr/sbin/nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    process
        .stdin
        .take()
        .unwrap()
        .write_all(rules(registries, exists).as_bytes())?;
    let output = process.wait_with_output()?;
    if !output.status.success() {
        return Err(Error(format!(
            "tenant nft: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hosted_firewall_drops_before_return_traffic_and_snats_each_subnet() {
        let r = crate::tenant::tests::registry("a");
        let mut second = crate::tenant::tests::registry("b");
        second.tap_index = 2;
        let text = rules(&[r, second], true);
        for name in [
            "tenant_host",
            "tenant_taps",
            "tenant_private",
            "tenant_inbound",
            "tenant_ipv6",
            "tenant_source",
        ] {
            assert!(
                text.contains(&format!("counter drop comment \"{name}\"")),
                "{name}"
            );
        }
        assert!(text.find("tenant_private").unwrap() < text.find("ct state").unwrap());
        assert!(text.contains("10.200.1.2 counter masquerade"));
        assert!(text.contains("10.200.2.2 counter masquerade"));
    }
}
