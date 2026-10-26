use super::*;

fn missing(_: &Path) -> io::Result<PathBuf> {
    Err(io::ErrorKind::NotFound.into())
}

fn alias(path: &Path) -> io::Result<PathBuf> {
    match path.to_str().unwrap() {
        "/home/u/link" => Ok("/mnt/c/project".into()),
        "/mnt/c/link" => Ok("/home/u/project".into()),
        _ => missing(path),
    }
}

#[test]
fn wsl_probe_and_mount_matrix() {
    for probe in [
        Probe::Interop,
        Probe::InteropLate,
        Probe::DistroName,
        Probe::Release,
        Probe::None,
    ] {
        let checks = Checks::test(probe).with_canonicalize(missing);
        for (path, drive) in [
            ("/mnt/c", true),
            ("/mnt/C/x", true),
            ("/mnt//z/./x", true),
            ("/opt/../mnt/c/x", true),
            ("/../mnt/c/x", true),
            ("/mnt/data", false),
            ("/mnt/cc", false),
            ("/mnt/c2", false),
            ("/mnt/é", false),
            ("/mnt/c/../data", false),
            ("/home/u/project", false),
            ("/c/project", false),
        ] {
            let expected = drive && !matches!(probe, Probe::None);
            let refusal = checks.refusal([Path::new(path)]);
            assert_eq!(refusal.is_some(), expected, "{probe:?}: {path}");
            if let Some(message) = refusal {
                assert_eq!(message, format!("ck-motor: this checkout is on a Windows drive ({path}). Under WSL, move it into the WSL filesystem (for example under ~/) and run ck motor there."));
            }
        }
    }
}

#[test]
fn wsl_checks_both_alias_directions_but_reports_supplied_spelling() {
    let checks = Checks::test(Probe::Interop).with_canonicalize(alias);
    for path in ["/home/u/link", "/mnt/c/link"] {
        assert_eq!(checks.refusal([Path::new(path)]), Some(format!("ck-motor: this checkout is on a Windows drive ({path}). Under WSL, move it into the WSL filesystem (for example under ~/) and run ck motor there.")));
    }
    let checks = Checks::test(Probe::None).with_canonicalize(|_| {
        panic!("a non-WSL host must not canonicalize paths for the WSL check")
    });
    assert_eq!(checks.refusal([Path::new("/mnt/c")]), None);
}

#[test]
fn home_jobs_are_eligible_for_spill_and_shadow_like_users_jobs() {
    let config = crate::Config {
        spill_to_hosted: true,
        spill_avoid_host: Some("build-server".into()),
        shadow_to_hosted: crate::ShadowConfig {
            enabled: true,
            sample_rate: 1.0,
            max_concurrent: 1,
        },
        ..Default::default()
    };
    for prefix in ["/Users", "/home"] {
        let request = motor_protocol::RunRequest::new(
            format!("{prefix}/u/project"),
            format!("{prefix}/u/project"),
            format!("{prefix}/u/project/sub"),
            "true",
        )
        .with_timeout(60)
        .with_siblings(vec![format!("{prefix}/u/lib")]);
        assert!(crate::hosted::validate(&request).is_ok(), "{prefix}");
        assert!(
            crate::spill::eligible(&config, &request, false, false),
            "{prefix}"
        );
        assert!(
            crate::shadow::eligible(&config, &request, false, false),
            "{prefix}"
        );
        assert!(!crate::spill::eligible(&config, &request, true, false));
        assert!(!crate::shadow::eligible(&config, &request, true, false));
    }
}
