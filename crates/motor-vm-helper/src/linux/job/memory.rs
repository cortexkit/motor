use super::*;
use std::io::{BufReader, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;

struct Process {
    pidfd: OwnedFd,
    directory: File,
}

pub(super) fn pin_pidfd(pid: u32) -> Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

impl Process {
    fn pin(pid: u32) -> Result<Self> {
        let pin = Self {
            pidfd: pin_pidfd(pid)?,
            directory: OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(format!("/proc/{pid}"))?,
        };
        pin.alive()?;
        Ok(pin)
    }

    fn alive(&self) -> Result<()> {
        let mut poll = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        match unsafe { libc::poll(&mut poll, 1, 0) } {
            0 => Ok(()),
            -1 => Err(std::io::Error::last_os_error().into()),
            _ => Err(Error("job VMM exited during memory operation".into())),
        }
    }

    fn smaps(&self) -> Result<String> {
        // The proc directory and pidfd stay pinned together: a reused numeric
        // PID must never redirect a read to the replacement process's mappings.
        self.alive()?;
        let mut file = fs::beneath(self.directory.as_raw_fd(), "smaps", libc::O_RDONLY, 0)?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        self.alive()?;
        Ok(text)
    }
}

impl LinuxJobs {
    fn memory_process(&self, record: &Record) -> Result<Process> {
        let pid = self
            .pid(record)?
            .ok_or_else(|| Error("job VM is not running".into()))?;
        let process = Process::pin(pid)?;
        // Recheck the trusted cgroup and jail UID after opening the pidfd.
        // Neither a PID nor a proc path ever comes from motor-host.
        if self.pid(record)? != Some(pid) {
            return Err(Error("job VMM changed during memory operation".into()));
        }
        process.alive()?;
        Ok(process)
    }

    pub(super) fn memory(&self, id: &str) -> Result<Response> {
        let view = self.running_view(id)?;
        // The published snapshot needs no writer lock during procfs I/O.
        // The pinned process's alive checks reject an exit during the read.
        let record = &view.record;
        let process = self.memory_process(record)?;
        #[cfg(test)]
        self.exit_checkpoint(ExitStep::MemoryRead)?;
        let resident_bytes = guest_resident_bytes(&process.smaps()?, record.start.memory_mib)?;
        process.alive()?;
        Ok(Response::new(Reply::JobMemory { resident_bytes }))
    }

    pub(super) fn balloon(&self, id: &str, amount_mib: u64) -> Result<Response> {
        let view = self.running_view(id)?;
        // A silent API peer must not prevent stop or collection from taking
        // the job lock. The pidfd stays pinned through the final alive check.
        let record = &view.record;
        if amount_mib > record.start.memory_mib {
            return Err(Error("balloon target exceeds guest RAM".into()));
        }
        let process = self.memory_process(record)?;
        // One absolute deadline covers connect, request and status line, so no
        // API peer state can hold this request thread without a bound.
        let deadline = std::time::Instant::now() + API_DEADLINE;
        let mut stream = api_socket(&self.root(id), record.uid, deadline)?;
        process.alive()?;
        patch_balloon(&mut stream, amount_mib, deadline)?;
        process.alive()?;
        Ok(Response::new(Reply::Ok))
    }
}

const API_DEADLINE: Duration = Duration::from_secs(2);

fn remaining(deadline: std::time::Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return Err(Error("Firecracker balloon API deadline passed".into()));
    }
    Ok(left)
}

fn api_socket(root: &Path, uid: u32, deadline: std::time::Instant) -> Result<UnixStream> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    // As with connect_relay, pin and validate the socket, not a mutable name
    // inside the VMM's jail. Connecting via the fd cannot follow a replacement.
    let socket = fs::beneath(directory.as_raw_fd(), "api.sock", libc::O_PATH, 0)?;
    let metadata = socket.metadata()?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(Error("API socket must be owned by the VM jail uid".into()));
    }
    // A non-blocking connect is refused at once when the listener's backlog is
    // full, where a blocking one would wait until the VMM accepted.
    let stream =
        tenant::connect_job_socket(Path::new(&format!("/proc/self/fd/{}", socket.as_raw_fd())))?;
    let left = remaining(deadline)?;
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))?;
    Ok(stream)
}

fn patch_balloon(
    stream: &mut UnixStream,
    amount_mib: u64,
    deadline: std::time::Instant,
) -> Result<()> {
    let body = json!({"amount_mib":amount_mib}).to_string();
    stream.set_write_timeout(Some(remaining(deadline)?))?;
    write!(stream, "PATCH /balloon HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    while !status.ends_with('\n') {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() || status.len() >= 1024 {
            return Err(Error(
                "Firecracker balloon PATCH status timed out or too long".into(),
            ));
        }
        reader.get_ref().set_read_timeout(Some(remaining))?;
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        status.push(byte[0] as char);
    }
    if status.split_whitespace().nth(1) != Some("204") {
        return Err(Error("Firecracker balloon PATCH failed".into()));
    }
    Ok(())
}

