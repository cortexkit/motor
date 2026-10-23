use super::*;
use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};

const TENANT: &str = "acct:01KXJJHK4V6DB42W1YAN2XD9QN";
const PREFIX: &str = "tenant/acct-01KXJJHK4V6DB42W1YAN2XD9QN/";

/// A provisioning directory in scratch, written the way producers write it:
/// each file to a temporary name in the same directory, chmodded, then renamed.
struct Host {
    _scratch: ScratchDir,
    layout: Layout,
}

impl Host {
    fn new(role: HostRole) -> Self {
        let scratch = ScratchDir::new("cache-provisioning");
        let layout = Layout::new(scratch.join(STATE_DIRECTORY), role);
        match role {
            HostRole::Boat => fs::DirBuilder::new()
                .mode(0o700)
                .create(layout.directory())
                .unwrap(),
            _ => layout.prepare().unwrap(),
        }
        Self {
            _scratch: scratch,
            layout,
        }
    }
    fn dir(&self) -> &Path {
        self.layout.directory()
    }
    fn write_raw(&self, name: &str, mode: u32, contents: &[u8]) {
        let temporary = self.dir().join(format!(".{name}.tmp"));
        fs::write(&temporary, contents).unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode)).unwrap();
        fs::rename(&temporary, self.dir().join(name)).unwrap();
    }
    fn config_value(&self, value: &Value) {
        self.write_raw(CONFIG_FILE, CONFIG_MODE, value.to_string().as_bytes());
    }
    fn config(&self, generation: u64) {
        self.config_value(&config_json(generation));
    }
    fn key(&self, role: KeyRole, generation: u64) {
        self.key_value(role, &key_json(role, generation, generation));
    }
    fn key_value(&self, role: KeyRole, value: &Value) {
        self.write_raw(role.file_name(), KEY_MODE, value.to_string().as_bytes());
    }
    fn provision(&self, generation: u64) {
        for key in self.layout.role().keys() {
            self.key(*key, generation);
        }
        self.config(generation);
    }
    fn provisioning(&self) -> Provisioning {
        Provisioning::new(self.layout.clone())
    }
}

fn config_json(generation: u64) -> Value {
    json!({
        "generation": generation,
        "tenant": TENANT,
        "endpoint": "https://s3.gra.io.cloud.ovh.net",
        "region": "gra",
        "bucket": "motor-cache",
        "prefix": PREFIX,
        "storage_class": "STANDARD",
        "enabled": true,
    })
}

/// Key ids and secrets name their role and a serial, so a test can tell which
/// file a credential came from.
fn key_json(role: KeyRole, generation: u64, serial: u64) -> Value {
    json!({
        "generation": generation,
        "access_key_id": format!("{}-id-{serial}", role.file_name()),
        "secret_access_key": secret(role, serial),
    })
}

fn secret(role: KeyRole, serial: u64) -> String {
    format!("{}-secret-{serial}", role.file_name())
}

fn unavailable(status: &Status) -> &Unavailable {
    match status {
        Status::Unavailable(reason) => reason,
        Status::Available => panic!("expected cache_unavailable, got available"),
    }
}

fn read_id(generation: &ActiveGeneration) -> &str {
    &generation.read_key.access_key_id
}

#[test]
fn derived_prefix_replaces_every_colon() {
    assert_eq!(derived_prefix(TENANT), PREFIX);
    assert_eq!(derived_prefix("a:b:c"), "tenant/a-b-c/");
}

