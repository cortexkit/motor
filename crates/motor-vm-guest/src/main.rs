#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let port = match args.as_slice() {
        [_] => 52,
        [_, flag, value] if flag == "--port" => value
            .parse()
            .map_err(|_| motor_vm_guest::invalid("invalid agent port"))?,
        _ => return Err(motor_vm_guest::invalid("usage: motor-vm-guest [--port N]")),
    };
    motor_vm_guest::linux::serve(port)
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("motor-vm-guest requires Linux");
    std::process::exit(1);
}
