use super::{command, paths, RunnerArgs};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
};

pub(super) const ARGS: &str = "/run/motor/runner-args";
const TOOLCHAIN: &str = "/toolchain";
// The image mirrors absolute paths below /toolchain. Binding the whole immutable
// prefix also preserves symlink targets such as npm's ../lib/node_modules.
const TOOL_PREFIXES: [&str; 1] = ["/usr/local"];

fn tool_bind_paths(toolchain: &Path) -> Vec<(PathBuf, PathBuf)> {
    TOOL_PREFIXES
        .iter()
        .map(|prefix| {
            (
                toolchain.join(prefix.trim_start_matches('/')),
                PathBuf::from(prefix),
            )
        })
        .collect()
}

#[derive(Debug)]
pub(super) struct BootFailure {
    pub(super) code: &'static str,
    pub(super) detail: String,
}
impl std::fmt::Display for BootFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for BootFailure {}
pub(super) fn failure(code: &'static str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(BootFailure {
        code,
        detail: error.to_string(),
    })
}

pub(super) fn enabled() -> io::Result<bool> {
    Ok(fs::read_to_string("/proc/cmdline")?
        .split_whitespace()
        .any(|word| word == "motor.per_job=1"))
}

fn dax_mount(mountinfo: &str) -> io::Result<String> {
    for line in mountinfo.lines().rev() {
        let Some((mount, filesystem)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<_> = mount.split_whitespace().collect();
        if fields.get(4) != Some(&TOOLCHAIN) {
            continue;
        }
        let fs: Vec<_> = filesystem.split_whitespace().collect();
        if fields.len() >= 6
            && fs.len() >= 3
            && fields[3] == "/"
            && fs[0] == "ext4"
            && fs[1] == "/dev/pmem0"
            && fields[5].split(',').any(|o| o == "ro")
            && fields[5].split(',').any(|o| o == "nosuid")
            && fields[5].split(',').any(|o| o == "nodev")
            && fs[2].split(',').any(|o| o == "ro")
            && fs[2].split(',').any(|o| o == "dax=always")
        {
            return Ok(fields[2].to_owned());
        }
        break;
    }
    Err(io::Error::other(
        "toolchain requires ext4 ro,nosuid,nodev,dax=always on /dev/pmem0",
    ))
}

fn virtio_pmem(sysfs_device: &Path) -> io::Result<()> {
    let device = fs::canonicalize(sysfs_device)?;
    for ancestor in device.ancestors() {
        if fs::read_link(ancestor.join("driver"))
            .ok()
            .is_some_and(|driver| driver.file_name().is_some_and(|name| name == "virtio_pmem"))
        {
            return Ok(());
        }
    }
    Err(io::Error::other("toolchain is not backed by virtio-pmem"))
}

pub(super) fn preflight() -> io::Result<()> {
    let check = || -> io::Result<()> {
        if !super::snapshot::mounted(TOOLCHAIN)? {
            fs::create_dir_all(TOOLCHAIN)?;
            paths::Directory::absolute(Path::new(TOOLCHAIN), 0)?;
            command(
                "mount",
                &[
                    "-t",
                    "ext4",
                    "-o",
                    "ro,nosuid,nodev,dax=always",
                    "/dev/pmem0",
                    TOOLCHAIN,
                ],
            )?;
        }
        let device = dax_mount(&fs::read_to_string("/proc/self/mountinfo")?)?;
        virtio_pmem(&Path::new("/sys/dev/block").join(device))?;
        for (source, target) in tool_bind_paths(Path::new(TOOLCHAIN)) {
            let source = paths::Directory::absolute(&source, 0)?;
            let target = paths::Directory::absolute(&target, 0)?;
            source.bind_toolchain(&target)?;
        }
        Ok(())
    };
    check().map_err(|error| failure("job_vm_dax_required", error))
}

pub(super) fn args() -> io::Result<RunnerArgs> {
    paths::Directory::absolute(Path::new("/run/motor"), 0)
        .map_err(|error| failure("job_vm_args_invalid", error))?;
    let mut args =
        RunnerArgs::read(Path::new(ARGS)).map_err(|error| failure("job_vm_args_invalid", error))?;
    // A fill boot runs a tenant runner for its one fill, not a job VM's
    // runner; `--fill-vm` still points it at the boot drive's credentials.
    if crate::fill_source::fill_boot(&fs::read_to_string("/proc/cmdline")?) {
        args.words.push("--fill-vm".into());
    } else {
        args.words.push("--per-job".into());
    }
    // The helper supplies the same root-owned fixed-path policy for Job and Fill
    // boots. Guest-exported source metadata cannot enable fixed paths.
    Ok(args)
}

fn decode_boot_args(cmdline: &str) -> io::Result<String> {
    let mut values = cmdline
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("motor.runner_args="));
    let encoded = values
        .next()
        .ok_or_else(|| io::Error::other("missing runner arguments"))?;
    if values.next().is_some() || encoded.len() > 2 * 16384 || encoded.len() % 2 != 0 {
        return Err(io::Error::other("invalid runner argument encoding"));
    }
    let digit = |byte| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(io::Error::other("runner arguments require lowercase hex")),
    };
    let bytes = encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[high, low]| Ok(digit(high)? * 16 + digit(low)?))
        .collect::<io::Result<Vec<_>>>()?;
    let text = String::from_utf8(bytes).map_err(io::Error::other)?;
    RunnerArgs::parse(&text)?;
    Ok(text)
}

