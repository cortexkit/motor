//! Format the identity-checked new cache volume through the Windows storage provider.

#![cfg(any(windows, test))]

use std::{io, time::Instant};

#[cfg(windows)]
mod wmi;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Volume {
    path: String,
    object_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FormatOptions {
    filesystem: &'static str,
    label: &'static str,
    allocation_unit_size: u32,
    full: bool,
    force: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            filesystem: "NTFS",
            label: crate::CACHE_LABEL,
            allocation_unit_size: 0,
            full: false,
            force: true,
        }
    }
}

struct FormatResult {
    return_value: u32,
    extended_status: String,
}

/// The provider operations are replaceable so selection, parameters and deadlines
/// can be tested without access to Windows disks.
trait StorageProvider {
    fn volumes(&mut self, deadline: Instant) -> io::Result<Vec<Volume>>;
    fn start(
        &mut self,
        object_path: &str,
        options: &FormatOptions,
        deadline: Instant,
    ) -> io::Result<()>;
    fn poll(&mut self, timeout_ms: i32) -> io::Result<Option<FormatResult>>;
}

fn remaining_ms(deadline: Instant) -> io::Result<i32> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "cache storage format timed out"))?;
    Ok(remaining.as_millis().clamp(1, 100) as i32)
}

fn validate_volume(volume: &str) -> io::Result<()> {
    if !crate::cache::is_volume_guid_path(volume) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid cache volume identity for format",
        ));
    }
    Ok(())
}

fn format_with(
    provider: &mut impl StorageProvider,
    volume: &str,
    deadline: Instant,
) -> io::Result<()> {
    validate_volume(volume)?;
    remaining_ms(deadline)?;
    let volumes = provider.volumes(deadline)?;
    let mut matching = volumes
        .iter()
        .filter(|candidate| candidate.path.eq_ignore_ascii_case(volume));
    let selected = matching
        .next()
        .ok_or_else(|| io::Error::other("cache volume absent from MSFT_Volume"))?;
    if selected.object_path.is_empty() || matching.next().is_some() {
        return Err(io::Error::other(
            "cache volume has no unique WMI object identity",
        ));
    }
    remaining_ms(deadline)?;
    provider.start(&selected.object_path, &FormatOptions::default(), deadline)?;
    loop {
        let result = provider.poll(remaining_ms(deadline)?)?;
        // A completion observed after the deadline is still ambiguous. The host
        // must discard the disk, even if the provider eventually finishes it.
        remaining_ms(deadline)?;
        if let Some(result) = result {
            return format_result(result);
        }
    }
}

fn format_result(result: FormatResult) -> io::Result<()> {
    if result.return_value == 0 {
        return Ok(());
    }
    let (kind, description) = match result.return_value {
        1 | 7 => (io::ErrorKind::Unsupported, "not supported"),
        3 => (io::ErrorKind::TimedOut, "provider timeout"),
        5 => (io::ErrorKind::InvalidInput, "invalid parameter"),
        40001 => (io::ErrorKind::PermissionDenied, "access denied"),
        40004 => (io::ErrorKind::Other, "unexpected I/O error"),
        42010 => (io::ErrorKind::Other, "system or critical partition"),
        43000 => (io::ErrorKind::Other, "invalid cluster size"),
        43001 => (io::ErrorKind::Other, "unsupported filesystem"),
        43002 => (io::ErrorKind::Other, "quick format unavailable"),
        43006 => (io::ErrorKind::Other, "read-only volume"),
        _ => (io::ErrorKind::Other, "provider failure"),
    };
    // Keep the return code ahead of the provider diagnostic so it survives
    // truncation to the cache error message's 1024-byte limit.
    Err(io::Error::new(
        kind,
        format!(
            "MSFT_Volume.Format returned {} ({description}); ExtendedStatus={:?}",
            result.return_value, result.extended_status,
        ),
    ))
}

fn call_complete(hresult: i32, operation_status: i32) -> io::Result<bool> {
    const WBEM_S_TIMEDOUT: i32 = 0x40004;
    if hresult == WBEM_S_TIMEDOUT {
        return Ok(false);
    }
    if hresult != 0 || operation_status != 0 {
        return Err(io::Error::other(format!(
            "cache WMI wait failed: HRESULT 0x{:08x}; operation status 0x{:08x}",
            hresult as u32, operation_status as u32,
        )));
    }
    Ok(true)
}

/// No shell, drive letter or mount folder is needed: WMI invokes Format on the
/// volume instance whose Path matches the GUID already selected by disk extents.
/// The deadline comes unchanged from WindowsStorage::initialize_raw and includes
/// partition creation in its 30-second budget.
#[cfg(windows)]
pub fn run(volume: &str, deadline: Instant) -> io::Result<()> {
    validate_volume(volume)?;
    remaining_ms(deadline)?;
    let mut provider = wmi::Provider::connect()?;
    format_with(&mut provider, volume, deadline)
}
