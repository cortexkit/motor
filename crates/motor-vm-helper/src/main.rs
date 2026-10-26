#[cfg(target_os = "linux")]
fn main() {
    use motor_vm_helper::linux::server;
    let result = (|| {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref() != Some(std::ffi::OsStr::new("--config")) {
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
    eprintln!("motor-vm-helper requires Linux, cgroup v2 and systemd DelegateSubgroup");
    std::process::exit(1);
}