#[test]
fn layout_follows_host_role() {
    let scratch = ScratchDir::new("cache-provisioning-layout");
    let mut config = crate::Config::new(scratch.path(), "fleet");
    let server = Layout::for_config(&config);
    assert_eq!(server.role(), HostRole::BuildServer);
    assert_eq!(
        server.directory(),
        config.state_dir().join("cache-provisioning")
    );
    assert_eq!(server.role().keys(), &[KeyRole::Read, KeyRole::Write]);
    assert_eq!(
        server.key_path(KeyRole::Write),
        config.state_dir().join("cache-provisioning/write-key")
    );

    config.tenant_vm = true;
    let tenant = Layout::for_config(&config);
    assert_eq!(tenant.role(), HostRole::TenantVm);
    assert_eq!(
        tenant.directory(),
        config.state_dir().join("cache-provisioning")
    );
    assert_eq!(tenant.role().keys(), &[KeyRole::Read]);

    config.tenant_vm = false;
    config.boat_profile = true;
    let boat = Layout::for_config(&config);
    assert_eq!(boat.role(), HostRole::Boat);
    assert_eq!(
        boat.config_path(),
        PathBuf::from("/run/motor/cache/config.json")
    );
    assert_eq!(
        boat.key_path(KeyRole::Read),
        PathBuf::from("/run/motor/cache/read-key")
    );
    assert_eq!(boat.role().keys(), &[KeyRole::Read]);
}

#[test]
fn prepare_creates_the_directory_private_and_restores_its_mode() {
    let scratch = ScratchDir::new("cache-provisioning-prepare");
    let layout = Layout::new(scratch.join(STATE_DIRECTORY), HostRole::BuildServer);
    layout.prepare().unwrap();
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().mode() & 0o7777;
    assert_eq!(mode(layout.directory()), 0o700);
    fs::set_permissions(layout.directory(), fs::Permissions::from_mode(0o755)).unwrap();
    layout.prepare().unwrap();
    assert_eq!(mode(layout.directory()), 0o700);

    // The broker owns Boat's directory; the runner never creates it.
    let boat = Layout::new(scratch.join("boat"), HostRole::Boat);
    boat.prepare().unwrap();
    assert!(!boat.directory().exists());
}

#[test]
fn build_server_generation_parses_with_both_keys() {
    let host = Host::new(HostRole::BuildServer);
    host.provision(1);
    let mut provisioning = host.provisioning();
    let reread = provisioning.reread();
    assert!(reread.activated);
    assert_eq!(reread.status, Status::Available);
    assert_eq!(reread.previous_enabled, None);
    assert_eq!(reread.enabled, Some(true));
    let active = provisioning.admit().expect("available and enabled");
    assert_eq!(active.generation, Generation::Number(1));
    assert_eq!(active.tenant, TENANT);
    assert_eq!(active.region, "gra");
    assert_eq!(active.storage_class, "STANDARD");
    assert_eq!(read_id(&active), "read-key-id-1");
    assert_eq!(active.read_key.secret_access_key, secret(KeyRole::Read, 1));
    let write = active
        .write_key
        .as_ref()
        .expect("the build server holds the write key");
    assert_eq!(write.access_key_id, "write-key-id-1");
    assert_eq!(
        active.identity(),
        StoreIdentity {
            endpoint: "https://s3.gra.io.cloud.ovh.net".into(),
            bucket: "motor-cache".into(),
            prefix: PREFIX.into(),
        }
    );
    assert_eq!(reread.identity, Some(active.identity()));
    let debug = format!("{active:?} {reread:?} {provisioning:?}");
    assert!(!debug.contains("secret-1"), "{debug}");
}

#[test]
fn boat_and_tenant_vm_need_only_the_read_key() {
    for role in [HostRole::Boat, HostRole::TenantVm] {
        let host = Host::new(role);
        host.provision(3);
        let mut provisioning = host.provisioning();
        assert_eq!(provisioning.reread().status, Status::Available, "{role:?}");
        let active = provisioning.admit().unwrap();
        assert_eq!(read_id(&active), "read-key-id-3");
        assert!(active.write_key.is_none());
    }
}

#[test]
fn build_server_without_its_write_key_has_no_complete_generation() {
    let host = Host::new(HostRole::BuildServer);
    host.key(KeyRole::Read, 1);
    host.config(1);
    let mut provisioning = host.provisioning();
    let status = provisioning.reread().status;
    let Unavailable::NoCompleteGeneration { detail } = unavailable(&status) else {
        panic!("{status:?}");
    };
    assert!(detail.contains("write-key: missing"), "{detail}");
    assert!(provisioning.admit().is_none());
}

