//! The operator's build disk probe: `motor-macvm build-disk-probe <sealed-image>
//! <work-directory>`, run on the macOS VM host without an installed runner.
//!
//! For each way of attaching the build disk (virtio block, then USB mass
//! storage) it boots two job VMs one after the other, each from a fresh APFS
//! clone of the sealed image, through the same job helper the runner starts.
//! The image's own guest agent finds the blank build disk in the first VM,
//! formats and mounts it at the checkout's `target/` and runs a command that
//! prints the mount line and writes a marker file. The second VM gets the first
//! one's disk, exactly as the runner hands a checkout's kept disk to its next
//! job; its agent mounts the existing volume (or prints why it could not and
//! reformats) and the command prints the mount line and reads the marker.
//!
//! Everything lives under the work directory, which must not exist yet. The
//! sealed image is only cloned, never written, and no runner state is read or
//! changed. Each build disk is a sparse file of `BUILD_DISK_SIZE`: the agent
//! recognizes the build disk by that exact size.
use crate::{
    BUILD_DISK, BUILD_DISK_BUS, BuildConfig, CARGO_HOME, DiskBus, JobVmConfig, JobVmRequest,
    SessionAcceptor, ShareLocation,
};
use motor_winagent::{Command, Frame, Peer, Profile, Protocol, Stream};
use std::io::{self, BufRead, Read};
use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

/// Where the probe's checkout appears in the guest. The agent only accepts
/// workspaces below /Users.
pub const CHECKOUT: &str = "/Users/motor-probe/checkout";

/// Prints the line `mount` shows for `target/` and what `diskutil` says about
/// the volume's device: whether macOS sees it as internal or removable and
/// whether the volume is encrypted.
const DESCRIBE: &str = r#"line=$(mount | grep -F " on $PWD/target (" || true)
echo "mount: ${line:-nothing is mounted at target}"
device=${line%% on *}
if [ -n "$line" ]; then
  /usr/sbin/diskutil info "$device" | grep -E 'Protocol|Device Location|Removable Media|Encrypted|FileVault|Locked' | sed 's/^ */diskutil: /'
fi
"#;

/// The first VM's command: describe the volume, then write the marker.
pub fn first_command(marker: &str) -> String {
    format!(
        "{DESCRIBE}printf '%s\\n' '{marker}' > target/probe-marker\necho \"wrote: $(cat target/probe-marker)\"\n"
    )
}

/// The second VM's command: describe the volume, then read the marker.
pub fn second_command() -> String {
    format!("{DESCRIBE}echo \"read: $(cat target/probe-marker 2>&1)\"\n")
}

/// What one probe VM reported.
#[derive(Debug, Default)]
pub struct Run {
    pub stdout: String,
    pub stderr: String,
    /// The agent's Exit or Signal; `None` when the session ended without one.
    pub terminal: Option<Frame>,
    /// Why the session failed, if it did.
    pub failure: Option<String>,
}

