use super::*;
use serde_json::Value;

#[test]
fn host_drain_heartbeat_body_matches_pinned_cloud_bytes() {
    let vector: Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/host-draining.json"
    ))
    .unwrap();
    let mut bindings = tenants();
    bindings.get_mut("vm").unwrap().vcpu_ceiling = 60;
    let mut h = Host::new(
        bindings,
        Snapshot::default(),
        Box::<MemoryJournal>::default(),
    );
    // The drain exchanges carry no warm workspaces; the busy exchanges are
    // checked by the test below.
    for exchange in vector["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|exchange| exchange["request"]["body"]["warm"] == serde_json::json!([]))
    {
        let expected = &exchange["request"];
        let draining = expected["body"]["draining"].as_bool().unwrap_or(false);
        h.set_draining(draining).unwrap();
        let heartbeat = h.heartbeat(vector["clock_ms"].as_u64().unwrap(), 245760, true);
        let decoded: motor_lease::Heartbeat =
            serde_json::from_str(expected["body_bytes"].as_str().unwrap()).unwrap();
        assert_eq!(heartbeat, decoded);
        // Explicit false is accepted from peers, but hosts omit it to preserve
        // the pre-drain heartbeat bytes when fleet placement is enabled.
        let bytes = if expected["body"]["draining"] == false {
            vector["exchanges"][1]["request"]["body_bytes"]
                .as_str()
                .unwrap()
        } else {
            expected["body_bytes"].as_str().unwrap()
        };
        assert_eq!(serde_json::to_vec(&heartbeat).unwrap(), bytes.as_bytes());
    }
}

/// CLOUD's busy exchanges decode, and a host writes them back byte for byte,
/// except that an explicit `"busy":false` is omitted (absent means false),
/// as with `draining`. Which leases make an entry busy is tested in billing.rs.
#[test]
fn warm_busy_heartbeat_bytes_match_pinned_cloud_vectors() {
    let vector: Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/host-draining.json"
    ))
    .unwrap();
    let mut seen = [false; 2];
    for exchange in vector["exchanges"].as_array().unwrap() {
        let Some(busy) = exchange["request"]["body"]["warm"][0]["busy"].as_bool() else {
            continue;
        };
        seen[usize::from(busy)] = true;
        let bytes = exchange["request"]["body_bytes"].as_str().unwrap();
        let decoded: motor_lease::Heartbeat = serde_json::from_str(bytes).unwrap();
        assert_eq!(decoded.warm[0].busy, busy);
        let expected = if busy {
            bytes.to_owned()
        } else {
            bytes.replace(",\"busy\":false", "")
        };
        assert_eq!(
            String::from_utf8(serde_json::to_vec(&decoded).unwrap()).unwrap(),
            expected
        );
    }
    assert_eq!(seen, [true, true], "both busy exchanges are pinned");
}

#[test]
fn host_drain_persists_across_restart_and_does_not_refuse_binding_leases() {
    let dir = scratch();
    let mut h = disk_host(dir.path());
    assert!(!h.snapshot.draining);
    h.set_draining(true).unwrap();
    h.retention_tick(NOW * 1000).unwrap();
    drop(h);
    let mut h = disk_host(dir.path());
    assert!(h.snapshot.draining);
    assert!(h.heartbeat(NOW * 1000, 245760, true).draining);
    // Admission still honors broker-issued placements, including canaries.
    assert!(h.claim(claims(JOB), NOW * 1000).is_ok());
    h.set_draining(false).unwrap();
    drop(h);
    let h = disk_host(dir.path());
    assert!(!h.snapshot.draining);
    let mut old = serde_json::to_value(&h.snapshot).unwrap();
    old.as_object_mut().unwrap().remove("draining");
    assert!(!serde_json::from_value::<Snapshot>(old).unwrap().draining);
}

#[test]
fn host_drain_is_not_published_when_the_journal_write_fails() {
    struct Full;
    impl Journal for Full {
        fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::other("disk full"))
        }
    }
    let mut h = host();
    h.replace_journal(Box::new(Full));
    assert!(h.set_draining(true).is_err());
    assert!(!h.snapshot.draining);
    assert!(!h.heartbeat(NOW * 1000, 245760, true).draining);
}
