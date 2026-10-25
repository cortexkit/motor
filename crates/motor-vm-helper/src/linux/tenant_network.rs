use crate::{tenant::Registry, Error, Result};
use std::{
    io::Write,
    process::{Command, Stdio},
};

mod lock;
mod ufw;

fn validate_uplink(config: &crate::config::Config) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let path = std::path::Path::new("/etc/motor-network.json");
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
        || metadata.len() > 4096
    {
        return Err(Error(format!(
            "unsafe network settings for uplink {}",
            config.uplink
        )));
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Network {
        uplink: String,
        public_ipv4: std::net::Ipv4Addr,
    }
    let network: Network = serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|e| Error(format!("uplink {} network settings: {e}", config.uplink)))?;
    if network.uplink != config.uplink || network.public_ipv4 != config.public_ipv4 {
        return Err(Error(format!(
            "uplink {} differs from /etc/motor-network.json",
            config.uplink
        )));
    }
    let output = Command::new("/usr/bin/timeout")
        .args([
            "--signal=TERM",
            "--kill-after=2s",
            "15s",
            "/usr/sbin/ip",
            "-j",
            "-4",
            "address",
            "show",
            "dev",
            &config.uplink,
        ])
        .output()?;
    if !output.status.success() || output.stdout.len() > 65536 {
        return Err(Error(format!(
            "uplink {} is missing or unreadable",
            config.uplink
        )));
    }
    let addresses = serde_json::from_slice(&output.stdout)
        .map_err(|e| Error(format!("uplink {} address query: {e}", config.uplink)))?;
    validate_uplink_addresses(&config.uplink, config.public_ipv4, &addresses)
}

fn validate_uplink_addresses(
    uplink: &str,
    public_ipv4: std::net::Ipv4Addr,
    addresses: &serde_json::Value,
) -> Result<()> {
    let expected = public_ipv4.to_string();
    let links = addresses
        .as_array()
        .ok_or_else(|| Error(format!("uplink {uplink} address query is not a list")))?;
    let valid = links.len() == 1
        && links[0]["ifname"] == uplink
        && links[0]["operstate"] == "UP"
        && links[0]["flags"]
            .as_array()
            .is_some_and(|flags| flags.iter().any(|flag| flag == "UP"))
        && links[0]["addr_info"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["family"] == "inet" && item["scope"] == "global" && item["local"] == expected
            })
        });
    if !valid {
        return Err(Error(format!(
            "uplink {uplink} must exist, be UP and carry public IPv4 {public_ipv4}"
        )));
    }
    Ok(())
}

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

/// The registries left once `removed` is gone. Tap indices are unique across
/// registries, so the tap identifies the removed tenant.
pub fn without(registries: Vec<Registry>, removed: &Registry) -> Vec<Registry> {
    registries
        .into_iter()
        .filter(|r| r.tap_index != removed.tap_index)
        .collect()
}

/// Deletes a removed tenant's tap. Reconciliation never deletes taps (a
/// stopped tenant keeps its own), so removal does it explicitly.
pub fn remove_tap(r: &Registry) -> Result<()> {
    let tap = r.tap();
    let exists = Command::new("/usr/sbin/ip")
        .args(["link", "show", "dev", &tap])
        .output()?
        .status
        .success();
    if exists {
        run(&["link", "delete", "dev", &tap])?;
    }
    Ok(())
}

#[cfg(test)]
fn rules(registries: &[Registry], existing: Option<&serde_json::Value>) -> Result<String> {
    policy_rules(
        "enp6s0",
        registries,
        existing,
        &std::collections::BTreeSet::new(),
    )
}

