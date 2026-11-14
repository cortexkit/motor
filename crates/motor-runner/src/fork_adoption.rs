//! Move a fork's checkout without copying files or changing their timestamps.
use crate::{caller_path as paths, storage::Catalog};
use motor_protocol::Run;
use std::{
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

#[derive(Debug)]
pub(crate) enum MoveError {
    Refused(io::Error),
    Recovery(io::Error),
}

impl From<io::Error> for MoveError {
    fn from(error: io::Error) -> Self {
        Self::Refused(error)
    }
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(error) | Self::Recovery(error) => error.fmt(formatter),
        }
    }
}

pub(crate) fn donor(catalog: &Catalog, request: &Run) -> io::Result<String> {
    let request_paths = paths::request(request);
    if catalog
        .workspaces
        .contains_key(request_paths.caller_workspace_key.text())
    {
        return Err(refusal("caller already has a catalog entry"));
    }
    if request_paths
        .caller_workspace_key
        .text()
        .starts_with("base:")
    {
        return Err(refusal("caller is a base workspace"));
    }
    if !request.siblings.is_empty() {
        return Err(refusal("request has siblings"));
    }
    let matching: Vec<_> = catalog
        .workspaces
        .iter()
        .filter(|(_, workspace)| {
            paths::workspace_record(workspace).repository.caller.text()
                == request_paths.caller_repository_root.text()
        })
        .collect();
    let [(key, workspace)] = matching.as_slice() else {
        return Err(refusal(
            "expected exactly one checkout of the repository root",
        ));
    };
    if key.starts_with("base:") {
        return Err(refusal("donor is a base workspace"));
    }
    if !workspace.siblings.is_empty() {
        return Err(refusal("donor has siblings"));
    }
    Ok((*key).clone())
}

pub(crate) fn checkout_path(roots: &Path, caller: &str) -> io::Result<PathBuf> {
    if !caller.starts_with('/')
        || caller.ends_with('/')
        || caller[1..]
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(refusal("path outside /tenant/roots"));
    }
    // A path already spelled on the checkout disk needs no bind-root prefix.
    if Path::new(caller).starts_with(roots) {
        return Ok(PathBuf::from(caller));
    }
    if !matches!(caller.split('/').nth(1), Some("home" | "Users")) {
        return Err(refusal("path outside /tenant/roots"));
    }
    Ok(roots.join(&caller[1..]))
}

fn refusal(reason: &str) -> io::Error {
    io::Error::other(reason)
}

fn directory(path: &Path, device: u64) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(refusal("symlinked component"));
    }
    if !metadata.is_dir() {
        return Err(refusal("component is not a directory"));
    }
    if metadata.dev() != device {
        return Err(refusal("different filesystem"));
    }
    Ok(())
}

fn components(roots: &Path, path: &Path, device: u64) -> io::Result<()> {
    if path == roots
        || !path.starts_with(roots)
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || path.strip_prefix(roots).is_ok_and(|p| {
            p.components()
                .next()
                .is_some_and(|c| c.as_os_str().to_string_lossy().starts_with(".adopt-"))
        })
    {
        return Err(refusal("path outside checkout roots"));
    }
    for ancestor in path.ancestors() {
        // Check symlinks even above roots, but filesystem boundaries above the
        // checkout disk are expected (the disk itself is mounted at /tenant).
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(refusal("symlinked component"));
            }
            Ok(_) if ancestor.starts_with(roots) => directory(ancestor, device)?,
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// After staging the source, the destination may contain only one empty chain.
/// Validate the entire chain before deleting any directory, so a refusal can
/// restore the source with a single rename.
fn empty_chain(destination: &Path, device: u64, source: Option<&Path>) -> io::Result<Vec<PathBuf>> {
    let mut chain = Vec::new();
    let mut current = destination.to_path_buf();
    if source.is_some_and(|source| current.starts_with(source)) {
        return Ok(chain);
    }
    loop {
        match fs::symlink_metadata(&current) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
            Ok(_) => directory(&current, device)?,
        }
        chain.push(current.clone());
        let children = fs::read_dir(&current)?
            .filter(|entry| {
                entry
                    .as_ref()
                    .map_or(true, |entry| Some(entry.path().as_path()) != source)
            })
            .collect::<io::Result<Vec<_>>>()?;
        match children.as_slice() {
            [] => break,
            [child] => current = child.path(),
            _ => return Err(refusal("non-empty destination")),
        }
    }
    Ok(chain)
}

