use super::{fs, File, Path, Read, ResizeActivation};
use crate::{Error, Result};
use std::os::unix::fs::MetadataExt;

pub(super) fn read_staged(path: &Path) -> Result<Option<ResizeActivation>> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    let mut file = fs::trusted_file(path)?;
    let meta = file.metadata()?;
    check_metadata(meta.uid(), meta.mode(), meta.is_file())?;
    // JSON may escape each input byte as six bytes. Bound reads independently
    // of the decoded argument limit so malformed root-owned input cannot grow
    // an unbounded allocation in the helper.
    let max = 6 * crate::tenant::activation::MAX_RUNNER_ARGS + 1024;
    let mut bytes = Vec::new();
    (&mut file).take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(Error("staged resize JSON too large".into()));
    }
    ResizeActivation::parse(&bytes).map(Some)
}

fn check_metadata(uid: u32, mode: u32, regular: bool) -> Result<()> {
    if uid != 0 || mode & 0o7777 != 0o600 || !regular {
        return Err(Error(
            "runner-args.staged must be a root-owned mode 0600 regular file".into(),
        ));
    }
    Ok(())
}

pub(super) fn available(path: &Path) -> Result<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| Error(e.to_string()))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    stat.f_bavail
        .checked_mul(stat.f_frsize)
        .ok_or_else(|| Error("host free-space overflow".into()))
}

pub(super) fn complete(path: &Path, activation: &ResizeActivation) -> Result<()> {
    let dir = File::open(path.parent().unwrap())?;
    complete_with(path, activation, || dir.sync_all().map_err(Into::into))
}

fn complete_with(
    path: &Path,
    activation: &ResizeActivation,
    sync: impl FnOnce() -> Result<()>,
) -> Result<()> {
    std::fs::remove_file(path)?;
    if let Err(error) = sync() {
        // An unlink whose directory barrier fails must not silently acknowledge
        // the activation. Restore the intent so a later cold boot re-delivers it.
        let bytes = serde_json::to_vec(activation).map_err(|e| Error(e.to_string()))?;
        super::durable_write(path, &bytes).map_err(|restore| {
            Error(format!(
                "{error}; restoring staged activation failed: {restore}"
            ))
        })?;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
