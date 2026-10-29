//! Job-only macOS VM plans and the helper's fail-closed control protocol.
//!
//! Image creation is deliberately a binary-only subcommand. A `JobVmConfig`
//! has no switch for adding a display or a network adapter.

use motor_winagent::{Frame, MACOS_PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
#[cfg(unix)]
use std::io::Write;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub const BOOT_DISK: &str = "disk.img";
pub const AUXILIARY_STORAGE: &str = "auxiliary-storage";
pub const HARDWARE_MODEL: &str = "hardware-model";
pub const MACHINE_IDENTIFIER: &str = "machine-identifier";
pub const VSOCK_PORT: u32 = 5000;
#[cfg(any(target_os = "macos", all(test, unix)))]
mod disk_release;
mod nonce;
pub use nonce::{
    BootConfig, Mount, NONCE_DISK_SIZE, NONCE_HEADER, create_nonce_disk, decode_boot_config,
    nonce_bytes, read_boot_candidate, read_boot_config,
};
pub const CARGO_HOME: &str = "/opt/motor/job/cargo-home";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShareLocation {
    pub host: PathBuf,
    pub tag: String,
    pub guest: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobVmRequest {
    pub clone: PathBuf,
    pub nonce_disk: PathBuf,
    pub workspace: ShareLocation,
    pub siblings: Vec<ShareLocation>,
    pub cargo_home: ShareLocation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub location: ShareLocation,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct JobVmConfig {
    request: JobVmRequest,
    disks: [Disk; 2],
    shares: Vec<Share>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn absolute_normal(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    path.starts_with('/')
        && !path[1..]
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
}

impl JobVmConfig {
    pub fn new(request: JobVmRequest) -> io::Result<Self> {
        if !absolute_normal(&request.clone) || !absolute_normal(&request.nonce_disk) {
            return Err(invalid(
                "clone and nonce disk must be normalized absolute paths",
            ));
        }
        if request.cargo_home.guest != Path::new(CARGO_HOME) {
            return Err(invalid(
                "cargo-home guest destination must be /opt/motor/job/cargo-home",
            ));
        }
        let mut tags = HashSet::new();
        let mut guests: Vec<PathBuf> = Vec::new();
        let mut shares = Vec::new();
        for (location, read_only) in std::iter::once((&request.workspace, false))
            .chain(request.siblings.iter().map(|s| (s, true)))
            .chain(std::iter::once((&request.cargo_home, false)))
        {
            if !absolute_normal(&location.host) || !absolute_normal(&location.guest) {
                return Err(invalid("share paths must be normalized absolute paths"));
            }
            if location.tag.is_empty()
                || location.tag.len() > 36
                || !location
                    .tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !tags.insert(location.tag.clone())
            {
                return Err(invalid(
                    "virtio-fs tags must be unique, 1-36 ASCII name characters",
                ));
            }
            if guests.iter().any(|p| {
                let left = p.to_string_lossy().to_ascii_lowercase();
                let right = location.guest.to_string_lossy().to_ascii_lowercase();
                Path::new(&left).starts_with(&right) || Path::new(&right).starts_with(&left)
            }) {
                return Err(invalid("guest share destinations must not overlap"));
            }
            if location.host.starts_with(&request.clone)
                || request.clone.starts_with(&location.host)
                || request.nonce_disk.starts_with(&location.host)
            {
                return Err(invalid("a share must not expose the image or nonce disk"));
            }
            guests.push(location.guest.clone());
            shares.push(Share {
                location: location.clone(),
                read_only,
            });
        }
        Ok(Self {
            disks: [
                Disk {
                    path: request.clone.join(BOOT_DISK),
                    read_only: false,
                },
                Disk {
                    path: request.nonce_disk.clone(),
                    read_only: true,
                },
            ],
            request,
            shares,
        })
    }

    pub fn disks(&self) -> &[Disk; 2] {
        &self.disks
    }
    pub fn auxiliary_storage(&self) -> PathBuf {
        self.request.clone.join(AUXILIARY_STORAGE)
    }
    pub fn hardware_model(&self) -> PathBuf {
        self.request.clone.join(HARDWARE_MODEL)
    }
    pub fn machine_identifier(&self) -> PathBuf {
        self.request.clone.join(MACHINE_IDENTIFIER)
    }
    pub fn shares(&self) -> &[Share] {
        &self.shares
    }
    pub fn mounts(&self) -> Vec<Mount> {
        self.shares
            .iter()
            .map(|share| Mount {
                tag: share.location.tag.clone(),
                destination: share.location.guest.clone(),
                read_only: share.read_only,
            })
            .collect()
    }
    pub fn vsock_devices(&self) -> usize {
        1
    }
    pub fn network_devices(&self) -> usize {
        0
    }
    pub fn graphics_devices(&self) -> usize {
        0
    }
    pub fn cpu_count(&self) -> usize {
        4
    }
    pub fn memory_bytes(&self) -> u64 {
        6 * 1024 * 1024 * 1024
    }
    pub fn request(&self) -> &JobVmRequest {
        &self.request
    }
}

/// Consumes the first connection even if its Hello is malformed. Authentication
/// failure cannot be retried by another process in the same VM.
pub struct SessionAcceptor {
    nonce: String,
    image: String,
    claimed: bool,
}

impl SessionAcceptor {
    pub fn new(nonce: String, image: String) -> io::Result<Self> {
        nonce::validate_nonce(&nonce)?;
        Ok(Self {
            nonce,
            image,
            claimed: false,
        })
    }

    pub fn claim(&mut self) -> io::Result<()> {
        if std::mem::replace(&mut self.claimed, true) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "VM session already consumed",
            ));
        }
        Ok(())
    }

    pub fn receive_hello(&self, input: &mut impl Read) -> io::Result<Frame> {
        let hello =
            motor_winagent::read_frame(input)?.ok_or_else(|| invalid("EOF before VM Hello"))?;
        self.authenticate(&hello)?;
        Ok(hello)
    }

    pub fn authenticate(&self, frame: &Frame) -> io::Result<()> {
        if !self.claimed {
            return Err(invalid("connection has not been claimed"));
        }
        match frame {
            Frame::Hello { version, .. } if *version != MACOS_PROTOCOL_VERSION => {
                Err(invalid(format!(
                    "guest_agent_version: image {} has agent version {version}, expected {MACOS_PROTOCOL_VERSION}",
                    self.image
                )))
            }
            Frame::Hello { nonce, .. }
                if constant_time_eq(nonce.as_bytes(), self.nonce.as_bytes()) =>
            {
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bad VM Hello or nonce",
            )),
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut different = a.len() ^ b.len();
    for (i, byte) in b.iter().enumerate() {
        different |= usize::from(a.get(i).copied().unwrap_or(0) ^ byte);
    }
    different == 0
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub start_seconds: u64,
    pub start_microseconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum HelperReport {
    Started {
        helper: ProcessIdentity,
        service: ProcessIdentity,
    },
    Failed {
        detail: String,
    },
    Stopped {
        reason: HelperStopReason,
    },
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum HelperStopReason {
    GateEofBeforeStart,
    GateEof,
    VmStopped,
    Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateEvent {
    Open,
    Eof,
}

/// The only byte that opens a helper's gate. All other input closes it.
pub fn watch_gate(mut input: impl Read, mut event: impl FnMut(GateEvent)) -> io::Result<()> {
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Ok(1) if byte[0] == b'G' => break,
            Ok(_) => {
                event(GateEvent::Eof);
                return Ok(());
            }
            Err(e) => {
                event(GateEvent::Eof);
                return Err(e);
            }
        }
    }
    event(GateEvent::Open);
    loop {
        match input.read(&mut byte) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                event(GateEvent::Eof);
                return Err(e);
            }
            Ok(_) => {
                event(GateEvent::Eof);
                return Ok(());
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperAction {
    Start,
    Stop,
    Exit,
}

#[derive(Default)]
pub struct HelperLifecycle {
    opened: bool,
    closed: bool,
    start_requested: bool,
    stopped: bool,
}

impl HelperLifecycle {
    pub fn stop_reason(&self, gate_eof: bool, stop_called: bool, failed: bool) -> HelperStopReason {
        if failed {
            return HelperStopReason::Failure;
        }
        if gate_eof && self.closed {
            if !self.start_requested {
                return HelperStopReason::GateEofBeforeStart;
            }
            if stop_called && self.stopped {
                return HelperStopReason::GateEof;
            }
        }
        HelperStopReason::VmStopped
    }
    pub fn gate(&mut self, event: GateEvent) {
        match event {
            GateEvent::Open if !self.closed => self.opened = true,
            GateEvent::Eof => self.closed = true,
            _ => {}
        }
    }
    pub fn vm_stopped(&mut self) {
        self.stopped = true;
    }
    pub fn next_action(&mut self) -> Option<HelperAction> {
        if self.closed {
            return Some(if !self.start_requested || self.stopped {
                HelperAction::Exit
            } else {
                HelperAction::Stop
            });
        }
        if self.stopped {
            return Some(HelperAction::Exit);
        }
        if self.opened && !self.start_requested {
            self.start_requested = true;
            return Some(HelperAction::Start);
        }
        None
    }
}

#[cfg(target_os = "macos")]
pub fn clone_image(sealed: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> i32;
    }
    let metadata = std::fs::symlink_metadata(sealed)?;
    if !metadata.is_dir() || !metadata.permissions().readonly() {
        return Err(invalid("sealed image must be a read-only directory"));
    }
    for name in [
        BOOT_DISK,
        AUXILIARY_STORAGE,
        HARDWARE_MODEL,
        MACHINE_IDENTIFIER,
    ] {
        let metadata = std::fs::symlink_metadata(sealed.join(name))?;
        if !metadata.is_file() || !metadata.permissions().readonly() {
            return Err(invalid(format!(
                "sealed image file {name} must be a read-only regular file"
            )));
        }
    }
    let src =
        CString::new(sealed.as_os_str().as_bytes()).map_err(|_| invalid("NUL in image path"))?;
    let dst = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| invalid("NUL in clone path"))?;
    // CLONE_NOFOLLOW refuses a symlink at the source; clonefile also refuses an
    // existing destination. There is deliberately no non-APFS copy fallback.
    if unsafe { clonefile(src.as_ptr(), dst.as_ptr(), 1) } != 0 {
        return Err(io::Error::last_os_error());
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o700))?;
    for name in [BOOT_DISK, AUXILIARY_STORAGE] {
        std::fs::set_permissions(
            destination.join(name),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    std::fs::File::open(destination)?.sync_all()?;
    std::fs::File::open(
        destination
            .parent()
            .ok_or_else(|| invalid("clone has no parent"))?,
    )?
    .sync_all()
}

#[cfg(not(target_os = "macos"))]
pub fn clone_image(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "APFS clonefile requires macOS",
    ))
}

/// The runner must durably record the helper's identity and intent to open its
/// gate before calling `open_gate`. Dropping the gate cancels the VM.
#[cfg(unix)]
pub struct JobHelper {
    pub child: std::process::Child,
    gate: Option<std::process::ChildStdin>,
    opened: bool,
    pub reports: std::process::ChildStdout,
    pub session: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl JobHelper {
    pub fn open_gate(&mut self) -> io::Result<()> {
        if self.opened {
            return Err(invalid("helper gate is already open"));
        }
        self.gate
            .as_mut()
            .ok_or_else(|| invalid("helper gate is closed"))?
            .write_all(b"G")?;
        self.opened = true;
        Ok(())
    }
    pub fn close_gate(&mut self) {
        self.gate.take();
    }
}

/// File descriptor 3 carries the authenticated guest session; descriptor 4
/// keeps the runner's already-acquired slot lock alive in the helper.
#[cfg(unix)]
pub fn spawn_job_helper(
    executable: &Path,
    config: &JobVmConfig,
    slot: &std::fs::File,
) -> io::Result<JobHelper> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let (session, child_session) = std::os::unix::net::UnixStream::pair()?;
    // Reserve copies above the fixed child descriptors so dup2 cannot clobber
    // either source, including when the caller had descriptors 0-4 closed.
    let duplicate = |fd| -> io::Result<OwnedFd> {
        let fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 5) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let guest_fd = duplicate(child_session.as_raw_fd())?;
    let slot_fd = duplicate(slot.as_raw_fd())?;
    let mut command = Command::new(executable);
    command
        .arg("job")
        .arg(serde_json::to_string(config.request())?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(guest_fd.as_raw_fd(), 3) == -1 || libc::dup2(slot_fd.as_raw_fd(), 4) == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    Ok(JobHelper {
        gate: Some(child.stdin.take().expect("piped gate")),
        opened: false,
        reports: child.stdout.take().expect("piped reports"),
        child,
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn share(host: &str, tag: &str, guest: &str) -> ShareLocation {
        ShareLocation {
            host: host.into(),
            tag: tag.into(),
            guest: guest.into(),
        }
    }
    fn request() -> JobVmRequest {
        JobVmRequest {
            clone: "/state/clones/job".into(),
            nonce_disk: "/state/nonces/job.raw".into(),
            workspace: share(
                "/state/jobs/job/workspace",
                "workspace",
                "/Users/caller/project",
            ),
            siblings: vec![share(
                "/state/jobs/job/sibling",
                "sibling",
                "/Users/caller/sibling",
            )],
            cargo_home: share("/state/jobs/job/cargo-home", "cargo-home", CARGO_HOME),
        }
    }
    #[test]
    fn job_configuration_has_exact_devices_and_share_access() {
        let plan = JobVmConfig::new(request()).unwrap();
        assert_eq!(
            plan.disks(),
            &[
                Disk {
                    path: "/state/clones/job/disk.img".into(),
                    read_only: false
                },
                Disk {
                    path: "/state/nonces/job.raw".into(),
                    read_only: true
                }
            ]
        );
        assert_eq!(
            plan.auxiliary_storage(),
            Path::new("/state/clones/job/auxiliary-storage")
        );
        assert_eq!(
            plan.hardware_model(),
            Path::new("/state/clones/job/hardware-model")
        );
        assert_eq!(
            plan.machine_identifier(),
            Path::new("/state/clones/job/machine-identifier")
        );
        assert_eq!(
            (
                plan.vsock_devices(),
                plan.network_devices(),
                plan.graphics_devices()
            ),
            (1, 0, 0)
        );
        assert_eq!(
            (plan.cpu_count(), plan.memory_bytes()),
            (4, 6 * 1024 * 1024 * 1024)
        );
        assert_eq!(
            plan.shares(),
            &[
                Share {
                    location: request().workspace,
                    read_only: false
                },
                Share {
                    location: request().siblings.remove(0),
                    read_only: true
                },
                Share {
                    location: request().cargo_home,
                    read_only: false
                },
            ]
        );
    }
    #[test]
    fn rejects_ambiguous_or_secret_exposing_shares() {
        for variant in 0..7 {
            let mut input = request();
            match variant {
                0 => input.siblings[0].tag = "workspace".into(),
                1 => input.siblings[0].guest = input.workspace.guest.join("nested"),
                2 => input.workspace.host = "/state".into(),
                3 => input.workspace.guest = "/Users/../tmp".into(),
                4 => input.workspace.tag = "invalid/tag".into(),
                5 => input.cargo_home.guest = "/other".into(),
                _ => input.clone = "relative".into(),
            }
            assert!(JobVmConfig::new(input).is_err(), "variant {variant}");
        }
        let mut json = serde_json::to_value(request()).unwrap();
        json["network"] = serde_json::json!(true);
        assert!(serde_json::from_value::<JobVmRequest>(json).is_err());
    }
    #[test]
    fn rejects_case_aliased_guest_share_overlap() {
        let mut value = request();
        value.siblings[0].guest = value
            .workspace
            .guest
            .join("nested")
            .to_string_lossy()
            .to_ascii_uppercase()
            .into();
        assert!(JobVmConfig::new(value).is_err());
    }
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    #[test]
    fn single_session_accepts_only_correct_first_hello() {
        let mut acceptor = SessionAcceptor::new(NONCE.into(), "sealed-image".into()).unwrap();
        acceptor.claim().unwrap();
        let mut wire = Vec::new();
        motor_winagent::write_frame(
            &mut wire,
            &Frame::Hello {
                version: MACOS_PROTOCOL_VERSION,
                nonce: NONCE.into(),
            },
        )
        .unwrap();
        acceptor.receive_hello(&mut wire.as_slice()).unwrap();
        assert!(acceptor.claim().is_err());
    }
    #[test]
    fn bad_hello_consumes_session_and_cannot_retry() {
        for frame in [
            Frame::Hello {
                version: MACOS_PROTOCOL_VERSION,
                nonce: "wrong".into(),
            },
            Frame::Hello {
                version: 999,
                nonce: NONCE.into(),
            },
            Frame::Cancel {},
        ] {
            let mut acceptor = SessionAcceptor::new(NONCE.into(), "sealed-image".into()).unwrap();
            acceptor.claim().unwrap();
            let mut wire = Vec::new();
            motor_winagent::write_frame(&mut wire, &frame).unwrap();
            let error = acceptor.receive_hello(&mut wire.as_slice()).unwrap_err();
            if matches!(frame, Frame::Hello { version: 999, .. }) {
                assert!(
                    error
                        .to_string()
                        .contains("guest_agent_version: image sealed-image")
                );
            }
            assert!(acceptor.claim().is_err());
        }
    }
    #[test]
    fn gate_eof_before_open_never_starts() {
        for input in [b"".as_slice(), b"X"] {
            let mut lifecycle = HelperLifecycle::default();
            watch_gate(input, |e| lifecycle.gate(e)).unwrap();
            assert_eq!(lifecycle.next_action(), Some(HelperAction::Exit));
        }
    }
    #[test]
    fn helper_exit_reason_distinguishes_eof_from_unrelated_stop() {
        let mut lifecycle = HelperLifecycle::default();
        lifecycle.gate(GateEvent::Eof);
        assert_eq!(
            lifecycle.stop_reason(true, false, false),
            HelperStopReason::GateEofBeforeStart
        );
        let mut lifecycle = HelperLifecycle::default();
        lifecycle.gate(GateEvent::Open);
        assert_eq!(lifecycle.next_action(), Some(HelperAction::Start));
        lifecycle.gate(GateEvent::Eof);
        lifecycle.vm_stopped();
        assert_eq!(
            lifecycle.stop_reason(true, true, false),
            HelperStopReason::GateEof
        );
        assert_eq!(
            lifecycle.stop_reason(true, false, false),
            HelperStopReason::VmStopped
        );
        assert_eq!(
            lifecycle.stop_reason(true, true, true),
            HelperStopReason::Failure
        );
    }
    #[test]
    fn gate_eof_after_open_waits_for_vm_stopped() {
        let mut lifecycle = HelperLifecycle::default();
        assert_eq!(lifecycle.next_action(), None);
        lifecycle.gate(GateEvent::Open);
        assert_eq!(lifecycle.next_action(), Some(HelperAction::Start));
        assert_eq!(lifecycle.next_action(), None);
        lifecycle.gate(GateEvent::Eof);
        for _ in 0..3 {
            assert_eq!(lifecycle.next_action(), Some(HelperAction::Stop));
        }
        lifecycle.vm_stopped();
        assert_eq!(lifecycle.next_action(), Some(HelperAction::Exit));
    }
    #[test]
    fn buffered_open_followed_by_eof_does_not_boot() {
        let mut lifecycle = HelperLifecycle::default();
        watch_gate(b"G".as_slice(), |e| lifecycle.gate(e)).unwrap();
        assert_eq!(lifecycle.next_action(), Some(HelperAction::Exit));
        lifecycle.gate(GateEvent::Open);
        assert_eq!(lifecycle.next_action(), Some(HelperAction::Exit));
    }
    #[test]
    fn gate_read_error_closes_helper() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken gate"))
            }
        }
        let mut lifecycle = HelperLifecycle::default();
        assert!(watch_gate(Broken, |e| lifecycle.gate(e)).is_err());
        assert_eq!(lifecycle.next_action(), Some(HelperAction::Exit));
    }
    #[cfg(unix)]
    #[test]
    fn raw_nonce_disk_is_exact_read_only_and_never_overwritten() {
        let root = cortexkit_test_support::scratch_root()
            .join(format!("macvm-nonce-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("nonce.raw");
        create_nonce_disk(&path, NONCE, &[]).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            NONCE_DISK_SIZE as u64
        );
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        assert_eq!(
            read_boot_config(std::fs::File::open(&path).unwrap())
                .unwrap()
                .nonce,
            NONCE
        );
        assert!(create_nonce_disk(&path, NONCE, &[]).is_err());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn spawned_helper_is_gated_and_inherits_session_and_slot() {
        use std::os::unix::fs::PermissionsExt;
        let root = cortexkit_test_support::scratch_root()
            .join(format!("macvm-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("helper");
        std::fs::write(&executable, "#!/bin/bash\nif IFS= read -r -n 1 gate; then\n  test \"$gate\" = G || exit 9\n  printf started >&3\n  printf held >&4\n  IFS= read -r -n 1 unused\nfi\nprintf stopped\\n\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let slot_path = root.join("slot");
        let slot = std::fs::File::create(&slot_path).unwrap();
        let plan = JobVmConfig::new(request()).unwrap();
        let mut helper = spawn_job_helper(&executable, &plan, &slot).unwrap();
        helper
            .session
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        let mut byte = [0];
        assert!(helper.session.read(&mut byte).is_err());
        assert!(std::fs::read(&slot_path).unwrap().is_empty());
        assert!(helper.child.try_wait().unwrap().is_none());
        helper.open_gate().unwrap();
        assert!(helper.open_gate().is_err());
        helper
            .session
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut started = [0; 7];
        helper.session.read_exact(&mut started).unwrap();
        assert_eq!(&started, b"started");
        helper.close_gate();
        assert!(helper.child.wait().unwrap().success());
        assert_eq!(std::fs::read(&slot_path).unwrap(), b"held");
        let mut helper = spawn_job_helper(&executable, &plan, &slot).unwrap();
        helper.close_gate();
        assert!(helper.child.wait().unwrap().success());
        assert_eq!(helper.session.read(&mut byte).unwrap(), 0);
        drop(helper);
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nonce_parser_refuses_bad_header_padding_and_length() {
        for index in [0, NONCE_HEADER.len(), NONCE_DISK_SIZE - 1] {
            let mut bytes = nonce_bytes(NONCE, &[]).unwrap();
            bytes[index] = b'X';
            assert!(read_boot_config(bytes.as_slice()).is_err());
        }
        assert!(read_boot_config(&b"short"[..]).is_err());
        assert!(nonce_bytes("ABCDEF01234567890123456789012345", &[]).is_err());
    }
}

/// Uses the kernel start timestamp as well as the PID to distinguish PID reuse.
#[cfg(target_os = "macos")]
pub fn process_identity(pid: i32) -> io::Result<ProcessIdentity> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if count != size {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(ProcessIdentity {
        pid,
        start_seconds: info.pbi_start_tvsec,
        start_microseconds: info.pbi_start_tvusec,
    })
}

/// Descriptor inspection proves service identity; an optional responsibility
/// lookup strengthens that proof but never replaces the clone vnode match.
#[cfg(target_os = "macos")]
pub mod process {
    use super::*;
    use std::ffi::{CStr, c_void};
    use std::mem::{MaybeUninit, size_of};
    use std::os::unix::fs::MetadataExt;
    const PROC_UID_ONLY: u32 = 4;
    const PROC_PIDLISTFILEPORTS: i32 = 14;
    const VNODE_PATH_INFO: i32 = 2;
    const SERVICE: &str = "com.apple.Virtualization.VirtualMachine";
    #[repr(C)]
    struct FileInfo {
        flags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guard_flags: u32,
    }
    #[repr(C)]
    struct VnodeFdInfo {
        file: FileInfo,
        vnode: libc::vnode_info_path,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FilePort {
        port: u32,
        kind: u32,
    }
    unsafe extern "C" {
        fn proc_pidfdinfo(pid: i32, fd: i32, flavor: i32, buffer: *mut c_void, size: i32) -> i32;
        fn proc_pidfileportinfo(
            pid: i32,
            port: u32,
            flavor: i32,
            buffer: *mut c_void,
            size: i32,
        ) -> i32;
    }
    fn clear_errno() {
        unsafe {
            *libc::__error() = 0;
        }
    }
    fn query_error() -> io::Error {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(0) {
            io::Error::other("incomplete libproc process inspection")
        } else {
            e
        }
    }
    fn list<T>(mut query: impl FnMut(*mut c_void, i32) -> i32) -> io::Result<Vec<T>> {
        clear_errno();
        let size = query(std::ptr::null_mut(), 0);
        if size < 0 || (size == 0 && io::Error::last_os_error().raw_os_error() != Some(0)) {
            return Err(query_error());
        }
        let mut capacity = size as usize / size_of::<T>() + 32;
        for _ in 0..4 {
            if capacity * size_of::<T>() > 16 * 1024 * 1024 {
                return Err(io::Error::other("libproc list exceeds safety limit"));
            }
            let mut entries: Vec<MaybeUninit<T>> = Vec::with_capacity(capacity);
            entries.resize_with(capacity, MaybeUninit::uninit);
            clear_errno();
            let count = query(
                entries.as_mut_ptr().cast(),
                (capacity * size_of::<T>()) as i32,
            );
            if count < 0 || (count == 0 && io::Error::last_os_error().raw_os_error() != Some(0)) {
                return Err(query_error());
            }
            if count as usize >= capacity * size_of::<T>() {
                capacity *= 2;
                continue;
            }
            if !(count as usize).is_multiple_of(size_of::<T>()) {
                return Err(io::Error::other("partial libproc list entry"));
            }
            return Ok(entries
                .into_iter()
                .take(count as usize / size_of::<T>())
                .map(|entry| unsafe { entry.assume_init() })
                .collect());
        }
        Err(io::Error::other(
            "libproc list changed too quickly to inspect",
        ))
    }
    fn same_uid_pids() -> io::Result<Vec<i32>> {
        let uid = unsafe { libc::geteuid() };
        let mut pids: Vec<i32> =
            list(|buffer, size| unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, buffer, size) })?;
        pids.retain(|pid| *pid > 0);
        Ok(pids)
    }
    pub fn identity_alive(identity: ProcessIdentity) -> io::Result<bool> {
        match process_identity(identity.pid) {
            Ok(current) => Ok(current == identity),
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            Err(e) => Err(e),
        }
    }
    fn executable(pid: i32) -> io::Result<PathBuf> {
        let mut bytes = vec![0u8; 4096];
        let count =
            unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
        if count <= 0 {
            return Err(query_error());
        }
        let path = CStr::from_bytes_until_nul(&bytes)
            .map_err(|_| io::Error::other("unterminated proc_pidpath"))?;
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Vnode {
        device: u32,
        inode: u64,
    }
    fn file_key(path: &Path) -> io::Result<Vnode> {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_file() {
            return Err(invalid("VM disk is not a regular file"));
        }
        Ok(Vnode {
            device: meta.dev() as u32,
            inode: meta.ino(),
        })
    }
    fn fd_key(pid: i32, fd: i32, port: bool) -> io::Result<Vnode> {
        let mut info = MaybeUninit::<VnodeFdInfo>::zeroed();
        let size = size_of::<VnodeFdInfo>() as i32;
        clear_errno();
        let count = unsafe {
            if port {
                proc_pidfileportinfo(
                    pid,
                    fd as u32,
                    VNODE_PATH_INFO,
                    info.as_mut_ptr().cast(),
                    size,
                )
            } else {
                proc_pidfdinfo(pid, fd, VNODE_PATH_INFO, info.as_mut_ptr().cast(), size)
            }
        };
        if count != size {
            return Err(query_error());
        }
        let info = unsafe { info.assume_init() };
        Ok(Vnode {
            device: info.vnode.vip_vi.vi_stat.vst_dev,
            inode: info.vnode.vip_vi.vi_stat.vst_ino,
        })
    }
    fn inspect_descriptor(pid: i32, fd: i32, port: bool) -> io::Result<Option<Vnode>> {
        match fd_key(pid, fd, port) {
            Ok(key) => Ok(Some(key)),
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
                if disk_release::skip_denied_descriptor(&executable(pid)?, &e) {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }
    fn open_files(
        pid: i32,
        owned_process: Option<ProcessIdentity>,
        owned_disks: &[Vnode],
    ) -> io::Result<Vec<Vnode>> {
        let identity = process_identity(pid)?;
        let fds: Vec<libc::proc_fdinfo> = list(|buffer, size| unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, buffer, size)
        })?;
        let ports: Vec<FilePort> = list(|buffer, size| unsafe {
            libc::proc_pidinfo(pid, PROC_PIDLISTFILEPORTS, 0, buffer, size)
        })?;
        let descriptors = fds
            .into_iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
            .map(|fd| inspect_descriptor(pid, fd.proc_fd, false))
            .chain(
                ports
                    .into_iter()
                    .filter(|port| port.kind == libc::PROX_FDTYPE_VNODE as u32)
                    .map(|port| inspect_descriptor(pid, port.port as i32, true)),
            );
        let keys = disk_release::collect_file_keys(
            owned_process == Some(identity),
            owned_disks,
            descriptors,
        )?;
        if process_identity(pid)? != identity {
            return Err(io::Error::other("process changed during file inspection"));
        }
        Ok(keys)
    }
    fn responsible_pid(pid: i32) -> io::Result<Option<i32>> {
        let pointer = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_get_pid_responsible_for_pid".as_ptr(),
            )
        };
        if pointer.is_null() {
            return Ok(None);
        }
        let lookup: unsafe extern "C" fn(i32) -> i32 = unsafe { std::mem::transmute(pointer) };
        let responsible = unsafe { lookup(pid) };
        if responsible <= 0 {
            return Err(io::Error::other(
                "responsible PID lookup is restricted or failed",
            ));
        }
        Ok(Some(responsible))
    }
    pub fn identify_service(clone: &Path, helper: ProcessIdentity) -> io::Result<ProcessIdentity> {
        let boot = file_key(&clone.join(BOOT_DISK))?;
        let mut observations = Vec::new();
        for pid in same_uid_pids()? {
            let path = match executable(pid) {
                Ok(path) => path,
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => continue,
                Err(e) => return Err(e),
            };
            if path.file_name() != Some(std::ffi::OsStr::new(SERVICE))
                || !path
                    .components()
                    .any(|c| c.as_os_str() == "Virtualization.framework")
            {
                continue;
            }
            let identity = process_identity(pid)?;
            let holds_boot_disk = open_files(pid, Some(helper), &[boot])?.contains(&boot);
            let responsible_pid = if holds_boot_disk {
                responsible_pid(pid)?
            } else {
                None
            };
            if process_identity(pid)? != identity {
                return Err(io::Error::other("VM service identity changed"));
            }
            observations.push(Ok(ServiceObservation {
                identity,
                holds_boot_disk,
                responsible_pid,
            }));
        }
        // Under a launchd agent, macOS attributes the helper, and the VM service
        // it starts, to the agent's own process rather than to the helper.
        select_service(helper, responsible_pid(helper.pid)?, observations)
    }
    /// Call only after the helper has exited and cannot open more descriptors.
    /// Failure means cleanup and slot reuse must remain blocked. Qualification
    /// must establish that VM disk holders use the motor UID.
    pub fn release_proven(
        service: Option<ProcessIdentity>,
        clone: &Path,
        nonce_disk: &Path,
    ) -> io::Result<bool> {
        if service.map(identity_alive).transpose()?.unwrap_or(false) {
            return Ok(false);
        }
        let keys = [
            file_key(&clone.join(BOOT_DISK))?,
            file_key(&clone.join(AUXILIARY_STORAGE))?,
            file_key(nonce_disk)?,
        ];
        for pid in same_uid_pids()? {
            match open_files(pid, service, &keys) {
                Ok(open) if open.iter().any(|key| keys.contains(key)) => return Ok(false),
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }
    /// A stopped helper may not have reported its service. Match only services
    /// that actually hold this job's private boot-disk vnode, never by PID/name alone.
    pub fn disk_services(clone: &Path) -> io::Result<Vec<ProcessIdentity>> {
        let boot = file_key(&clone.join(BOOT_DISK))?;
        let mut services = Vec::new();
        for pid in same_uid_pids()? {
            let path = match executable(pid) {
                Ok(path) => path,
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => continue,
                Err(e) => return Err(e),
            };
            if path.file_name() != Some(std::ffi::OsStr::new(SERVICE))
                || !path
                    .components()
                    .any(|c| c.as_os_str() == "Virtualization.framework")
            {
                continue;
            }
            let identity = process_identity(pid)?;
            if open_files(pid, None, &[boot])?.contains(&boot) {
                if process_identity(pid)? != identity {
                    return Err(io::Error::other("VM service identity changed"));
                }
                services.push(identity);
            }
        }
        Ok(services)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn vanished_descriptor_query_reports_ebadf() {
            let error = fd_key(std::process::id() as i32, -1, false).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        }
        #[test]
        fn descriptor_query_matches_actual_open_file_vnode() {
            use std::os::fd::AsRawFd;
            let root = cortexkit_test_support::scratch_root()
                .join(format!("macvm-fd-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("disk");
            let file = std::fs::File::create(&path).unwrap();
            assert_eq!(
                fd_key(std::process::id() as i32, file.as_raw_fd(), false).unwrap(),
                file_key(&path).unwrap()
            );
            drop(file);
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(root).unwrap();
        }
        #[test]
        fn incomplete_and_denied_libproc_lists_fail_closed() {
            assert!(
                list::<u64>(|_, _| {
                    unsafe {
                        *libc::__error() = libc::EPERM;
                    }
                    0
                })
                .is_err()
            );
            assert!(list::<u64>(|_, _| -1).is_err());
            assert!(list::<u64>(|_, _| 3).is_err());
            assert!(list::<u64>(|_, size| if size == 0 { 8 } else { size }).is_err());
            assert!(list::<u64>(|_, _| 0).unwrap().is_empty());
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod native_tests {
    use super::*;
    #[test]
    fn helper_identity_uses_kernel_start_time() {
        let identity = process_identity(std::process::id() as i32).unwrap();
        assert_eq!(identity.pid, std::process::id() as i32);
        assert!(identity.start_seconds > 0);
        assert!(process::identity_alive(identity).unwrap());
        assert!(
            !process::identity_alive(ProcessIdentity {
                start_seconds: identity.start_seconds + 1,
                ..identity
            })
            .unwrap()
        );
    }
    #[test]
    fn apfs_clone_is_private_and_refuses_existing_destination() {
        use std::os::unix::fs::PermissionsExt;
        let root = cortexkit_test_support::scratch_root()
            .join(format!("macvm-clone-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let sealed = root.join("sealed");
        let clone = root.join("clone");
        std::fs::create_dir(&sealed).unwrap();
        for name in [
            BOOT_DISK,
            AUXILIARY_STORAGE,
            HARDWARE_MODEL,
            MACHINE_IDENTIFIER,
        ] {
            std::fs::write(sealed.join(name), b"sealed").unwrap();
            std::fs::set_permissions(sealed.join(name), std::fs::Permissions::from_mode(0o400))
                .unwrap();
        }
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o500)).unwrap();
        clone_image(&sealed, &clone).unwrap();
        std::fs::write(clone.join(BOOT_DISK), b"private").unwrap();
        assert_eq!(std::fs::read(sealed.join(BOOT_DISK)).unwrap(), b"sealed");
        assert_eq!(std::fs::read(clone.join(BOOT_DISK)).unwrap(), b"private");
        assert!(clone_image(&sealed, &clone).is_err());
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn live_service_identity_blocks_release_before_any_path_scan() {
        let identity = process_identity(std::process::id() as i32).unwrap();
        assert!(
            !process::release_proven(
                Some(identity),
                Path::new("/nonexistent-clone"),
                Path::new("/nonexistent-nonce")
            )
            .unwrap()
        );
    }
    #[test]
    #[ignore = "operator: same-UID libproc descriptor visibility on the rented Mac"]
    fn disk_holder_blocks_release_without_reported_service() {
        let scratch = cortexkit_test_support::ScratchDir::new("macvm-unreported-holder");
        let clone = scratch.join("clone");
        std::fs::create_dir(&clone).unwrap();
        for name in [BOOT_DISK, AUXILIARY_STORAGE] {
            std::fs::write(clone.join(name), "disk").unwrap();
        }
        let nonce = scratch.join("nonce");
        std::fs::write(&nonce, "nonce").unwrap();
        let holder = std::fs::File::open(clone.join(BOOT_DISK)).unwrap();
        assert!(!process::release_proven(None, &clone, &nonce).unwrap());
        drop(holder);
        assert!(process::release_proven(None, &clone, &nonce).unwrap());
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ServiceObservation {
    pub identity: ProcessIdentity,
    pub holds_boot_disk: bool,
    pub responsible_pid: Option<i32>,
}

/// Observations must cover every same-UID Virtualization service candidate.
/// An unreadable candidate is an error, not an absent match.
///
/// A service holding the boot disk must be attributed either to the helper
/// itself or to the process the helper is attributed to. Which one macOS picks
/// depends on how the helper was launched; the clone vnode match above is what
/// ties the service to this job either way.
pub fn select_service(
    helper: ProcessIdentity,
    helper_responsible: Option<i32>,
    observations: impl IntoIterator<Item = io::Result<ServiceObservation>>,
) -> io::Result<ProcessIdentity> {
    let mut matches = Vec::new();
    for observation in observations {
        let observation = observation?;
        if observation.holds_boot_disk {
            if let Some(responsible) = observation.responsible_pid
                && responsible != helper.pid
                && Some(responsible) != helper_responsible
            {
                return Err(io::Error::other(format!(
                    "VM service responsibility does not match helper: service is attributed to pid {responsible}, helper pid {} is attributed to {helper_responsible:?}",
                    helper.pid
                )));
            }
            matches.push(observation.identity);
        }
    }
    match matches.as_slice() {
        [identity] => Ok(*identity),
        [] => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no VM service holds the clone boot disk",
        )),
        _ => Err(io::Error::other(format!(
            "expected exactly one VM service holding clone boot disk, found {}",
            matches.len()
        ))),
    }
}

#[cfg(test)]
mod service_tests {
    use super::*;
    #[test]
    fn service_identity_requires_exactly_one_inspectable_disk_owner() {
        let helper = ProcessIdentity {
            pid: 100,
            start_seconds: 1,
            start_microseconds: 2,
        };
        let service = ProcessIdentity { pid: 101, ..helper };
        let matching = ServiceObservation {
            identity: service,
            holds_boot_disk: true,
            responsible_pid: Some(helper.pid),
        };
        assert_eq!(
            select_service(helper, None, [Ok(matching)]).unwrap(),
            service
        );
        assert_eq!(
            select_service(
                helper,
                None,
                [Ok(ServiceObservation {
                    responsible_pid: None,
                    ..matching
                })]
            )
            .unwrap(),
            service
        );
        assert!(select_service(helper, None, [Ok(matching), Ok(matching)]).is_err());
        assert!(
            select_service(
                helper,
                None,
                [Ok(ServiceObservation {
                    holds_boot_disk: false,
                    ..matching
                })]
            )
            .is_err()
        );
        assert!(
            select_service(
                helper,
                None,
                [Ok(ServiceObservation {
                    responsible_pid: Some(999),
                    ..matching
                })]
            )
            .is_err()
        );
        // A service attributed to the process the helper is attributed to
        // (a launchd agent) is the helper's own service.
        let agent = ServiceObservation {
            responsible_pid: Some(50),
            ..matching
        };
        assert_eq!(
            select_service(helper, Some(50), [Ok(agent)]).unwrap(),
            service
        );
        assert!(select_service(helper, Some(51), [Ok(agent)]).is_err());
        assert!(
            select_service(
                helper,
                None,
                [
                    Ok(matching),
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "unreadable candidate"
                    ))
                ]
            )
            .is_err()
        );
    }
}
