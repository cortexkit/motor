//! Recovery boots have no network or runner. Their only account input is a disk.
use super::*;

pub(super) fn enabled() -> io::Result<bool> {
    Ok(fs::read_to_string("/proc/cmdline")?
        .split_whitespace()
        .any(|w| w == "motor.recovery=1"))
}

pub(super) fn run(budget_ms: u64) -> io::Result<Value> {
    if !enabled()? {
        return Err(io::Error::other("not a recovery VM"));
    }
    let marker = Path::new("/run/motor-workspace-recovered");
    if let Ok(bytes) = fs::read(marker) {
        return serde_json::from_slice(&bytes).map_err(io::Error::other);
    }
    let deadline = recovery_deadline(Instant::now(), Some(budget_ms));
    fs::create_dir_all("/tenant")?;
    paths::Directory::absolute(Path::new("/tenant"), 0)?;
    let outcome = recover_disk(&mut GuestDisk { deadline })?;
    let report = json!({"ok":true,"recovery":outcome});
    fs::write(marker, serde_json::to_vec(&report)?)?;
    Ok(report)
}

trait Disk {
    fn command(&mut self, program: &str, args: &[&str]) -> io::Result<()>;
    fn mounted(&self) -> io::Result<bool>;
    fn stores(&mut self) -> io::Result<()>;
    fn cleanup(&mut self) -> io::Result<()>;
}

struct GuestDisk {
    deadline: Instant,
}
impl Disk for GuestDisk {
    fn command(&mut self, program: &str, args: &[&str]) -> io::Result<()> {
        recovery_command(self.deadline, program, program, args)
    }
    fn mounted(&self) -> io::Result<bool> {
        snapshot::mounted("/tenant")
    }
    fn stores(&mut self) -> io::Result<()> {
        check_tenant_stores(self.deadline)
    }
    fn cleanup(&mut self) -> io::Result<()> {
        paths::prepare_disk_layout(Path::new("/tenant"), 0)?.clear_execution_history()
    }
}

fn unmount(disk: &mut impl Disk) -> io::Result<()> {
    if disk.mounted()? {
        disk.command("umount", &["/tenant"])?;
    }
    Ok(())
}
fn mount(disk: &mut impl Disk) -> io::Result<()> {
    disk.command("mount", &["-t", "xfs", "/dev/vdb", "/tenant"])
}
fn check(disk: &mut impl Disk) -> io::Result<()> {
    disk.command("xfs_repair", &["-n", "/dev/vdb"])
}
fn recover_disk(disk: &mut impl Disk) -> io::Result<&'static str> {
    unmount(disk)?;
    let clean = check(disk)
        .and_then(|_| mount(disk))
        .and_then(|_| disk.stores());
    let outcome = match clean {
        Ok(()) => "clean",
        Err(_) => {
            unmount(disk)?;
            let repaired = recovery_steps(|step| match step {
                "replay" | "remount" => mount(disk),
                "unmount" => unmount(disk),
                "xfs_check" => check(disk),
                "stores" => disk.stores(),
                _ => unreachable!(),
            });
            if repaired.is_ok() {
                "repaired"
            } else {
                unmount(disk)?;
                disk.command("mkfs.xfs", &["-f", "/dev/vdb"])?;
                mount(disk)?;
                "reformatted"
            }
        }
    };
    disk.cleanup()?;
    disk.command("sync", &[])?;
    unmount(disk)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct NoNetwork {
        checks_to_fail: u32,
        mounted: bool,
        cleaned: bool,
        fail: &'static str,
        commands: Vec<String>,
    }
    impl Disk for NoNetwork {
        fn command(&mut self, program: &str, args: &[&str]) -> io::Result<()> {
            // Model a guest with no eth0 and no job services. Unexpected
            // programs (including ip or systemctl) must fail this test.
            assert!(matches!(
                program,
                "xfs_repair" | "mount" | "umount" | "mkfs.xfs" | "sync"
            ));
            self.commands.push(program.into());
            if program == self.fail {
                return Err(io::Error::other("injected command failure"));
            }
            if program == "xfs_repair" && self.checks_to_fail > 0 {
                self.checks_to_fail -= 1;
                return Err(io::Error::other("damaged disk"));
            }
            if program == "mount" {
                assert_eq!(args, ["-t", "xfs", "/dev/vdb", "/tenant"]);
                self.mounted = true;
            }
            if program == "umount" {
                self.mounted = false;
            }
            Ok(())
        }
        fn mounted(&self) -> io::Result<bool> {
            Ok(self.mounted)
        }
        fn stores(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn cleanup(&mut self) -> io::Result<()> {
            if self.fail == "cleanup" {
                return Err(io::Error::other("injected cleanup failure"));
            }
            self.cleaned = true;
            Ok(())
        }
    }

    #[test]
    fn recovery_without_eth0_never_sets_up_network_or_starts_runner() {
        for (checks_to_fail, path) in [(0, "clean"), (1, "repaired"), (2, "reformatted")] {
            let mut disk = NoNetwork {
                checks_to_fail,
                ..Default::default()
            };
            let result = recover_disk(&mut disk).unwrap();
            assert_eq!(result, path);
            assert!(disk.cleaned);
            assert!(!disk.mounted);
            assert_eq!(
                disk.commands.contains(&"mkfs.xfs".into()),
                path == "reformatted"
            );
        }
    }

    #[test]
    fn failed_reformat_or_cleanup_never_reports_success() {
        for fail in ["mkfs.xfs", "cleanup", "sync", "umount"] {
            let mut disk = NoNetwork {
                fail,
                checks_to_fail: 2,
                ..Default::default()
            };
            assert!(recover_disk(&mut disk).is_err());
        }
    }
}
