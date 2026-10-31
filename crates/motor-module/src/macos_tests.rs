use crate::{Config, MacosRunner};
use motor_protocol::Platform;
use serde_json::json;

fn runner() -> MacosRunner {
    MacosRunner {
        host: "macos.example.invalid".into(),
        port: 2222,
        user: "motor".into(),
        host_key: ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .to_openssh()
            .unwrap(),
        instance: "macos-test".into(),
        state_root: "/Users/motor/state".into(),
    }
}

#[test]
fn macos_config_defaults_omit_endpoint_defaults_and_retired_linux_keys() {
    let config = Config {
        worktree_roots: vec!["/worktrees".into()],
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_string(&config).unwrap(),
        r#"{"runner_instance":"main","worktree_roots":["/worktrees"],"ssh_config_override":null,"inject_tree_hash_mismatch":false,"inject_snapshot_failure":false}"#
    );
    assert_eq!(
        serde_json::to_value(MacosRunner::default()).unwrap(),
        json!({})
    );
    let restored: Config = serde_json::from_value(json!({})).unwrap();
    assert!(restored.macos_runner.is_none());
    let configured = Config {
        macos_runner: Some(runner()),
        ..config
    };
    let restored: Config =
        serde_json::from_str(&serde_json::to_string(&configured).unwrap()).unwrap();
    assert_eq!(restored.macos_runner, configured.macos_runner);
    assert!(serde_json::from_value::<MacosRunner>(json!({"max_concurrent": 2})).is_err());
}

#[test]
fn macos_config_validates_account_endpoint_and_distinct_instances() {
    let config = Config {
        macos_runner: Some(runner()),
        ..Default::default()
    };
    config.validate().unwrap();
    for user in ["root", "rentamac", "", "Motor", "motorjob", "worker"] {
        let mut invalid = config.clone();
        invalid.macos_runner.as_mut().unwrap().user = user.into();
        assert!(invalid.validate().is_err(), "accepted user {user:?}");
    }
    for instance in ["", "mac.os", "mac/os", "mac os", "mac;os"] {
        let mut invalid = config.clone();
        invalid.macos_runner.as_mut().unwrap().instance = instance.into();
        assert!(
            invalid.validate().is_err(),
            "accepted instance {instance:?}"
        );
    }
    for root in [
        "relative",
        "",
        "/Users/motor/state space",
        "/Users/motor/$HOME",
        "/Users/motör",
    ] {
        let mut invalid = config.clone();
        invalid.macos_runner.as_mut().unwrap().state_root = root.into();
        assert!(invalid.validate().is_err(), "accepted state root {root:?}");
    }
    for key in ["", "not a key", "ssh-ed25519 broken\nextra"] {
        let mut invalid = config.clone();
        invalid.macos_runner.as_mut().unwrap().host_key = key.into();
        assert!(invalid.validate().is_err(), "accepted host key {key:?}");
    }
    for host in ["", "-host", "host;command", "host\nother"] {
        let mut invalid = config.clone();
        invalid.macos_runner.as_mut().unwrap().host = host.into();
        assert!(invalid.validate().is_err(), "accepted host {host:?}");
    }
    let mut invalid = config.clone();
    invalid.macos_runner.as_mut().unwrap().port = 0;
    assert!(invalid.validate().is_err());
    let instance = config.macos_runner.as_ref().unwrap().instance.clone();
    let mut invalid = config.clone();
    invalid.runner_instance = instance.clone();
    assert!(invalid.validate().is_err());
    let mut invalid = config.clone();
    invalid.windows_runner_instance = Some(instance.clone());
    invalid.windows_runner_state_root = Some("/windows".into());
    assert!(invalid.validate().is_err());
    let mut invalid = config.clone();
    let macos = config.macos_runner.as_ref().unwrap();
    invalid.windows_runners.push(crate::WindowsRunner {
        name: "windows".into(),
        host: "windows.example.invalid".into(),
        user: "worker".into(),
        instance,
        state_root: "/windows".into(),
        host_key: macos.host_key.clone(),
        ..Default::default()
    });
    assert!(invalid.validate().is_err());
}

#[test]
fn macos_manifest_and_target_require_the_optional_block() {
    let mut config = Config::default();
    let provides = |config: &Config| {
        crate::module::manifest_with_config("ck-motor", config)
            .capabilities
            .unwrap()
            .provides
    };
    assert!(!provides(&config).contains(&"exec-remote-macos/v1".into()));
    assert_eq!(
        config
            .target(Some(&Platform::Macos))
            .err()
            .unwrap()
            .to_string(),
        "requested platform macos is not configured"
    );
    config.macos_runner = Some(runner());
    assert!(provides(&config).contains(&"exec-remote-macos/v1".into()));
    let target = config.target(Some(&Platform::Macos)).unwrap();
    assert_eq!(target.instance, "macos-test");
    assert_eq!(target.state_root.as_deref(), Some("/Users/motor/state"));
    assert_eq!(target.macos, config.macos_runner);
    assert!(target.windows.is_none());
    assert_eq!(
        config.stored_target(Some("macos-test")).unwrap().macos,
        config.macos_runner
    );
}

#[test]
fn platform_arguments_parse_and_reject_macos_hosted_or_spill_in_either_order() {
    use crate::cli::{parse_args, Invocation};
    let words = |text: &str| {
        text.split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    for (name, expected) in [
        ("linux", Platform::Linux),
        ("windows", Platform::Windows),
        ("macos", Platform::Macos),
    ] {
        for flags in [format!("--platform {name}"), format!("--platform={name}")] {
            let Invocation::Run(args) = parse_args(&words(&format!("{flags} -- true"))).unwrap()
            else {
                panic!()
            };
            assert_eq!(args.platform, Some(expected.clone()));
        }
    }
    for flag in ["--hosted", "--allow-spill"] {
        for args in [
            format!("--platform macos {flag} -- true"),
            format!("{flag} --platform=macos -- true"),
        ] {
            assert!(parse_args(&words(&args))
                .unwrap_err()
                .contains("--platform macos cannot be combined"));
        }
    }
    for args in [
        "--platform",
        "--platform= -- true",
        "--platform darwin -- true",
    ] {
        assert!(parse_args(&words(args)).is_err(), "accepted {args}");
    }
}

mod routing;
