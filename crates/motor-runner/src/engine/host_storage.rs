use super::{Catalog, Config};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs, io,
    os::unix::ffi::OsStrExt,
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
        for key in catalog
            .workspaces
            .keys()
            .filter(|key| Path::new(key).is_absolute())
        {
            let target = crate::caller_path::host_path(config, key).join("target");
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
        let name = CString::new(ancestor.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // Mount ID identifies the visible mount, even at a stacked or bind mount
        // where matching the path or st_dev in mountinfo can select the wrong row.
        let status = unsafe {
            libc::statx(
                libc::AT_FDCWD,
                name.as_ptr(),
                0,
                libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        };
        if status == 0 {
            let stat = unsafe { stat.assume_init() };
            if stat.stx_mask & libc::STATX_MNT_ID != 0 {
                return Ok(stat.stx_mnt_id);
            }
            return Err(io::Error::other(format!(
                "no mount ID for {}",
                ancestor.display()
            )));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(io::Error::other(format!(
                "cannot find mount for {}: {error}",
                ancestor.display()
            )));
        }
    }
    Err(io::Error::other(format!("no mount for {}", path.display())))
}

#[cfg(test)]
mod tests;