pub(crate) fn clean_staging(roots: &Path) -> io::Result<()> {
    match fs::symlink_metadata(roots) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(refusal("checkout roots is not a directory"))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => (),
    }
    let entries = match fs::read_dir(roots) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(".adopt-") {
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

/// Settle an interrupted build-base copy only after all adoption checks pass,
/// before the first rename can invalidate its recorded filesystem locations.
pub(crate) fn move_checkout(
    roots: &Path,
    source: &Path,
    destination: &Path,
    repair: impl FnOnce() -> io::Result<()>,
    commit: impl FnOnce() -> io::Result<()>,
) -> Result<(), MoveError> {
    let device = fs::symlink_metadata(roots)?.dev();
    components(roots, source, device)?;
    components(roots, destination, device)?;
    directory(source, device)?;
    empty_chain(destination, device, Some(source))?;
    repair()?;
    let staging = roots.join(format!(".adopt-{}", uuid::Uuid::now_v7()));
    let missing_parents: Vec<_> = destination
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|path| !path.exists())
        .map(Path::to_path_buf)
        .collect();
    fs::rename(source, &staging)?;
    let mut removed = Vec::new();
    let result = (|| {
        let chain = empty_chain(destination, device, None)?;
        for path in chain.iter().rev() {
            let metadata = fs::metadata(path)?;
            fs::remove_dir(path)?;
            removed.push((path.clone(), metadata));
        }
        // This is the same parent creation used by Trees::sync_workspace for
        // a new clone, including the daemon's ownership and umask.
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::rename(&staging, destination)?;
        crate::filesystem::sync_directory(destination.parent().unwrap())?;
        crate::filesystem::sync_directory(roots)?;
        commit()
    })();
    if let Err(error) = result {
        let rollback = (|| -> io::Result<()> {
            if !staging.exists() {
                fs::rename(destination, &staging)?;
            }
            // Empty parents created for a nested destination must not obstruct
            // putting the donor back. Never remove a directory containing data.
            for path in &missing_parents {
                match fs::remove_dir(path) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(_) => break,
                }
            }
            for (path, metadata) in removed.iter().rev() {
                fs::create_dir_all(path)?;
                fs::set_permissions(path, metadata.permissions())?;
                std::os::unix::fs::chown(path, Some(metadata.uid()), Some(metadata.gid()))?;
            }
            fs::rename(&staging, source)?;
            crate::filesystem::sync_directory(roots)
        })();
        rollback.map_err(|rollback| {
            MoveError::Recovery(io::Error::other(format!(
                "{error}; restoring donor failed: {rollback}"
            )))
        })?;
        return Err(MoveError::Refused(error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    fn move_to(roots: &Path, source: &Path, destination: &Path) -> Result<(), MoveError> {
        move_checkout(roots, source, destination, || Ok(()), || Ok(()))
    }

    #[test]
    fn destination_inside_source_preserves_files_and_times() {
        let roots = ScratchDir::new("adopt-nested-destination");
        let source = roots.join("r");
        fs::create_dir(&source).unwrap();
        let file = source.join("artifact");
        fs::write(&file, b"built").unwrap();
        let before = fs::metadata(&file).unwrap();
        let destination = source.join(".worktrees/y");
        move_to(&roots, &source, &destination).unwrap();
        let after = fs::metadata(destination.join("artifact")).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert!(!source.join("artifact").exists());
    }

    #[test]
    fn source_inside_destination_removes_empty_chain() {
        let roots = ScratchDir::new("adopt-nested-source");
        let destination = roots.join("r");
        let source = destination.join(".worktrees/y");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("artifact"), b"built").unwrap();
        move_to(&roots, &source, &destination).unwrap();
        assert_eq!(fs::read(destination.join("artifact")).unwrap(), b"built");
        assert!(!destination.join(".worktrees").exists());
    }

    #[test]
    fn nonempty_destination_restores_staged_source() {
        let roots = ScratchDir::new("adopt-nonempty");
        let source = roots.join("donor");
        let destination = roots.join("new");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("artifact"), b"built").unwrap();
        fs::write(destination.join("unrelated"), b"keep").unwrap();
        assert!(move_to(&roots, &source, &destination).is_err());
        assert_eq!(fs::read(source.join("artifact")).unwrap(), b"built");
        assert_eq!(fs::read(destination.join("unrelated")).unwrap(), b"keep");
        assert_eq!(fs::read_dir(&roots).unwrap().count(), 2);
    }

    #[test]
    fn symlinked_component_is_refused_without_following_it() {
        let roots = ScratchDir::new("adopt-symlink");
        let source = roots.join("donor");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("artifact"), b"built").unwrap();
        std::os::unix::fs::symlink(&source, roots.join("alias")).unwrap();
        let error = move_to(&roots, &source, &roots.join("alias/new")).unwrap_err();
        assert!(error.to_string().contains("symlinked component"));
        assert_eq!(fs::read(source.join("artifact")).unwrap(), b"built");
    }

    #[test]
    fn outside_roots_is_refused() {
        let scratch = ScratchDir::new("adopt-outside");
        let roots = scratch.join("roots");
        let source = roots.join("donor");
        fs::create_dir_all(&source).unwrap();
        let error = move_to(&roots, &source, &scratch.join("outside")).unwrap_err();
        assert!(error.to_string().contains("outside checkout roots"));
        assert!(source.is_dir());
    }

    #[test]
    fn filesystem_boundary_is_refused_by_component_guard() {
        let roots = ScratchDir::new("adopt-device");
        let device = fs::metadata(&roots).unwrap().dev();
        // procfs supplies a real foreign filesystem without requiring a mount
        // privilege or writing outside the test scratch root.
        let foreign = Path::new("/proc");
        assert_ne!(fs::metadata(foreign).unwrap().dev(), device);
        let error = directory(foreign, device).unwrap_err();
        assert!(error.to_string().contains("different filesystem"));
    }

    #[test]
    fn refused_destination_never_repairs_seed_state() {
        let roots = ScratchDir::new("adopt-prechecks");
        let source = roots.join("donor");
        let destination = roots.join("new");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(destination.join("one")).unwrap();
        fs::create_dir_all(destination.join("two")).unwrap();
        let repaired = std::cell::Cell::new(false);
        assert!(move_checkout(
            &roots,
            &source,
            &destination,
            || {
                repaired.set(true);
                Ok(())
            },
            || Ok(())
        )
        .is_err());
        assert!(!repaired.get());
        assert!(source.is_dir());
        assert!(destination.join("one").is_dir());
        assert!(destination.join("two").is_dir());
    }

    #[test]
    fn persistence_refusal_restores_source_and_empty_destination_chain() {
        for (from, to) in [
            ("r", "r/.worktrees/y"),
            ("r/.worktrees/y", "r"),
            ("donor", "new"),
        ] {
            let roots = ScratchDir::new("adopt-rollback");
            let source = roots.join(from);
            let destination = roots.join(to);
            fs::create_dir_all(&source).unwrap();
            fs::write(source.join("artifact"), b"built").unwrap();
            if from == "donor" {
                fs::create_dir_all(destination.join("empty/leaf")).unwrap();
            }
            let error = move_checkout(
                &roots,
                &source,
                &destination,
                || Ok(()),
                || Err(refusal("catalog persistence failed")),
            )
            .unwrap_err();
            assert!(matches!(error, MoveError::Refused(_)), "{error}");
            assert_eq!(fs::read(source.join("artifact")).unwrap(), b"built");
            if from == "donor" {
                assert!(destination.join("empty/leaf").is_dir());
            } else if from == "r" {
                assert!(!destination.exists());
                assert!(!source.join(".worktrees").exists());
            }
            assert!(fs::read_dir(&roots).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".adopt-")));
        }
    }

    #[test]
    fn startup_discards_only_adoption_staging_without_following_symlinks() {
        let roots = ScratchDir::new("adopt-startup");
        fs::create_dir(roots.join(".adopt-crashed")).unwrap();
        fs::write(roots.join(".adopt-crashed/artifact"), b"orphan").unwrap();
        fs::create_dir(roots.join("checkout")).unwrap();
        fs::write(roots.join("checkout/artifact"), b"keep").unwrap();
        std::os::unix::fs::symlink(roots.join("checkout"), roots.join(".adopt-link")).unwrap();
        clean_staging(&roots).unwrap();
        assert!(!roots.join(".adopt-crashed").exists());
        assert!(!roots.join(".adopt-link").exists());
        assert_eq!(fs::read(roots.join("checkout/artifact")).unwrap(), b"keep");
    }
}