/// Files must have exactly the stated modes and be owned by the runner's
/// account; a key file another account could read, a config with any mode
/// other than 0444, a symlink, a non-regular file, an oversized file or a
/// widened directory makes the set incomplete.
#[test]
fn files_with_wrong_mode_owner_or_type_are_refused() {
    type Damage = fn(&Host);
    let cases: [(&str, Damage, &str); 9] = [
        (
            "config writable by owner",
            |h| {
                fs::set_permissions(h.dir().join(CONFIG_FILE), fs::Permissions::from_mode(0o644))
                    .unwrap()
            },
            "config.json: mode 0644, expected 0444",
        ),
        (
            "config not readable by others",
            |h| {
                fs::set_permissions(h.dir().join(CONFIG_FILE), fs::Permissions::from_mode(0o400))
                    .unwrap()
            },
            "config.json: mode 0400, expected 0444",
        ),
        (
            "key readable by group",
            |h| {
                fs::set_permissions(h.dir().join("read-key"), fs::Permissions::from_mode(0o440))
                    .unwrap()
            },
            "read-key: mode 0440, expected 0400",
        ),
        (
            "key readable by others",
            |h| {
                fs::set_permissions(h.dir().join("write-key"), fs::Permissions::from_mode(0o404))
                    .unwrap()
            },
            "write-key: mode 0404, expected 0400",
        ),
        (
            "key writable by owner",
            |h| {
                fs::set_permissions(h.dir().join("read-key"), fs::Permissions::from_mode(0o600))
                    .unwrap()
            },
            "read-key: mode 0600, expected 0400",
        ),
        (
            "key is a symlink",
            |h| {
                let target = h.dir().join("elsewhere");
                fs::rename(h.dir().join("read-key"), &target).unwrap();
                std::os::unix::fs::symlink(&target, h.dir().join("read-key")).unwrap();
            },
            "read-key: ",
        ),
        (
            "key is a directory",
            |h| {
                fs::remove_file(h.dir().join("write-key")).unwrap();
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(h.dir().join("write-key"))
                    .unwrap();
            },
            "write-key: ",
        ),
        (
            "directory traversable by others",
            |h| fs::set_permissions(h.dir(), fs::Permissions::from_mode(0o711)).unwrap(),
            "has mode 0711, expected 0700",
        ),
        (
            "config is oversized",
            |h| {
                let mut config = config_json(1);
                config["tenant"] = Value::String("a".repeat(70 * 1024));
                h.config_value(&config);
            },
            "config.json: larger than 65536 bytes",
        ),
    ];
    for (case, damage, expected) in cases {
        let host = Host::new(HostRole::BuildServer);
        host.provision(1);
        damage(&host);
        let mut provisioning = host.provisioning();
        let status = provisioning.reread().status;
        let Unavailable::NoCompleteGeneration { detail } = unavailable(&status) else {
            panic!("{case}: {status:?}");
        };
        assert!(detail.contains(expected), "{case}: {detail}");
        assert!(!detail.contains("secret"), "{case}: {detail}");
        assert!(provisioning.admit().is_none(), "{case}");
    }

    let host = Host::new(HostRole::BuildServer);
    host.provision(1);
    // SAFETY: geteuid has no preconditions and cannot fail.
    let other = unsafe { libc::geteuid() } + 1;
    let mut provisioning = Provisioning::new(host.layout.clone().with_owner(other));
    let status = provisioning.reread().status;
    let Unavailable::NoCompleteGeneration { detail } = unavailable(&status) else {
        panic!("{status:?}");
    };
    assert!(detail.contains("not the runner's uid"), "{detail}");
}

