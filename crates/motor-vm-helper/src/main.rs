#[cfg(target_os = "linux")]
fn main() {
    use motor_vm_helper::linux::{offline, server, LinuxMachine};
    let result = server::dispatch(
        std::env::args_os().skip(1),
        |path| {
            let config = server::load_config(path)?;
            let count = config.slots.len();
            let allowed = config.allowed_uids.clone();
            let gid = config.socket_gid;
            server::serve(LinuxMachine::new(config)?, count, allowed, gid)
        },
        |tenant, resume| {
            let quarantine = offline::remove(tenant, resume)?;
            eprintln!(
                "tenant {tenant}: offline removal complete; data quarantined at {}",
                quarantine.display()
            );
            Ok(())
        },
    );
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
