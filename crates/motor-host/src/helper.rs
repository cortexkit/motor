use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FRAME_LIMIT: usize = 1_048_576;

pub trait Control: Send + Sync {
    fn metrics(&self, _tenant: &str) -> Result<crate::billing::Metrics> {
        anyhow::bail!("tenant metrics unavailable")
    }
    fn stop(&self, _tenant: &str) -> Result<()> {
        anyhow::bail!("tenant stop unavailable")
    }
    fn adopt(&self, tenant: &str) -> Result<bool>;
    fn wake(&self, tenant: &str, initialized_generation: Option<u64>) -> Result<u64>;
    fn quota(&self, tenant: &str, vcpus: u32) -> Result<()>;
    fn relay(&self, tenant: &str, port: u32) -> Result<UnixStream>;
    fn admin(&self, tenant: &str, request: &Value) -> Result<Value>;
    fn suspend(&self, tenant: &str) -> Result<()>;
    fn available_bytes(&self) -> Result<u64>;
}

pub struct Helper {
    pub socket: PathBuf,
    pub storage: PathBuf,
    pub guest_init: BTreeMap<String, Value>,
}
impl Helper {
    fn request(&self, request: Value) -> Result<(Value, Vec<OwnedFd>)> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        stream.set_write_timeout(Some(Duration::from_secs(60)))?;
        let bytes = serde_json::to_vec(&request)?;
        tracing::info!(op = %request["op"], tenant = %request["tenant"], vcpus = %request["vcpus"], port = %request["port"], "helper_request");
        ensure!(bytes.len() < 4096, "helper request too large");
        stream.write_all(&bytes)?;
        stream.write_all(b"\n")?;
        let (mut bytes, fds) = receive(&stream)?;
        while !bytes.contains(&b'\n') {
            ensure!(bytes.len() < 4096, "helper response too large");
            let mut buffer = [0; 512];
            let n = stream.read(&mut buffer)?;
            ensure!(n > 0, "helper response closed");
            bytes.extend_from_slice(&buffer[..n]);
        }
        let reply: Value = serde_json::from_slice(&bytes)?;
        tracing::info!(op = %request["op"], tenant = %request["tenant"], result = %reply["result"], running = %reply["running"], restored = %reply["restored"], generation = %reply["generation"], "helper_reply");
        ensure!(
            reply["result"] != "error",
            "helper refused {}: {}",
            request["op"],
            reply["message"]
        );
        Ok((reply, fds))
    }
}
impl Control for Helper {
    fn metrics(&self, tenant: &str) -> Result<crate::billing::Metrics> {
        let (reply, _) = self.request(json!({"op":"tenant_metrics", "tenant":tenant}))?;
        Ok(crate::billing::Metrics {
            memory_bytes: reply["memory_bytes"]
                .as_u64()
                .context("tenant memory sample")?,
            egress_bytes: reply["egress_bytes"]
                .as_u64()
                .context("tenant egress sample")?,
        })
    }
    fn stop(&self, tenant: &str) -> Result<()> {
        self.request(json!({"op":"stop_tenant", "tenant":tenant}))?;
        Ok(())
    }
    fn adopt(&self, tenant: &str) -> Result<bool> {
        Ok(self.request(json!({"op":"adopt", "tenant":tenant}))?.0["running"] == true)
    }
    fn wake(&self, tenant: &str, initialized_generation: Option<u64>) -> Result<u64> {
        let (adopted, _) = self.request(json!({"op":"adopt", "tenant":tenant}))?;
        let running = adopted["running"] == true;
        if running
            && adopted["recovery_required"] == true
            && self
                .request(json!({"op":"recovery_complete","tenant":tenant}))
                .is_ok()
        {
            return adopted["generation"].as_u64().context("helper generation");
        }
        let (reply, _) = if running && adopted["recovery_required"] != true {
            (adopted, vec![])
        } else {
            self.request(json!({"op":"ensure", "tenant":tenant}))?
        };
        let generation = reply["generation"].as_u64().context("helper generation")?;
        let recovery = reply["recovery_required"] == true;
        if recovery || !running || initialized_generation != Some(generation) {
            self.quota(tenant, 1)?;
            let mut request = if reply["restored"] == true {
                json!({"op":"post_restore"})
            } else {
                let mut request = self
                    .guest_init
                    .get(tenant)
                    .context("tenant guest configuration")?
                    .clone();
                request["op"] = json!("attach_tenant");
                request["generation"] = json!(generation);
                if recovery {
                    request["recovery_required"] = json!(true);
                }
                request
            };
            request["unix_ms"] = json!(crate::server::unix_ms());
            request["seed"] = json!(motor_lease::base64url(&rand::random::<[u8; 32]>()));
            let initialized = self.admin(tenant, &request);
            match initialized {
                Ok(reply) if reply["ok"] == true => {}
                result => {
                    let detail: String = format!("{result:?}").chars().take(4096).collect();
                    tracing::error!(tenant, detail, "tenant_recovery_or_initialization_failed");
                    return Err(crate::state::Error::Refused(
                        crate::state::Refusal::InternalFailure,
                    ))
                    .context(format!("guest tenant initialization failed: {detail}"));
                }
            }
            if recovery {
                self.request(json!({"op":"recovery_complete", "tenant":tenant}))?;
            }
        }
        Ok(generation)
    }
    fn quota(&self, tenant: &str, vcpus: u32) -> Result<()> {
        self.request(json!({"op":"set_cpu_max", "tenant":tenant, "vcpus":vcpus}))?;
        Ok(())
    }
    fn relay(&self, tenant: &str, port: u32) -> Result<UnixStream> {
        ensure!((52..=54).contains(&port), "invalid relay port");
        let (reply, mut fds) =
            self.request(json!({"op":"open_relay", "tenant":tenant, "port":port}))?;
        ensure!(
            reply["result"] == "relay" && reply["fd_count"] == 1 && fds.len() == 1,
            "helper relay descriptor count"
        );
        Ok(UnixStream::from(fds.remove(0)))
    }
    fn admin(&self, tenant: &str, request: &Value) -> Result<Value> {
        let stream = self
            .relay(tenant, 54)
            .with_context(|| format!("tenant {tenant} control port54 open relay"))?;
        exchange_control(stream, tenant, request, control_read_deadline(request))
    }
    fn suspend(&self, tenant: &str) -> Result<()> {
        self.request(json!({"op":"suspend", "tenant":tenant}))?;
        Ok(())
    }
    fn available_bytes(&self) -> Result<u64> {
        available_bytes(&self.storage)
    }
}

