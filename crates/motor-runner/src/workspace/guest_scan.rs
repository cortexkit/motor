//! Change scans of guest-writable checkouts never load the guest's Git metadata.
use super::*;

pub(super) struct Repository {
    directory: PathBuf,
    objects: PathBuf,
}

impl Drop for Repository {
    fn drop(&mut self) {
        // This directory is host-private, outside every guest workspace share.
        if let Err(error) = fs::remove_dir_all(&self.directory) {
            tracing::warn!(%error, path = %self.directory.display(), "change-scan metadata cleanup failed");
        }
    }
}

impl Repository {
    fn new(parent: &Path, objects: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(parent)?;
        let directory = parent.join(format!("change-scan-{}", motor_protocol::Uuid::now_v7()));
        fs::create_dir(&directory)?;
        let repository = Self { directory, objects };
        let output = repository
            .command(parent)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .args(["init", "--bare", "--template="])
            .output()?;
        if !output.status.success() {
            return Err(invalid(String::from_utf8_lossy(&output.stderr)));
        }
        Ok(repository)
    }

    pub(super) fn command(&self, worktree: &Path) -> Command {
        let mut command = change_scan_command(worktree);
        // Explicit metadata and a clean environment bypass .git files, config
        // includes and global configuration supplied by the guest or the host.
        // Attributes may select filters, but no guest filter definitions exist
        // in this fresh repository. Never recurse into submodule repositories.
        command
            .env_clear()
            .env("GIT_DIR", &self.directory)
            .env("GIT_WORK_TREE", worktree)
            .env("GIT_OBJECT_DIRECTORY", &self.objects)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .args([
                "-c",
                "core.bare=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "diff.ignoreSubmodules=all",
                "-c",
                "status.submoduleSummary=false",
            ]);
        command
    }
}

pub(super) fn changes(
    trees: &Trees,
    worktree: &Path,
    prepared_javascript_directories: &[PathBuf],
    request: &Run,
) -> io::Result<Vec<String>> {
    let mirror = trees.mirror(paths::request(request).caller_repository_root.text());
    let caller_commit = request.base_commit.clone().unwrap_or_else(|| {
        resolve_caller_commit(&mirror, &request.snapshot_commit, &request.tree_hash)
    });
    let parent = trees.state.join("jobs").join(request.job_id.to_string());
    let repository = Repository::new(&parent, mirror.join("objects"))?;
    let scan = ChangeScan {
        path: worktree,
        guest: Some(&repository),
    };
    scan.git(&["read-tree", &caller_commit])?;
    scan.git(&["update-ref", "HEAD", &caller_commit])?;

    // Git status would otherwise enter populated submodules and consult their
    // guest-controlled configuration. Such a scan is unavailable, not clean.
    for entry in scan
        .git(&["ls-files", "--stage", "-z"])?
        .split(|byte| *byte == 0)
    {
        if !entry.starts_with(b"160000 ") {
            continue;
        }
        let name = entry
            .splitn(2, |byte| *byte == b'\t')
            .nth(1)
            .ok_or_else(|| invalid("missing submodule index path"))?;
        let metadata = worktree.join(change_path(name)).join(".git");
        match fs::symlink_metadata(metadata) {
            Ok(_) => {
                return Err(invalid(
                    "change scan cannot inspect guest submodule metadata",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    trees.changes_with_scan(
        scan,
        prepared_javascript_directories,
        Some(ChangeBaseline {
            objects: &mirror,
            caller_commit: &caller_commit,
            tree_hash: &request.tree_hash,
        }),
    )
}
