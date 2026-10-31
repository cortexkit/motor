mod admission;
mod admission_capacity;
mod billing;
#[cfg(target_os = "linux")]
mod broker;
mod capacity;
mod closure;
mod drain;
mod enroll;
mod helper;
mod replacement;
mod resize;
#[cfg(target_os = "linux")]
mod resize_operator;
mod store;
mod transport;

use motor_host::journal::{FileJournal, Journal};
use motor_host::state::{Host, Snapshot, Tenant};
use motor_lease::{Claims, Confirmation, Placement, Purpose};
use motor_protocol::{RefusedCause, TenantEvent, TenantEventKind};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const NOW: u64 = 1_800_000_000;
const JOB: &str = "00000000-0000-7000-8000-000000000001";
const JOB2: &str = "00000000-0000-7000-8000-000000000002";
const JOB3: &str = "00000000-0000-7000-8000-000000000003";

fn claims(job: &str) -> Claims {
    Claims {
        workspace_key_hash: Some(motor_lease::base64url(&[5; 32])),
        iss: motor_lease::ISSUER.into(),
        aud: "host".into(),
        jti: job.into(),
        sub: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        purpose: Purpose::Run,
        iat: NOW,
        exp: NOW + 300,
        vcpus: 1,
        mem_mib: 16_384,
        platform: "linux-x64".into(),
        net: false,
        placement: Placement {
            host: "host".into(),
            vm: Some("vm".into()),
        },
        max_s: 30,
        cnf: Confirmation {
            jkt: motor_lease::base64url(&[1; 32]),
        },
    }
}
fn tenants() -> BTreeMap<String, Tenant> {
    BTreeMap::from([(
        "vm".into(),
        Tenant {
            tenant: "tenant".into(),
            sub: claims(JOB).sub,
            vcpu_ceiling: 2,
            max_weight: None,
            guest_ram_mib: 18432,
            idle_suspend_s: 60,
            host_floor_bytes: 2147483648,
        },
    )])
}
#[derive(Default)]
struct MemoryJournal(Arc<Mutex<Vec<Snapshot>>>);
impl Journal for MemoryJournal {
    fn store(&mut self, snapshot: &Snapshot) -> std::io::Result<()> {
        self.0.lock().unwrap().push(snapshot.clone());
        Ok(())
    }
}
fn host() -> Host {
    Host::new(
        tenants(),
        Snapshot::default(),
        Box::<MemoryJournal>::default(),
    )
}
fn event(
    seq: u64,
    kind: TenantEventKind,
    job: Option<&str>,
    cause: Option<RefusedCause>,
) -> TenantEvent {
    TenantEvent {
        event_seq: seq,
        event: kind,
        jti: job.map(|s| s.parse().unwrap()),
        cause,
        setup: None,
        mem_peak_mib: None,
    }
}
fn disk_host(path: &std::path::Path) -> Host {
    let (journal, snapshot) = FileJournal::open(path).unwrap();
    Host::new(tenants(), snapshot, Box::new(journal))
}

fn scratch() -> tempfile::TempDir {
    let root = cortexkit_test_support::scratch_root();
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