/// Whether the second VM mounted the volume the first one formatted. Returns
/// the reason when it did not.
pub fn verdict(marker: &str, first: &Run, second: &Run) -> Result<(), String> {
    let clean = |run: &Run, which: &str| -> Result<(), String> {
        if let Some(failure) = &run.failure {
            return Err(format!("{which} VM: {failure}"));
        }
        if run.terminal != Some(Frame::Exit { code: 0 }) {
            return Err(format!("{which} VM ended with {:?}", run.terminal));
        }
        Ok(())
    };
    clean(first, "first")?;
    if !first.stdout.contains(&format!("wrote: {marker}")) {
        return Err("first VM did not write the marker".into());
    }
    clean(second, "second")?;
    if let Some(line) = second
        .stderr
        .lines()
        .find(|line| line.contains("build disk could not be used"))
    {
        return Err(format!("second VM reformatted the disk: {line}"));
    }
    if !second.stdout.contains(&format!("read: {marker}")) {
        return Err("second VM did not read the first VM's marker".into());
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut random = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

/// Runs the probe. Returns an error when the production attachment
/// (`BUILD_DISK_BUS`) did not let the second VM mount the first VM's volume.
pub fn run(image: &Path, work: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !work.is_absolute() || !image.is_absolute() {
        return Err(invalid(
            "the image and work directory must be absolute paths",
        ));
    }
    std::fs::DirBuilder::new().mode(0o700).create(work)?;
    let helper = std::env::current_exe()?;
    let mut production = Err(invalid("the production attachment was not probed"));
    for bus in [DiskBus::Virtio, DiskBus::Usb] {
        println!("== build disk attached as {} ==", bus.name());
        let directory = work.join(bus.name());
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let marker = format!("motor-probe-{}", random_hex(8)?);
        let first = boot(
            &helper,
            image,
            &directory,
            "first",
            bus,
            None,
            &first_command(&marker),
        );
        let disk = directory.join("first").join(BUILD_DISK);
        let second = if first.failure.is_none() {
            boot(
                &helper,
                image,
                &directory,
                "second",
                bus,
                Some(&disk),
                &second_command(),
            )
        } else {
            Run {
                failure: Some("not started: the first VM failed".into()),
                ..Run::default()
            }
        };
        let result = verdict(&marker, &first, &second);
        match &result {
            Ok(()) => println!(
                "RESULT {}: the second VM mounted the first VM's volume",
                bus.name()
            ),
            Err(reason) => println!("RESULT {}: not reused: {reason}", bus.name()),
        }
        if bus == BUILD_DISK_BUS {
            production = result.map_err(invalid);
        }
    }
    production
}

/// Boots one job VM through the job helper and runs `command` in it. Never
/// fails outright: every problem is recorded in the returned `Run`.
fn boot(
    helper: &Path,
    image: &Path,
    directory: &Path,
    name: &str,
    bus: DiskBus,
    disk: Option<&Path>,
    command: &str,
) -> Run {
    println!("-- {name} VM --");
    let mut run = Run::default();
    if let Err(error) = boot_with(helper, image, directory, name, bus, disk, command, &mut run) {
        run.failure = Some(error.to_string());
    }
    match &run.failure {
        Some(failure) => println!("{name} VM failed: {failure}"),
        None => println!("{name} VM ended with {:?}", run.terminal),
    }
    run
}

#[allow(clippy::too_many_arguments)]
fn boot_with(
    helper: &Path,
    image: &Path,
    directory: &Path,
    name: &str,
    bus: DiskBus,
    disk: Option<&Path>,
    command: &str,
    run: &mut Run,
) -> io::Result<()> {
    let clone = directory.join(name);
    crate::clone_image(image, &clone)?;
    if let Some(disk) = disk {
        // As the runner does with a kept disk: replace the blank one.
        std::fs::rename(disk, clone.join(BUILD_DISK))?;
    }
    let workspace = directory.join(format!("{name}-checkout"));
    std::fs::create_dir_all(workspace.join("target"))?;
    let cargo_home = directory.join(format!("{name}-cargo-home"));
    std::fs::create_dir_all(&cargo_home)?;
    let nonce_disk = directory.join(format!("{name}.nonce"));
    let plan = JobVmConfig::new(JobVmRequest {
        clone: clone.clone(),
        nonce_disk: nonce_disk.clone(),
        workspace: ShareLocation {
            host: workspace,
            tag: "workspace".into(),
            guest: CHECKOUT.into(),
        },
        siblings: Vec::new(),
        cargo_home: ShareLocation {
            host: cargo_home,
            tag: "cargo-home".into(),
            guest: CARGO_HOME.into(),
        },
    })?
    .with_build_disk_bus(bus);
    let nonce = random_hex(16)?;
    crate::create_job_nonce_disk(&nonce_disk, &nonce, &plan.mounts(), &BuildConfig::default())?;
    let slot = std::fs::File::create(directory.join(format!("{name}.slot")))?;
    let mut job = crate::spawn_job_helper(helper, &plan, &slot)?;
    let reports = std::fs::File::from(job.reports.as_fd().try_clone_to_owned()?);
    let printer = std::thread::spawn(move || {
        for line in io::BufReader::new(reports).lines().map_while(Result::ok) {
            println!("helper: {line}");
        }
    });
    job.open_gate()?;
    let result = converse(&mut job.session, &nonce, &clone, command, run);
    // Closing the gate stops the VM; the helper exits once it has stopped.
    job.close_gate();
    let status = job.child.wait()?;
    let _ = printer.join();
    if !status.success() {
        println!("helper exited with {status}");
    }
    result
}

/// The runner's side of one job session: authenticate the agent's Hello, send
/// the command and collect its output until Exit, Signal or the session's end.
fn converse(
    session: &mut std::os::unix::net::UnixStream,
    nonce: &str,
    clone: &Path,
    command: &str,
    run: &mut Run,
) -> io::Result<()> {
    let mut acceptor = SessionAcceptor::new(nonce.into(), clone.display().to_string())?;
    acceptor.claim()?;
    let mut protocol = Protocol::with_profile(Profile::Macos, nonce)?;
    // Booting a fresh clone takes a while; the command itself is quick.
    session.set_read_timeout(Some(Duration::from_secs(600)))?;
    let mut input = io::BufReader::new(session.try_clone()?);
    let hello = motor_winagent::read_frame(&mut input)?
        .ok_or_else(|| invalid("the session closed before the agent's Hello"))?;
    acceptor.authenticate(&hello)?;
    protocol.accept(Peer::Agent, &hello)?;
    let tree_end = Frame::TreeEnd {
        command: Command {
            command: command.into(),
            cwd: CHECKOUT.into(),
            workspace: CHECKOUT.into(),
            source_commit: "0".repeat(40),
            env: Default::default(),
        },
    };
    protocol.accept(Peer::Host, &tree_end)?;
    motor_winagent::write_frame(session, &tree_end)?;
    loop {
        let Some(frame) = motor_winagent::read_frame(&mut input)? else {
            return Err(invalid(
                "the agent closed the session without Exit or Signal",
            ));
        };
        protocol.accept(Peer::Agent, &frame)?;
        match frame {
            Frame::Started {} => println!("agent: started"),
            Frame::Output { stream, bytes } => {
                let text = String::from_utf8_lossy(&bytes);
                let (prefix, collected) = match stream {
                    Stream::Stdout => ("stdout", &mut run.stdout),
                    Stream::Stderr => ("stderr", &mut run.stderr),
                };
                for line in text.lines() {
                    println!("{prefix}: {line}");
                }
                collected.push_str(&text);
            }
            terminal @ (Frame::Exit { .. } | Frame::Signal { .. }) => {
                run.terminal = Some(terminal);
                return Ok(());
            }
            other => return Err(invalid(format!("unexpected agent frame {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(stdout: &str, stderr: &str) -> Run {
        Run {
            stdout: stdout.into(),
            stderr: stderr.into(),
            terminal: Some(Frame::Exit { code: 0 }),
            failure: None,
        }
    }

    #[test]
    fn reuse_needs_both_clean_exits_the_marker_and_no_reformat() {
        let first = exited("mount: x\nwrote: m1\n", "");
        assert_eq!(verdict("m1", &first, &exited("read: m1\n", "")), Ok(()));
        let reformatted = exited(
            "read: m1\n",
            "motor: the build disk could not be used (mount_apfs failed); it was reformatted, so this build starts cold\n",
        );
        assert!(
            verdict("m1", &first, &reformatted)
                .unwrap_err()
                .contains("reformatted")
        );
        let missing = exited(
            "read: cat: target/probe-marker: No such file or directory\n",
            "",
        );
        assert!(verdict("m1", &first, &missing).is_err());
        let closed = Run {
            failure: Some("the agent closed the session without Exit or Signal".into()),
            ..exited("read: m1\n", "")
        };
        assert!(
            verdict("m1", &first, &closed)
                .unwrap_err()
                .contains("second VM")
        );
        let failed_first = Run {
            terminal: Some(Frame::Exit { code: 1 }),
            ..exited("wrote: m1\n", "")
        };
        assert!(verdict("m1", &failed_first, &exited("read: m1\n", "")).is_err());
    }

    #[test]
    fn commands_print_the_mount_line_and_write_then_read_the_marker() {
        let first = first_command("m1");
        assert!(first.contains("mount | grep -F \" on $PWD/target (\""));
        assert!(first.contains("'m1' > target/probe-marker"));
        assert!(second_command().contains("read: $(cat target/probe-marker"));
        let output = std::process::Command::new("/bin/bash")
            .args(["-n", "-c", &first])
            .status()
            .unwrap();
        assert!(output.success());
        assert!(
            std::process::Command::new("/bin/bash")
                .args(["-n", "-c", &second_command()])
                .status()
                .unwrap()
                .success()
        );
    }
}
