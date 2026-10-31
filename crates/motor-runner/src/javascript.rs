//! Platform-independent discovery of tracked JavaScript install roots.
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

pub(crate) const JAVASCRIPT_LOCKFILES: [&str; 4] = [
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "pnpm-lock.yaml",
];
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct JavascriptInstall {
    pub directory: PathBuf,
    pub lockfile: &'static str,
}

pub(crate) fn discover_javascript_installs(
    paths: &[PathBuf],
) -> io::Result<Vec<JavascriptInstall>> {
    let mut directories: BTreeMap<PathBuf, (bool, [bool; 4])> = BTreeMap::new();
    for path in paths {
        let name = path.file_name().unwrap_or_default();
        if name == "package.json" || JAVASCRIPT_LOCKFILES.iter().any(|lock| name == *lock) {
            let entry = directories
                .entry(path.parent().unwrap_or(Path::new("")).to_path_buf())
                .or_default();
            if name == "package.json" {
                entry.0 = true;
            } else if let Some(i) = JAVASCRIPT_LOCKFILES.iter().position(|lock| name == *lock) {
                entry.1[i] = true;
            }
        }
    }
    let mut installs = Vec::new();
    for (directory, (manifest, locks)) in directories {
        let lockfile = locks
            .iter()
            .position(|present| *present)
            .map(|i| JAVASCRIPT_LOCKFILES[i]);
        let label = if directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &directory
        };
        match (manifest, lockfile) {
            (false, Some(lockfile)) => return Err(io::Error::other(format!(
                "workspace directory {} has {} but no package.json; refusing an unlocked JavaScript install", label.display(), directory.join(lockfile).display()
            ))),
            (true, None) if directory.as_os_str().is_empty() => return Err(io::Error::other(
                "workspace directory . has package.json but no supported JavaScript lockfile (expected bun.lock, bun.lockb, package-lock.json, or pnpm-lock.yaml); refusing an unlocked JavaScript install"
            )),
            (true, Some(lockfile)) => installs.push(JavascriptInstall { directory, lockfile }),
            _ => {}
        }
    }
    installs.sort_by(|a, b| {
        (a.directory.components().count(), &a.directory)
            .cmp(&(b.directory.components().count(), &b.directory))
    });
    Ok(installs)
}
