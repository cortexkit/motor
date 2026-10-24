//! Cache-path validation still runs when the writer account is missing: caller
//! paths must never cause checkout ownership repair to hand cache storage to
//! the foreground job account, motor-job.
use crate::{engine::Config, workspace};
use motor_protocol::SiblingSnapshot;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
mod cache;
pub(crate) mod declaration;
pub(crate) mod fill;
#[cfg(target_os = "linux")]
pub use cache::CacheFill;

/// Resolve symlinks in existing ancestors without requiring a not-yet-created
/// checkout to exist. A dangling symlink or a permission failure is not a safe
/// missing ancestor and is refused rather than treated as a lexical path.
pub(crate) fn canonical_future(path: &Path) -> io::Result<PathBuf> {
    if !workspace::absolute(&path.to_string_lossy()) {
        return Err(io::Error::other(
            "writer path exclusion requires an absolute, clean path",
        ));
    }
    let mut existing = path;
    let mut tail = Vec::new();
    let mut looked_again = false;
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for component in tail.into_iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::symlink_metadata(existing) {
                    // Created after canonicalize looked, by a setup running
                    // alongside (a first workspace and a cache fill both
                    // create the shared parent folder): look once more.
                    Ok(metadata) if !metadata.file_type().is_symlink() && !looked_again => {
                        looked_again = true;
                        continue;
                    }
                    Ok(_) => return Err(error),
                    Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                    Err(other) => return Err(other),
                }
                tail.push(existing.file_name().ok_or(error)?);
                existing = existing
                    .parent()
                    .ok_or_else(|| io::Error::other("path has no existing ancestor"))?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn has_base_component(path: &Path, prefix: &str) -> bool {
    path.components()
        .any(|component| component.as_os_str().to_string_lossy().starts_with(prefix))
}

fn contains_base_checkout(path: &Path, prefix: &str) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(false);
    }
    // Legacy base checkouts are siblings of workspaces. Catalog/base-job paths
    // cover deeper containment; inspecting direct children also fences unknown
    // checkouts. Never walk a workspace's retained target/ build directory: its
    // private job-owned directories may be unreadable to the runner, and build
    // trees can be enormous.
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            let kind = entry.file_type()?;
            if kind.is_dir() || kind.is_symlink() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn validate_paths(
    config: &Config,
    workspace: &str,
    cwd: Option<&str>,
    siblings: &[SiblingSnapshot],
    known_checkouts: &[PathBuf],
) -> io::Result<()> {
    validate_resolved_paths(
        config,
        known_checkouts,
        std::iter::once(workspace)
            .chain(cwd)
            .chain(siblings.iter().map(|s| s.canonical_path.as_str()))
            .map(|path| Ok((PathBuf::from(path), canonical_future(Path::new(path))?))),
    )
}

pub(crate) fn validate_alias_paths(
    config: &Config,
    aliases: &[workspace::SiblingAlias],
    known_checkouts: &[PathBuf],
) -> io::Result<()> {
    validate_resolved_paths(
        config,
        known_checkouts,
        aliases
            .iter()
            .map(|alias| Ok((alias.path.clone(), alias.canonical.clone()))),
    )
}

fn validate_resolved_paths(
    config: &Config,
    known_checkouts: &[PathBuf],
    paths: impl IntoIterator<Item = io::Result<(PathBuf, PathBuf)>>,
) -> io::Result<()> {
    let mut protected = vec![
        canonical_runner_path(&config.state_dir().join("sccache"))?,
        canonical_runner_path(&config.writer_scratch_dir())?,
    ];
    for trash in config.boat_workspace_trash_roots() {
        protected.push(canonical_runner_path(&trash)?);
    }
    for checkout in known_checkouts {
        protected.push(canonical_future(checkout)?);
    }
    let prefix = format!(".motor-base-{}-", config.instance);
    for path in paths {
        let (supplied, canonical) = path?;
        if protected.iter().any(|owned| overlaps(&canonical, owned))
            || has_base_component(&supplied, &prefix)
            || has_base_component(&canonical, &prefix)
            || contains_base_checkout(&canonical, &prefix)?
        {
            return Err(io::Error::other(format!(
                "caller path overlaps writer-owned storage: {}",
                supplied.display()
            )));
        }
    }
    Ok(())
}

fn canonical_runner_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        canonical_future(path)
    } else {
        canonical_future(&std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests;