fn private_inventory() -> Result<String> {
    let source = include_str!("../../../../script/runner/motor-net.nft");
    source
        .split_once("set non_public_v4")
        .and_then(|(_, s)| s.split_once("elements"))
        .and_then(|(_, s)| s.split_once('{'))
        .and_then(|(_, s)| s.split_once('}'))
        .map(|(s, _)| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .ok_or_else(|| Error("versioned non-public inventory missing".into()))
}

fn object_names(
    existing: Option<&serde_json::Value>,
    kind: &str,
) -> Result<std::collections::BTreeSet<String>> {
    let Some(value) = existing else {
        return Ok(Default::default());
    };
    let objects = value["nftables"]
        .as_array()
        .ok_or_else(|| Error("missing nft objects".into()))?;
    Ok(objects
        .iter()
        .filter_map(|item| {
            let object = &item[kind];
            (object["family"] == "inet" && object["table"] == "motor_tenant")
                .then(|| object["name"].as_str().map(str::to_owned))
                .flatten()
        })
        .collect())
}

fn replace_chain(
    text: &mut String,
    chains: &std::collections::BTreeSet<String>,
    name: &str,
    attributes: &str,
) {
    if chains.contains(name) {
        text.push_str(&format!(
            "flush chain inet motor_tenant {name}\ndelete chain inet motor_tenant {name}\n"
        ));
    }
    text.push_str(&format!(
        "add chain inet motor_tenant {name} {{ {attributes} }}\n"
    ));
}

fn set_members(table: &str, name: &str, taps: &std::collections::BTreeSet<String>) -> String {
    let mut text = format!("flush set inet {table} {name}\n");
    if !taps.is_empty() {
        text.push_str(&format!(
            "add element inet {table} {name} {{ {} }}\n",
            taps.iter()
                .map(|tap| format!("\"{tap}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    text
}

fn prelude(
    registries: &[Registry],
    existing: Option<&serde_json::Value>,
    closed: &std::collections::BTreeSet<String>,
) -> Result<String> {
    let desired: std::collections::BTreeSet<_> = registries.iter().map(Registry::tap).collect();
    if desired.len() != registries.len() || !closed.is_subset(&desired) {
        return Err(Error("duplicate or invalid tenant tap set".into()));
    }
    let sets = object_names(existing, "set")?;
    let chains = object_names(existing, "chain")?;
    let mut text = String::new();
    if existing.is_none() {
        text.push_str("add table inet motor_tenant\n");
    }
    for name in ["tenant_taps", "closed_taps"] {
        if !sets.contains(name) {
            text.push_str(&format!(
                "add set inet motor_tenant {name} {{ type ifname; }}\n"
            ));
        }
    }
    text.push_str(&set_members("motor_tenant", "tenant_taps", &desired));
    text.push_str(&set_members("motor_tenant", "closed_taps", closed));
    if !sets.contains("non_public_v4") {
        text.push_str(
            "add set inet motor_tenant non_public_v4 { type ipv4_addr; flags interval; }\n",
        );
    }
    text.push_str(&format!("flush set inet motor_tenant non_public_v4\nadd element inet motor_tenant non_public_v4 {{ {} }}\n",private_inventory()?));
    replace_chain(
        &mut text,
        &chains,
        "tenant_guard_input",
        "type filter hook input priority -20; policy accept;",
    );
    text.push_str("add rule inet motor_tenant tenant_guard_input iifname \"mtap*\" counter drop\n");
    replace_chain(
        &mut text,
        &chains,
        "tenant_guard_forward",
        "type filter hook forward priority -20; policy accept;",
    );
    for interface in ["iifname", "oifname"] {
        text.push_str(&format!("add rule inet motor_tenant tenant_guard_forward {interface} @closed_taps counter drop\n"));
    }
    for interface in ["iifname", "oifname"] {
        text.push_str(&format!("add rule inet motor_tenant tenant_guard_forward {interface} \"mtap*\" {interface} != @tenant_taps counter drop\n"));
    }
    Ok(text)
}

fn policy_rules(
    uplink: &str,
    registries: &[Registry],
    existing: Option<&serde_json::Value>,
    closed: &std::collections::BTreeSet<String>,
) -> Result<String> {
    if !crate::config::valid_uplink(uplink) {
        return Err(Error(format!("invalid uplink {uplink}")));
    }
    // Object presence alone cannot prove that an older forward chain has the
    // current denies. Replace rule chains atomically, retaining named counters.
    if existing.is_some_and(|value| {
        value["nftables"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item.as_object().is_none_or(|object| {
                    object.len() != 1
                        || object.keys().any(|kind| {
                            !["metainfo", "table", "counter", "chain", "set", "rule"]
                                .contains(&kind.as_str())
                        })
                })
            })
        })
    }) {
        return Err(Error(
            "foreign objects in helper-owned tenant policy".into(),
        ));
    }
    let counters = object_names(existing, "counter")?;
    let chains = object_names(existing, "chain")?;
    let sets = object_names(existing, "set")?;
    if counters
        .iter()
        .any(|name| managed_tap(name, "_egress").is_none())
        || chains.iter().any(|name| {
            !["tenant_guard_input", "tenant_guard_forward"].contains(&name.as_str())
                && !["_input", "_forward", "_nat"]
                    .iter()
                    .any(|suffix| managed_tap(name, suffix).is_some())
        })
        || sets
            .iter()
            .any(|name| !["tenant_taps", "closed_taps", "non_public_v4"].contains(&name.as_str()))
    {
        return Err(Error(
            "foreign objects in helper-owned tenant policy".into(),
        ));
    }
    let desired: std::collections::BTreeSet<_> = registries.iter().map(Registry::tap).collect();
    let mut text = prelude(registries, existing, closed)?;
    let mut managed = std::collections::BTreeSet::new();
    for name in &counters {
        if let Some(tap) = managed_tap(name, "_egress") {
            managed.insert(tap);
        }
    }
    for name in &chains {
        for suffix in ["_input", "_forward", "_nat"] {
            if let Some(tap) = managed_tap(name, suffix) {
                managed.insert(tap);
            }
        }
    }
    for tap in managed.difference(&desired) {
        for suffix in ["input", "forward", "nat"] {
            let name = format!("tenant_{tap}_{suffix}");
            if chains.contains(&name) {
                text.push_str(&format!(
                    "flush chain inet motor_tenant {name}\ndelete chain inet motor_tenant {name}\n"
                ));
            }
        }
        let name = format!("tenant_{tap}_egress");
        if counters.contains(&name) {
            text.push_str(&format!("delete counter inet motor_tenant {name}\n"));
        }
    }
    for r in registries {
        let tap = r.tap();
        let counter = format!("tenant_{tap}_egress");
        let present = usize::from(counters.contains(&counter))
            + ["input", "forward", "nat"]
                .iter()
                .filter(|suffix| chains.contains(&format!("tenant_{tap}_{suffix}")))
                .count();
        if present != 0 && present != 4 {
            return Err(Error(format!("incomplete tenant firewall objects: {tap}")));
        }
        if present == 0 {
            text.push_str(&format!("add counter inet motor_tenant {counter} {{ }}\n"));
        }
        let input = format!("tenant_{tap}_input");
        let forward = format!("tenant_{tap}_forward");
        let nat = format!("tenant_{tap}_nat");
        replace_chain(
            &mut text,
            &chains,
            &input,
            "type filter hook input priority -10; policy accept;",
        );
        text.push_str(&format!("add rule inet motor_tenant {input} iifname \"{tap}\" counter drop comment \"tenant_host\"\n"));
        replace_chain(
            &mut text,
            &chains,
            &forward,
            "type filter hook forward priority -10; policy accept;",
        );
        for (predicate, comment) in [
            ("oifname \"mtap*\"", "tenant_taps"),
            ("meta nfproto ipv6", "tenant_ipv6"),
            ("fib daddr type local", "tenant_host"),
            ("ip daddr @non_public_v4", "tenant_private"),
        ] {
            text.push_str(&format!("add rule inet motor_tenant {forward} iifname \"{tap}\" {predicate} counter drop comment \"{comment}\"\n"));
        }
        text.push_str(&format!("add rule inet motor_tenant {forward} iifname \"{tap}\" ip saddr != 10.200.{}.2 counter drop comment \"tenant_source\"\n",r.tap_index));
        text.push_str(&format!("add rule inet motor_tenant {forward} iifname \"{tap}\" oifname \"{uplink}\" meta nfproto ipv4 ip saddr 10.200.{}.2 ct state new,established,related counter name {counter} accept comment \"tenant_public\"\n",r.tap_index));
        text.push_str(&format!("add rule inet motor_tenant {forward} oifname \"{tap}\" iifname \"{uplink}\" meta nfproto ipv4 ip daddr 10.200.{}.2 ct state established,related counter accept\n",r.tap_index));
        text.push_str(&format!("add rule inet motor_tenant {forward} oifname \"{tap}\" counter drop comment \"tenant_inbound\"\nadd rule inet motor_tenant {forward} iifname \"{tap}\" counter drop comment \"tenant_unmatched\"\n"));
        replace_chain(
            &mut text,
            &chains,
            &nat,
            "type nat hook postrouting priority srcnat; policy accept;",
        );
        text.push_str(&format!("add rule inet motor_tenant {nat} iifname \"{tap}\" oifname \"{uplink}\" ip saddr 10.200.{}.2 counter masquerade\n",r.tap_index));
    }
    Ok(text)
}

fn managed_tap(name: &str, suffix: &str) -> Option<String> {
    let digits = name.strip_prefix("tenant_mtap")?.strip_suffix(suffix)?;
    let index: u8 = digits.parse().ok()?;
    (index > 0 && index < 255 && digits == index.to_string()).then(|| format!("mtap{index}"))
}

pub fn install(registries: &[Registry], config: &crate::config::Config) -> Result<()> {
    let _lock = lock::acquire()?;
    validate_uplink(config)?;
    let uplink = config.uplink.as_str();
    let tables = nft_json(&["-j", "list", "tables"])?;
    let table = |name: &str| -> Result<Option<serde_json::Value>> {
        let exists = tables["nftables"]
            .as_array()
            .ok_or_else(|| Error("missing nft tables".into()))?
            .iter()
            .any(|item| item["table"]["family"] == "inet" && item["table"]["name"] == name);
        if exists {
            Ok(Some(nft_json(&["-j", "list", "table", "inet", name])?))
        } else {
            Ok(None)
        }
    };
    let existing = table("motor_tenant")?;
    let motor_net = table("motor_net")?;
    let inspected = if registries.is_empty() && !std::path::Path::new("/usr/sbin/ufw").exists() {
        Ok(ufw::State::default())
    } else {
        ufw::inspect(uplink)
    };
    reconcile_policy(
        uplink,
        registries,
        existing.as_ref(),
        motor_net.as_ref(),
        inspected,
        apply,
        |prior| {
            if !registries.is_empty() || std::path::Path::new("/usr/sbin/ufw").exists() {
                ufw::reconcile(registries, prior, uplink)?;
            }
            Ok(())
        },
    )
}

fn reconcile_policy(
    uplink: &str,
    registries: &[Registry],
    existing: Option<&serde_json::Value>,
    motor_net: Option<&serde_json::Value>,
    inspected: Result<ufw::State>,
    mut apply: impl FnMut(&str) -> Result<()>,
    reconcile_ufw: impl FnOnce(&ufw::State) -> Result<()>,
) -> Result<()> {
    use std::collections::BTreeSet;
    let desired: BTreeSet<_> = registries.iter().map(Registry::tap).collect();
    let ready: BTreeSet<_> = inspected
        .as_ref()
        .map(|state| {
            registries
                .iter()
                .filter(|r| state.ready.contains(&r.tap_index))
                .map(Registry::tap)
                .collect()
        })
        .unwrap_or_default();
    let closed = desired.difference(&ready).cloned().collect();
    let transaction = policy_rules(uplink, registries, existing, &closed).and_then(|mut text| {
        text.push_str(&motor_net_members(motor_net, &ready)?);
        Ok(text)
    });
    match transaction {
        Ok(text) => apply(&text)?,
        Err(error) => {
            // Guards close every registered TAP even if a legacy/partial policy
            // cannot be upgraded. Never publish exemptions without protection.
            apply(&prelude(registries, existing, &desired)?)?;
            return Err(error);
        }
    }
    let prior = inspected?;
    if let Err(error) = reconcile_ufw(&prior) {
        if !ready.is_empty() {
            let mut fence = set_members("motor_tenant", "closed_taps", &desired);
            fence.push_str(&motor_net_members(motor_net, &BTreeSet::new())?);
            apply(&fence)?;
        }
        return Err(error);
    }
    // Only live, comment-owned UFW grants may release the per-TAP gate. An
    // earlier failure leaves new/mismatched tenants closed and unready.
    let mut open = set_members("motor_tenant", "closed_taps", &BTreeSet::new());
    open.push_str(&motor_net_members(motor_net, &desired)?);
    apply(&open)
}

fn motor_net_members(
    existing: Option<&serde_json::Value>,
    taps: &std::collections::BTreeSet<String>,
) -> Result<String> {
    let Some(value) = existing else {
        return Ok(String::new());
    };
    let items = value["nftables"]
        .as_array()
        .ok_or_else(|| Error("missing motor_net objects".into()))?;
    let set = items
        .iter()
        .map(|v| &v["set"])
        .find(|v| v["name"] == "tenant_taps");
    let Some(set) = set else {
        // Legacy policy already blocks TAPs. Empty delegation needs no new
        // grant, so standalone non-tenant helper users remain compatible.
        if taps.is_empty() {
            return Ok(String::new());
        }
        return Err(Error(
            "legacy motor_net policy: run updated install-job-network.py --apply".into(),
        ));
    };
    if set["type"] != "ifname"
        || set
            .get("flags")
            .is_some_and(|flags| flags.as_array().is_none_or(|flags| !flags.is_empty()))
    {
        return Err(Error("unsafe motor_net tenant_taps schema".into()));
    }
    let expression = value["nftables"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .filter_map(|v| {
                    let rule = &v["rule"];
                    (rule["chain"] == "forward")
                        .then(|| rule["expr"].as_array())
                        .flatten()
                })
                .next_back()
        })
        .ok_or_else(|| Error("motor_net forward catch-all missing".into()))?;
    if expression.len() != 6
        || expression[5] != serde_json::json!({"drop":null})
        || expression[4]
            .as_object()
            .is_none_or(|v| v.len() != 1 || !v.contains_key("counter"))
    {
        return Err(Error(
            "motor_net tenant delegation catch-all differs".into(),
        ));
    }
    for (at, key, right) in [
        (0, "iifname", "mnet-*"),
        (1, "oifname", "mnet-*"),
        (2, "iifname", "@tenant_taps"),
        (3, "oifname", "@tenant_taps"),
    ] {
        if expression[at]
            != serde_json::json!({"match":{"op":"!=","left":{"meta":{"key":key}},"right":right}})
        {
            return Err(Error(
                "motor_net tenant delegation catch-all differs".into(),
            ));
        }
    }
    Ok(set_members("motor_net", "tenant_taps", taps))
}

fn apply(transaction: &str) -> Result<()> {
    if transaction.is_empty() {
        return Ok(());
    }
    let mut process = Command::new("/usr/sbin/nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    process
        .stdin
        .take()
        .unwrap()
        .write_all(transaction.as_bytes())?;
    let output = process.wait_with_output()?;
    if !output.status.success() {
        return Err(Error(format!(
            "tenant nft: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

pub fn egress_bytes(registry: &Registry) -> Result<u64> {
    let counter = format!("tenant_{}_egress", registry.tap());
    let value = nft_json(&["-j", "list", "counter", "inet", "motor_tenant", &counter])?;
    forward_bytes(&value, &registry.tap())
}

fn nft_json(args: &[&str]) -> Result<serde_json::Value> {
    let output = Command::new("/usr/sbin/nft").args(args).output()?;
    if !output.status.success() {
        return Err(Error(format!(
            "tenant nft query: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| Error(format!("invalid nft JSON: {e}")))
}

fn forward_bytes(value: &serde_json::Value, tap: &str) -> Result<u64> {
    let name = format!("tenant_{tap}_egress");
    let rules = value["nftables"]
        .as_array()
        .ok_or_else(|| Error("missing nft rules".into()))?;
    let mut found = None;
    for item in rules {
        let counter = &item["counter"];
        if counter["family"] == "inet"
            && counter["table"] == "motor_tenant"
            && counter["name"] == name
        {
            let bytes = counter["bytes"]
                .as_u64()
                .ok_or_else(|| Error("missing tenant byte counter".into()))?;
            if found.replace(bytes).is_some() {
                return Err(Error("duplicate tenant byte counter".into()));
            }
        }
    }
    found.ok_or_else(|| Error("tenant forward counter unavailable".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_uplink_preserves_the_original_enp6s0_nft_and_nat_bytes() {
        // Captured from the policy renderer before the uplink became configurable,
        // for one tenant on TAP 1. The fixture is not generated by this test.
        let expected = include_str!("../../fixtures/tenant-enp6s0.nft");
        let registry = crate::tenant::tests::registry("a");
        let closed = std::collections::BTreeSet::new();
        assert_eq!(
            policy_rules("enp6s0", &[registry.clone()], None, &closed).unwrap(),
            expected
        );
        assert_eq!(
            policy_rules("eno1", &[registry], None, &closed).unwrap(),
            expected.replace("enp6s0", "eno1")
        );
    }

    #[test]
    fn uplink_must_exist_be_up_and_carry_the_configured_public_ipv4() {
        let ip = "8.8.8.8".parse().unwrap();
        let good = serde_json::json!([{"ifname":"eno1", "operstate":"UP", "flags":["UP", "LOWER_UP"],
            "addr_info":[{"family":"inet", "scope":"global", "local":"8.8.8.8"}]}]);
        validate_uplink_addresses("eno1", ip, &good).unwrap();
        for broken in [
            serde_json::json!([]),
            {
                let mut value = good.clone();
                value[0]["operstate"] = "DOWN".into();
                value
            },
            {
                let mut value = good.clone();
                value[0]["addr_info"][0]["local"] = "10.0.0.1".into();
                value
            },
            {
                let mut value = good.clone();
                value[0]["flags"] = serde_json::json!([]);
                value
            },
        ] {
            assert!(validate_uplink_addresses("eno1", ip, &broken)
                .unwrap_err()
                .0
                .contains("uplink eno1"));
        }
    }

    #[test]
    fn tenant_forward_bytes_select_only_own_forward_rule_not_host_or_nat() {
        let value = serde_json::json!({"nftables":[
            {"counter":{"family":"inet","table":"another_table","name":"tenant_mtap1_egress","bytes":999}},
            {"counter":{"family":"inet","table":"motor_tenant","name":"tenant_mtap2_egress","bytes":888}},
            {"rule":{"family":"inet","table":"motor_tenant","chain":"input","expr":[{"counter":{"bytes":777}}]}},
            {"counter":{"family":"inet","table":"motor_tenant","name":"tenant_mtap1_egress","bytes":123}}
        ]});
        assert_eq!(forward_bytes(&value, "mtap1").unwrap(), 123);
        assert!(forward_bytes(&value, "mtap3").is_err());
    }
    #[test]
    fn hosted_firewall_drops_before_return_traffic_and_snats_each_subnet() {
        let r = crate::tenant::tests::registry("a");
        let mut second = crate::tenant::tests::registry("b");
        second.tap_index = 2;
        let text = rules(&[r, second], None).unwrap();
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
        assert!(text.contains("counter name tenant_mtap1_egress accept comment \"tenant_public\""));
        assert!(text.contains("counter name tenant_mtap2_egress accept comment \"tenant_public\""));
        assert!(text.contains("ct state new,established,related"));
        assert!(text.contains("counter drop comment \"tenant_unmatched\""));
    }

    #[derive(Default)]
    struct NftObjects {
        counters: std::collections::BTreeMap<String, u64>,
        chains: std::collections::BTreeSet<String>,
        sets: std::collections::BTreeSet<String>,
    }
    impl NftObjects {
        fn snapshot(&self) -> serde_json::Value {
            let mut objects = Vec::new();
            for (name, bytes) in &self.counters {
                objects.push(serde_json::json!({"counter":{"family":"inet","table":"motor_tenant","name":name,"bytes":bytes}}));
            }
            for name in &self.chains {
                objects.push(serde_json::json!({"chain":{"family":"inet","table":"motor_tenant","name":name}}));
            }
            for name in &self.sets {
                objects.push(
                    serde_json::json!({"set":{"family":"inet","table":"motor_tenant","name":name}}),
                );
            }
            serde_json::json!({"nftables":objects})
        }
        fn apply(&mut self, transaction: &str) {
            // Model object lifetime, not packet filtering. Traffic increments
            // below are independent of the generated reconciliation commands.
            for line in transaction.lines() {
                let words: Vec<_> = line.split_whitespace().collect();
                assert!(!(words[0] == "delete" && words[1] == "table"));
                match (words[0], words[1]) {
                    ("add", "table") => {}
                    ("add", "set") => {
                        assert!(self.sets.insert(words[4].into()));
                    }
                    ("flush", "set") | ("add", "element") => {
                        assert!(self.sets.contains(words[4]));
                    }
                    ("add", "counter") => {
                        assert!(
                            self.counters.insert(words[4].into(), 0).is_none(),
                            "live counter recreated"
                        );
                    }
                    ("delete", "counter") => {
                        assert!(self.counters.remove(words[4]).is_some());
                    }
                    ("add", "chain") => {
                        assert!(self.chains.insert(words[4].into()));
                    }
                    ("flush", "chain") => {
                        assert!(self.chains.contains(words[4]));
                    }
                    ("delete", "chain") => {
                        assert!(self.chains.remove(words[4]));
                    }
                    ("add", "rule") => {
                        assert!(self.chains.contains(words[4]));
                        if let Some(at) = words.windows(2).position(|w| w == ["counter", "name"]) {
                            assert!(self.counters.contains_key(words[at + 2]));
                        }
                    }
                    _ => panic!("unexpected transaction line: {line}"),
                }
            }
        }
        fn traffic(&mut self, tap: &str, bytes: u64) {
            *self
                .counters
                .get_mut(&format!("tenant_{tap}_egress"))
                .expect("live tenant counter") += bytes;
        }
    }

    #[test]
    fn tenant_a_counter_keeps_increasing_across_tenant_b_enable_disable_and_helper_reconcile() {
        let a = crate::tenant::tests::registry("a");
        let mut b = crate::tenant::tests::registry("b");
        b.tap_index = 2;
        let mut nft = NftObjects::default();
        nft.apply(&rules(std::slice::from_ref(&a), None).unwrap());
        nft.traffic("mtap1", 100);
        let enable_b = rules(&[a.clone(), b], Some(&nft.snapshot())).unwrap();
        assert!(!enable_b.contains("add counter inet motor_tenant tenant_mtap1_egress"));
        assert!(!enable_b.contains("table inet"));
        nft.apply(&enable_b);
        nft.traffic("mtap1", 25);
        nft.traffic("mtap2", 70);
        assert_eq!(forward_bytes(&nft.snapshot(), "mtap1").unwrap(), 125);
        let disable_b = rules(std::slice::from_ref(&a), Some(&nft.snapshot())).unwrap();
        assert!(!disable_b.contains("add counter inet motor_tenant tenant_mtap1_egress"));
        assert!(!disable_b.contains("table inet"));
        nft.apply(&disable_b);
        nft.traffic("mtap1", 30);
        assert_eq!(forward_bytes(&nft.snapshot(), "mtap1").unwrap(), 155);
        assert!(forward_bytes(&nft.snapshot(), "mtap2").is_err());
        let reconcile = rules(&[a], Some(&nft.snapshot())).unwrap();
        assert!(!reconcile.contains("add counter inet motor_tenant tenant_mtap1_egress"));
        assert!(!reconcile.contains("delete counter inet motor_tenant tenant_mtap1_egress"));
        nft.apply(&reconcile);
        nft.traffic("mtap1", 10);
        assert_eq!(forward_bytes(&nft.snapshot(), "mtap1").unwrap(), 165);
    }

    #[test]
    fn removing_a_tenant_drops_only_its_firewall_objects_and_keeps_other_counters() {
        let a = crate::tenant::tests::registry("a");
        let mut b = crate::tenant::tests::registry("b");
        b.tap_index = 2;
        let mut nft = NftObjects::default();
        nft.apply(&rules(&[a.clone(), b.clone()], None).unwrap());
        nft.traffic("mtap1", 40);
        nft.traffic("mtap2", 90);
        let remaining = without(vec![a.clone(), b.clone()], &b);
        assert_eq!(
            remaining.iter().map(|r| r.tap_index).collect::<Vec<_>>(),
            [1]
        );
        nft.apply(&rules(&remaining, Some(&nft.snapshot())).unwrap());
        for suffix in ["input", "forward", "nat"] {
            assert!(!nft.chains.contains(&format!("tenant_mtap2_{suffix}")));
            assert!(nft.chains.contains(&format!("tenant_mtap1_{suffix}")));
        }
        assert!(forward_bytes(&nft.snapshot(), "mtap2").is_err());
        nft.traffic("mtap1", 2);
        assert_eq!(forward_bytes(&nft.snapshot(), "mtap1").unwrap(), 42);
    }

    #[test]
    fn incomplete_live_objects_are_fenced_instead_of_resetting_the_counter() {
        let a = crate::tenant::tests::registry("a");
        let existing = serde_json::json!({"nftables":[{"counter":{"family":"inet","table":"motor_tenant","name":"tenant_mtap1_egress","bytes":123}}]});
        assert!(rules(&[a], Some(&existing)).is_err());
        assert!(managed_tap(
            "tenant_mtap1_egress; delete table inet motor_tenant",
            "_egress"
        )
        .is_none());
    }
}

#[cfg(test)]
mod reconciliation_tests {
    use super::*;
    #[test]
    fn ufw_failure_keeps_new_tenant_gate_closed_and_returns_unready() {
        let registry = crate::tenant::tests::registry("a");
        let writes = std::cell::RefCell::new(Vec::<String>::new());
        let error = reconcile_policy(
            "enp6s0",
            &[registry],
            None,
            None,
            Ok(ufw::State::default()),
            |text| {
                writes.borrow_mut().push(text.into());
                Ok(())
            },
            |_| {
                assert!(writes.borrow()[0]
                    .contains("add element inet motor_tenant closed_taps { \"mtap1\" }"));
                Err(Error("simulated UFW failure".into()))
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("simulated UFW failure"));
        assert_eq!(
            writes.borrow().len(),
            1,
            "must not publish the final open transaction"
        );
    }
    #[test]
    fn guarded_policy_precedes_ufw_and_only_success_releases_the_tenant_gate() {
        let registry = crate::tenant::tests::registry("a");
        let writes = std::cell::RefCell::new(Vec::<String>::new());
        reconcile_policy(
            "enp6s0",
            &[registry],
            None,
            None,
            Ok(ufw::State::default()),
            |text| {
                writes.borrow_mut().push(text.into());
                Ok(())
            },
            |_| {
                assert!(writes.borrow()[0].contains("closed_taps { \"mtap1\" }"));
                assert!(writes.borrow()[0].contains("tenant_unmatched"));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(writes.borrow().len(), 2);
        assert_eq!(
            writes.borrow()[1],
            "flush set inet motor_tenant closed_taps\n"
        );
    }
}

#[cfg(test)]
mod ufw_failure_fence_tests {
    use super::*;
    #[test]
    fn ufw_failure_revokes_previously_ready_tenant_egress_before_returning_error() {
        let registry = crate::tenant::tests::registry("a");
        let mut writes = Vec::<String>::new();
        let state = ufw::State {
            owned: [1].into(),
            ready: [1].into(),
        };
        assert!(reconcile_policy(
            "enp6s0",
            &[registry],
            None,
            None,
            Ok(state),
            |text| {
                writes.push(text.into());
                Ok(())
            },
            |_| Err(Error("UFW failure".into()))
        )
        .is_err());
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1], "flush set inet motor_tenant closed_taps\nadd element inet motor_tenant closed_taps { \"mtap1\" }\n");
    }
}

#[cfg(test)]
mod delegation_schema_tests {
    use super::*;
    #[test]
    fn motor_net_delegation_requires_exact_set_and_final_guard_shape() {
        let document: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../script/runner/test-fixtures/nft-motor-net-v2-added-set.json"
        ))
        .unwrap();
        let taps = ["mtap1".to_owned()].into();
        assert_eq!(motor_net_members(Some(&document), &taps).unwrap(), "flush set inet motor_net tenant_taps\nadd element inet motor_net tenant_taps { \"mtap1\" }\n");
        let mut widened = document.clone();
        let objects = widened["nftables"].as_array_mut().unwrap();
        let expression = objects
            .iter_mut()
            .filter_map(|v| v.get_mut("rule"))
            .filter_map(|rule| {
                if rule["chain"] == "forward" {
                    rule["expr"].as_array_mut()
                } else {
                    None
                }
            })
            .next_back()
            .unwrap();
        expression[2]["match"]["right"] = serde_json::json!("mtap*");
        assert!(motor_net_members(Some(&widened), &taps).is_err());
        assert_eq!(motor_net_members(None, &taps).unwrap(), "");
        let legacy: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../script/runner/test-fixtures/nft-motor-net-1.1.6.json"
        ))
        .unwrap();
        assert!(motor_net_members(Some(&legacy), &taps).is_err());
        assert_eq!(
            motor_net_members(Some(&legacy), &Default::default()).unwrap(),
            ""
        );
    }
}

#[cfg(test)]
mod legacy_migration_tests {
    use super::*;
    #[test]
    fn legacy_policy_upgrade_rewrites_forward_guards_without_recreating_named_counter() {
        let document: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../script/runner/test-fixtures/nft-motor-tenant-v1-capture.json"
        ))
        .unwrap();
        let registry = crate::tenant::tests::registry("a");
        let text = policy_rules(
            "enp6s0",
            &[registry],
            Some(&document),
            &["mtap1".into()].into(),
        )
        .unwrap();
        assert!(text.contains("tenant_public"));
        assert!(text.contains("tenant_unmatched"));
        assert!(text.contains("closed_taps { \"mtap1\" }"));
        assert!(!text.contains("add counter inet motor_tenant tenant_mtap1_egress"));
        assert!(!text.contains("delete counter inet motor_tenant tenant_mtap1_egress"));
        assert_eq!(forward_bytes(&document, "mtap1").unwrap(), 0);
    }
}
