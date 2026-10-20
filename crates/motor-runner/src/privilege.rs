//! How the runner runs its privileged helpers: setfacl, chown, chmod, mkdir,
//! systemctl, systemd-run, nsenter and the network tools.
//!
//! On the build server the runner is an unprivileged account with a narrow
//! passwordless sudo grant, so each helper runs as `sudo -n <helper>`. A
//! tenant-VM runner is already root inside its own VM. Going through sudo
//! there adds nothing, and each sudo call evaluates its policy again, which
//! includes looking up the machine's host name. A tenant runner therefore
//! switches to running helpers directly, once at startup, before any helper
//! runs. The choice is process-wide so that call sites stay unconditional.

use std::{
    io,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

const SUDO: &str = "/usr/bin/sudo";

/// The PATH that sudo's environment reset gives helpers on Debian
/// (`secure_path`). Direct helpers get the same environment sudo would give
/// them, so behavior does not depend on how the helper was reached.
const HELPER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

static DIRECT: AtomicBool = AtomicBool::new(false);

/// Run privileged helpers without sudo for the rest of this process. Refused
/// unless the process is root, because a helper run directly by any other
/// account would simply fail its privileged operation.
pub fn run_helpers_directly_as_root() -> io::Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other(
            "privileged helpers can run without sudo only in a root runner",
        ));
    }
    DIRECT.store(true, Ordering::SeqCst);
    Ok(())
}

pub(crate) fn direct() -> bool {
    DIRECT.load(Ordering::SeqCst)
}

/// A command that runs `program` (an absolute path) with root privilege.
pub(crate) fn command(program: &str) -> Command {
    command_for(program, direct())
}

fn command_for(program: &str, direct: bool) -> Command {
    if direct {
        let mut command = Command::new(program);
        command.env_clear().env("PATH", HELPER_PATH);
        command
    } else {
        let mut command = Command::new(SUDO);
        command.args(["-n", program]);
        command
    }
}

/// The argument words that run `program` with root privilege from another
/// tool that executes commands itself, such as `find -exec`.
pub(crate) fn exec_words(program: &'static str) -> Vec<&'static str> {
    exec_words_for(program, direct())
}

fn exec_words_for(program: &'static str, direct: bool) -> Vec<&'static str> {
    if direct {
        vec![program]
    } else {
        vec![SUDO, "-n", program]
    }
}

/// Whether the runner's sudo grants must be present. A root runner that runs
/// its helpers directly needs none, so it skips the grant inventory.
pub(crate) fn uses_sudo() -> bool {
    !direct()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn server_runner_reaches_helpers_through_noninteractive_sudo() {
        let command = command_for("/usr/bin/setfacl", false);
        assert_eq!(words(&command), ["/usr/bin/sudo", "-n", "/usr/bin/setfacl"]);
        assert_eq!(command.get_envs().count(), 0);
        assert_eq!(
            exec_words_for("/usr/bin/setfacl", false),
            ["/usr/bin/sudo", "-n", "/usr/bin/setfacl"]
        );
    }

    #[test]
    fn root_runner_runs_helpers_directly_with_the_environment_sudo_would_give() {
        let command = command_for("/usr/bin/chown", true);
        assert_eq!(words(&command), ["/usr/bin/chown"]);
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(
            envs,
            [(
                std::ffi::OsStr::new("PATH"),
                Some(std::ffi::OsStr::new(HELPER_PATH))
            )]
        );
        assert_eq!(
            exec_words_for("/usr/bin/setfacl", true),
            ["/usr/bin/setfacl"]
        );
    }

    #[test]
    fn helpers_use_sudo_unless_a_root_runner_opts_out() {
        // Unit tests never switch the process-wide mode, so every other test
        // in this binary keeps observing the sudo commands it asserts on.
        assert!(uses_sudo());
        assert_eq!(words(&command("/usr/bin/chmod"))[0], SUDO);
    }
}
