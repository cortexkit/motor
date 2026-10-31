//! Requested-file collection. Callers must hold the job user's impersonation
//! guard for the entire walk and read, including error paths.

use crate::{Frame, invalid, is_reparse, open_source_file, source_directory, validate_component};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

pub const MAX_BYTES: u64 = 128 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Workspace-relative, slash-separated names, in manifest order.
    pub paths: Vec<String>,
    pub max_bytes: u64,
}

impl Request {
    pub fn validate(&self) -> io::Result<()> {
        if self.paths.is_empty()
            || self.paths.len() > 64
            || self.max_bytes > MAX_BYTES
            || self.paths.iter().any(|p| p.len() > 16 * 1024)
        {
            return Err(invalid("invalid requested-file count or cap"));
        }
        Ok(())
    }
}

/// Typed per-file failure causes for requested-file collection. The host maps
/// them to motor_protocol::ReturnPathsFailureCause in its durable manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    OverCap,
    Missing,
    Escape,
    NotRegular,
    ReadError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Returned { size: u64, sha256: String },
    Failed { cause: Failure, size: Option<u64> },
}

pub fn validate_relative(path: &str) -> Result<(), Failure> {
    if path.is_empty() || path.contains('\\') {
        return Err(Failure::Escape);
    }
    for component in path.split('/') {
        validate_component(component).map_err(|_| Failure::Escape)?;
    }
    Ok(())
}

fn path_error(error: io::Error) -> Failure {
    if error.kind() == io::ErrorKind::NotFound {
        Failure::Missing
    } else {
        Failure::ReadError
    }
}

// Keep each ancestor open without write/delete sharing on Windows. Merely
// checking attributes before opening a pathname leaves a junction-swap race.
fn directories(root: &Path, relative: &str) -> Result<(PathBuf, Vec<File>), Failure> {
    let mut path = root.to_owned();
    let mut handles = Vec::new();
    for component in std::iter::once("").chain(relative.split('/').filter(|c| !c.is_empty())) {
        path.push(component);
        let metadata = std::fs::symlink_metadata(&path).map_err(path_error)?;
        if is_reparse(&metadata) {
            return Err(Failure::Escape);
        }
        if !metadata.is_dir() {
            return Err(Failure::NotRegular);
        }
        handles.push(source_directory(&path).map_err(path_error)?);
    }
    Ok((path, handles))
}

fn open(root: &Path, relative: &str) -> Result<(File, Vec<File>), Failure> {
    validate_relative(relative)?;
    let (parent, name) = relative.rsplit_once('/').unwrap_or(("", relative));
    let (parent, handles) = directories(root, parent)?;
    let path = parent.join(name);
    let metadata = std::fs::symlink_metadata(&path).map_err(path_error)?;
    if is_reparse(&metadata) {
        return Err(Failure::Escape);
    }
    if !metadata.is_file() {
        return Err(Failure::NotRegular);
    }
    let file = open_source_file(&path).map_err(path_error)?;
    Ok((file, handles))
}

fn identity(file: &File) -> io::Result<(u64, u64)> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            info.dwVolumeSerialNumber as u64,
            (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64,
        ))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok((meta.dev(), meta.ino()))
    }
}

/// Streams only requested files. The callback is synchronous: neither a file
/// nor the complete collection is ever accumulated in memory.
pub fn collect(
    root: &Path,
    workspace: &str,
    request: &Request,
    mut send: impl FnMut(Frame) -> io::Result<()>,
) -> io::Result<()> {
    collect_before_read(root, workspace, request, &mut send, |_| {})
}

