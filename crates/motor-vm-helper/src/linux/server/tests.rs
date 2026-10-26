use super::*;
use crate::{tenant, Kind, Lease, Machine, Peer};
use std::{io::Write, os::fd::OwnedFd, path::PathBuf, process::Command};

struct FencedMachine {
    config: crate::config::Config,
    tenants: tenant::Controller<tenant::tests::Fake>,
}

impl FencedMachine {
    fn new(missing: bool) -> Self {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/helper.json")).unwrap();
        let problem = if missing {
            value.as_object_mut().unwrap().remove("uplink");
            "uplink <missing>: invalid uplink"
        } else {
            value["uplink"] = serde_json::json!("bad/name");
            "uplink bad/name: invalid uplink"
        };
        let config = serde_json::from_value(value).unwrap();
        Self {
            config,
            tenants: tenant::tests::degraded_service_fixture(problem),
        }
    }
}

impl Machine for FencedMachine {
    fn reconcile(&mut self) -> Result<()> {
        self.config.validate_resources()?;
        super::super::tenant_network::reconciliation_tests::fenced_startup(&self.config)?
            .serve_cleanup();
        Ok(())
    }
    fn start(&mut self, _: usize, _: &Lease, _: Kind) -> Result<Vec<OwnedFd>> {
        panic!("fixture must not start a slot")
    }
    fn stop(&mut self, _: usize, _: &Lease, _: bool) -> Result<()> {
        unreachable!()
    }
    fn cleanup(&mut self, _: usize) -> Result<()> {
        unreachable!()
    }
    fn alive(&mut self, _: usize) -> bool {
        false
    }
    fn tenant(&mut self, name: &str, op: tenant::Operation) -> Result<Response> {
        self.tenants.handle(name, op)
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = cortexkit_test_support::scratch_root().join(format!(
            "hs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn paths(root: &Path) -> crate::removal::Paths {
    crate::removal::Paths {
        root: root.into(),
        registry: root.join("registry"),
        provision: root.join("provision"),
        bindings: root.join("bindings.json"),
    }
}

fn request(stream: &mut BufReader<UnixStream>, op: &str) -> serde_json::Value {
    writeln!(stream.get_mut(), "{{\"op\":\"{op}\",\"tenant\":\"a\"}}").unwrap();
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn invalid_or_missing_uplink_binds_fences_and_serves_cleanup_but_refuses_opening() {
    for missing in [false, true] {
        let scratch = Scratch::new();
        let socket = scratch.0.join("helper.sock");
        let (helper, listener) =
            initialize_helper(FencedMachine::new(missing), 1, vec![1000], &socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let id = helper
                .lock()
                .unwrap()
                .connect(Peer {
                    uid: 1000,
                    gid: 1000,
                })
                .unwrap();
            connection_loop(&helper, id, stream).unwrap();
        });
        let stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut client = BufReader::new(stream);
        assert_eq!(request(&mut client, "adopt")["result"], "tenant");
        assert_eq!(request(&mut client, "stop_tenant")["result"], "tenant");
        for op in ["ensure", "restore", "prepare_start", "set_egress"] {
            let refusal = request(&mut client, op);
            assert_eq!(refusal["result"], "start_refused", "{op}: {refusal}");
            assert_eq!(refusal["case"], "uplink");
            assert!(refusal["message"].as_str().unwrap().contains(if missing {
                "<missing>"
            } else {
                "bad/name"
            }));
        }
        assert_eq!(request(&mut client, "remove_tenant")["result"], "removed");
        // The fake backend now has no registry, so this second request must
        // exercise Controller's interrupted-removal route over the same socket.
        assert_eq!(request(&mut client, "remove_tenant")["result"], "removed");
        drop(client);
        server.join().unwrap();
    }
}

#[test]
fn qualification_helper_subprocess() {
    let Some(root) = std::env::var_os("MOTOR_TEST_QUALIFICATION_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    // Requiring provision.lock at startup conflicts with qualification, which
    // holds that lock until its helper has started and exited. The legacy
    // branch reproduces that conflict; normal startup must bind without it.
    let _legacy_lock = if std::env::var_os("MOTOR_TEST_LEGACY_STARTUP").is_some() {
        Some(crate::removal::lock(&paths(&root)).unwrap())
    } else {
        None
    };
    let (_helper, _listener) = initialize_helper(
        FencedMachine::new(true),
        1,
        vec![1000],
        &root.join("helper.sock"),
    )
    .unwrap();
    assert!(UnixStream::connect(root.join("helper.sock")).is_ok());
    println!("QUALIFICATION_HELPER_BOUND");
}

#[test]
fn qualification_lock_allows_helper_startup_and_reproduces_legacy_lock_failure() {
    let scratch = Scratch::new();
    let _qualification_lock = crate::removal::lock(&paths(&scratch.0)).unwrap();
    let child = |legacy| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "linux::server::tests::qualification_helper_subprocess",
                "--nocapture",
            ])
            .env("MOTOR_TEST_QUALIFICATION_ROOT", &scratch.0)
            .env_remove("MOTOR_TEST_LEGACY_STARTUP");
        if legacy {
            command.env("MOTOR_TEST_LEGACY_STARTUP", "1");
        }
        command.output().unwrap()
    };
    let legacy = child(true);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&legacy.stdout),
        String::from_utf8_lossy(&legacy.stderr)
    );
    assert!(
        !legacy.status.success(),
        "the old wrapper must conflict with qualification"
    );
    assert!(output.contains("provisioning is in progress"), "{output}");
    assert!(!output.contains("QUALIFICATION_HELPER_BOUND"));
    let current = child(false);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&current.stdout),
        String::from_utf8_lossy(&current.stderr)
    );
    assert!(current.status.success(), "{output}");
    assert!(output.contains("QUALIFICATION_HELPER_BOUND"), "{output}");
}
