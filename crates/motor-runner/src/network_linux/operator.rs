use super::{operator_generation_path, OperatorGenerationGuard};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Validate the live policy using the producer's checker, then publish only a
/// private test generation. Never load policy or change the host's readiness.
pub(crate) fn validated_generation(root: &Path) -> (OperatorGenerationGuard, PathBuf) {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "run operator tests as the non-root operator account"
    );
    let settings = match (std::env::var("MOTOR_NET_TEST_UPLINK"), std::env::var("MOTOR_NET_TEST_PUBLIC_IPV4")) {
        (Ok(uplink), Ok(public_ipv4)) => serde_json::json!({"uplink":uplink,"public_ipv4":public_ipv4}).to_string(),
        (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => String::new(),
        _ => panic!("set both MOTOR_NET_TEST_UPLINK and MOTOR_NET_TEST_PUBLIC_IPV4, or neither to use /etc/motor-network.json"),
    };
    let producer = root.join("operator-policy");
    fs::create_dir(&producer).unwrap();
    for (name, source) in [
        (
            "install-job-network.py",
            include_str!("../../../../script/runner/install-job-network.py"),
        ),
        (
            "motor_network_policy.py",
            include_str!("../../../../script/runner/motor_network_policy.py"),
        ),
        (
            "motor_network_ready.py",
            include_str!("../../../../script/runner/motor_network_ready.py"),
        ),
        (
            "motor-net.nft",
            include_str!("../../../../script/runner/motor-net.nft"),
        ),
    ] {
        fs::write(producer.join(name), source).unwrap();
    }
    let result = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "/usr/bin/python3",
            "-I",
            "-c",
            r#"import json,pathlib,runpy,sys
p=pathlib.Path(sys.argv[1])
ns=runpy.run_path(str(p/'install-job-network.py'))
settings=json.loads(sys.argv[2]) if sys.argv[2] else None
ns['check_live'](settings=settings)
ready=ns['tenant_policy'].readiness
ready.READY=p/'ready'
ready.publish()
print('PASS: read-only live policy validation; published private operator generation')
"#,
        ])
        .arg(&producer)
        .arg(settings)
        .output()
        .unwrap();
    assert!(result.status.success(),
        "read-only operator network validation failed (no policy was applied): {}\n{}\nIf installed settings are absent, set MOTOR_NET_TEST_UPLINK and MOTOR_NET_TEST_PUBLIC_IPV4 explicitly. If live policy differs, deploy the producer, rerun isolated_net_, then deploy the runner.",
        String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
    let marker = producer.join("ready");
    (operator_generation_path(marker.clone()), marker)
}
