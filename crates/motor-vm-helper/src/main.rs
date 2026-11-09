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
        motor_vm_helper::linux::cache::disable_core_dumps()?;
        if command.as_deref() == Some(std::ffi::OsStr::new("set-remote-cache")) {
            return motor_vm_helper::linux::cache::install_command(args.collect());
        }
        if command.as_deref() != Some(std::ffi::OsStr::new("--config")) {
            return Err(motor_vm_helper::Error(
                "usage: motor-vm-helper --config /etc/motor-vm-helper.json".into(),
            ));
        }
        let path = args
            .next()
            .ok_or_else(|| motor_vm_helper::Error("missing config path".into()))?;
        if args.next().is_some() {
            return Err(motor_vm_helper::Error("unexpected arguments".into()));
        }
        server::serve(server::load_config(std::path::Path::new(&path))?)
    })();
    if let Err(e) = result {
        eprintln!("motor-vm-helper: {e}");
        std::process::exit(1);
    }
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
