use super::{Catalog, Config};
use crate::caller_path;
use std::{
    collections::BTreeSet,
    fs, io,
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::{Path, PathBuf},
};

pub(super) fn check(config: &Config, state: &Path, catalog: &Catalog) -> io::Result<()> {
    if !config.isolate_jobs || config.vm_instance() {
        return Ok(());
    }
    check_floor(state, config.free_space_floor_bytes, fs2::total_space)?;
    if config.boat_profile {
        // Boat's free-space probe is bounded by both the loop XFS and its backing volume.
        check_floor(
            Path::new("/"),
            config.free_space_floor_bytes,
            fs2::total_space,
        )?;
    } else {
        let mut roots: BTreeSet<_> = config
            .workspace_roots
            .iter()
            .chain(&config.owned_workspace_roots)
            .cloned()
            .chain(
                config
                    .caller_roots
                    .iter()
                    .map(|root| root.host_root.clone()),
            )
            .collect();
        for path in caller_path::keys(&catalog.workspaces)
            .filter(|path| crate::workspace::absolute(path.caller.text()))
        {
            let target = caller_path::host_path(config, path.caller.text()).join("target");
            // Cargo cleanup does not traverse absent or symlinked targets.
            if target.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                roots.insert(target);
            }
        }
        check_atime(roots)?;
    }
    check_reflink(state, |state| {
        crate::caller_path::startup::reflink(&fs::File::open(state)?)
    })
}

fn check_floor<'a>(
    path: &'a Path,
    floor: u64,
    total_space: impl FnOnce(&'a Path) -> io::Result<u64>,
) -> io::Result<()> {
    if floor == 0 {
        return Ok(());
    }
    let total = total_space(path).map_err(|error| {
        io::Error::other(format!(
            "cannot measure total volume size at {}: {error}",
            path.display()
        ))
    })?;
    if floor >= total {
        return Err(io::Error::other(format!(
            "--free-space-floor-bytes sets a hard floor of {floor} bytes, which must be below the total volume size of {total} bytes at {}",
            path.display(),
        )));
    }
    Ok(())
}

fn check_reflink(state: &Path, probe: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
    probe(state).map_err(|error| {
        io::Error::other(format!(
            "native Linux storage requires FICLONE reflinks in {}: {error}",
            state.display()
        ))
    })
}

fn check_atime(roots: BTreeSet<PathBuf>) -> io::Result<()> {
    let targets = roots
        .into_iter()
        .map(|path| covering_mount_id(&path).map(|id| (path, id)))
        .collect::<io::Result<Vec<_>>>()?;
    let table = fs::read("/proc/self/mountinfo")?;
    crate::caller_path::startup::check_gc_mounts(&table, targets)
}

fn covering_mount_id(path: &Path) -> io::Result<u64> {
    for ancestor in path.ancestors() {
        // An O_PATH handle needs no read permission on the directory itself.
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(ancestor)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(io::Error::other(format!(
                    "cannot find mount for {}: {error}",
                    ancestor.display()
                )))
            }
        };
        // The mount ID identifies the visible mount, even at a stacked or bind
        // mount where matching the path or st_dev in mountinfo can select the
        // wrong row. fdinfo reports it on every libc, unlike statx.
        let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
        return info
            .lines()
            .find_map(|line| line.strip_prefix("mnt_id:"))
            .ok_or_else(|| io::Error::other(format!("no mount ID for {}", ancestor.display())))?
            .trim()
            .parse()
            .map_err(io::Error::other);
    }
    Err(io::Error::other(format!("no mount for {}", path.display())))
}

#[cfg(test)]
mod tests;
