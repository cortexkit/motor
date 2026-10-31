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
    /// Asks the helper to remove a stopped tenant; returns its quarantine
    /// folder. The helper derives everything it deletes from the name.
    fn remove(&self, _tenant: &str) -> Result<String> {
        anyhow::bail!("tenant removal unavailable")
    }
    fn adopt(&self, tenant: &str) -> Result<bool>;
    fn wake(&self, tenant: &str, initialized_generation: Option<u64>) -> Result<u64>;
    fn quota(&self, tenant: &str, vcpus: u32) -> Result<()>;
    fn relay(&self, tenant: &str, port: u32) -> Result<UnixStream>;
    fn admin(&self, tenant: &str, request: &Value) -> Result<Value>;
    fn suspend(&self, tenant: &str) -> Result<()>;
    fn available_bytes(&self) -> Result<u64>;
    /// Reads the host capacity file. Without one, the storage refusal stays
    /// latched and free memory is advertised as 0.
    fn capacity(&self) -> Result<crate::capacity::Capacity> {
        anyhow::bail!("host capacity file unavailable")
    }
    /// The running guest's `mem_available_bytes` and the permits its cache
    /// fill holds, both from the tenant runner's `status` health. A runner
    /// whose health lacks `mem_available_bytes` gives `None` there; one
    /// without `fill_permits` (an older runner, or one not filling) gives 0.
    fn runner_sample(&self, tenant: &str) -> Result<RunnerSample> {
        let reply = self.admin(tenant, &json!({"op":"status"}))?;
        ensure!(reply["ok"] == true, "tenant runner status refused");
        Ok(RunnerSample::from_health(&reply["health"]))
    }
}

impl RunnerSample {
    fn from_health(health: &Value) -> Self {
        Self {
            mem_available_bytes: health["mem_available_bytes"].as_u64(),
            fill_permits: health["fill_permits"]
                .as_u64()
                .map_or(0, |permits| u32::try_from(permits).unwrap_or(u32::MAX)),
            held_workspaces: health
                .get("held_workspaces")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default(),
        }
    }
}

/// One probe of a running tenant's runner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunnerSample {
    pub mem_available_bytes: Option<u64>,
    /// Permits held by the tenant's own cache fill. Guest-reported, so they
    /// only lower that tenant's advertised capacity.
    pub fill_permits: u32,
    pub held_workspaces: Vec<motor_protocol::HeldWorkspace>,
}

/// The helper refused to start a tenant VM for a host policy reason, such as
/// an invalid capacity file, a memory misfit or a failed resize activation.
/// The lease that asked for the start is refused as `host_full`, which the
/// requester retries, and the tenant advertises no capacity until a start
/// succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRefused {
    /// The helper's refusal case, such as `capacity_json` or `fit`.
    pub case: String,
    pub message: String,
}
impl std::fmt::Display for StartRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "helper start_refused {}: {}", self.case, self.message)
    }
}
impl std::error::Error for StartRefused {}

/// A recovery exhausted its shared disk-check budget before the guest was ready.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct RecoveryTimeout {
    pub step: String,
    pub elapsed_ms: u64,
}
impl std::fmt::Display for RecoveryTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tenant recovery timed out step={} elapsed_ms={}",
            self.step, self.elapsed_ms
        )
    }
}
impl std::error::Error for RecoveryTimeout {}

pub struct Helper {
    pub socket: PathBuf,
    pub storage: PathBuf,
    /// The capacity file; production uses [`crate::capacity::CONFIG`].
    pub capacity: PathBuf,
    pub guest_init: BTreeMap<String, Value>,
    pub vcpu_ceiling: BTreeMap<String, u32>,
}
impl Helper {
    fn request(&self, request: Value) -> Result<(Value, Vec<OwnedFd>)> {
        let started = std::time::Instant::now();
        let deadline = helper_reply_deadline(&request);
        tracing::info!(op = %request["op"], tenant = %request["tenant"], vcpus = %request["vcpus"], port = %request["port"], reply_timeout_ms = deadline.as_millis() as u64, "helper_request");
        let result = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect helper socket {}", self.socket.display()))
            .and_then(|stream| exchange_helper(stream, &request))
            .with_context(|| {
                format!(
                    "helper {} tenant {} reply timeout={}ms elapsed={}ms",
                    request["op"],
                    request["tenant"],
                    deadline.as_millis(),
                    started.elapsed().as_millis()
                )
            });
        match &result {
            Ok((reply, _)) => {
                tracing::info!(op = %request["op"], tenant = %request["tenant"], result = %reply["result"], running = %reply["running"], restored = %reply["restored"], generation = %reply["generation"], elapsed_ms = started.elapsed().as_millis() as u64, "helper_reply")
            }
            Err(error) => {
                tracing::warn!(op = %request["op"], tenant = %request["tenant"], detail = %format!("{error:#}"), "helper_request_failed")
            }
        }
        result
    }
}

