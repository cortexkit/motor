//! Where the runner's trusted JavaScript dependency setup finds Bun, Node, npm
//! and pnpm.
//!
//! The setup runs as the daemon, before any job code, so it must run exactly
//! the tools the image recipe installed for this machine's role, never
//! whatever the daemon's HOME or PATH happens to name. Each role's locations
//! are those of `script/runner/toolchain.json` (`tools.<name>.path`, with
//! `path_by_role` overrides); a unit test keeps the table below equal to it.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// The machine layout a runner was installed with, selected by the same
/// `serve` flags that select its other role behaviour: `--boat-profile` for
/// Boat, `--tenant-vm` alone for a hosted tenant VM, neither for the build
/// server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolchainRole {
    BuildServer,
    Boat,
    Tenant,
}

impl ToolchainRole {
    pub(crate) fn of(config: &crate::engine::Config) -> Self {
        if config.boat_profile {
            Self::Boat
        } else if config.tenant_vm {
            Self::Tenant
        } else {
            Self::BuildServer
        }
    }

    /// The role's name in `script/runner/toolchain.json`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::BuildServer => "build-server",
            Self::Boat => "boat",
            Self::Tenant => "tenant",
        }
    }
}

/// Where every role's jobs see Bun: the first directory of each role's job
/// PATH in the toolchain manifest. The runner mounts its own Bun directory
/// there.
pub(crate) const JOB_BUN_DIR: &str = "/home/ubuntu/.bun/bin";
/// Bun on the build server and in Boat (the manifest's `tools.bun.path`).
const HOME_BUN_DIR: &str = "/home/ubuntu/.bun/bin";
/// Bun in the tenant image. A tenant VM's /home is the tenant's own data
/// directory, bound over the image's /home before the runner starts, so the
/// image keeps Bun outside it.
const TENANT_BUN_DIR: &str = "/usr/local/lib/motor/bun/bin";
/// Node, npm and pnpm on every role.
const NODE_DIR: &str = "/usr/local/bin";
/// Directories after the JavaScript tools on the package managers' PATH, for
/// the few programs they may start themselves (git for a git dependency, sh).
const SYSTEM_PATH: [&str; 3] = ["/usr/local/bin", "/usr/bin", "/bin"];

/// The JavaScript tool locations of one role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JavascriptTools {
    role: ToolchainRole,
    bun_dir: PathBuf,
    /// Holds node, npm and pnpm. npm and pnpm are Node scripts started
    /// through `/usr/bin/env node`, so this directory also leads their PATH.
    node_dir: PathBuf,
}

impl JavascriptTools {
    pub(crate) fn for_role(role: ToolchainRole) -> Self {
        let bun_dir = match role {
            ToolchainRole::BuildServer | ToolchainRole::Boat => HOME_BUN_DIR,
            ToolchainRole::Tenant => TENANT_BUN_DIR,
        };
        Self {
            role,
            bun_dir: bun_dir.into(),
            node_dir: NODE_DIR.into(),
        }
    }

    pub(crate) fn for_config(config: &crate::engine::Config) -> Self {
        Self::for_role(ToolchainRole::of(config))
    }

    /// Tools in other directories, for tests that substitute fake managers or
    /// lay out a role's image in a scratch directory.
    #[cfg(test)]
    pub(crate) fn at(role: ToolchainRole, bun_dir: &Path, node_dir: &Path) -> Self {
        Self {
            role,
            bun_dir: bun_dir.to_path_buf(),
            node_dir: node_dir.to_path_buf(),
        }
    }

    pub(crate) fn bun_dir(&self) -> &Path {
        &self.bun_dir
    }

    /// The configured path of `name` (bun, node, npm or pnpm), whether or
    /// not it exists.
    pub(crate) fn executable(&self, name: &str) -> PathBuf {
        if name == "bun" {
            self.bun_dir.join(name)
        } else {
            self.node_dir.join(name)
        }
    }

    /// The configured path of `name`, or an error naming that path when no
    /// executable file is there. Nothing else is searched.
    pub(crate) fn require(&self, name: &str) -> io::Result<PathBuf> {
        let path = self.executable(name);
        let executable = std::fs::metadata(&path).is_ok_and(|metadata| {
            use std::os::unix::fs::PermissionsExt;
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        });
        if executable {
            Ok(path)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{name} not installed at {}, where the {} toolchain layout \
                     (script/runner/toolchain.json) puts it",
                    path.display(),
                    self.role.name()
                ),
            ))
        }
    }

    /// The PATH the package managers run with: the configured tool
    /// directories first, then fixed system directories, never the daemon's
    /// own PATH.
    pub(crate) fn search_path(&self) -> OsString {
        let mut directories: Vec<&Path> = Vec::new();
        for directory in [self.bun_dir.as_path(), self.node_dir.as_path()]
            .into_iter()
            .chain(SYSTEM_PATH.iter().map(Path::new))
        {
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
        std::env::join_paths(directories).expect("tool directories contain no ':'")
    }
}

#[cfg(test)]
#[path = "javascript_tools_tests.rs"]
mod tests;
