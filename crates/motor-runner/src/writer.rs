//! Cache-path validation still runs when the writer account is missing: caller
//! paths must never cause checkout ownership repair to hand cache storage to
//! the foreground job account, motor-job.
use crate::{engine::Config, workspace, workspace_access::Unreadable};
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
    resolve_canonical_future(path)
        .map_err(|error| Unreadable::wrap(path, "resolve writer path", error))
}

fn resolve_canonical_future(path: &Path) -> io::Result<PathBuf> {
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

fn contains_base_checkout(_config: &Config, path: &Path, prefix: &str) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(Unreadable::wrap(
                path,
                "inspect directory for runner base checkouts",
                error,
            ));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(false);
    }
    // Legacy base checkouts are siblings of workspaces. Catalog/base-job paths
    // cover deeper containment; inspecting direct children also fences unknown
    // checkouts. Never walk a workspace's retained target/ build directory: its
    // private job-owned directories may be unreadable to the runner, and build
    // trees can be enormous.
    #[cfg(test)]
    if let Some(hook) = &_config.long_work {
        hook("writer_base_checkout_metadata");
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        // Snapshot-mode siblings can be retired after the metadata read. A
        // missing directory cannot contain a writer checkout to hand to a job.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(Unreadable::wrap(
                path,
                "listing the directory for runner base checkouts",
                error,
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            Unreadable::wrap(
                path,
                "read directory entry for runner base checkouts",
                error,
            )
        })?;
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            let kind = entry.file_type().map_err(|error| {
                Unreadable::wrap(&entry.path(), "inspect runner base checkout entry", error)
            })?;
            if kind.is_dir() || kind.is_symlink() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn validate_paths(
    config: &Config,
    workspace: crate::caller_path::HostPath<'_>,
    cwd: Option<crate::caller_path::HostPath<'_>>,
    siblings: &[SiblingSnapshot],
    known_checkouts: &[PathBuf],
) -> io::Result<()> {
    validate_resolved_paths(
        config,
        known_checkouts,
        std::iter::once(workspace)
            .chain(cwd)
            .chain(siblings.iter().map(|s| crate::caller_path::sibling(s).host))
            .map(|path| {
                let canonical = canonical_future(path.as_path())?;
                Ok((path.as_path().to_path_buf(), canonical))
            }),
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
            || contains_base_checkout(config, &canonical, &prefix)?
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
        let cwd = std::env::current_dir().map_err(|error| {
            Unreadable::wrap(path, "resolve runner storage relative path", error)
        })?;
        canonical_future(&cwd.join(path))
    }
}

#[cfg(test)]
mod tests;
