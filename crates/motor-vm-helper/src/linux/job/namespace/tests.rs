use super::*;

#[test]
fn namespace_identity_is_derived_without_changing_records() {
    let id = job::test_id(1);
    assert_eq!(
        path(&id).unwrap(),
        Path::new(ROOT).join(format!("motor-job-{id}"))
    );
    for id in ["../escape", "", "with/slash"] {
        assert!(path(id).is_err());
    }
}

#[test]
fn namespace_policy_is_routed_default_deny_without_nat() {
    let text = policy(Some(&Tap {
        tap_index: 17,
        uid: 500000,
    }));
    for chain in ["input", "output", "forward"] {
        assert!(text.contains(&format!("add chain inet motor_job {chain} {{ type filter hook {chain} priority 0; policy drop; }}")));
    }
    assert_eq!(text.matches(" accept\n").count(), 2);
    assert!(text
        .contains("iifname \"mtap17\" oifname \"uplink\" meta nfproto ipv4 ip saddr 10.200.17.2"));
    assert!(text.contains("iifname \"uplink\" oifname \"mtap17\" meta nfproto ipv4 ip daddr 10.200.17.2 ct state established,related"));
    for forbidden in ["nat", "masquerade", "10.201.", "lo"] {
        assert!(!text.contains(forbidden));
    }
    assert!(!policy(None).contains("accept"));
}

#[test]
fn veth_ownership_needs_both_kind_and_exact_job_alias() {
    let tap = Tap {
        tap_index: 17,
        uid: 500000,
    };
    let link =
        json!({"ifname":"mtap17", "ifalias":"motor-job-job-1", "linkinfo":{"info_kind":"veth"}});
    assert!(owned(&link, "job-1", &tap).unwrap());
    assert!(!owned(&link, "job-2", &tap).unwrap());
    for field in ["ifalias", "ifname"] {
        let mut foreign = link.clone();
        foreign[field] = json!("foreign");
        assert!(!owned(&foreign, "job-1", &tap).unwrap());
    }
    let mut foreign = link;
    foreign["linkinfo"]["info_kind"] = json!("tun");
    assert!(!owned(&foreign, "job-1", &tap).unwrap());
}