#[derive(Default)]
struct Mapping {
    length: u64,
    anonymous_private_rw: bool,
    size: Option<u64>,
    rss: Option<u64>,
    flags: Option<bool>,
}

fn kib_field(text: &str) -> Result<u64> {
    let mut fields = text.split_whitespace();
    let value = fields
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .and_then(|v| v.checked_mul(1024))
        .ok_or_else(|| Error("invalid smaps memory value".into()))?;
    if fields.next() != Some("kB") || fields.next().is_some() {
        return Err(Error("invalid smaps memory unit".into()));
    }
    Ok(value)
}

fn guest_resident_bytes(text: &str, guest_mib: u64) -> Result<u64> {
    // Cold-boot RAM uses private anonymous read/write MAP_NORESERVE mappings:
    // Firecracker v1.17.0 src/vmm/src/vstate/memory.rs:884-960,261-296.
    // https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/src/vmm/src/vstate/memory.rs
    // Our boot has no vhost-user block or snapshot backend. Pmem instead maps a
    // shared host file (devices/virtio/pmem/device.rs:181-255), never guest RAM.
    // Collect only those VMAs and require their *complete* size to equal the
    // configured RAM. Splits are allowed, but missing/extra/ambiguous regions
    // fail closed rather than falling back to process RSS or a largest VMA.
    let mut current: Option<Mapping> = None;
    let (mut size, mut rss) = (0u64, 0u64);
    let mut collect = |mapping: Mapping| -> Result<()> {
        if !mapping.anonymous_private_rw {
            return Ok(());
        }
        if !mapping
            .flags
            .ok_or_else(|| Error("smaps missing VmFlags".into()))?
        {
            return Ok(());
        }
        let mapped_size = mapping
            .size
            .ok_or_else(|| Error("smaps missing Size".into()))?;
        let resident = mapping
            .rss
            .ok_or_else(|| Error("smaps missing Rss".into()))?;
        if mapped_size != mapping.length || resident > mapped_size {
            return Err(Error("inconsistent guest RAM mapping".into()));
        }
        size = size
            .checked_add(mapped_size)
            .ok_or_else(|| Error("smaps size overflow".into()))?;
        rss = rss
            .checked_add(resident)
            .ok_or_else(|| Error("smaps RSS overflow".into()))?;
        Ok(())
    };
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if let Some((start, end)) = fields.first().and_then(|v| v.split_once('-')) {
            if let Some(mapping) = current.take() {
                collect(mapping)?;
            }
            let start = u64::from_str_radix(start, 16)
                .map_err(|_| Error("invalid smaps address".into()))?;
            let end =
                u64::from_str_radix(end, 16).map_err(|_| Error("invalid smaps address".into()))?;
            let length = end
                .checked_sub(start)
                .filter(|v| *v > 0)
                .ok_or_else(|| Error("invalid smaps range".into()))?;
            if fields.len() < 5 {
                return Err(Error("invalid smaps header".into()));
            }
            current = Some(Mapping {
                length,
                anonymous_private_rw: fields.len() == 5
                    && fields[1] == "rw-p"
                    && fields[2].bytes().all(|v| v == b'0')
                    && fields[3] == "00:00"
                    && fields[4] == "0",
                ..Default::default()
            });
        } else if let Some(mapping) = current.as_mut().filter(|m| m.anonymous_private_rw) {
            if let Some(value) = line.strip_prefix("Size:") {
                if mapping.size.replace(kib_field(value)?).is_some() {
                    return Err(Error("duplicate smaps Size".into()));
                }
            } else if let Some(value) = line.strip_prefix("Rss:") {
                if mapping.rss.replace(kib_field(value)?).is_some() {
                    return Err(Error("duplicate smaps Rss".into()));
                }
            } else if let Some(value) = line.strip_prefix("VmFlags:") {
                if mapping
                    .flags
                    .replace(value.split_whitespace().any(|f| f == "nr"))
                    .is_some()
                {
                    return Err(Error("duplicate smaps VmFlags".into()));
                }
            }
        }
    }
    if let Some(mapping) = current {
        collect(mapping)?;
    }
    if size == 0 || Some(size) != guest_mib.checked_mul(1024 * 1024) {
        return Err(Error(
            "cannot identify exact configured guest RAM mappings".into(),
        ));
    }
    Ok(rss)
}

#[cfg(test)]
mod tests;