fn boot_args(cmdline: &str) -> io::Result<String> {
    decode_boot_args(cmdline).map_err(|error| failure("job_vm_args_invalid", error))
}

fn write_boot_args(
    cmdline: &str,
    directory: &paths::Directory,
    owner: u32,
    group: u32,
) -> io::Result<()> {
    let text = boot_args(cmdline)?;
    let path = directory.fd_path().join("runner-args");
    let created = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path);
    let mut file = match created {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // /run survives agent restarts. Reuse only the same private boot
            // input; never repair or overwrite an unexpected existing node.
            let existing = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(&path)?;
            let metadata = existing.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != owner
                || metadata.gid() != group
                || metadata.mode() & 0o7777 != 0o600
                || metadata.len() != text.len() as u64
            {
                return Err(io::Error::other("unexpected existing runner arguments"));
            }
            let mut bytes = Vec::new();
            existing.take(16385).read_to_end(&mut bytes)?;
            if bytes != text.as_bytes() {
                return Err(io::Error::other(
                    "existing runner arguments differ from boot source",
                ));
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if unsafe { libc::fchown(file.as_raw_fd(), owner, group) } != 0 {
        return Err(io::Error::last_os_error());
    }
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

pub(super) fn install_args() -> io::Result<()> {
    let install = || {
        let cmdline = fs::read_to_string("/proc/cmdline")?;
        let run = paths::Directory::absolute(Path::new("/run"), 0)?;
        let directory = run.create("motor", 0o755)?;
        write_boot_args(&cmdline, &directory, 0, 0)
    };
    install().map_err(|error| failure("job_vm_args_invalid", error))
}

fn workspace_steps(
    kind: Option<&str>,
    mut run: impl FnMut(&str, &[&str]) -> io::Result<()>,
    grow: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match kind {
        Some("xfs") => {}
        None => run("mkfs.xfs", &["/dev/vdb"])?,
        Some(_) => return Err(io::Error::other("workspace disk is not XFS")),
    }
    run(
        "mount",
        &["-t", "xfs", "-o", "nosuid,nodev", "/dev/vdb", "/tenant"],
    )?;
    // The host grows a checkout's disk file when its size cap rises, so an
    // existing filesystem can be smaller than its disk. A failed grow leaves
    // the job its old space, which is no reason to refuse it.
    if let Err(error) = grow() {
        eprintln!("motor-tenant-agent: workspace filesystem not grown: {error}");
    }
    Ok(())
}

pub(super) fn mount_workspace() -> io::Result<()> {
    let output = Command::new("blkid")
        .args(["-p", "-s", "TYPE", "-o", "value", "/dev/vdb"])
        .output()?;
    let kind = match output.status.code() {
        Some(0) => Some(String::from_utf8(output.stdout).map_err(io::Error::other)?),
        Some(2) => {
            // A missing signature alone might be a damaged filesystem. Never
            // format a disk whose header still contains nonzero data.
            let mut header = vec![0; 1024 * 1024];
            fs::File::open("/dev/vdb")?.read_exact(&mut header)?;
            if header.iter().any(|byte| *byte != 0) {
                return Err(io::Error::other("unrecognized nonempty workspace disk"));
            }
            None
        }
        _ => return Err(io::Error::other("workspace filesystem probe failed")),
    };
    workspace_steps(
        kind.as_deref().map(str::trim),
        command,
        super::resize::grow_to_device,
    )
}

pub(super) fn mount_sources(image_cargo: &Path) -> io::Result<()> {
    use crate::isolation::cargo_sources;
    if image_cargo != Path::new(cargo_sources::IMAGE_CARGO) {
        return Err(io::Error::other(
            "job image CARGO_HOME does not match the immutable source layout",
        ));
    }
    let home = paths::Directory::absolute(Path::new("/tenant/cargo-home"), 0)?;
    let backing = paths::Directory::absolute(Path::new("/tenant"), 0)?
        .create("cargo-source-layers", 0o700)?;
    for layer in cargo_sources::layers(home.path(), image_cargo) {
        if super::snapshot::mounted(layer.target.to_str().unwrap())? {
            continue;
        }
        paths::Directory::absolute(&layer.lower, 0)?;
        let name = layer.target.file_name().unwrap().to_str().unwrap();
        let root = backing.create(name, 0o700)?;
        let upper = root.create("upper", 0o755)?;
        root.create("work", 0o700)?;
        if name == "registry" {
            upper.create("cache", 0o755)?;
        }
        command(
            "mount",
            &[
                "-t",
                "overlay",
                "-o",
                &layer.options(),
                "overlay",
                layer.target.to_str().unwrap(),
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNER: &str = "--backend host --tenant-vm --job-network veth --state-root /tenant/state --max-weight 2 --rustc-slots 2 --sccache-size-gib 1 --free-space-floor-bytes 67108864 --workspace-root /Users --owned-workspace-root /Users --workspace-root /home --owned-workspace-root /home";
    fn encoded(text: &str) -> String {
        format!(
            "motor.runner_args={}",
            text.as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }

    #[test]
    fn runner_boot_source_refuses_missing_duplicate_nonhex_odd_oversized_and_invalid_options() {
        let valid = encoded(RUNNER);
        assert_eq!(boot_args(&format!("quiet {valid}")).unwrap(), RUNNER);
        for source in [
            "quiet".to_owned(),
            format!("{valid} {valid}"),
            "motor.runner_args=zz".into(),
            "motor.runner_args=€a".into(),
            "motor.runner_args=aéa".into(),
            encoded(RUNNER)
                .to_uppercase()
                .replacen("MOTOR.RUNNER_ARGS=", "motor.runner_args=", 1),
            encoded(RUNNER).replacen("20", "+a", 1),
            "motor.runner_args=ff".into(),
            "motor.runner_args=0".into(),
            encoded(&format!("{RUNNER}{}", " ".repeat(16385))),
            encoded(&format!("{RUNNER} --per-job")),
            encoded(&RUNNER.replace("--max-weight 2", "--max-weight 0")),
        ] {
            let error = boot_args(&source).expect_err("must refuse boot source");
            let reply = super::super::agent_result(Err(error));
            assert_eq!(reply["error"], "job_vm_args_invalid");
        }
    }

    #[test]
    fn runner_boot_file_is_private_regular_and_restart_reuses_only_identical_input() {
        use std::os::unix::fs::{symlink, MetadataExt};
        let scratch = cortexkit_test_support::ScratchDir::new("runner-boot-args");
        fs::set_permissions(
            scratch.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let directory = paths::Directory::open(scratch.path(), uid).unwrap();
        write_boot_args(&encoded(RUNNER), &directory, uid, gid).unwrap();
        let path = scratch.join("runner-args");
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(
            (metadata.uid(), metadata.gid(), metadata.mode() & 0o777),
            (uid, gid, 0o600)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), RUNNER);
        write_boot_args(&encoded(RUNNER), &directory, uid, gid).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), metadata.ino());
        let changed = RUNNER.replace(
            "--max-weight 2 --rustc-slots 2",
            "--max-weight 4 --rustc-slots 4",
        );
        assert!(write_boot_args(&encoded(&changed), &directory, uid, gid).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), RUNNER);
        assert!(write_boot_args(&encoded(RUNNER), &directory, uid.wrapping_add(1), gid).is_err());
        assert!(write_boot_args(&encoded(RUNNER), &directory, uid, gid.wrapping_add(1)).is_err());
        for mode in [0o644, 0o400, 0o601, 0o4600] {
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
            assert!(write_boot_args(&encoded(RUNNER), &directory, uid, gid).is_err());
        }
        fs::remove_file(&path).unwrap();
        let sentinel = scratch.join("sentinel");
        fs::write(&sentinel, "untouched").unwrap();
        symlink(&sentinel, &path).unwrap();
        assert!(write_boot_args(&encoded(RUNNER), &directory, uid, gid).is_err());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "untouched");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(write_boot_args(&encoded(RUNNER), &directory, uid, gid).is_err());
    }

    const DAX: &str =
        "44 25 259:0 / /toolchain ro,nosuid,nodev,relatime - ext4 /dev/pmem0 ro,dax=always";

    #[test]
    fn dax_validation_uses_the_visible_stacked_mount() {
        let hidden = DAX.replace(",dax=always", "");
        assert!(dax_mount(&format!("{DAX}\n{hidden}\n")).is_err());
        assert_eq!(dax_mount(&format!("{hidden}\n{DAX}\n")).unwrap(), "259:0");
    }

    #[test]
    fn daxless_boot_refuses_before_workspace_mount_or_account_input() {
        for mount in [
            DAX.replace(",dax=always", ""),
            DAX.replace("ext4", "xfs"),
            DAX.replace("/dev/pmem0", "/dev/vdb"),
            DAX.replace("ro", "rw"),
            DAX.replace(",nosuid", ""),
            DAX.replace(",nodev", ""),
        ] {
            let mounted = std::cell::Cell::new(false);
            let read_account = std::cell::Cell::new(false);
            let result = super::super::start_after_probes(
                || {
                    dax_mount(&mount)
                        .map(|_| ())
                        .map_err(|e| failure("job_vm_dax_required", e))
                },
                || {
                    mounted.set(true);
                    read_account.set(true);
                    Ok(())
                },
            );
            let reply = super::super::agent_result(result.map(|_| serde_json::json!({"ok":true})));
            assert_eq!(reply["error"], "job_vm_dax_required");
            assert_eq!(reply["cause"], "workspace_setup_failed");
            assert!(!mounted.get());
            assert!(!read_account.get());
        }
        assert_eq!(dax_mount(DAX).unwrap(), "259:0");
    }

    #[test]
    fn empty_workspace_is_formatted_once_and_existing_xfs_is_kept() {
        for kind in [None, Some("xfs"), Some("ext4")] {
            let calls = std::cell::RefCell::new(Vec::new());
            let result = workspace_steps(
                kind,
                |program, args| {
                    calls
                        .borrow_mut()
                        .push((program.to_owned(), args.join(" ")));
                    Ok(())
                },
                || {
                    calls.borrow_mut().push(("grow".into(), String::new()));
                    Ok(())
                },
            );
            let calls = calls.into_inner();
            let mount = (
                "mount".to_owned(),
                "-t xfs -o nosuid,nodev /dev/vdb /tenant".to_owned(),
            );
            let grow = ("grow".to_owned(), String::new());
            match kind {
                None => assert_eq!(calls, [("mkfs.xfs".into(), "/dev/vdb".into()), mount, grow]),
                Some("xfs") => assert_eq!(calls, [mount, grow]),
                _ => {
                    assert!(result.is_err());
                    assert!(calls.is_empty());
                }
            }
        }
    }

    /// A disk the host grew keeps its warm build, so a failed grow must not
    /// cost the job its workspace: the job runs with the space it had.
    #[test]
    fn failed_workspace_grow_still_mounts_the_workspace() {
        let mut calls = Vec::new();
        workspace_steps(
            Some("xfs"),
            |program, _| {
                calls.push(program.to_owned());
                Ok(())
            },
            || Err(io::Error::other("xfs_growfs failed")),
        )
        .unwrap();
        assert_eq!(calls, ["mount"]);
    }

    #[test]
    fn workspace_mount_disables_suid_and_devices_without_disabling_execution() {
        for kind in [None, Some("xfs")] {
            workspace_steps(
                kind,
                |program, args| {
                    if program == "mount" {
                        let options = args
                            .windows(2)
                            .find(|pair| pair[0] == "-o")
                            .expect("workspace mount options missing")[1];
                        let options: Vec<_> = options.split(',').collect();
                        assert!(options.contains(&"nosuid"));
                        assert!(options.contains(&"nodev"));
                        assert!(!options.contains(&"noexec"));
                    }
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap();
        }
    }

    #[test]
    fn toolchain_bind_preserves_all_tenant_tool_and_cache_paths() {
        let root = cortexkit_test_support::ScratchDir::new("motor-toolchain-paths");
        let toolchain = root.join("toolchain");
        let binds = tool_bind_paths(&toolchain);
        assert_eq!(
            binds,
            [(
                root.join("toolchain/usr/local"),
                PathBuf::from("/usr/local")
            )]
        );
        let paths = [
            "/usr/local/lib/motor/rustup",
            "/usr/local/lib/motor/cargo/bin",
            "/usr/local/lib/motor/cargo/registry",
            "/usr/local/lib/motor/cargo/git",
            "/usr/local/lib/motor/bun/bin",
            "/usr/local/lib/motor/bun/install/cache",
            "/usr/local/bin/node",
            "/usr/local/bin/npm",
            "/usr/local/bin/pnpm",
            "/usr/local/lib/node_modules",
        ];
        for path in paths {
            let source = root.join("toolchain").join(path.trim_start_matches('/'));
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::write(&source, path).unwrap();
            let (source, target) = binds
                .iter()
                .find(|(_, target)| Path::new(path).starts_with(target))
                .unwrap();
            let resolved = source.join(Path::new(path).strip_prefix(target).unwrap());
            assert_eq!(fs::read_to_string(resolved).unwrap(), path);
        }
    }
}
