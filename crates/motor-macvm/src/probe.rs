//! The operator's build disk probe: `motor-macvm build-disk-probe <sealed-image>
//! <work-directory>`, run on the macOS VM host without an installed runner.
//!
//! For each way of attaching the build disk (virtio block, then USB mass
//! storage) it boots two job VMs one after the other, each from a fresh APFS
//! clone of the sealed image, through the same job helper the runner starts.
//! The image's own guest agent finds the blank build disk in the first VM,
//! formats and mounts it at `BUILD_MOUNT` and runs a command that prints the
//! mount line, builds a small Cargo package and writes a marker file through
//! the checkout's `target` link. The second VM gets the first one's disk,
//! exactly as the runner hands a checkout's kept disk to its next job; its
//! agent mounts the existing volume (or prints why it could not and
//! reformats) and the command prints the mount line, builds again and reads
//! the marker.
//!
//! Each VM is set up as the runner sets up a job: the workspace and cargo-home
//! shares, a checkout whose directories have mode 0700 and whose `target` is
//! the runner's link to `BUILD_MOUNT`, the same Hello, command and output
//! exchange, and a returned file under `target/` in the boot configuration, so
//! the agent copies it out of the volume. The agent runs the command as the
//! job account with the image's shell and `CARGO_TARGET_DIR`, as for every
//! job. Both commands write a file into the checkout root, pause 3 s, build,
//! then wait 11 s, and check after each step that `target/` and
//! `CARGO_TARGET_DIR` still reach the build volume. After each VM stops, the
//! probe lists the agent's returns directory in the cargo-home share, which
//! must hold only the returned file, and reads the checkout's `target` link.
//! The runner's other host steps are not reproduced: the Git checkout,
//! sealing source files read-only, source modification times, the caller's
//! environment and the pause before the command is sent while the runner
//! admits the job.
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
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::time::Duration;

/// Where the probe's checkout appears in the guest. The agent only accepts
/// workspaces below /Users.
pub const CHECKOUT: &str = "/Users/motor-probe/checkout";

/// Prints the line `mount` shows for the build mount (`BUILD_MOUNT`), what
/// `diskutil` says about the volume's device (internal or removable,
/// encrypted or not), the checkout's `target` link and `CARGO_TARGET_DIR`. It
/// also defines `check`, which prints whether `target/` (followed through its
/// link) and `CARGO_TARGET_DIR` still reach the build volume at a named step:
/// the same directory, on another device than the checkout. When the volume
/// was mounted over the shared `target/` directory, the path stopped reaching
/// it about 3.5 seconds after mounting.
const DESCRIBE: &str = r#"line=$(mount | grep -F " on /private/var/motor/build (" || true)
echo "mount: ${line:-nothing is mounted at /private/var/motor/build}"
device=${line%% on *}
if [ -n "$line" ]; then
  /usr/sbin/diskutil info "$device" | grep -E 'Protocol|Device Location|Removable Media|Encrypted|FileVault|Locked' | sed 's/^ */diskutil: /'
fi
echo "target: $(readlink target 2>&1)"
echo "cargo-target-dir: ${CARGO_TARGET_DIR:-unset}"
check() {
  here=$(stat -f %d .)
  there=$(stat -L -f %d:%i target 2>/dev/null) || there=missing
  mount=$(stat -L -f %d:%i "${CARGO_TARGET_DIR:-/nonexistent}" 2>/dev/null) || mount=missing
  if [ "$there" != missing ] && [ "$there" = "$mount" ] && [ "${there%%:*}" != "$here" ]; then
    echo "check $1: volume"
  else
    echo "check $1: share (target $there, CARGO_TARGET_DIR $mount, checkout dev $here)"
  fi
}
check start
"#;

/// What the runner's end-to-end job does around `target/`: a file written
/// into the checkout root (that job sends Cargo's log there), a pause of a
/// few seconds, a Cargo build, then a wait of more than 10 seconds, which
/// is longer than the mount over the share used to last, checking `target/`
/// after each step.
const BUILD: &str = r#": > probe-root-write
check after-root-write
sleep 3
check after-sleep
cargo build --offline 2>build.log
echo "cargo: exit $?"
check after-cargo
sleep 11
check after-long-wait
"#;

/// The probe checkout's Cargo package, built by both VMs.
const CARGO_TOML: &str =
    "[package]\nname = \"buildcheck\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const CARGO_LOCK: &str = "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"buildcheck\"\nversion = \"0.1.0\"\n";
const MAIN_RS: &str = "fn main() {\n    println!(\"build disk check\");\n}\n";

/// The returned file each VM's agent copies out of the build volume into
/// `TARGET_RETURNS` in the cargo-home share, relative to `target/`, as for a
/// runner job with a return path there.
pub const RETURNED: &str = "debug/buildcheck";
const RETURN_MAX_BYTES: u64 = 64 << 20;

