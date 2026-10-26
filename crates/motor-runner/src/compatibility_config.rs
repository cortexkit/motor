//! Test-only configuration for selecting the identity compatibility scenarios.
use crate::{caller_path::PathPlan, Config};
use std::{fs, path::Path};

pub(crate) fn modes() -> &'static [bool] {
    match std::env::var("MOTOR_RUNNER_COMPAT_CALLER_ROOT").as_deref() {
        Ok("off") => &[false],
        Ok("on") if cfg!(target_os = "linux") => &[true],
        Err(std::env::VarError::NotPresent) if cfg!(target_os = "linux") => &[false, true],
        Err(std::env::VarError::NotPresent) => &[false],
        other => panic!("invalid MOTOR_RUNNER_COMPAT_CALLER_ROOT mode: {other:?}"),
    }
}

pub(crate) fn configure(config: &mut Config, scratch: &Path, enabled: bool) {
    assert!(config.caller_roots.is_empty());
    if enabled {
        let host = scratch.join("callers/home");
        fs::create_dir_all(&host).unwrap();
        config
            .caller_roots
            .push(crate::CallerRoot::parse(&format!("/home={}", host.display())).unwrap());
        config.test_allow_unisolated_caller_roots = true;
    }
    config.check_options().unwrap();
    assert_eq!(config.caller_roots.len(), usize::from(enabled));
    eprintln!(
        "compatibility caller-root mode: {}",
        if enabled { "on" } else { "off" }
    );
}

/// Assert the real plan remains identity under the fixture's configured roots.
/// Wire encoders and the argv finalizer do not take Config; their fixtures check
/// the namespace here before supplying those functions with the planned paths.
pub(crate) fn assert_identity(config: &Config, plan: PathPlan<'_>) {
    let caller = Path::new(plan.caller.text());
    for root in &config.caller_roots {
        assert!(!caller.starts_with(&root.caller_prefix));
        assert!(!caller.starts_with(&root.host_root));
    }
    assert_eq!(plan.host.as_path().as_os_str(), caller.as_os_str());
}