/// A Boat broker that writes the raw secret instead of the JSON object leaves
/// the cache unavailable, and the secret never reaches the reported reason.
#[test]
fn raw_key_or_unknown_fields_leave_no_complete_generation() {
    let host = Host::new(HostRole::Boat);
    host.config(1);
    host.write_raw("read-key", KEY_MODE, b"RAWSECRETVALUE\n");
    let mut provisioning = host.provisioning();
    let status = provisioning.reread().status;
    let Unavailable::NoCompleteGeneration { detail } = unavailable(&status) else {
        panic!("{status:?}");
    };
    assert!(detail.contains("read-key: not valid JSON"), "{detail}");
    assert!(!detail.contains("RAWSECRETVALUE"), "{detail}");

    host.write_raw("read-key", KEY_MODE, b"\"RAWSECRETVALUE\"");
    let status = provisioning.reread().status;
    let Unavailable::NoCompleteGeneration { detail } = unavailable(&status) else {
        panic!("{status:?}");
    };
    assert!(!detail.contains("RAWSECRETVALUE"), "{detail}");

    let mut extra = key_json(KeyRole::Read, 1, 1);
    extra["session_token"] = "x".into();
    host.key_value(KeyRole::Read, &extra);
    assert!(matches!(
        unavailable(&provisioning.reread().status),
        Unavailable::NoCompleteGeneration { .. }
    ));

    host.key(KeyRole::Read, 1);
    for (case, field, value) in [
        ("unknown field", "extra", json!(1)),
        ("tenant adds a path segment", "tenant", json!("acct/../x")),
        ("endpoint not http", "endpoint", json!("ftp://s3.example")),
        (
            "endpoint with credentials",
            "endpoint",
            json!("https://a:b@s3.example"),
        ),
        ("empty generation", "generation", json!("")),
        ("negative generation", "generation", json!(-1)),
        ("enabled not a boolean", "enabled", json!("true")),
        ("empty bucket", "bucket", json!("")),
    ] {
        let mut config = config_json(1);
        config[field] = value;
        host.config_value(&config);
        let mut fresh = host.provisioning();
        let status = fresh.reread().status;
        let Unavailable::NoCompleteGeneration { detail } = unavailable(&status) else {
            panic!("{case}: {status:?}");
        };
        assert!(detail.starts_with("config.json: "), "{case}: {detail}");
    }
    host.config(1);
    assert_eq!(provisioning.reread().status, Status::Available);
}

#[test]
fn string_generations_compare_exactly() {
    let host = Host::new(HostRole::TenantVm);
    let mut config = config_json(0);
    config["generation"] = json!("2026-10-24");
    host.config_value(&config);
    let mut key = key_json(KeyRole::Read, 0, 1);
    key["generation"] = json!("2026-10-24");
    host.key_value(KeyRole::Read, &key);
    let mut provisioning = host.provisioning();
    assert_eq!(provisioning.reread().status, Status::Available);
    assert_eq!(
        provisioning.admit().unwrap().generation,
        Generation::Text("2026-10-24".into())
    );
    // The number 1 and the string "1" are different generations.
    config["generation"] = json!(1);
    host.config_value(&config);
    key["generation"] = json!("1");
    host.key_value(KeyRole::Read, &key);
    let reread = provisioning.reread();
    assert!(!reread.activated);
    assert_eq!(
        provisioning.admit().unwrap().generation,
        Generation::Text("2026-10-24".into())
    );
}

