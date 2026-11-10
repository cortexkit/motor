#[cfg(target_os = "linux")]
fn main() {
    if let Some(version) = version_output(std::env::args_os().skip(1)) {
        println!("{version}");
        return;
    }
    use motor_vm_helper::linux::server;
    let result = (|| {
        let mut args = std::env::args_os().skip(1);
        let command = args.next();
        if command.as_deref() == Some(std::ffi::OsStr::new("set-remote-cache")) {
            motor_vm_helper::linux::cache::disable_core_dumps()?;
            return motor_vm_helper::linux::cache::install_command(args.collect());
        }
        let (check_only, path) = config_command(command, args)?;
        let config = server::load_config(&path)?;
        if check_only {
            config.validate()?;
            motor_vm_helper::linux::startup_check::check(&config)?;
            println!("helper config settings valid");
            return Ok(());
        }
        motor_vm_helper::linux::cache::disable_core_dumps()?;
        server::serve(config)
    })();
    if let Err(e) = result {
        eprintln!("motor-vm-helper: {e}");
        std::process::exit(1);
    }
}

#[cfg(any(target_os = "linux", test))]
fn config_command(
    command: Option<std::ffi::OsString>,
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> motor_vm_helper::Result<(bool, std::path::PathBuf)> {
    let check_only = command.as_deref() == Some(std::ffi::OsStr::new("check-config"));
    if !check_only && command.as_deref() != Some(std::ffi::OsStr::new("--config")) {
        return Err(motor_vm_helper::Error(
            "usage: motor-vm-helper [--config | check-config] /etc/motor-vm-helper.json; \
             check-config is read-only; existing job records are parsed but not revalidated \
             against new settings until service startup"
                .into(),
        ));
    }
    let path = args
        .next()
        .ok_or_else(|| motor_vm_helper::Error("missing config path".into()))?;
    if args.next().is_some() {
        return Err(motor_vm_helper::Error("unexpected arguments".into()));
    }
    Ok((check_only, path.into()))
}

#[cfg(not(target_os = "linux"))]
fn main() {
    if let Some(version) = version_output(std::env::args_os().skip(1)) {
        println!("{version}");
        return;
    }
    eprintln!("motor-vm-helper requires Linux, cgroup v2 and systemd DelegateSubgroup");
    std::process::exit(1);
}

fn version_output(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<String> {
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--version")) || args.next().is_some() {
        return None;
    }
    Some(format!(
        "motor-vm-helper {} {}",
        env!("CARGO_PKG_VERSION"),
        option_env!("MOTOR_BUILD_COMMIT").unwrap_or("unstamped")
    ))
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn config_command_distinguishes_read_only_preflight_and_rejects_missing_or_extra_paths() {
        for (command, check_only) in [("check-config", true), ("--config", false)] {
            let parse = |paths: &[&str]| {
                super::config_command(
                    Some(command.into()),
                    paths.iter().map(std::ffi::OsString::from),
                )
            };
            let (check, path) = parse(&["/etc/helper.json"]).unwrap();
            assert_eq!(check, check_only);
            assert_eq!(path, std::path::Path::new("/etc/helper.json"));
            assert_eq!(parse(&[]).unwrap_err().0, "missing config path");
            assert_eq!(
                parse(&["one", "two"]).unwrap_err().0,
                "unexpected arguments"
            );
        }
        let error = super::config_command(None, std::iter::empty())
            .unwrap_err()
            .0;
        assert!(error.contains("check-config is read-only"));
        assert!(error.contains("not revalidated"));
    }

    #[test]
    fn version_is_an_exact_early_flag_and_reports_the_compile_time_stamp() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
                .into_iter()
        };
        let output = super::version_output(args(&["--version"])).unwrap();
        assert_eq!(
            output,
            format!(
                "motor-vm-helper 0.1.0 {}",
                option_env!("MOTOR_BUILD_COMMIT").unwrap_or("unstamped")
            )
        );
        for values in [
            vec![],
            vec!["--config", "helper.json"],
            vec!["--version", "helper.json"],
            vec!["--version-extra"],
        ] {
            assert!(super::version_output(args(&values)).is_none());
        }
    }
}