const ORDINARY_HELPER_TIMEOUT: Duration = Duration::from_secs(60);
/// Suspend writes a complete guest-memory image and its durable snapshot record.
/// Give that storage-heavy operation a separate ten-minute reply budget without
/// weakening short control writes or waiting indefinitely for a stuck helper.
const SUSPEND_HELPER_REPLY_TIMEOUT: Duration = Duration::from_secs(600);

fn helper_reply_deadline(request: &Value) -> Duration {
    if request["op"] == "suspend" {
        SUSPEND_HELPER_REPLY_TIMEOUT
    } else {
        ORDINARY_HELPER_TIMEOUT
    }
}

fn exchange_helper(stream: UnixStream, request: &Value) -> Result<(Value, Vec<OwnedFd>)> {
    exchange_helper_with_deadline(stream, request, helper_reply_deadline(request))
}

fn exchange_helper_with_deadline(
    mut stream: UnixStream,
    request: &Value,
    deadline: Duration,
) -> Result<(Value, Vec<OwnedFd>)> {
    stream
        .set_read_timeout(Some(deadline))
        .context("set helper read timeout")?;
    stream
        .set_write_timeout(Some(ORDINARY_HELPER_TIMEOUT))
        .context("set helper write timeout")?;
    let bytes = serde_json::to_vec(request)?;
    ensure!(bytes.len() < 4096, "helper request too large");
    stream.write_all(&bytes).context("write helper request")?;
    stream
        .write_all(b"\n")
        .context("write helper request boundary")?;
    let started = std::time::Instant::now();
    let (mut bytes, fds) = receive(&stream).context("read helper reply")?;
    while !bytes.contains(&b'\n') {
        ensure!(bytes.len() < 4096, "helper response too large");
        let remaining = deadline.saturating_sub(started.elapsed());
        ensure!(!remaining.is_zero(), "helper reply deadline exceeded");
        stream.set_read_timeout(Some(remaining))?;
        let mut buffer = [0; 512];
        let n = stream
            .read(&mut buffer)
            .context("read helper reply continuation")?;
        ensure!(n > 0, "helper response closed");
        bytes.extend_from_slice(&buffer[..n]);
    }
    let reply: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        reply["result"] != "error",
        "helper refused {}: {}",
        request["op"],
        reply["message"]
    );
    Ok((reply, fds))
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
    fn remove(&self, tenant: &str) -> Result<String> {
        let (reply, _) = self.request(json!({"op":"remove_tenant", "tenant":tenant}))?;
        ensure!(
            reply["result"] == "removed",
            "unexpected helper removal reply"
        );
        Ok(reply["quarantine"]
            .as_str()
            .context("helper quarantine folder")?
            .to_owned())
    }
    fn adopt(&self, tenant: &str) -> Result<bool> {
        let (reply, _) = self.request(json!({"op":"adopt", "tenant":tenant}))?;
        check_start_reply(&reply)?;
        Ok(reply["running"] == true)
    }
    fn wake(&self, tenant: &str, initialized_generation: Option<u64>) -> Result<u64> {
        let (adopted, _) = self
            .request(json!({"op":"adopt", "tenant":tenant}))
            .map_err(|error| self.activation_refused(tenant, error))?;
        let activation_pending = adopted["result"] == "start_refused";
        let running = adopted["running"] == true;
        if running
            && adopted["recovery_required"] == true
            && self
                .request(json!({"op":"recovery_complete","tenant":tenant}))
                .is_ok()
        {
            self.activate(tenant)?;
            return adopted["generation"].as_u64().context("helper generation");
        }
        let (reply, _) = if running && adopted["recovery_required"] != true {
            (adopted, vec![])
        } else {
            self.request(json!({"op":"prepare_start", "tenant":tenant}))
                .map_err(|error| {
                    if activation_pending {
                        self.activation_refused(tenant, error)
                    } else {
                        error
                    }
                })?
        };
        check_start_reply(&reply).map_err(|error| {
            if activation_pending {
                self.activation_refused(tenant, error)
            } else {
                error
            }
        })?;
        let generation = reply["generation"]
            .as_u64()
            .context("helper generation")
            .map_err(|error| {
                if activation_pending {
                    self.activation_refused(tenant, error)
                } else {
                    error
                }
            })?;
        let recovery = reply["recovery_required"] == true;
        if recovery || !running || initialized_generation != Some(generation) {
            // Recovery checks run before any lease can start. Large disks need
            // the configured VM capacity rather than the idle one-vCPU quota.
            let vcpus = if recovery {
                *self
                    .vcpu_ceiling
                    .get(tenant)
                    .context("tenant vCPU ceiling")?
            } else {
                1
            };
            self.quota(tenant, vcpus).map_err(|error| {
                if activation_pending {
                    self.activation_refused(tenant, error)
                } else {
                    error
                }
            })?;
            let mut request = if reply["restored"] == true {
                json!({"op":"post_restore"})
            } else {
                let mut request = self
                    .guest_init
                    .get(tenant)
                    .context("tenant guest configuration")
                    .map_err(|error| {
                        if activation_pending {
                            self.activation_refused(tenant, error)
                        } else {
                            error
                        }
                    })?
                    .clone();
                request["op"] = json!("attach_tenant");
                request["generation"] = json!(generation);
                if recovery {
                    request["recovery_required"] = json!(true);
                    request["recovery_budget_ms"] = json!((control_read_deadline(&request)
                        - motor_protocol::TENANT_ATTACH_READ_DEADLINE
                        - motor_protocol::TENANT_RECOVERY_REPLY_MARGIN)
                        .as_millis()
                        as u64);
                }
                request
            };
            request["unix_ms"] = json!(crate::server::unix_ms());
            request["seed"] = json!(motor_lease::base64url(&rand::random::<[u8; 32]>()));
            let started = Instant::now();
            let initialized = self.admin(tenant, &request);
            if recovery {
                if let Ok(reply) = &initialized {
                    if reply["ok"] != true && reply["timed_out"] == true {
                        return Err(RecoveryTimeout {
                            step: reply["step"].as_str().unwrap_or("unknown").into(),
                            elapsed_ms: reply["elapsed_ms"]
                                .as_u64()
                                .unwrap_or(started.elapsed().as_millis() as u64),
                        }
                        .into());
                    }
                } else if initialized.as_ref().unwrap_err().chain().any(|cause| {
                    cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        )
                    })
                }) {
                    return Err(RecoveryTimeout {
                        step: "attach_reply".into(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    }
                    .into());
                }
            }
            match initialized {
                Ok(reply) if reply["ok"] == true => {}
                result => {
                    let detail: String = format!("{result:?}").chars().take(4096).collect();
                    tracing::error!(tenant, detail, "tenant_recovery_or_initialization_failed");
                    if activation_pending {
                        return Err(self.activation_refused(
                            tenant,
                            anyhow::anyhow!("guest tenant initialization failed: {detail}"),
                        ));
                    }
                    return Err(crate::state::Error::Refused(
                        crate::state::Refusal::InternalFailure,
                    ))
                    .context(format!("guest tenant initialization failed: {detail}"));
                }
            }
            if recovery {
                self.request(json!({"op":"recovery_complete", "tenant":tenant}))
                    .map_err(|error| {
                        if activation_pending {
                            self.activation_refused(tenant, error)
                        } else {
                            error
                        }
                    })?;
            }
        }
        self.activate(tenant)?;
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
    fn capacity(&self) -> Result<crate::capacity::Capacity> {
        crate::capacity::read(&self.capacity, 0)
    }
}