/// Config, read key and write key updated one at a time in every order: no
/// half-updated set activates, requests admitted before activation keep the
/// old generation, and requests admitted after it use the new one.
#[test]
fn staggered_updates_activate_only_complete_generations() {
    let orders: [[&str; 3]; 6] = [
        ["config", "read", "write"],
        ["config", "write", "read"],
        ["read", "config", "write"],
        ["read", "write", "config"],
        ["write", "config", "read"],
        ["write", "read", "config"],
    ];
    for order in orders {
        let host = Host::new(HostRole::BuildServer);
        host.provision(1);
        let mut provisioning = host.provisioning();
        assert!(provisioning.reread().activated);
        let admitted_first = provisioning.admit().unwrap();
        for (step, file) in order.iter().enumerate() {
            match *file {
                "config" => host.config(2),
                "read" => host.key(KeyRole::Read, 2),
                _ => host.key(KeyRole::Write, 2),
            }
            let reread = provisioning.reread();
            assert_eq!(reread.status, Status::Available, "{order:?} step {step}");
            let admitted = provisioning.admit().unwrap();
            if step < 2 {
                assert!(!reread.activated, "{order:?} step {step}");
                assert_eq!(admitted.generation, Generation::Number(1));
                assert_eq!(read_id(&admitted), "read-key-id-1");
                assert_eq!(
                    admitted.write_key.as_ref().unwrap().access_key_id,
                    "write-key-id-1"
                );
            } else {
                assert!(reread.activated, "{order:?}");
                assert_eq!(admitted.generation, Generation::Number(2));
                assert_eq!(read_id(&admitted), "read-key-id-2");
                assert_eq!(
                    admitted.write_key.as_ref().unwrap().access_key_id,
                    "write-key-id-2"
                );
            }
        }
        // A request admitted before activation finishes with its own key.
        assert_eq!(admitted_first.generation, Generation::Number(1));
        assert_eq!(read_id(&admitted_first), "read-key-id-1");
    }
}

/// A rotated generation with an unchanged store keeps the identity; the
/// outcome says a generation activated so key users can switch.
#[test]
fn rotation_keeps_the_store_identity() {
    let host = Host::new(HostRole::TenantVm);
    host.provision(1);
    let mut provisioning = host.provisioning();
    provisioning.reread();
    let before = provisioning.admit().unwrap();
    host.provision(2);
    let reread = provisioning.reread();
    assert!(reread.activated);
    assert_eq!(reread.previous_identity, reread.identity);
    let after = provisioning.admit().unwrap();
    assert_eq!(read_id(&before), "read-key-id-1");
    assert_eq!(read_id(&after), "read-key-id-2");
}

/// `enabled` is read from every parsed config on its own: flipping it under
/// the same generation switches admission within one re-read and activates
/// nothing, and even a config of a not yet complete generation flips it.
#[test]
fn enabled_flips_within_one_reread_without_a_new_generation() {
    let host = Host::new(HostRole::BuildServer);
    host.provision(1);
    let mut provisioning = host.provisioning();
    provisioning.reread();
    let active = provisioning.admit().unwrap();

    let mut off = config_json(1);
    off["enabled"] = false.into();
    host.config_value(&off);
    let reread = provisioning.reread();
    assert!(!reread.activated);
    assert_eq!(reread.status, Status::Available);
    assert_eq!(
        (reread.previous_enabled, reread.enabled),
        (Some(true), Some(false))
    );
    assert!(provisioning.admit().is_none());
    assert!(!provisioning.enabled());

    host.config(1);
    let reread = provisioning.reread();
    assert!(!reread.activated);
    assert_eq!(
        (reread.previous_enabled, reread.enabled),
        (Some(false), Some(true))
    );
    assert!(Arc::ptr_eq(&provisioning.admit().unwrap(), &active));

    let mut next_off = config_json(2);
    next_off["enabled"] = false.into();
    host.config_value(&next_off);
    let reread = provisioning.reread();
    assert!(!reread.activated);
    assert_eq!(reread.enabled, Some(false));
    assert!(provisioning.admit().is_none());
    assert_eq!(
        provisioning.active().unwrap().generation,
        Generation::Number(1)
    );
}

/// One unparsable config between good reads changes neither the active
/// generation nor `enabled`.
#[test]
fn unparsable_reread_keeps_the_previous_state() {
    let host = Host::new(HostRole::BuildServer);
    host.provision(1);
    let mut provisioning = host.provisioning();
    provisioning.reread();
    host.write_raw(CONFIG_FILE, CONFIG_MODE, b"{\"generation\": 1,");
    let reread = provisioning.reread();
    assert_eq!(reread.status, Status::Available);
    assert_eq!(
        (reread.previous_enabled, reread.enabled),
        (Some(true), Some(true))
    );
    assert_eq!(reread.previous_identity, reread.identity);
    assert!(provisioning.admit().is_some());
    host.config(1);
    let reread = provisioning.reread();
    assert!(!reread.activated);
    assert_eq!(reread.previous_enabled, reread.enabled);
}

