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

/// Reconcile only tenant-owned objects in one transaction. A live tenant's
/// named counter is never recreated when another tenant is enabled or disabled.
fn rules(registries: &[Registry], existing: Option<&serde_json::Value>) -> Result<String> {
    use std::collections::BTreeSet;
    let mut counters = BTreeSet::new();
    let mut chains = BTreeSet::new();
    if let Some(value) = existing {
        let objects = value["nftables"]
            .as_array()
            .ok_or_else(|| Error("missing nft objects".into()))?;
        for item in objects {
            for (kind, names) in [("counter", &mut counters), ("chain", &mut chains)] {
                let object = &item[kind];
                if object["family"] == "inet" && object["table"] == "motor_tenant" {
                    if let Some(name) = object["name"].as_str() {
                        names.insert(name.to_owned());
                    }
                }
            }
        }
    }
    let desired: BTreeSet<_> = registries.iter().map(Registry::tap).collect();
    if desired.len() != registries.len() {
        return Err(Error("duplicate tenant tap".into()));
    }
    let mut text = String::new();
    if existing.is_none() {
        text.push_str("add table inet motor_tenant\n");
    }
    let mut managed = BTreeSet::new();
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
            let chain = format!("tenant_{tap}_{suffix}");
            if chains.contains(&chain) {
                text.push_str(&format!("flush chain inet motor_tenant {chain}\ndelete chain inet motor_tenant {chain}\n"));
            }
        }
        let counter = format!("tenant_{tap}_egress");
        if counters.contains(&counter) {
            text.push_str(&format!("delete counter inet motor_tenant {counter}\n"));
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
        if present == 4 {
            continue;
        }
        if present != 0 {
            return Err(Error(format!("incomplete tenant firewall objects: {tap}")));
        }
        text.push_str(&format!("add counter inet motor_tenant {counter} {{ }}\n"));
        let input = format!("tenant_{tap}_input");
        let forward = format!("tenant_{tap}_forward");
        let nat = format!("tenant_{tap}_nat");
        text.push_str(&format!("add chain inet motor_tenant {input} {{ type filter hook input priority -10; policy accept; }}\n"));
        text.push_str(&format!("add rule inet motor_tenant {input} iifname \"{tap}\" counter drop comment \"tenant_host\"\n"));
        text.push_str(&format!("add chain inet motor_tenant {forward} {{ type filter hook forward priority -10; policy accept; }}\n"));
        for (predicate,comment) in [
            ("oifname \"mtap*\"", "tenant_taps"),
            ("meta nfproto ipv6", "tenant_ipv6"),
            ("fib daddr type local", "tenant_host"),
            ("ip daddr { 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.0.0.0/24, 192.0.2.0/24, 192.168.0.0/16, 198.18.0.0/15, 198.51.100.0/24, 203.0.113.0/24, 224.0.0.0/4, 240.0.0.0/4 }", "tenant_private"),
        ] {
            text.push_str(&format!("add rule inet motor_tenant {forward} iifname \"{tap}\" {predicate} counter drop comment \"{comment}\"\n"));
        }
        text.push_str(&format!("add rule inet motor_tenant {forward} iifname \"{tap}\" ip saddr != 10.200.{}.2 counter drop comment \"tenant_source\"\n",r.tap_index));
        text.push_str(&format!(
            "add rule inet motor_tenant {forward} iifname \"{tap}\" counter name {counter}\n"
        ));
        text.push_str(&format!("add rule inet motor_tenant {forward} oifname \"{tap}\" ct state established,related counter accept\n"));
        text.push_str(&format!("add rule inet motor_tenant {forward} oifname \"{tap}\" counter drop comment \"tenant_inbound\"\n"));
        text.push_str(&format!("add chain inet motor_tenant {nat} {{ type nat hook postrouting priority srcnat; policy accept; }}\n"));
        text.push_str(&format!("add rule inet motor_tenant {nat} iifname \"{tap}\" ip saddr 10.200.{}.2 counter masquerade\n",r.tap_index));
    }
    Ok(text)
}

fn managed_tap(name: &str, suffix: &str) -> Option<String> {
    let digits = name.strip_prefix("tenant_mtap")?.strip_suffix(suffix)?;
    let index: u8 = digits.parse().ok()?;
    (index > 0 && index < 255 && digits == index.to_string()).then(|| format!("mtap{index}"))
}

pub fn install(registries: &[Registry]) -> Result<()> {
    let tables = nft_json(&["-j", "list", "tables"])?;
    let exists = tables["nftables"]
        .as_array()
        .ok_or_else(|| Error("missing nft tables".into()))?
        .iter()
        .any(|item| item["table"]["family"] == "inet" && item["table"]["name"] == "motor_tenant");
    let existing = if exists {
        Some(nft_json(&["-j", "list", "table", "inet", "motor_tenant"])?)
    } else {
        None
    };
    let transaction = rules(registries, existing.as_ref())?;
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
        assert!(text.contains("iifname \"mtap1\" counter name tenant_mtap1_egress"));
        assert!(text.contains("iifname \"mtap2\" counter name tenant_mtap2_egress"));
    }

    #[derive(Default)]
    struct NftObjects {
        counters: std::collections::BTreeMap<String, u64>,
        chains: std::collections::BTreeSet<String>,
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
        assert!(!enable_b.contains("tenant_mtap1"));
        assert!(!enable_b.contains("table inet"));
        nft.apply(&enable_b);
        nft.traffic("mtap1", 25);
        nft.traffic("mtap2", 70);
        assert_eq!(forward_bytes(&nft.snapshot(), "mtap1").unwrap(), 125);
        let disable_b = rules(std::slice::from_ref(&a), Some(&nft.snapshot())).unwrap();
        assert!(!disable_b.contains("tenant_mtap1"));
        assert!(!disable_b.contains("table inet"));
        nft.apply(&disable_b);
        nft.traffic("mtap1", 30);
        assert_eq!(forward_bytes(&nft.snapshot(), "mtap1").unwrap(), 155);
        assert!(forward_bytes(&nft.snapshot(), "mtap2").is_err());
        let reconcile = rules(&[a], Some(&nft.snapshot())).unwrap();
        assert!(reconcile.is_empty());
        nft.apply(&reconcile);
        nft.traffic("mtap1", 10);
        assert_eq!(forward_bytes(&nft.snapshot(), "mtap1").unwrap(), 165);
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
