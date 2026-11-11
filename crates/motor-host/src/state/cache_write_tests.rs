use super::*;

#[derive(Default)]
struct Memory;
impl Journal for Memory {
    fn store(&mut self, _: &Snapshot) -> io::Result<()> {
        Ok(())
    }
}

fn claims() -> Claims {
    serde_json::from_value(serde_json::json!({
        "iss":motor_lease::ISSUER,"aud":"host","jti":"job","sub":"acct:test",
        "purpose":"run","iat":1800000000,"exp":1800000300,"vcpus":1,"mem_mib":1024,
        "platform":"linux-x64","net":false,"placement":{"host":"host","vm":"vm"},
        "max_s":30,"cnf":{"jkt":"key"},"workspace_key_hash":"hash"
    }))
    .unwrap()
}

fn host(boat: bool) -> Host {
    let mut host = Host::sandbox(2, Snapshot::default(), Box::<Memory>::default());
    if !boat {
        let mut tenant = host.tenants.remove("sandbox").unwrap();
        tenant.tenant = "tenant".into();
        tenant.sub = "acct:test".into();
        host.tenants.insert("vm".into(), tenant);
        host.provider_managed = false;
    }
    host
}

#[test]
fn first_claim_ceiling_requires_explicit_permission_registered_host_and_no_block() {
    for claim in [None, Some(false), Some(true)] {
        for blocked in [false, true] {
            for boat in [false, true] {
                let mut host = host(boat);
                host.set_remote_cache_write_block(blocked).unwrap();
                let mut claims = claims();
                claims.cache_write = claim;
                host.claim(claims, 1_800_000_000_000).unwrap();
                assert_eq!(
                    host.snapshot.leases["job"].cache_write,
                    claim == Some(true) && !blocked && !boat
                );
            }
        }
    }
}

#[test]
fn later_run_attach_or_block_change_never_replaces_the_first_claim_ceiling() {
    for admitted in [None, Some(false), Some(true)] {
        let mut host = host(false);
        let mut claims = claims();
        claims.cache_write = admitted;
        let (id, _) = host.claim(claims.clone(), 1_800_000_000_000).unwrap();
        host.close(id).unwrap();
        host.set_remote_cache_write_block(true).unwrap();
        // Reopening a persisted record must retain the original decision.
        let bytes = serde_json::to_vec(&host.snapshot).unwrap();
        host.snapshot = serde_json::from_slice(&bytes).unwrap();
        for purpose in [Purpose::Run, Purpose::Attach] {
            for later in [None, Some(false), Some(true)] {
                claims.iat += 1;
                claims.purpose = purpose;
                claims.cache_write = later;
                let (id, _) = host.claim(claims.clone(), 1_800_000_000_000).unwrap();
                assert_eq!(
                    host.snapshot.leases["job"].cache_write,
                    admitted == Some(true)
                );
                host.close(id).unwrap();
            }
        }
    }
}