/// A complete set that repeats the active generation with a changed field or
/// key is refused as `generation reused`, re-evaluated at every re-read.
#[test]
fn generation_reused_with_changes_is_unavailable() {
    let host = Host::new(HostRole::BuildServer);
    host.provision(1);
    let mut provisioning = host.provisioning();
    provisioning.reread();

    let mut moved = config_json(1);
    moved["endpoint"] = "https://s3.sbg.io.cloud.ovh.net".into();
    host.config_value(&moved);
    let reread = provisioning.reread();
    assert!(!reread.activated);
    let reason = unavailable(&reread.status).clone();
    assert_eq!(
        reason,
        Unavailable::GenerationReused {
            generation: Generation::Number(1)
        }
    );
    assert_eq!(reason.to_string(), "generation reused");
    assert!(provisioning.admit().is_none());
    assert_eq!(
        provisioning.active().unwrap().endpoint,
        "https://s3.gra.io.cloud.ovh.net"
    );

    host.config(1);
    assert_eq!(provisioning.reread().status, Status::Available);

    host.key_value(KeyRole::Write, &key_json(KeyRole::Write, 1, 9));
    assert_eq!(
        unavailable(&provisioning.reread().status).to_string(),
        "generation reused"
    );
    assert!(provisioning.admit().is_none());

    host.provision(2);
    let reread = provisioning.reread();
    assert!(reread.activated);
    assert_eq!(reread.status, Status::Available);
    assert_eq!(read_id(&provisioning.admit().unwrap()), "read-key-id-2");
}

/// A complete set whose prefix is not the tenant's derived prefix (here it
/// keeps the `:`) is unavailable, whether or not an older generation exists.
#[test]
fn prefix_not_matching_the_derivation_is_unavailable() {
    let host = Host::new(HostRole::BuildServer);
    for key in [KeyRole::Read, KeyRole::Write] {
        host.key(key, 1);
    }
    let mut colon = config_json(1);
    colon["prefix"] = format!("tenant/{TENANT}/").into();
    host.config_value(&colon);
    let mut provisioning = host.provisioning();
    let reread = provisioning.reread();
    assert!(!reread.activated);
    let Unavailable::PrefixMismatch {
        expected, found, ..
    } = unavailable(&reread.status)
    else {
        panic!("{:?}", reread.status);
    };
    assert_eq!(expected, PREFIX);
    assert_eq!(found, &format!("tenant/{TENANT}/"));
    assert!(provisioning.admit().is_none());
    assert!(provisioning.active().is_none());

    host.config(1);
    assert!(provisioning.reread().activated);
    for key in [KeyRole::Read, KeyRole::Write] {
        host.key(key, 2);
    }
    let mut other = config_json(2);
    other["prefix"] = "tenant/acct-SOMEONEELSE/".into();
    host.config_value(&other);
    let reread = provisioning.reread();
    assert!(!reread.activated);
    assert!(matches!(
        unavailable(&reread.status),
        Unavailable::PrefixMismatch { .. }
    ));
    assert!(provisioning.admit().is_none());

    host.config(2);
    assert!(provisioning.reread().activated);
    assert_eq!(
        provisioning.admit().unwrap().generation,
        Generation::Number(2)
    );
}

#[test]
fn no_files_at_all_is_unavailable() {
    let host = Host::new(HostRole::Boat);
    let mut provisioning = host.provisioning();
    let status = provisioning.reread().status;
    let Unavailable::NoCompleteGeneration { detail } = unavailable(&status) else {
        panic!("{status:?}");
    };
    assert!(detail.contains("config.json: missing"), "{detail}");
    assert!(detail.contains("read-key: missing"), "{detail}");
    assert!(!provisioning.enabled());
}

/// Production re-reads happen at least every 10 s and notice a changed file
/// sooner than that.
#[test]
fn production_reread_cadence() {
    assert!(REREAD_INTERVAL <= Duration::from_secs(10));
    assert!(WATCH_POLL < REREAD_INTERVAL);
}