/// The first VM's command: describe the volume, build, then write the marker.
pub fn first_command(marker: &str) -> String {
    format!(
        "{DESCRIBE}{BUILD}printf '%s\\n' '{marker}' > target/probe-marker\necho \"wrote: $(cat target/probe-marker)\"\ncheck end\n"
    )
}

/// The second VM's command: describe the volume, build, then read the marker.
pub fn second_command() -> String {
    format!("{DESCRIBE}{BUILD}echo \"read: $(cat target/probe-marker 2>&1)\"\ncheck end\n")
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
    /// What the host found in the agent's returns directory in the
    /// cargo-home share once the VM stopped, relative to it, sorted:
    /// directories end with `/`, links are shown with their target. Only the
    /// returned file belongs there.
    pub returns: Vec<String>,
    /// The checkout's `target` link once the VM stopped, as read on the host.
    pub target_link: Option<String>,
}

/// Whether the second VM mounted the volume the first one formatted, with
/// `target/` reaching the volume at every step of both commands and the
/// shared `target/` holding only the returned file. Returns the reason when
/// it did not.
pub fn verdict(marker: &str, first: &Run, second: &Run) -> Result<(), String> {
    let clean = |run: &Run, which: &str| -> Result<(), String> {
        if let Some(failure) = &run.failure {
            return Err(format!("{which} VM: {failure}"));
        }
        if let Some(line) = run
            .stderr
            .lines()
            .find(|line| line.contains("build disk could not be released"))
        {
            return Err(format!("{which} VM: {line}"));
        }
        if run.terminal != Some(Frame::Exit { code: 0 }) {
            return Err(format!("{which} VM ended with {:?}", run.terminal));
        }
        if let Some(line) = run
            .stdout
            .lines()
            .find(|line| line.starts_with("check ") && !line.ends_with(": volume"))
        {
            return Err(format!("{which} VM: target/ left the build volume: {line}"));
        }
        if !run.stdout.contains("check end: volume") {
            return Err(format!("{which} VM did not finish its checks"));
        }
        if !run.stdout.contains("cargo: exit 0") {
            return Err(format!("{which} VM: the Cargo build failed"));
        }
        if run.target_link.as_deref() != Some(crate::BUILD_MOUNT) {
            return Err(format!(
                "{which} VM: the checkout's target is {:?}, not the link to {}",
                run.target_link,
                crate::BUILD_MOUNT
            ));
        }
        let expected = ["debug/".to_owned(), RETURNED.to_owned()];
        if run.returns != expected {
            return Err(format!(
                "{which} VM: the returns directory holds {:?}, not just the returned file",
                run.returns
            ));
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
    // As the runner stages a checkout: its directories have mode 0700 (the
    // runner's job tree), and target is a link to the guest's build mount.
    write_checkout(&workspace)?;
    let cargo_home = directory.join(format!("{name}-cargo-home"));
    std::fs::DirBuilder::new().mode(0o700).create(&cargo_home)?;
    let nonce_disk = directory.join(format!("{name}.nonce"));
    let plan = JobVmConfig::new(JobVmRequest {
        clone: clone.clone(),
        nonce_disk: nonce_disk.clone(),
        workspace: ShareLocation {
            host: workspace.clone(),
            tag: "workspace".into(),
            guest: CHECKOUT.into(),
        },
        siblings: Vec::new(),
        cargo_home: ShareLocation {
            host: cargo_home.clone(),
            tag: "cargo-home".into(),
            guest: CARGO_HOME.into(),
        },
    })?
    .with_build_disk_bus(bus);
    let nonce = random_hex(16)?;
    // A runner job with a return path under target/ asks the agent for it.
    let build = BuildConfig {
        returns: vec![RETURNED.into()],
        return_max_bytes: RETURN_MAX_BYTES,
    };
    crate::create_job_nonce_disk(&nonce_disk, &nonce, &plan.mounts(), &build)?;
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
    // The VM has stopped. The agent's returns directory should hold only the
    // returned file, and the checkout's target should still be the link.
    match list(&cargo_home.join(crate::TARGET_RETURNS)) {
        Ok(entries) => run.returns = entries,
        Err(error) => println!("returns: unreadable: {error}"),
    }
    for entry in &run.returns {
        println!("returns: {entry}");
    }
    run.target_link = std::fs::read_link(workspace.join("target"))
        .ok()
        .map(|link| link.to_string_lossy().into_owned());
    println!("target link: {:?}", run.target_link);
    result
}

/// Writes the probe's Cargo checkout, with directories created as the runner
/// creates its staged ones (mode 0700) and `target` the runner's link to the
/// guest's build mount.
fn write_checkout(workspace: &Path) -> io::Result<()> {
    for directory in [workspace.to_owned(), workspace.join("src")] {
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    }
    std::os::unix::fs::symlink(crate::BUILD_MOUNT, workspace.join("target"))?;
    std::fs::write(workspace.join("Cargo.toml"), CARGO_TOML)?;
    std::fs::write(workspace.join("Cargo.lock"), CARGO_LOCK)?;
    std::fs::write(workspace.join("src/main.rs"), MAIN_RS)?;
    std::fs::write(workspace.join(".gitignore"), "target/\n")
}

/// Everything below `root`, relative to it and sorted, without following
/// links: directories end with `/` and links show their target.
pub fn list(root: &Path) -> io::Result<Vec<String>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|error| invalid(error.to_string()))?
                .to_string_lossy()
                .into_owned();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_symlink() {
                found.push(format!(
                    "{relative} -> {}",
                    std::fs::read_link(&path)?.display()
                ));
            } else if metadata.is_dir() {
                found.push(format!("{relative}/"));
                pending.push(path);
            } else {
                found.push(relative);
            }
        }
    }
    found.sort();
    Ok(found)
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

    /// A clean exit whose checks all found the volume, whose build succeeded
    /// and whose shared target/ holds only the returned file.
    fn exited(stdout: &str, stderr: &str) -> Run {
        Run {
            stdout: format!("check start: volume\ncargo: exit 0\n{stdout}check end: volume\n"),
            stderr: stderr.into(),
            terminal: Some(Frame::Exit { code: 0 }),
            failure: None,
            returns: vec!["debug/".into(), RETURNED.into()],
            target_link: Some(crate::BUILD_MOUNT.into()),
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

    /// The failing runner job's signs: target/ leaving the build volume at
    /// some step, the agent's release failure, anything but the returned file
    /// in the returns directory, and the checkout's target link replaced.
    #[test]
    fn target_leaving_the_volume_fails_the_probe_and_names_the_step() {
        let second = exited("read: m1\n", "");
        let left = exited(
            "wrote: m1\ncheck after-long-wait: share (target 889192450:2, CARGO_TARGET_DIR 16777244:2, checkout dev 889192450)\n",
            "",
        );
        let error = verdict("m1", &left, &second).unwrap_err();
        assert!(error.contains("check after-long-wait: share"), "{error}");
        let unreleased = exited(
            "wrote: m1\n",
            "motor: the build disk could not be released: the build mount no longer names the build volume's root\n",
        );
        let error = verdict("m1", &unreleased, &second).unwrap_err();
        assert!(error.contains("could not be released"), "{error}");
        let written = Run {
            returns: vec!["CACHEDIR.TAG".into(), "debug/".into(), RETURNED.into()],
            ..exited("wrote: m1\n", "")
        };
        let error = verdict("m1", &written, &second).unwrap_err();
        assert!(error.contains("CACHEDIR.TAG"), "{error}");
        let replaced = Run {
            target_link: None,
            ..exited("wrote: m1\n", "")
        };
        let error = verdict("m1", &replaced, &second).unwrap_err();
        assert!(error.contains("not the link"), "{error}");
        let unbuilt = Run {
            stdout: "check start: volume\ncargo: exit 101\nwrote: m1\ncheck end: volume\n".into(),
            ..exited("", "")
        };
        assert!(
            verdict("m1", &unbuilt, &second)
                .unwrap_err()
                .contains("Cargo")
        );
    }

    #[test]
    fn returns_are_listed_without_following_links() {
        let scratch = cortexkit_test_support::ScratchDir::new("probe-list");
        let root = scratch.join("returns");
        std::fs::create_dir_all(root.join("debug")).unwrap();
        std::fs::write(root.join("debug/buildcheck"), "x").unwrap();
        std::os::unix::fs::symlink("/", root.join("escape")).unwrap();
        assert_eq!(
            list(&root).unwrap(),
            ["debug/", "debug/buildcheck", "escape -> /"]
        );
    }

    /// Both commands check target/ around a root write, a 3 s pause, a Cargo
    /// build and a wait of more than 10 s, and are valid shell.
    #[test]
    fn commands_check_the_build_mount_over_more_than_ten_seconds() {
        let first = first_command("m1");
        assert!(first.contains(&format!("mount | grep -F \" on {} (\"", crate::BUILD_MOUNT)));
        assert!(first.contains("'m1' > target/probe-marker"));
        assert!(second_command().contains("read: $(cat target/probe-marker"));
        for command in [&first, &second_command()] {
            for step in [
                "check start",
                ": > probe-root-write\ncheck after-root-write",
                "sleep 3\ncheck after-sleep",
                "cargo build --offline 2>build.log",
                "check after-cargo",
                "sleep 11\ncheck after-long-wait",
                "check end",
            ] {
                assert!(command.contains(step), "{step}");
            }
            assert!(
                std::process::Command::new("/bin/bash")
                    .args(["-n", "-c", command])
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
}
