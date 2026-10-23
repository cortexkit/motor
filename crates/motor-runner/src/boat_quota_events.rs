//! Observe the kernel's actual project hard-limit warnings, not usage heuristics.
use std::{
    collections::BTreeMap,
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};

type Monitor = (Arc<AtomicBool>, std::thread::JoinHandle<()>);
static MONITORS: OnceLock<Mutex<BTreeMap<PathBuf, Monitor>>> = OnceLock::new();

fn attrs(mut bytes: &[u8]) -> Vec<(u16, &[u8])> {
    let mut result = Vec::new();
    while bytes.len() >= 4 {
        let length = u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as usize;
        if length < 4 || length > bytes.len() {
            break;
        }
        let kind = u16::from_ne_bytes(bytes[2..4].try_into().unwrap()) & 0x3fff;
        result.push((kind, &bytes[4..length]));
        let next = (length + 3) & !3;
        if next > bytes.len() {
            break;
        }
        bytes = &bytes[next..];
    }
    result
}
fn u32_value(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn hard_hit(payload: &[u8], project: u32, device: u64) -> bool {
    if payload.first() != Some(&1) {
        return false;
    } // QUOTA_NL_C_WARNING
    let values: BTreeMap<_, _> = attrs(payload.get(4..).unwrap_or_default())
        .into_iter()
        .collect();
    let number = |kind| values.get(&kind).and_then(|bytes| u32_value(bytes));
    let id = values
        .get(&2)
        .and_then(|bytes| bytes.get(..8))
        .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap()));
    number(1) == Some(2)
        && id == Some(u64::from(project))
        && matches!(number(3), Some(1 | 4))
        && number(4) == Some(libc::major(device))
        && number(5) == Some(libc::minor(device))
}

fn socket() -> io::Result<OwnedFd> {
    // SAFETY: socket returns a new descriptor, transferred to OwnedFd on success.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_GENERIC,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as u16;
    let timeout = libc::timeval {
        tv_sec: 1,
        tv_usec: 0,
    };
    // SAFETY: the sockaddr/timeval sizes and pointers match their socket APIs.
    if unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &address as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&address) as _,
        )
    } == -1
        || unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &timeout as *const _ as *const _,
                std::mem::size_of_val(&timeout) as _,
            )
        } == -1
    {
        return Err(io::Error::last_os_error());
    }
    let name = b"VFS_DQUOT\0";
    let attr_length = 4 + name.len();
    let mut request = Vec::new();
    request.extend_from_slice(&(20u32 + ((attr_length + 3) & !3) as u32).to_ne_bytes());
    request.extend_from_slice(&0x10u16.to_ne_bytes()); // GENL_ID_CTRL
    request.extend_from_slice(&1u16.to_ne_bytes()); // NLM_F_REQUEST
    request.extend_from_slice(&1u32.to_ne_bytes());
    request.extend_from_slice(&0u32.to_ne_bytes());
    request.extend_from_slice(&[3, 1, 0, 0]); // CTRL_CMD_GETFAMILY
    request.extend_from_slice(&(attr_length as u16).to_ne_bytes());
    request.extend_from_slice(&2u16.to_ne_bytes()); // CTRL_ATTR_FAMILY_NAME
    request.extend_from_slice(name);
    request.resize((request.len() + 3) & !3, 0);
    if unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            request.as_ptr() as *const _,
            request.len(),
            0,
            &address as *const _ as *const _,
            std::mem::size_of_val(&address) as _,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    let mut response = [0u8; 8192];
    let count = unsafe {
        libc::recv(
            fd.as_raw_fd(),
            response.as_mut_ptr() as *mut _,
            response.len(),
            0,
        )
    };
    if count < 20 {
        return Err(io::Error::other("VFS_DQUOT netlink family unavailable"));
    }
    let mut group = None;
    for (kind, value) in attrs(&response[20..count as usize]) {
        if kind != 7 {
            continue;
        } // CTRL_ATTR_MCAST_GROUPS
        for (_, nested) in attrs(value) {
            let values: BTreeMap<_, _> = attrs(nested).into_iter().collect();
            if values.get(&1) == Some(&b"events\0".as_slice()) {
                group = values.get(&2).and_then(|value| u32_value(value));
            }
        }
    }
    let group = group.ok_or_else(|| {
        io::Error::other(
            "VFS_DQUOT events group unavailable; kernel requires CONFIG_QUOTA_NETLINK_INTERFACE",
        )
    })?;
    if unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_NETLINK,
            libc::NETLINK_ADD_MEMBERSHIP,
            &group as *const _ as *const _,
            std::mem::size_of_val(&group) as _,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

pub(crate) fn start(manifest: &Path, project: u32, device: u64) -> io::Result<()> {
    let fd = socket()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let path = manifest.to_path_buf();
    let handle = std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            // SAFETY: recv writes at most buffer.len() initialized bytes.
            let count = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    buffer.as_mut_ptr() as *mut _,
                    buffer.len(),
                    0,
                )
            };
            if count > 0 {
                let mut bytes = &buffer[..count as usize];
                while bytes.len() >= 20 {
                    let length = u32_value(bytes).unwrap() as usize;
                    if length < 20 || length > bytes.len() {
                        break;
                    }
                    if hard_hit(&bytes[16..length], project, device) {
                        let saved = fs::write(path.with_extension("hit"), b"hard limit\n");
                        tracing::warn!(project, manifest = %path.display(), persisted = saved.is_ok(), "Boat job hit disk quota hard limit (kernel warning)");
                        return;
                    }
                    let next = (length + 3) & !3;
                    if next > bytes.len() {
                        break;
                    }
                    bytes = &bytes[next..];
                }
            } else if stop.load(Ordering::Acquire) || !path.exists() {
                return;
            }
        }
    });
    MONITORS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(manifest.to_path_buf(), (stopping, handle));
    Ok(())
}

pub(crate) fn finish(manifest: &Path) -> bool {
    if let Some((stop, handle)) = MONITORS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(manifest)
    {
        stop.store(true, Ordering::Release);
        let _ = handle.join();
    }
    manifest.with_extension("hit").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_only_this_project_and_device_hard_limit_events() {
        let device = libc::makedev(7, 3);
        let event = |warning: u32| {
            let mut result = vec![1, 1, 0, 0];
            for (kind, value) in [
                (1u16, 2u32.to_ne_bytes().to_vec()),
                (2, 42u64.to_ne_bytes().to_vec()),
                (3, warning.to_ne_bytes().to_vec()),
                (4, 7u32.to_ne_bytes().to_vec()),
                (5, 3u32.to_ne_bytes().to_vec()),
            ] {
                result.extend_from_slice(&((value.len() + 4) as u16).to_ne_bytes());
                result.extend_from_slice(&kind.to_ne_bytes());
                result.extend(value);
            }
            result
        };
        assert!(hard_hit(&event(4), 42, device));
        assert!(hard_hit(&event(1), 42, device));
        assert!(!hard_hit(&event(4), 43, device));
        assert!(!hard_hit(&event(4), 42, libc::makedev(7, 4)));
        assert!(!hard_hit(&event(3), 42, device));
        assert!(!hard_hit(&[1, 1, 0, 0, 2, 0, 1, 0], 42, device));
    }
}