/// On a resume the broker may write the config only after the gateway has
/// started; the watcher picks it up without a restart.
#[tokio::test]
async fn watcher_activates_a_config_written_after_start() {
    let host = Host::new(HostRole::Boat);
    host.key(KeyRole::Read, 1);
    let watch = Watch::new(host.layout.clone());
    let (sender, mut outcomes) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(watch.clone().run(
        Duration::from_millis(10),
        Duration::from_secs(3600),
        move |reread| {
            let _ = sender.send(reread);
        },
    ));
    let first = outcomes.recv().await.unwrap();
    assert!(matches!(
        first.status,
        Status::Unavailable(Unavailable::NoCompleteGeneration { .. })
    ));
    assert!(watch.admit().is_none());

    // Only a metadata change can trigger this re-read: the full re-read
    // interval is an hour.
    host.config(1);
    let activated = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let reread = outcomes.recv().await.unwrap();
            if reread.activated {
                return reread;
            }
        }
    })
    .await
    .expect("the watcher noticed the new config");
    assert_eq!(activated.status, Status::Available);
    assert_eq!(watch.status(), Status::Available);
    assert!(watch.enabled());
    assert_eq!(read_id(&watch.admit().unwrap()), "read-key-id-1");
    task.abort();
}

/// Without any file change, the watcher still re-reads every full interval.
#[tokio::test]
async fn watcher_rereads_at_the_full_interval_without_changes() {
    let host = Host::new(HostRole::TenantVm);
    host.provision(1);
    let watch = Watch::new(host.layout.clone());
    let (sender, mut outcomes) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(watch.clone().run(
        Duration::from_millis(10),
        Duration::from_millis(50),
        move |reread| {
            let _ = sender.send(reread);
        },
    ));
    for _ in 0..3 {
        let reread = tokio::time::timeout(Duration::from_secs(10), outcomes.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reread.status, Status::Available);
    }
    task.abort();
}

/// `motor-job` and `motor-cache` cannot open any key file: as each account,
/// `open()` fails with EACCES (cat reports "Permission denied"), both through
/// the runner's 0700 directory and, with the directory opened for traversal,
/// on the 0400 file itself. A world-readable control file in the same
/// directory shows the second check reaches the file. The runner itself still
/// reads the complete generation.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn job_and_fill_accounts_cannot_open_key_files() {
    use std::process::Command;
    let host = Host::new(HostRole::BuildServer);
    host.provision(1);
    let mut provisioning = host.provisioning();
    assert_eq!(provisioning.reread().status, Status::Available);
    let open_as = |user: &str, path: &Path| {
        Command::new("/usr/bin/sudo")
            .args(["-n", "-u", user, "--", "/usr/bin/cat"])
            .arg(path)
            .output()
            .unwrap()
    };
    let refused = |user: &str, path: &Path| {
        let output = open_as(user, path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{user} opened {}", path.display());
        assert!(
            stderr.contains("Permission denied"),
            "{user} {}: {stderr}",
            path.display()
        );
        assert!(output.stdout.is_empty());
    };
    let accounts = ["motor-job", crate::isolation::WRITER_USER];
    for user in accounts {
        for key in [KeyRole::Read, KeyRole::Write] {
            refused(user, &host.layout.key_path(key));
        }
    }
    fs::set_permissions(host.dir(), fs::Permissions::from_mode(0o711)).unwrap();
    host.write_raw("control", 0o444, b"control");
    for user in accounts {
        let control = open_as(user, &host.dir().join("control"));
        assert!(
            control.status.success(),
            "{user} cannot reach the directory at all: {}",
            String::from_utf8_lossy(&control.stderr)
        );
        for key in [KeyRole::Read, KeyRole::Write] {
            refused(user, &host.layout.key_path(key));
        }
    }
    fs::set_permissions(host.dir(), fs::Permissions::from_mode(0o700)).unwrap();
}
