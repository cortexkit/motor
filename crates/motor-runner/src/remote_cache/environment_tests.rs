//! The remote cache's job environment, checked against the copy the toolchain
//! parity check reads.
use super::*;
use std::path::Path;
use std::process::Command;

/// The Python data file and this module must agree name for name and value for
/// value. The parity check is a separate program that cannot read Rust, so the
/// values exist twice, and this is the test that keeps the copies honest.
#[test]
fn the_parity_copy_names_the_same_variables_with_the_same_values() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../script/runner/remote-cache-environment.py");
    let script = script.canonicalize().unwrap();
    let python = r#"
import importlib.util, json, sys

spec = importlib.util.spec_from_file_location('remote_cache_environment', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
print(json.dumps(
    {role: module.environment(role) for role in module.JOB_ENVIRONMENT_BY_ROLE},
    sort_keys=True,
))
"#;
    let output = Command::new("python3")
        .args(["-c", python])
        .arg(&script)
        .output()
        .expect("run the parity copy with python3");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let written: BTreeMap<String, BTreeMap<String, String>> =
        serde_json::from_slice(&output.stdout).unwrap();
    for (role, expected) in [
        ("build-server", HostRole::BuildServer),
        ("boat", HostRole::Boat),
        ("tenant", HostRole::TenantVm),
    ] {
        let forwarder = Forwarder::new(Tool::Sccache, 4181);
        let here = delta(
            expected,
            &[forwarder],
            "<remote-cache-bucket>",
            "<remote-cache-region>",
        );
        assert_eq!(
            written[role], here,
            "the Rust and Python copies of the {role} delta disagree"
        );
    }
}

/// The switch-on environment of each host type is its switch-off environment
/// plus exactly the delta the spec lists: nothing else changes, and nothing of
/// the other switch state survives.
#[test]
fn each_roles_switch_on_job_environment_is_its_baseline_plus_the_delta() {
    let forwarders = [Forwarder::new(Tool::Sccache, 4181)];
    let baseline = |role: HostRole| -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("CARGO_HOME".to_string(), "/motor-home/cargo".to_string()),
            ("CARGO_NET_OFFLINE".to_string(), "true".to_string()),
        ]);
        if role == HostRole::BuildServer {
            // The build server's read-only local level 1 and its wrapper.
            for (key, value) in [
                ("RUSTC_WRAPPER", "/motor-tools/prefrontal-build-slot"),
                ("SCCACHE_DIR", "/motor-cache"),
                ("SCCACHE_CACHE_SIZE", "200G"),
                ("SCCACHE_LOCAL_RW_MODE", "READ_ONLY"),
                ("SCCACHE_IGNORE_SERVER_IO_ERROR", "1"),
            ] {
                env.insert(key.into(), value.into());
            }
        }
        env
    };
    for role in [HostRole::BuildServer, HostRole::Boat, HostRole::TenantVm] {
        let mut env = baseline(role);
        env.extend(delta(role, &forwarders, "fleet-bucket", "gra"));
        let mut expected = baseline(role);
        for (key, value) in [
            ("SCCACHE_BUCKET", "fleet-bucket"),
            ("SCCACHE_REGION", "gra"),
            ("SCCACHE_ENDPOINT", "http://127.0.0.1:4181"),
            ("SCCACHE_S3_USE_SSL", "false"),
            ("SCCACHE_S3_ENABLE_VIRTUAL_HOST_STYLE", "false"),
            ("SCCACHE_S3_RW_MODE", "READ_ONLY"),
            ("AWS_ACCESS_KEY_ID", "remote-cache"),
            ("AWS_SECRET_ACCESS_KEY", "remote-cache"),
            ("AWS_EC2_METADATA_DISABLED", "true"),
        ] {
            expected.insert(key.into(), value.into());
        }
        match role {
            HostRole::BuildServer => {
                expected.insert("SCCACHE_MULTILEVEL_CHAIN".into(), "disk,s3".into());
            }
            HostRole::Boat | HostRole::TenantVm => {
                // These hosts have no level 1 and no baseline wrapper, so the
                // delta supplies the trusted wrapper itself.
                expected.insert("RUSTC_WRAPPER".into(), "/motor-tools/sccache".into());
                assert!(!expected.contains_key("SCCACHE_DIR"));
                assert!(!env.keys().any(|key| key == "SCCACHE_MULTILEVEL_CHAIN"));
            }
        }
        assert_eq!(env, expected, "{role:?}");
    }
}

/// A fill writes every writable level and waits for each of them, and it is
/// the only unit with read-write access to the store.
#[test]
fn a_fill_adds_the_write_error_policy_and_writes_every_level() {
    assert_eq!(
        fill_values(),
        BTreeMap::from([(
            "SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY".to_string(),
            "all".to_string()
        )])
    );
    assert!(
        !small_values(HostRole::BuildServer).contains_key("SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY")
    );
    assert!(!small_values(HostRole::Boat).contains_key("SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY"));
    // A fill is the only unit that may write, and it is never read-only.
    assert_eq!(READ_ONLY_MODE, "READ_ONLY");
    assert_eq!(READ_WRITE_MODE, "READ_WRITE");
    assert_eq!(
        small_values(HostRole::Boat)["SCCACHE_S3_RW_MODE"],
        READ_ONLY_MODE
    );
}

/// Everything inherited from the runner's own environment is removed: neither
/// an sccache setting nor an AWS credential may survive into a job.
#[test]
fn inherited_sccache_and_aws_variables_are_scrubbed() {
    let mut env = BTreeMap::from([
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("SCCACHE_BUCKET".to_string(), "operator-bucket".to_string()),
        ("SCCACHE_DIR".to_string(), "/operator/cache".to_string()),
        ("SCCACHE_LOG".to_string(), "trace".to_string()),
        ("AWS_ACCESS_KEY_ID".to_string(), "real-key".to_string()),
        (
            "AWS_SECRET_ACCESS_KEY".to_string(),
            "real-secret".to_string(),
        ),
        ("AWS_PROFILE".to_string(), "operator".to_string()),
        ("AWS_SESSION_TOKEN".to_string(), "session".to_string()),
    ]);
    crate::backend::scrub_inherited_cache_environment(&mut env);
    assert_eq!(
        env,
        BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())])
    );
    for prefix in INHERITED_PREFIXES {
        assert!(env.keys().all(|key| !key.starts_with(prefix)));
    }
}

/// A forwarder for a tool the unit does not run must not become the endpoint.
#[test]
fn a_unit_without_an_sccache_socket_gets_no_endpoint() {
    let env = delta(
        HostRole::Boat,
        &[Forwarder::new(Tool::Ccache, 4182)],
        "b",
        "r",
    );
    assert!(!env.contains_key("SCCACHE_ENDPOINT"));
    let env = delta(HostRole::Boat, &[], "b", "r");
    assert!(!env.contains_key("SCCACHE_ENDPOINT"));
}
