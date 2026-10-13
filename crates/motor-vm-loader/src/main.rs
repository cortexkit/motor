#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    use std::os::fd::FromRawFd;
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 || args[1] != "--memory-fd" || args[3] != "--listen-fd" {
        return Err(motor_vm_loader::invalid(
            "usage: motor-vm-loader --memory-fd N --listen-fd N",
        ));
    }
    let parse = |s: &str| {
        s.parse::<i32>()
            .ok()
            .filter(|n| *n >= 3)
            .ok_or_else(|| motor_vm_loader::invalid("invalid inherited fd"))
    };
    let memory = parse(&args[2])?;
    let listen = parse(&args[4])?;
    if memory == listen {
        return Err(motor_vm_loader::invalid("fds must differ"));
    }
    unsafe {
        motor_vm_loader::linux::run(
            std::fs::File::from_raw_fd(memory),
            std::os::unix::net::UnixListener::from_raw_fd(listen),
        )
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("motor-vm-loader requires Linux");
    std::process::exit(1);
}
