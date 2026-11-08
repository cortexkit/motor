use super::{command, paths, RunnerArgs};
use std::{
    fs,
    io::{self, Read},
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
    for line in mountinfo.lines() {
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
            && fs[2].split(',').any(|o| o == "ro")
            && fs[2].split(',').any(|o| o == "dax=always")
        {
            return Ok(fields[2].to_owned());
        }
        break;
    }
    Err(io::Error::other(
        "toolchain requires ext4 ro,dax=always on /dev/pmem0",
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
                &["-t", "ext4", "-o", "ro,dax=always", "/dev/pmem0", TOOLCHAIN],
            )?;
        }
        let device = dax_mount(&fs::read_to_string("/proc/self/mountinfo")?)?;
        virtio_pmem(&Path::new("/sys/dev/block").join(device))?;
        for (source, target) in tool_bind_paths(Path::new(TOOLCHAIN)) {
            let source = paths::Directory::absolute(&source, 0)?;
            let target = paths::Directory::absolute(&target, 0)?;
            source.bind(&target, true)?;
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
    args.words.push("--per-job".into());
    Ok(args)
}

fn workspace_steps(
    kind: Option<&str>,
    mut run: impl FnMut(&str, &[&str]) -> io::Result<()>,
) -> io::Result<()> {
    match kind {
        Some("xfs") => {}
        None => run("mkfs.xfs", &["/dev/vdb"])?,
        Some(_) => return Err(io::Error::other("workspace disk is not XFS")),
    }
    run("mount", &["-t", "xfs", "/dev/vdb", "/tenant"])
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
    workspace_steps(kind.as_deref().map(str::trim), command)
}

pub(super) fn seed_sources(image_cargo: &Path) -> io::Result<()> {
    // Shared sources stay writable on the workspace disk, just as in a tenant
    // VM. Never bind a writable cache over the immutable toolchain image.
    for name in ["registry", "git"] {
        let source = image_cargo.join(name);
        if source.exists() {
            let status = Command::new("cp")
                .args(["-a", "--no-clobber"])
                .arg(source.join("."))
                .arg(Path::new("/tenant/cargo-home").join(name))
                .status()?;
            if !status.success() {
                return Err(io::Error::other("copy toolchain Cargo sources failed"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAX: &str = "44 25 259:0 / /toolchain ro,relatime - ext4 /dev/pmem0 ro,dax=always";

    #[test]
    fn daxless_boot_refuses_before_workspace_mount_or_account_input() {
        for mount in [
            DAX.replace(",dax=always", ""),
            DAX.replace("ext4", "xfs"),
            DAX.replace("/dev/pmem0", "/dev/vdb"),
            DAX.replace("ro", "rw"),
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
            let mut calls = Vec::new();
            let result = workspace_steps(kind, |program, args| {
                calls.push((program.to_owned(), args.join(" ")));
                Ok(())
            });
            match kind {
                None => assert_eq!(
                    calls,
                    [
                        ("mkfs.xfs".into(), "/dev/vdb".into()),
                        ("mount".into(), "-t xfs /dev/vdb /tenant".into())
                    ]
                ),
                Some("xfs") => {
                    assert_eq!(calls, [("mount".into(), "-t xfs /dev/vdb /tenant".into())])
                }
                _ => {
                    assert!(result.is_err());
                    assert!(calls.is_empty());
                }
            }
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
