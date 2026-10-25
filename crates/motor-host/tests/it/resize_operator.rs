//! Operator-only qualification of the real host/helper/guest resize handshake.
use super::*;
use motor_host::helper::{Control, Helper};
use motor_host::server::{unix_ms, Runtime};
use motor_vm_helper::tenant::{Registry, ResizeActivation};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

struct PeerIdentity {
    gid: u32,
}
impl PeerIdentity {
    fn enter(uid: u32, gid: u32) -> Self {
        // The helper authenticates SO_PEERCRED, not root's ability to open its
        // socket. Retain saved root identity to inspect root-only intent later.
        let original_gid = unsafe { libc::getegid() };
        assert_eq!(unsafe { libc::setegid(gid) }, 0);
        assert_eq!(unsafe { libc::seteuid(uid) }, 0);
        Self { gid: original_gid }
    }
}
impl Drop for PeerIdentity {
    fn drop(&mut self) {
        // Restore root before changing the effective group back.
        assert_eq!(unsafe { libc::seteuid(0) }, 0);
        assert_eq!(unsafe { libc::setegid(self.gid) }, 0);
    }
}
struct StopOnExit {
    helper: Arc<Helper>,
    tenant: String,
    stopped: bool,
}
impl Drop for StopOnExit {
    fn drop(&mut self) {
        if !self.stopped {
            if let Err(error) = self.helper.stop(&self.tenant) {
                eprintln!("operator cleanup failed for {}: {error:#}", self.tenant);
            }
        }
    }
}
fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn service_state(name: &str) -> String {
    let output = Command::new("systemctl")
        .args(["is-active", name])
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
#[ignore = "requires a stopped disposable tenant with staged quota growth on a dedicated root/KVM host"]
fn isolated_staged_resize_cold_start() {
    assert_eq!(
        std::env::var("MOTOR_RESIZE_OPERATOR_ACK").unwrap(),
        "stopped-disposable-tenant"
    );
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "run as root; the test connects as an authorized helper peer"
    );
    let tenant = std::env::var("MOTOR_VM_TEST_TENANT").unwrap();
    motor_vm_helper::tenant::validate_name(&tenant).unwrap();
    assert!(
        tenant.starts_with("resize-canary-"),
        "only a resize-canary-* throwaway tenant is permitted"
    );
    assert_eq!(service_state("motor-host.service"), "inactive");
    assert_eq!(service_state("motor-vm-helper.service"), "active");
    let config = json_file(Path::new(&std::env::var("MOTOR_HOST_TEST_CONFIG").unwrap()));
    let registry_dir = Path::new(config["registry_dir"].as_str().unwrap());
    let r: Registry =
        serde_json::from_value(json_file(&registry_dir.join(format!("{tenant}.json")))).unwrap();
    r.validate(&tenant).unwrap();
    for entry in std::fs::read_dir(registry_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let other: Registry = serde_json::from_value(json_file(&path)).unwrap();
            assert!(
                !other.generation.with_file_name("running.json").exists(),
                "all registered tenants must be stopped: {}",
                path.display()
            );
        }
    }
    let staged_path = r.generation.with_file_name("runner-args.staged");
    let staged: ResizeActivation = serde_json::from_value(json_file(&staged_path)).unwrap();
    let staged_metadata = std::fs::symlink_metadata(&staged_path).unwrap();
    assert!(staged_metadata.is_file());
    assert_eq!(
        (staged_metadata.uid(), staged_metadata.mode() & 0o777),
        (0, 0o600)
    );
    assert!(
        staged.quota_grew,
        "stage a quota increase to exercise the real post-grow check"
    );
    assert_eq!(
        std::fs::metadata(&r.disk).unwrap().len(),
        r.disk_quota_bytes
    );
    let storage = Path::new(config["storage"].as_str().unwrap());
    assert_eq!(
        std::fs::metadata(storage).unwrap().dev(),
        std::fs::metadata(&r.disk).unwrap().dev()
    );
    let bindings = json_file(Path::new(config["bindings"].as_str().unwrap()));
    let selected: Vec<_> = bindings
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, b)| b["tenant"] == tenant)
        .collect();
    assert_eq!(
        selected.len(),
        1,
        "throwaway tenant must have exactly one binding"
    );
    let (vm, binding) = selected[0];
    assert!(!vm.is_empty(), "VM binding must have a name");
    let sub = binding["sub"].as_str().unwrap();
    let identity = format!("{}\n{sub}\n{vm}", config["host_id"].as_str().unwrap());
    let machine_id = format!("{:x}", Sha256::digest(identity.as_bytes()))[..32].to_owned();
    let helper = Arc::new(Helper {
        socket: config["helper_socket"].as_str().unwrap().into(),
        storage: storage.into(),
        capacity: "/etc/motor-host/capacity.json".into(),
        guest_init: BTreeMap::from([(
            tenant.clone(),
            json!({
                "address":format!("10.200.{}.2/30", r.tap_index), "gateway":format!("10.200.{}.1", r.tap_index),
                "resolvers":config["resolvers"], "hostname":format!("motor-{}",r.tap_index), "machine_id":machine_id
            }),
        )]),
    });
    let host = Host::new(
        BTreeMap::from([(
            vm.clone(),
            Tenant {
                tenant: tenant.clone(),
                sub: sub.into(),
                vcpu_ceiling: r.vcpu_ceiling,
                max_weight: r.max_weight,
                guest_ram_mib: r.guest_ram_mib,
                idle_suspend_s: r.idle_suspend_s,
                host_floor_bytes: r.host_floor_bytes,
            },
        )]),
        Snapshot::default(),
        Box::<MemoryJournal>::default(),
    );
    let runtime = Runtime::new(
        config["host_id"].as_str().unwrap().into(),
        host,
        helper.clone(),
    );
    let helper_config = json_file(Path::new("/etc/motor-vm-helper.json"));
    let uid = u32::try_from(helper_config["allowed_uids"][0].as_u64().unwrap()).unwrap();
    let gid = std::fs::metadata(&helper.socket).unwrap().gid();
    let live_args;
    {
        let _peer = PeerIdentity::enter(uid, gid);
        let mut stop = StopOnExit {
            helper: helper.clone(),
            tenant: tenant.clone(),
            stopped: false,
        };
        let now = unix_ms();
        runtime
            .host
            .lock()
            .unwrap()
            .start_refused(&tenant, "activation", now);
        runtime
            .retry_refused_starts(now + motor_host::state::START_RETRY_MS)
            .unwrap();
        assert!(runtime.host.lock().unwrap().is_ready(&tenant));
        live_args = helper.admin(&tenant, &json!({"op":"args_status"})).unwrap();
        assert_eq!(live_args["ok"], true);
        assert!(helper.available_bytes().unwrap() > 0);
        helper.stop(&tenant).unwrap();
        stop.stopped = true;
    }
    assert!(
        !staged_path.exists(),
        "intent must complete only after real guest/host barriers"
    );
    assert!(
        !r.generation.with_file_name("running.json").exists(),
        "qualification must leave a cleanly stopped VM"
    );
    let words: Vec<_> = staged.runner_args.split_whitespace().collect();
    for (flag, field) in [
        ("--max-weight", "max_weight"),
        ("--sccache-size-gib", "sccache_size_gib"),
        ("--free-space-floor-bytes", "free_space_floor_bytes"),
    ] {
        let expected = words.windows(2).find(|w| w[0] == flag).unwrap()[1]
            .parse::<u64>()
            .unwrap();
        assert_eq!(live_args[field].as_u64(), Some(expected), "live {field}");
    }
    println!("PASS tenant={tenant}: cold attach, resize, post-grow check, effective args, durable completion, clean stop; args_status={live_args}");
}
