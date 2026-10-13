use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::MetadataExt,
};

fn checked(config: &Config, program: &str, args: &[String]) -> io::Result<Output> {
    let output = command(config, program, args)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output)
}

fn account(config: &Config, name: &str) -> io::Result<u32> {
    let passwd = checked(config, "/usr/bin/getent", &["passwd".into(), name.into()])?;
    let text = std::str::from_utf8(&passwd.stdout).map_err(invalid)?;
    let fields: Vec<_> = text.trim().split(':').collect();
    if fields.len() != 7
        || fields[0] != name
        || ![
            "/usr/sbin/nologin",
            "/sbin/nologin",
            "/bin/false",
            "/usr/bin/false",
        ]
        .contains(&fields[6])
    {
        return Err(invalid(format!(
            "provision {name} with a non-login shell before starting the Windows runner"
        )));
    }
    let uid = fields[2].parse::<u32>().map_err(invalid)?;
    if uid == 0 || uid == unsafe { libc::geteuid() } {
        return Err(invalid(format!(
            "{name} must differ from root and the runner"
        )));
    }
    let groups = checked(config, "/usr/bin/id", &["-nG".into(), name.into()])?;
    if String::from_utf8_lossy(&groups.stdout)
        .split_whitespace()
        .any(|g| ["sudo", "admin", "adm", "lxd", "docker", "wheel"].contains(&g))
    {
        return Err(invalid(format!("{name} has an administrative group")));
    }
    checked(
        config,
        "/usr/bin/sudo",
        &[
            "-n".into(),
            "-u".into(),
            name.into(),
            "/usr/bin/true".into(),
        ],
    )?;
    checked(
        config,
        "/usr/bin/sudo",
        &[
            "-n".into(),
            "-u".into(),
            name.into(),
            "/usr/bin/test".into(),
            "!".into(),
            "-e".into(),
            format!("{}/.ssh", fields[5]),
        ],
    )?;
    let sudo = command(
        config,
        "/usr/bin/sudo",
        &[
            "-n".into(),
            "/usr/bin/sudo".into(),
            "-n".into(),
            "-l".into(),
            "-U".into(),
            name.into(),
        ],
    )?;
    let denial = format!(
        "{}{}",
        String::from_utf8_lossy(&sudo.stdout),
        String::from_utf8_lossy(&sudo.stderr)
    );
    // sudo-rs (Ubuntu's default sudo) exits 0 for `sudo -l -U` whether or not
    // the user has grants, so the exit status says nothing. Judge by the
    // output: a denial line must be present and no grant listing.
    if !sudo_policy_denies(&denial) {
        return Err(invalid(format!(
            "{name} must have no sudo grants; could not verify a sudo-policy denial"
        )));
    }
    Ok(uid)
}

fn sudo_policy_denies(listing: &str) -> bool {
    listing.contains("is not allowed to run sudo") && !listing.contains("may run the following")
}

