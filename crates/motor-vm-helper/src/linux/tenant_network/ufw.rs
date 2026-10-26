use super::*;
use std::{collections::BTreeSet, io::Read, os::unix::fs::MetadataExt, path::Path};

#[derive(Clone, Default)]
pub(super) struct State {
    pub owned: BTreeSet<u8>,
    pub ready: BTreeSet<u8>,
}

fn index(comment: &str) -> Result<u8> {
    let digits = comment
        .strip_prefix("motor-tenant-mtap")
        .ok_or_else(|| Error("unknown owned UFW comment".into()))?;
    let index: u8 = digits
        .parse()
        .map_err(|_| Error("invalid owned UFW tap".into()))?;
    if index == 0 || index == 255 || digits != index.to_string() {
        return Err(Error("noncanonical owned UFW tap".into()));
    }
    Ok(index)
}
fn hex_comment(value: &str) -> Result<String> {
    if !value.len().is_multiple_of(2) {
        return Err(Error("invalid UFW comment encoding".into()));
    }
    let bytes = value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| {
            std::str::from_utf8(p)
                .ok()
                .and_then(|v| u8::from_str_radix(v, 16).ok())
                .ok_or_else(|| Error("invalid UFW comment encoding".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    String::from_utf8(bytes).map_err(|_| Error("invalid UFW comment encoding".into()))
}
fn tap_index(tap: &str) -> Result<u8> {
    index(&format!("motor-tenant-{tap}"))
}
fn rule_index(line: &str, chain: &str, uplink: &str) -> Result<Option<u8>> {
    let words: Vec<_> = line.split_whitespace().collect();
    if words.first() != Some(&"-A") || words.get(1) != Some(&chain) {
        return Ok(None);
    }
    if !words.iter().any(|w| w.starts_with("mtap")) {
        return Ok(None);
    }
    let mut values = std::collections::BTreeMap::new();
    for pair in words[2..].chunks(2) {
        if pair.len() != 2
            || !["-i", "-o", "-s", "-j"].contains(&pair[0])
            || values.insert(pair[0], pair[1]).is_some()
        {
            return Err(Error("unexpected tenant UFW rule predicates".into()));
        }
    }
    let tap = values
        .get("-i")
        .ok_or_else(|| Error("reverse tenant UFW grant".into()))?;
    let n = tap_index(tap)?;
    let source = format!("10.200.{n}.2");
    if values.len() != 4
        || values.get("-o") != Some(&uplink)
        || values.get("-j") != Some(&"ACCEPT")
        || values.get("-s").map(|s| s.strip_suffix("/32").unwrap_or(s)) != Some(source.as_str())
    {
        return Err(Error(
            "tenant UFW grant must have exact source, tap and uplink".into(),
        ));
    }
    Ok(Some(n))
}

pub(super) fn parse_owned(text: &str, ipv6: bool, uplink: &str) -> Result<BTreeSet<u8>> {
    let mut owned = BTreeSet::new();
    let mut pending = None;
    for line in text.lines() {
        if let Some(tuple) = line.strip_prefix("### tuple ### ") {
            if pending.is_some() {
                return Err(Error("owned UFW tuple has no rule".into()));
            }
            pending = None;
            let words: Vec<_> = tuple.split_whitespace().collect();
            if let Some(encoded) = words.iter().find_map(|w| w.strip_prefix("comment=")) {
                let comment = hex_comment(encoded)?;
                if comment.starts_with("motor-tenant-") {
                    let n = index(&comment)?;
                    let source = format!("10.200.{n}.2");
                    let interfaces = format!("in_mtap{n}!out_{uplink}");
                    if ipv6
                        || words.len() != 8
                        || words[..5] != ["route:allow", "any", "any", "0.0.0.0/0", "any"]
                        || words[5].strip_suffix("/32").unwrap_or(words[5]) != source
                        || words[6] != interfaces
                    {
                        return Err(Error("malformed owned tenant UFW tuple".into()));
                    }
                    pending = Some(n);
                }
            }
        } else if line.starts_with("-A ") {
            let n = rule_index(
                line,
                if ipv6 {
                    "ufw6-user-forward"
                } else {
                    "ufw-user-forward"
                },
                uplink,
            )?;
            match (pending.take(), n) {
                (Some(expected), Some(actual)) if expected == actual && owned.insert(actual) => {}
                (None, None) => {}
                _ => {
                    return Err(Error(
                        "unowned, duplicate or mismatched tenant UFW grant".into(),
                    ))
                }
            }
        }
    }
    if pending.is_some() {
        return Err(Error("owned UFW tuple has no rule".into()));
    }
    Ok(owned)
}

pub(super) fn live_grants(text: &str, ipv6: bool, uplink: &str) -> Result<BTreeSet<u8>> {
    let mut values = BTreeSet::new();
    for line in text.lines() {
        if let Some(n) = rule_index(
            line,
            if ipv6 {
                "ufw6-user-forward"
            } else {
                "ufw-user-forward"
            },
            uplink,
        )? {
            if ipv6 || !values.insert(n) {
                return Err(Error("IPv6 or duplicate tenant UFW grant".into()));
            }
        }
    }
    Ok(values)
}

fn read_policy(path: &str) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(Error("unsafe UFW policy file".into()));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(Error("UFW policy exceeds read bound".into()));
    }
    String::from_utf8(bytes).map_err(|_| Error("invalid UFW policy encoding".into()))
}
fn command(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/timeout")
        .args(["--signal=TERM", "--kill-after=2s", "15s", program])
        .args(args)
        .env("LC_ALL", "C")
        .output()?;
    if !output.status.success() {
        return Err(Error(format!(
            "tenant firewall command failed: {program} {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| Error("invalid firewall command encoding".into()))
}

fn status_active(status: &str) -> Result<bool> {
    let active = status.lines().any(|s| s == "Status: active");
    if !active && !status.lines().any(|s| s == "Status: inactive") {
        return Err(Error("unrecognized UFW status".into()));
    }
    Ok(active)
}

pub(super) fn inspect(uplink: &str) -> Result<State> {
    let owned = parse_owned(&read_policy("/etc/ufw/user.rules")?, false, uplink)?;
    if Path::new("/etc/ufw/user6.rules").exists() {
        parse_owned(&read_policy("/etc/ufw/user6.rules")?, true, uplink)?;
    }
    let status = command("/usr/sbin/ufw", &["status"])?;
    let active = status_active(&status)?;
    if !active {
        return Ok(State {
            ready: owned.clone(),
            owned,
        });
    }
    let live = live_grants(
        &command("/usr/sbin/iptables", &["-S", "ufw-user-forward"])?,
        false,
        uplink,
    )?;
    let before = command("/usr/sbin/iptables", &["-S", "ufw-before-forward"])?;
    if !before.lines().any(|line| {
        let words: Vec<_> = line.split_whitespace().collect();
        words.len() == 8
            && words[..4] == ["-A", "ufw-before-forward", "-m", "conntrack"]
            && words[4] == "--ctstate"
            && words[6..] == ["-j", "ACCEPT"]
            && words[5].split(',').collect::<BTreeSet<_>>()
                == BTreeSet::from(["ESTABLISHED", "RELATED"])
    }) {
        return Err(Error("UFW established/related return rule missing".into()));
    }
    let defaults = std::fs::read_to_string("/etc/default/ufw")?;
    if defaults
        .lines()
        .any(|s| matches!(s.trim(), "IPV6=yes" | "IPV6=\"yes\"" | "IPV6='yes'"))
    {
        if !Path::new("/etc/ufw/user6.rules").exists() {
            return Err(Error("UFW IPv6 persistent policy missing".into()));
        }
        live_grants(
            &command("/usr/sbin/ip6tables", &["-S", "ufw6-user-forward"])?,
            true,
            uplink,
        )?;
    }
    if !live.is_subset(&owned) {
        return Err(Error("live tenant UFW grant is not comment-owned".into()));
    }
    Ok(State {
        ready: owned.intersection(&live).copied().collect(),
        owned,
    })
}

fn arguments(n: u8, delete: bool, uplink: &str) -> Vec<String> {
    let mut values: Vec<String> = if delete {
        vec!["--force", "delete", "route", "allow"]
    } else {
        vec!["route", "allow"]
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    values.extend([
        "in".into(),
        "on".into(),
        format!("mtap{n}"),
        "out".into(),
        "on".into(),
        uplink.into(),
        "from".into(),
        format!("10.200.{n}.2"),
        "to".into(),
        "any".into(),
        "comment".into(),
        format!("motor-tenant-mtap{n}"),
    ]);
    values
}

pub(super) fn reconcile(registries: &[Registry], prior: &State, uplink: &str) -> Result<State> {
    let desired: BTreeSet<_> = registries.iter().map(|r| r.tap_index).collect();
    for (n, delete) in prior
        .owned
        .difference(&desired)
        .map(|n| (*n, true))
        .chain(desired.difference(&prior.owned).map(|n| (*n, false)))
    {
        let args = arguments(n, delete, uplink);
        command(
            "/usr/sbin/ufw",
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
    }
    for n in desired
        .intersection(&prior.owned)
        .filter(|n| !prior.ready.contains(n))
    {
        for delete in [true, false] {
            let args = arguments(*n, delete, uplink);
            command(
                "/usr/sbin/ufw",
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
            )?;
        }
    }
    let state = inspect(uplink)?;
    if state.owned != desired || state.ready != desired {
        return Err(Error("tenant UFW grants are not live and exact".into()));
    }
    Ok(state)
}

fn removal_uplinks(text: &str, target: u8) -> Result<BTreeSet<String>> {
    let comment = format!("motor-tenant-mtap{target}");
    let encoded_comment: String = comment.bytes().map(|byte| format!("{byte:02x}")).collect();
    let mut result = BTreeSet::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(tuple) = line.strip_prefix("### tuple ### ") else {
            continue;
        };
        let words: Vec<_> = tuple.split_whitespace().collect();
        let Some(encoded) = words.iter().find_map(|word| word.strip_prefix("comment=")) else {
            continue;
        };
        if !encoded.eq_ignore_ascii_case(&encoded_comment) {
            continue;
        }
        let prefix = format!("in_mtap{target}!out_");
        let uplink = words
            .get(6)
            .and_then(|value| value.strip_prefix(&prefix))
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 15
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            })
            .ok_or_else(|| Error("malformed owned removal direction".into()))?;
        let rule = lines
            .find(|line| !line.trim().is_empty())
            .ok_or_else(|| Error("owned UFW tuple has no rule".into()))?;
        let owned = parse_owned(&format!("{line}\n{rule}\n"), false, uplink)?;
        if owned != BTreeSet::from([target]) || !result.insert(uplink.into()) {
            return Err(Error("ambiguous tenant UFW removal grant".into()));
        }
    }
    Ok(result)
}

pub(super) fn remove_owned(index: u8) -> Result<()> {
    if !Path::new("/etc/ufw/user.rules").exists() {
        return Ok(());
    }
    // The saved, comment-owned tuple supplies the old interface spelling. Its
    // existence, address and current configured uplink are irrelevant to deletion.
    for uplink in removal_uplinks(&read_policy("/etc/ufw/user.rules")?, index)? {
        let args = arguments(index, true, &uplink);
        command(
            "/usr/sbin/ufw",
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
    }
    if !removal_uplinks(&read_policy("/etc/ufw/user.rules")?, index)?.is_empty() {
        return Err(Error("owned UFW removal grant survived deletion".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CAPTURE: &str = "### tuple ### route:allow any any 0.0.0.0/0 any 10.200.1.2 in_mtap1!out_enp6s0 comment=6d6f746f722d74656e616e742d6d74617031\n-A ufw-user-forward -i mtap1 -o enp6s0 -s 10.200.1.2 -j ACCEPT\n";
    #[test]
    fn cleanup_uses_only_the_owned_comment_and_saved_direction_even_if_uplink_is_gone() {
        let gone = CAPTURE.replace("enp6s0", "gone0");
        assert_eq!(removal_uplinks(&gone, 1).unwrap(), ["gone0".into()].into());
        assert!(removal_uplinks(&gone, 2).unwrap().is_empty());
        let foreign = gone.replace("6d6f746f722d74656e616e742d6d74617031", "666f726569676e");
        assert!(removal_uplinks(&foreign, 1).unwrap().is_empty());
        assert!(removal_uplinks(&gone.replace("10.200.1.2", "10.200.2.2"), 1).is_err());
        assert_eq!(arguments(1, true, "gone0")[9], "gone0");
    }
    #[test]
    fn alternate_uplink_is_used_for_owned_tuples_live_rules_and_commands() {
        let capture = CAPTURE.replace("enp6s0", "eno1");
        assert_eq!(
            parse_owned(&capture, false, "eno1").unwrap(),
            BTreeSet::from([1])
        );
        assert_eq!(
            live_grants(&capture, false, "eno1").unwrap(),
            BTreeSet::from([1])
        );
        assert!(parse_owned(&capture, false, "enp6s0").is_err());
        assert_eq!(
            arguments(1, false, "eno1"),
            arguments(1, false, "enp6s0")
                .into_iter()
                .map(|value| if value == "enp6s0" {
                    "eno1".into()
                } else {
                    value
                })
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn owned_ufw_tuple_and_live_grant_require_exact_tap_source_and_direction() {
        assert_eq!(
            parse_owned(CAPTURE, false, "enp6s0").unwrap(),
            BTreeSet::from([1])
        );
        for broken in [
            CAPTURE.replace("-i mtap1", "-i mtap+"),
            CAPTURE.replace(" -s 10.200.1.2", ""),
            CAPTURE.replace("-i mtap1 -o enp6s0", "-i enp6s0 -o mtap1"),
        ] {
            assert!(parse_owned(&broken, false, "enp6s0").is_err());
            assert!(live_grants(&broken, false, "enp6s0").is_err());
        }
        assert!(parse_owned(CAPTURE, true, "enp6s0").is_err());
        assert!(parse_owned(
            &format!("{CAPTURE}-A ufw-user-forward -i enp6s0 -o mtap1 -j ACCEPT\n"),
            false,
            "enp6s0"
        )
        .is_err());
        assert_eq!(
            live_grants(
                "-A ufw-user-forward -s 10.200.1.2/32 -i mtap1 -o enp6s0 -j ACCEPT",
                false,
                "enp6s0"
            )
            .unwrap(),
            BTreeSet::from([1])
        );
        assert_eq!(parse_owned("### tuple ### route:allow any any 0.0.0.0/0 any 0.0.0.0/0 in_mnet-+!out_enp6s0\n-A ufw-user-forward -i mnet-+ -o enp6s0 -j ACCEPT\n",false,"enp6s0").unwrap(),BTreeSet::new());
    }
    #[test]
    fn ufw_operations_are_exact_comment_owned_ipv4_outbound_only() {
        assert_eq!(
            arguments(1, false, "enp6s0"),
            [
                "route",
                "allow",
                "in",
                "on",
                "mtap1",
                "out",
                "on",
                "enp6s0",
                "from",
                "10.200.1.2",
                "to",
                "any",
                "comment",
                "motor-tenant-mtap1"
            ]
        );
        assert_eq!(
            &arguments(1, true, "enp6s0")[..4],
            ["--force", "delete", "route", "allow"]
        );
    }

    #[test]
    fn unknown_ufw_status_is_not_mistaken_for_inactive_firewall() {
        assert!(status_active("Status: active\n").unwrap());
        assert!(!status_active("Status: inactive\n").unwrap());
        assert!(status_active("unavailable").is_err());
    }
}