pub fn collect_before_read(
    root: &Path,
    workspace: &str,
    request: &Request,
    send: &mut impl FnMut(Frame) -> io::Result<()>,
    mut before_read: impl FnMut(usize),
) -> io::Result<()> {
    #[cfg(windows)]
    require_user_impersonation()?;
    request.validate()?;
    if !workspace.is_empty() {
        validate_relative(workspace).map_err(|_| invalid("invalid workspace"))?;
    }
    let workspace = directories(root, workspace);
    let mut read_bytes = 0u64;
    let mut buffer = vec![0; CHUNK_BYTES];
    for (index, path) in request.paths.iter().enumerate() {
        let mut size = 0;
        let mut measured_size = None;
        let mut transport_error = None;
        let result = (|| -> Result<String, Failure> {
            let (workspace, _ancestors) = workspace.as_ref().map_err(|cause| *cause)?;
            let (mut file, _parents) = open(workspace, path)?;
            let before = file.metadata().map_err(path_error)?;
            measured_size = Some(before.len());
            // Reject known oversize files before touching their payload. Failed
            // reads also spend the collection allowance, not just returned files.
            if read_bytes > request.max_bytes
                || before.len() > request.max_bytes.saturating_sub(read_bytes)
            {
                return Err(Failure::OverCap);
            }
            let id = identity(&file).map_err(path_error)?;
            before_read(index);
            let mut hash = Sha256::new();
            loop {
                // One lookahead byte distinguishes EOF from a file exceeding
                // the cap. It is counted, but never sent or stored on the host.
                let want = request
                    .max_bytes
                    .saturating_sub(read_bytes)
                    .min(CHUNK_BYTES as u64)
                    .max(1) as usize;
                let n = file
                    .read(&mut buffer[..want])
                    .map_err(|_| Failure::ReadError)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                read_bytes += n as u64;
                if read_bytes > request.max_bytes {
                    let after = file.metadata().map_err(path_error)?;
                    if before.len() != after.len()
                        || before.modified().ok() != after.modified().ok()
                    {
                        return Err(Failure::ReadError);
                    }
                    return Err(Failure::OverCap);
                }
                hash.update(&buffer[..n]);
                send(Frame::ReturnChunk {
                    bytes: buffer[..n].to_vec(),
                })
                .map_err(|error| {
                    transport_error = Some(error);
                    Failure::ReadError
                })?;
            }
            let after = file.metadata().map_err(path_error)?;
            let (reopened, _parents) = open(workspace, path).map_err(|_| Failure::ReadError)?;
            if before.len() != size
                || after.len() != size
                || before.modified().ok() != after.modified().ok()
                || identity(&reopened).map_err(path_error)? != id
            {
                return Err(Failure::ReadError);
            }
            Ok(format!("{:x}", hash.finalize()))
        })();
        if let Some(error) = transport_error {
            return Err(error);
        }
        let outcome = match result {
            Ok(sha256) => Outcome::Returned { size, sha256 },
            Err(cause) => Outcome::Failed {
                cause,
                size: if cause == Failure::OverCap {
                    measured_size
                } else {
                    Some(size)
                },
            },
        };
        send(Frame::ReturnFileEnd { outcome })?;
    }
    send(Frame::ReturnEnd {})
}

#[cfg(windows)]
fn require_user_impersonation() -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{
            GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
            WinLocalSystemSid,
        },
        System::Threading::{GetCurrentThread, OpenThreadToken},
    };
    let mut token = std::ptr::null_mut();
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
        return Err(invalid(
            "requested-file collection requires job-user impersonation",
        ));
    }
    let mut data = [0usize; 64];
    let mut length = 0;
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            data.as_mut_ptr().cast(),
            std::mem::size_of_val(&data) as u32,
            &mut length,
        )
    };
    let system = read != 0
        && unsafe {
            IsWellKnownSid(
                (*(data.as_ptr().cast::<TOKEN_USER>())).User.Sid,
                WinLocalSystemSid,
            )
        } != 0;
    unsafe {
        CloseHandle(token);
    }
    if read == 0 || system {
        return Err(invalid(
            "requested-file collection refuses the SYSTEM token",
        ));
    }
    Ok(())
}