fn acl(
    config: &Config,
    path: &Path,
    owner: &str,
    group: &str,
    other: &str,
    grants: &[(u32, &str)],
) -> io::Result<()> {
    let output = checked(
        config,
        "/usr/bin/getfacl",
        &[
            "-cpn".into(),
            "--".into(),
            path.to_string_lossy().into_owned(),
        ],
    )?;
    let mut actual = BTreeMap::new();
    for line in std::str::from_utf8(&output.stdout)
        .map_err(invalid)?
        .lines()
    {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let (key, perms) = line
            .rsplit_once(':')
            .ok_or_else(|| invalid("malformed host-layout ACL"))?;
        if actual.insert(key.to_string(), perms.to_string()).is_some() {
            return Err(invalid("duplicate host-layout ACL entry"));
        }
    }
    let mut expected = BTreeMap::from([
        ("user:".into(), owner.into()),
        ("group:".into(), group.into()),
        ("other:".into(), other.into()),
    ]);
    if !grants.is_empty() {
        let mut mask = [false; 3];
        for perms in std::iter::once(group).chain(grants.iter().map(|(_, p)| *p)) {
            for (i, byte) in perms.bytes().enumerate() {
                mask[i] |= byte != b'-';
            }
        }
        let perms: String = mask
            .iter()
            .zip(['r', 'w', 'x'])
            .map(|(set, ch)| if *set { ch } else { '-' })
            .collect();
        expected.insert("mask:".into(), perms);
    }
    for (uid, perms) in grants {
        expected.insert(format!("user:{uid}"), (*perms).into());
    }
    if actual != expected {
        return Err(invalid(format!(
            "unsafe or missing host-layout ACL at {}: expected {expected:?}, got {actual:?}",
            path.display()
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
// These parameters name distinct permission fields and UID grants.
fn path(
    config: &Config,
    path: &Path,
    device: u64,
    directory: bool,
    owner: &str,
    group: &str,
    other: &str,
    grants: &[(u32, &str)],
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || metadata.is_dir() != directory
        || (!directory && !metadata.is_file())
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.dev() != device
        || metadata.mode() & 0o7000 != 0
    {
        return Err(invalid(format!(
            "unsafe Windows host-layout path (type, owner, device or mode): {}",
            path.display()
        )));
    }
    acl(config, path, owner, group, other, grants)
}

pub(super) fn validate(config: &Config) -> io::Result<()> {
    let vm = config.windows.as_ref().expect("validated Windows config");
    let root = &vm.layout_root;
    // Only test command injection permits a scratch layout. Production always
    // uses the dedicated /srv/build tree, never a checkout or a home directory.
    #[cfg(not(test))]
    let scratch = false;
    #[cfg(test)]
    let scratch = config.windows_command.is_some();
    if (!scratch && root != &Path::new("/srv/build/motor-windows").join(&config.instance))
        || config.state_root != root.join("state")
    {
        return Err(invalid("Windows host layout must be /srv/build/motor-windows/<instance>, with --state-root <layout_root>/state"));
    }
    for ancestor in root.ancestors() {
        if !fs::symlink_metadata(ancestor)?.is_dir() {
            return Err(invalid("Windows host layout contains a symlink"));
        }
    }
    let qemu = account(config, "motor-qemu")?;
    let prep = account(config, "motor-prep")?;
    let job = checked(config, "/usr/bin/id", &["-u".into(), "motor-job".into()])?;
    let job_uid = String::from_utf8_lossy(&job.stdout)
        .trim()
        .parse::<u32>()
        .map_err(invalid)?;
    if BTreeSet::from([qemu, prep, job_uid]).len() != 3 {
        return Err(invalid(
            "motor-qemu, motor-prep and motor-job must be distinct accounts",
        ));
    }
    let device = fs::symlink_metadata(&config.state_root)?.dev();
    let public = [
        (root
            .parent()
            .ok_or_else(|| invalid("layout root has no parent"))?
            .to_path_buf()),
        root.clone(),
        root.join("vm"),
        root.join("vm/sockets"),
    ];
    for p in public {
        path(config, &p, device, true, "rwx", "--x", "--x", &[])?;
    }
    path(
        config,
        &root.parent().unwrap().join("slot.lock"),
        device,
        false,
        "rw-",
        "---",
        "---",
        &[(qemu, "rw-")],
    )?;
    for p in [
        config.state_root.clone(),
        config.state_dir(),
        root.join("staging"),
    ] {
        path(
            config,
            &p,
            device,
            true,
            "rwx",
            "---",
            "---",
            &[(prep, "--x")],
        )?;
    }
    path(
        config,
        &root.join("baselines"),
        device,
        true,
        "rwx",
        "r-x",
        "---",
        &[(qemu, "r-x")],
    )?;
    path(
        config,
        &vm.baseline_path(),
        device,
        false,
        "r--",
        "r--",
        "---",
        &[(qemu, "r--")],
    )?;
    for p in [root.join("vm/overlays"), root.join("vm/caches")] {
        path(
            config,
            &p,
            device,
            true,
            "rwx",
            "---",
            "---",
            &[(qemu, "--x")],
        )?;
    }
    let jobs = config.state_dir().join("jobs");
    if jobs.exists() {
        path(config, &jobs, device, true, "rwx", "---", "---", &[])?;
    }
    Ok(())
}

pub(super) fn grant_mirrors(config: &Config) -> io::Result<()> {
    // Trusted preparation can traverse the state containers but can read only
    // these two trees. Physical traversal never follows a mirror symlink.
    for name in ["mirrors", "git-deps"] {
        let tree = config.state_dir().join(name);
        fs::create_dir_all(&tree)?;
        if !fs::symlink_metadata(&tree)?.is_dir() {
            return Err(invalid("Windows mirror tree is a symlink"));
        }
        checked(
            config,
            "/usr/bin/sudo",
            &[
                "-n".into(),
                "/usr/bin/setfacl".into(),
                "-R".into(),
                "-P".into(),
                "-m".into(),
                "u:motor-prep:rX".into(),
                "--".into(),
                tree.to_string_lossy().into_owned(),
            ],
        )?;
        checked(
            config,
            "/usr/bin/find",
            &[
                "-P".into(),
                tree.to_string_lossy().into_owned(),
                "-type".into(),
                "d".into(),
                "-exec".into(),
                "/usr/bin/sudo".into(),
                "-n".into(),
                "/usr/bin/setfacl".into(),
                "-P".into(),
                "-d".into(),
                "-m".into(),
                "u:motor-prep:rX".into(),
                "--".into(),
                "{}".into(),
                "+".into(),
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod sudo_policy_tests {
    use super::sudo_policy_denies;

    // sudo-rs 0.2.13 `sudo -l -U` output shapes with neutral host/account labels.
    // Both commands exit 0, so only the text can tell them apart.
    #[test]
    fn denial_is_judged_by_output_not_exit_status() {
        assert!(sudo_policy_denies(
            "User denied-user is not allowed to run sudo on build-host.\n"
        ));
        assert!(!sudo_policy_denies(
            "User granted-user may run the following commands on build-host:\n    (ALL : ALL) ALL\n"
        ));
        assert!(!sudo_policy_denies(""));
    }
}