impl Helper {
    fn activate(&self, tenant: &str) -> Result<()> {
        let result = (|| {
            let (reply, _) = self.request(json!({"op":"activate", "tenant":tenant}))?;
            check_start_reply(&reply)?;
            ensure!(
                reply["result"] == "tenant" && reply["running"] == true,
                "unexpected helper activation reply"
            );
            Ok(())
        })();
        result.map_err(|error| self.activation_refused(tenant, error))
    }

    fn activation_refused(&self, tenant: &str, error: anyhow::Error) -> anyhow::Error {
        let mut refusal = error
            .downcast_ref::<StartRefused>()
            .cloned()
            .unwrap_or_else(|| StartRefused {
                case: "activation".into(),
                message: format!("{error:#}"),
            });
        if let Err(stop) = self.stop(tenant) {
            refusal
                .message
                .push_str(&format!("; clean stop failed: {stop:#}"));
        }
        refusal.into()
    }
}

fn check_start_reply(reply: &Value) -> Result<()> {
    if reply["result"] == "start_refused" {
        return Err(StartRefused {
            case: reply["case"].as_str().unwrap_or("unknown").to_owned(),
            message: reply["message"].as_str().unwrap_or("").to_owned(),
        }
        .into());
    }
    Ok(())
}

/// How long the host waits for the guest's reply to a control request. A
/// fresh attach starts the tenant runner, so its deadline is derived from the
/// guest's runner-readiness wait; recovery also checks the tenant disk.
pub fn control_read_deadline(request: &Value) -> Duration {
    if request["recovery_required"] == true {
        motor_protocol::TENANT_ATTACH_READ_DEADLINE + motor_protocol::TENANT_RECOVERY_ALLOWANCE
    } else if request["op"] == "set_remote_cache" {
        // Stop plus restart can use the full runner readiness budget. Do not
        // abandon a credential swap after the ordinary 30-second query window.
        motor_protocol::TENANT_ATTACH_READ_DEADLINE + Duration::from_secs(120)
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
    let result = read_json(&mut stream).map_err(|error| {
        if operation == "set_remote_cache" {
            anyhow!("remote_cache_invalid_reply")
        } else {
            error
        }
    });
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

#[cfg(test)]
mod helper_deadline_tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    const REPLY: &[u8] =
        b"{\"result\":\"tenant\",\"running\":false,\"restored\":false,\"generation\":7}\n";

    #[test]
    fn runner_samples_from_old_guests_have_no_warm_workspaces() {
        for health in [
            Value::Null,
            json!({}),
            json!({"fill_permits": 2}),
            json!({"held_workspaces": "invalid"}),
        ] {
            assert!(RunnerSample::from_health(&health)
                .held_workspaces
                .is_empty());
        }
        let sample = RunnerSample::from_health(
            &json!({"held_workspaces": [{"job_id": "00000000-0000-7000-8000-000000000001", "used_ms": 42}]}),
        );
        assert_eq!(sample.held_workspaces.len(), 1);
        assert_eq!(sample.held_workspaces[0].used_ms, 42);
    }

    #[test]
    fn suspend_reply_budget_is_applied_to_the_actual_helper_socket_only() {
        for (op, expected) in [
            ("suspend", 600),
            ("adopt", 60),
            ("tenant_metrics", 60),
            ("ensure", 60),
        ] {
            let (client, mut server) = UnixStream::pair().unwrap();
            let observer = client.try_clone().unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let request = json!({"op":op,"tenant":"tenant-a"});
            let worker = std::thread::spawn(move || exchange_helper(client, &request));
            let mut line = String::new();
            BufReader::new(server.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let read = observer.read_timeout().unwrap();
            let write = observer.write_timeout().unwrap();
            server.write_all(REPLY).unwrap();
            let (reply, fds) = worker.join().unwrap().unwrap();
            assert_eq!(read, Some(Duration::from_secs(expected)), "{op}");
            assert_eq!(
                write,
                Some(Duration::from_secs(60)),
                "control writes remain bounded"
            );
            assert_eq!(reply["generation"], 7);
            assert!(fds.is_empty());
        }
    }

    #[test]
    fn delayed_suspend_reply_survives_a_scaled_ordinary_request_budget() {
        let ordinary = Duration::from_millis(20);
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let worker = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(server.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            std::thread::sleep(Duration::from_millis(100));
            server.write_all(REPLY).unwrap();
        });
        let started = std::time::Instant::now();
        let (reply, fds) = exchange_helper_with_deadline(
            client,
            &json!({"op":"suspend","tenant":"tenant-a"}),
            Duration::from_secs(3),
        )
        .unwrap();
        worker.join().unwrap();
        assert!(started.elapsed() > ordinary);
        assert_eq!(reply["result"], "tenant");
        assert_eq!(reply["running"], false);
        assert!(fds.is_empty());
    }
}