/// How long the host waits for the guest's reply to a control request. A
/// fresh attach starts the tenant runner, so its deadline is derived from the
/// guest's runner-readiness wait; recovery also checks the tenant disk.
pub fn control_read_deadline(request: &Value) -> Duration {
    if request["recovery_required"] == true {
        Duration::from_secs(600)
    } else if request["op"] == "attach_tenant" {
        motor_protocol::TENANT_ATTACH_READ_DEADLINE
    } else {
        Duration::from_secs(30)
    }
}

pub(crate) fn exchange_control(
    mut stream: UnixStream,
    tenant: &str,
    request: &Value,
    timeout: Duration,
) -> Result<Value> {
    let operation = request["op"].as_str().unwrap_or("unknown");
    // SAFETY: the stream owns a live descriptor; F_GETFL takes no pointer.
    let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
    ensure!(
        flags >= 0,
        "tenant {tenant} control {operation} F_GETFL failed: {}",
        std::io::Error::last_os_error()
    );
    let nonblocking = flags & libc::O_NONBLOCK != 0;
    stream
        .set_read_timeout(Some(timeout))
        .context("tenant control set read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .context("tenant control set write timeout")?;
    let started = Instant::now();
    tracing::info!(
        tenant,
        operation,
        port = 54,
        nonblocking,
        timeout_ms = timeout.as_millis() as u64,
        "tenant_control_started"
    );
    write_json(&mut stream, request)
        .with_context(|| format!("tenant {tenant} control {operation} port54 write request"))?;
    let result = read_json(&mut stream);
    match result {
        Ok(reply) => {
            tracing::info!(
                tenant,
                operation,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "tenant_control_replied"
            );
            Ok(reply)
        }
        Err(error) => {
            let timed_out = !nonblocking
                && error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    )
                });
            tracing::error!(tenant, operation, port=54, nonblocking, timed_out,
                elapsed_ms=started.elapsed().as_millis() as u64, detail=%format!("{error:#}"), "tenant_control_read_failed");
            if timed_out && operation == "attach_tenant" {
                Err(error).context(format!("tenant initialization timed out waiting for runner readiness (tenant={tenant}, operation={operation}, port54, budget_ms={})", timeout.as_millis()))
            } else {
                Err(error).context(format!("tenant {tenant} control {operation} port54 read reply (nonblocking={nonblocking})"))
            }
        }
    }
}

pub fn write_json(writer: &mut impl Write, frame: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    ensure!(bytes.len() <= FRAME_LIMIT, "frame too large");
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}
pub fn read_json(reader: &mut impl Read) -> Result<Value> {
    let mut length = [0; 4];
    reader
        .read_exact(&mut length)
        .context("read guest JSON frame header")?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(length > 0 && length <= FRAME_LIMIT, "invalid frame length");
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .context("read guest JSON frame body")?;
    Ok(serde_json::from_slice(&bytes)?)
}

// Receive ancillary data with the first response bytes, never with a later read:
// stream reads would otherwise silently discard the helper's relay descriptor.
fn receive(stream: &UnixStream) -> Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut bytes = vec![0u8; 4096];
    let mut control = [0usize; 32];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: message buffers live through recvmsg; received descriptors are
    // immediately owned and closed even if subsequent validation fails.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        let n = libc::recvmsg(stream.as_raw_fd(), &mut msg, 0);
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        ensure!(n > 0, "empty helper response");
        let mut fds = Vec::new();
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let length =
                    ((*header).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let data = libc::CMSG_DATA(header).cast::<libc::c_int>();
                for i in 0..length / std::mem::size_of::<libc::c_int>() {
                    let fd = OwnedFd::from_raw_fd(*data.add(i));
                    if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    fds.push(fd);
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
        ensure!(
            msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) == 0,
            "truncated helper response"
        );
        bytes.truncate(n as usize);
        Ok((bytes, fds))
    }
}
fn available_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs receives a valid path and writable output structure.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stat) != 0 {
            bail!(std::io::Error::last_os_error());
        }
        (stat.f_bavail as u64)
            .checked_mul(stat.f_frsize as u64)
            .ok_or_else(|| anyhow!("free-space overflow"))
    }
}
