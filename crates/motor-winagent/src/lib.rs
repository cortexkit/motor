//! The shared host/guest protocol for a single disposable job VM.
//!
//! Frames are JSON preceded by a four-byte big-endian payload length. Both peers
//! must validate each incoming and outgoing frame with the same state machine.
//! Transport deadlines, persistence of Started, and VM cleanup belong to the host.
//!
//! One `Frame` type and one framing serve two guest operating systems. The
//! [`Profile`] chosen when a [`Protocol`] or [`Session`] is constructed decides
//! which frames and which order that conversation accepts.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub mod bun;
pub mod cache;
pub mod cache_format;
pub mod cache_steps;
pub mod environment;
pub mod returned;
pub mod transport;

/// Version of the Windows profile; the Windows agent sends it in its Hello.
pub const PROTOCOL_VERSION: u32 = 6;
/// Version of the macOS profile, versioned independently of the Windows one.
pub const MACOS_PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_COLLECT_BYTES: usize = 16 * 1024 * 1024;

/// Both the checkout's target junction and Cargo's default point at this cache volume.
pub const CACHE_MOUNT: &str = "C:\\t\\";

/// Staged with the tree, but executed only after the separate Start frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub command: String,
    pub cwd: String,
    pub workspace: String,
    pub env: BTreeMap<String, String>,
    pub source_commit: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Service diagnostics, never subprocess output. Fixed fields keep guest-provided
/// log data bounded and prevent arbitrary messages from entering the host log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheTimings {
    pub elapsed_ms: u64,
    pub disk_selection_ms: u64,
    /// Time spent initializing a RAW cache disk (native GPT layout plus
    /// format.com). The name is kept from the PowerShell formatter it replaced
    /// because it is part of the wire format.
    pub powershell_ms: u64,
    /// PowerShell Set-Disk time, zero when the native path is used.
    pub set_disk_ms: u64,
    /// Native disk attribute query/update/verification, excluding layout guards.
    pub native_attributes_ms: u64,
    pub volume_ms: u64,
    pub mountpoint_ms: u64,
    pub dirty_check_ms: u64,
    pub folder_acl_ms: u64,
    pub volume_acl_ms: u64,
    pub folder_acl_updated: bool,
    pub volume_acl_updated: bool,
}

/// Tree and collection payloads are chunks of tar archives, not guest paths.
/// Chunk sizes must account for JSON encoding overhead within the frame cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Hello {
        version: u32,
        nonce: String,
    },
    CacheMounted {
        timings: CacheTimings,
    },
    CacheProgress {
        step: cache_steps::CacheStep,
        elapsed_ms: u64,
    },
    CacheFailed {
        failure: cache_steps::CacheFailure,
    },
    ReceiveTree {
        bytes: Vec<u8>,
    },
    TreeEnd {
        command: Command,
    },
    TreeReceived {},
    BunPreparationFailed {
        refusal: bun::Refusal,
    },
    Start {},
    RequestReturns {
        request: returned::Request,
    },
    Started {},
    Output {
        stream: Stream,
        bytes: Vec<u8>,
    },
    Exit {
        code: u32,
    },
    /// The command was terminated by this signal number. Only the macOS profile
    /// accepts it; Windows reports every termination through `Exit`.
    Signal {
        signal: i32,
    },
    Cancel {},
    Collect {},
    CollectReturns {},
    ReturnChunk {
        bytes: Vec<u8>,
    },
    ReturnFileEnd {
        outcome: returned::Outcome,
    },
    ReturnEnd {},
    Collected {
        bytes: Vec<u8>,
    },
    CollectEnd {
        deleted: Vec<String>,
    },
    Dismount {},
    Dismounted {
        clean: bool,
    },
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// A clean EOF is allowed only between frames. Reject the advertised length
/// before allocating a payload buffer or reading any payload bytes.
pub fn read_frame(reader: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0; 4];
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    reader.read_exact(&mut header[1..])?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(invalid("frame length must be between 1 byte and 1 MiB"));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| invalid(format!("malformed agent frame: {error}")))
}

struct BoundedPayload(Vec<u8>);

impl Write for BoundedPayload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME_BYTES - self.0.len() {
            return Err(invalid("encoded frame exceeds 1 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Serialize into a bounded buffer first, so an oversized frame writes nothing
/// to the transport and cannot allocate an arbitrarily large encoded payload.
pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let mut payload = BoundedPayload(Vec::new());
    serde_json::to_writer(&mut payload, frame)
        .map_err(|error| invalid(format!("cannot encode agent frame: {error}")))?;
    writer.write_all(&(payload.0.len() as u32).to_be_bytes())?;
    writer.write_all(&payload.0)?;
    writer.flush()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer {
    Host,
    Agent,
}

/// Which guest operating system a conversation serves. The profile is fixed when
/// the conversation is constructed and selects the accepted frames and order;
/// the frame type and framing are the same for both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// The Hyper-V/QEMU Windows agent: tree upload, cache mount, collection
    /// and dismount around the command.
    Windows,
    /// The macOS VM agent. The host shares the workspace into the guest, so the
    /// conversation carries the command, its output and its termination
    /// without the Windows tree-upload, cache, collection or dismount frames.
    Macos,
}

impl Profile {
    /// The Hello version an agent speaking this profile must send.
    pub fn version(self) -> u32 {
        match self {
            Profile::Windows => PROTOCOL_VERSION,
            Profile::Macos => MACOS_PROTOCOL_VERSION,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    AwaitHello,
    ReceivingTree,
    /// macOS only: Hello accepted, waiting for the host's TreeEnd, which carries
    /// the command and no tree chunks.
    AwaitCommand,
    AwaitTreeReceived,
    Ready,
    AwaitStarted,
    Running,
    Exited,
    Collecting,
    Returning,
    CollectionComplete,
    AwaitDismounted,
    Complete,
    Failed,
}

/// Tracks both directions of one boot's conversation. Any rejected message
/// poisons the conversation: neither peer may resume or start a second command.
#[derive(Debug)]
pub struct Protocol {
    profile: Profile,
    nonce: String,
    state: State,
    collected_bytes: usize,
    cache_reported: bool,
    cache_progress_frames: u32,
    returns: Option<returned::Request>,
    return_index: usize,
    return_committed: u64,
    return_discarded: u64,
    return_current: u64,
}

impl Protocol {
    /// A Windows-profile conversation.
    pub fn new(nonce: impl Into<String>) -> io::Result<Self> {
        Self::with_profile(Profile::Windows, nonce)
    }

    pub fn with_profile(profile: Profile, nonce: impl Into<String>) -> io::Result<Self> {
        let nonce = nonce.into();
        if nonce.is_empty() {
            return Err(invalid("boot nonce must not be empty"));
        }
        Ok(Self {
            profile,
            nonce,
            state: State::AwaitHello,
            collected_bytes: 0,
            cache_reported: false,
            cache_progress_frames: 0,
            returns: None,
            return_index: 0,
            return_committed: 0,
            return_discarded: 0,
            return_current: 0,
        })
    }

    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn collected_bytes(&self) -> usize {
        self.collected_bytes
    }

    /// `sender` identifies the actual transport direction, not a field provided
    /// by the untrusted peer. Call before acting on a received message.
    pub fn accept(&mut self, sender: Peer, frame: &Frame) -> io::Result<()> {
        let next = match self.profile {
            Profile::Windows => self.transition(sender, frame),
            Profile::Macos => self.macos_transition(sender, frame),
        };
        match next {
            Ok(state) => {
                self.state = state;
                Ok(())
            }
            Err(error) => {
                self.state = State::Failed;
                Err(error)
            }
        }
    }

    fn check_hello(&self, version: u32, nonce: &str) -> io::Result<()> {
        if version != self.profile.version() {
            return Err(invalid(format!(
                "unsupported agent protocol version {version}"
            )));
        }
        if nonce != self.nonce {
            return Err(invalid("agent hello boot nonce mismatch"));
        }
        Ok(())
    }

    /// The macOS sequence: Hello, TreeEnd, Started, any
    /// number of Output, then Exit or Signal. Cancel is allowed while the
    /// command is starting or running. Every other frame, including the Windows
    /// cache, tree-chunk, collect and dismount frames, poisons the conversation.
    fn macos_transition(&mut self, sender: Peer, frame: &Frame) -> io::Result<State> {
        use Frame::*;
        use Peer::{Agent, Host};
        use State::*;

        let next = match (self.state, sender, frame) {
            (AwaitHello, Agent, Hello { version, nonce }) => {
                self.check_hello(*version, nonce)?;
                AwaitCommand
            }
            (AwaitCommand, Host, TreeEnd { command }) => {
                command.validate_posix()?;
                AwaitStarted
            }
            (AwaitStarted, Agent, Started {}) => Running,
            (AwaitStarted | Running, Host, Cancel {}) => self.state,
            (Running, Agent, Output { .. }) => Running,
            (Running, Agent, Exit { .. }) => Complete,
            (Running, Agent, Signal { signal }) => {
                if *signal <= 0 {
                    return Err(invalid(format!("invalid termination signal {signal}")));
                }
                Complete
            }
            _ => {
                return Err(invalid(format!(
                    "out-of-order agent frame from {sender:?} in {:?}",
                    self.state
                )));
            }
        };
        Ok(next)
    }

    fn transition(&mut self, sender: Peer, frame: &Frame) -> io::Result<State> {
        use Frame::*;
        use Peer::{Agent, Host};
        use State::*;

        let next = match (self.state, sender, frame) {
            (AwaitHello, Agent, Hello { version, nonce }) => {
                self.check_hello(*version, nonce)?;
                ReceivingTree
            }
            (ReceivingTree, Host, ReceiveTree { .. }) => ReceivingTree,
            (ReceivingTree | AwaitTreeReceived, Agent, CacheMounted { .. })
                if !self.cache_reported =>
            {
                self.cache_reported = true;
                self.state
            }
            (ReceivingTree | AwaitTreeReceived, Agent, CacheProgress { .. })
                if !self.cache_reported && self.cache_progress_frames < 64 =>
            {
                self.cache_progress_frames += 1;
                self.state
            }
            (ReceivingTree | AwaitTreeReceived, Agent, CacheFailed { failure })
                if !self.cache_reported =>
            {
                failure.validate()?;
                Failed
            }
            (ReceivingTree, Host, TreeEnd { command }) => {
                command.validate()?;
                AwaitTreeReceived
            }
            (AwaitTreeReceived, Agent, TreeReceived {}) => Ready,
            (AwaitTreeReceived, Agent, BunPreparationFailed { refusal }) => {
                refusal.validate()?;
                Failed
            }
            (Ready, Host, Start {}) => AwaitStarted,
            (Ready, Host, RequestReturns { request }) if self.returns.is_none() => {
                request.validate()?;
                self.returns = Some(request.clone());
                Ready
            }
            (AwaitStarted, Agent, Started {}) => Running,
            (AwaitStarted | Running, Host, Cancel {}) => self.state,
            (Running, Agent, Output { .. }) => Running,
            (Running, Agent, Exit { .. }) => Exited,
            (Exited, Host, Collect {}) if self.returns.is_none() => Collecting,
            (Exited, Host, CollectReturns {}) if self.returns.is_some() => Returning,
            (Returning, Agent, ReturnChunk { bytes }) => {
                let request = self.returns.as_ref().unwrap();
                if self.return_index >= request.paths.len()
                    || bytes.is_empty()
                    || bytes.len() > returned::CHUNK_BYTES
                    || self.return_committed
                        + self.return_discarded
                        + self.return_current
                        + bytes.len() as u64
                        > request.max_bytes
                {
                    return Err(invalid("returned-file chunk exceeds negotiated limits"));
                }
                self.return_current += bytes.len() as u64;
                Returning
            }
            (Returning, Agent, ReturnFileEnd { outcome }) => {
                if self.return_index >= self.returns.as_ref().unwrap().paths.len() {
                    return Err(invalid("unexpected returned-file outcome"));
                }
                if let returned::Outcome::Returned { size, sha256 } = outcome {
                    if *size != self.return_current
                        || sha256.len() != 64
                        || !sha256.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        return Err(invalid("invalid returned-file size or digest"));
                    }
                    self.return_committed += size;
                } else {
                    self.return_discarded += self.return_current;
                }
                self.return_index += 1;
                self.return_current = 0;
                Returning
            }
            (Returning, Agent, ReturnEnd {})
                if self.return_index == self.returns.as_ref().unwrap().paths.len() =>
            {
                CollectionComplete
            }
            (Collecting, Agent, Frame::Collected { bytes }) => {
                if bytes.len() > MAX_COLLECT_BYTES - self.collected_bytes {
                    return Err(invalid("collect output exceeds 16 MiB"));
                }
                self.collected_bytes += bytes.len();
                Collecting
            }
            (Collecting, Agent, CollectEnd { deleted }) => {
                for path in deleted {
                    validate_archive_path(path)?;
                    if !path.starts_with("m/") {
                        return Err(invalid(format!("deletion is not a source path: {path}")));
                    }
                    if path.len() > MAX_COLLECT_BYTES - self.collected_bytes {
                        return Err(invalid("collect output exceeds 16 MiB"));
                    }
                    self.collected_bytes += path.len();
                }
                State::CollectionComplete
            }
            (State::CollectionComplete, Host, Dismount {}) => AwaitDismounted,
            (AwaitDismounted, Agent, Dismounted { clean: true }) => Complete,
            (AwaitDismounted, Agent, Dismounted { clean: false }) => {
                return Err(invalid("agent reported an unclean cache dismount"));
            }
            _ => {
                return Err(invalid(format!(
                    "out-of-order agent frame from {sender:?} in {:?}",
                    self.state
                )));
            }
        };
        Ok(next)
    }
}

/// A peer's validated framing entry points. A transport or framing error makes
/// the session unusable, just like a protocol violation.
#[derive(Debug)]
pub struct Session {
    peer: Peer,
    protocol: Protocol,
}

impl Session {
    /// A Windows-profile session.
    pub fn new(peer: Peer, nonce: impl Into<String>) -> io::Result<Self> {
        Self::with_profile(peer, Profile::Windows, nonce)
    }

    pub fn with_profile(
        peer: Peer,
        profile: Profile,
        nonce: impl Into<String>,
    ) -> io::Result<Self> {
        Ok(Self {
            peer,
            protocol: Protocol::with_profile(profile, nonce)?,
        })
    }

    pub fn profile(&self) -> Profile {
        self.protocol.profile()
    }

    pub fn state(&self) -> State {
        self.protocol.state()
    }

    pub fn send(&mut self, writer: &mut impl Write, frame: &Frame) -> io::Result<()> {
        self.protocol.accept(self.peer, frame)?;
        if let Err(error) = write_frame(writer, frame) {
            self.protocol.state = State::Failed;
            return Err(error);
        }
        Ok(())
    }

    pub fn receive(&mut self, reader: &mut impl Read) -> io::Result<Option<Frame>> {
        let sender = match self.peer {
            Peer::Host => Peer::Agent,
            Peer::Agent => Peer::Host,
        };
        let result = match read_frame(reader) {
            Ok(Some(frame)) => self.protocol.accept(sender, &frame).map(|()| Some(frame)),
            Ok(None) if self.state() == State::Complete => Ok(None),
            Ok(None) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                match self.profile() {
                    Profile::Windows => "agent session closed before dismount",
                    Profile::Macos => "agent session closed before exit or signal",
                },
            )),
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.protocol.state = State::Failed;
        }
        result
    }
}

/// The QEMU SMBIOS type-11 string and virtio port name shared with the host.
pub const SMBIOS_NONCE_PREFIX: &str = "motor.boot_nonce=";
pub const VIRTIO_PORT_NAME: &str = "org.cortexkit.motor.winagent.0";
pub const SERVICE_NAME: &str = "motor-winagent";
pub const JOB_USER: &str = "motor-job";
pub const CACHE_LABEL: &str = "motor-target";
pub const CHUNK_BYTES: usize = 128 * 1024;

/// NTFS Modify rights, excluding permission/owner changes, with inheritance to
/// both files and directories. Values are portable for host-side ACL tests.
pub const CACHE_MODIFY_MASK: u32 = 0x0013_01bf;
pub const CACHE_INHERIT_FLAGS: u8 = 0x03;

/// A parsed ordinary allow/deny ACE; SID bytes are compared without name lookup.
#[derive(Clone, Debug)]
pub struct CacheAce {
    pub allow: bool,
    pub mask: u32,
    pub flags: u8,
    pub sid: Vec<u8>,
}

/// Detect the exact explicit root grant installed by the agent. Inherited or
/// inherit-only ACEs do not suffice: the root itself must grant Modify, and its
/// files/directories must inherit it. A matching grant avoids Windows' recursive
/// propagation when SetSecurityInfo is called, even with an unchanged DACL.
pub fn cache_has_modify_grant(aces: &[CacheAce], sid: &[u8]) -> bool {
    aces.iter().any(|ace| {
        ace.allow
            && ace.mask == CACHE_MODIFY_MASK
            && ace.flags == CACHE_INHERIT_FLAGS
            && ace.sid == sid
    })
}

/// Windows rejects these names even when followed by an extension.
pub fn validate_component(component: &str) -> io::Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with(['.', ' '])
        || component
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
    {
        return Err(invalid(format!(
            "invalid Windows path component: {component:?}"
        )));
    }
    let stem = component
        .split('.')
        .next()
        .unwrap()
        .trim_end_matches(' ')
        .to_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if device {
        return Err(invalid(format!(
            "reserved Windows device path: {component:?}"
        )));
    }
    Ok(())
}

pub fn validate_archive_path(path: &str) -> io::Result<()> {
    let path = path.strip_suffix('/').unwrap_or(path);
    let mut components = path.split('/');
    if !matches!(components.next(), Some("m" | "cargo-home" | "bun-seed")) {
        return Err(invalid(format!(
            "archive path must be under m/, cargo-home/ or bun-seed/: {path:?}"
        )));
    }
    for component in components {
        validate_component(component)
            .map_err(|error| invalid(format!("archive path {path:?}: {error}")))?;
    }
    Ok(())
}

pub fn mapped_cwd(posix: &str) -> io::Result<String> {
    let relative = posix
        .strip_prefix('/')
        .ok_or_else(|| invalid(format!("cwd is not an absolute POSIX path: {posix:?}")))?;
    if relative.is_empty() {
        return Ok("C:\\m".into());
    }
    for component in relative.split('/') {
        validate_component(component)
            .map_err(|error| invalid(format!("caller path {posix:?}: {error}")))?;
    }
    Ok(format!("C:\\m\\{}", relative.replace('/', "\\")))
}

impl Command {
    pub fn validate(&self) -> io::Result<()> {
        mapped_cwd(&self.cwd)?;
        mapped_cwd(&self.workspace)?;
        self.validate_text()
    }

    /// The macOS guest uses caller paths as they are, so Windows name rules do
    /// not apply. Paths must still be absolute and normalized: no empty, `.` or
    /// `..` components and no NUL.
    pub fn validate_posix(&self) -> io::Result<()> {
        for path in [&self.cwd, &self.workspace] {
            let relative = path
                .strip_prefix('/')
                .ok_or_else(|| invalid(format!("path is not an absolute POSIX path: {path:?}")))?;
            if relative.contains('\0')
                || (!relative.is_empty()
                    && relative
                        .split('/')
                        .any(|component| matches!(component, "" | "." | "..")))
            {
                return Err(invalid(format!("path is not normalized: {path:?}")));
            }
        }
        self.validate_text()
    }

    fn validate_text(&self) -> io::Result<()> {
        if self.command.contains('\0')
            || self.source_commit.contains('\0')
            || self.env.iter().any(|(key, value)| {
                key.is_empty() || key.contains(['=', '\0']) || value.contains('\0')
            })
        {
            return Err(invalid(
                "command or environment contains an invalid NUL or key",
            ));
        }
        Ok(())
    }

    /// Overlay the loaded user's Windows environment without inheriting service variables.
    /// Agent-owned values still take precedence over accepted request values.
    pub fn environment(
        &self,
        profile: &str,
        base: BTreeMap<String, String>,
        git_bin: &str,
    ) -> io::Result<BTreeMap<String, String>> {
        self.validate()?;
        let cargo_home = format!("{profile}\\.cargo");
        let mut env: BTreeMap<_, _> = base
            .into_iter()
            .map(|(key, value)| (key.to_uppercase(), value))
            .collect();
        let forced = [
            (
                "PATH",
                environment::job_path(env.get("PATH").map_or("", String::as_str), git_bin),
            ),
            ("RUSTUP_HOME", "C:\\rust\\rustup".into()),
            ("HOME", profile.into()),
            ("USERPROFILE", profile.into()),
            ("CARGO_HOME", cargo_home.clone()),
            ("TMP", format!("{profile}\\AppData\\Local\\Temp")),
            ("TEMP", format!("{profile}\\AppData\\Local\\Temp")),
            ("CARGO_NET_OFFLINE", "true".into()),
            ("BUN_INSTALL_CACHE_DIR", bun::CACHE_DIR.into()),
            ("CARGO_NET_GIT_FETCH_WITH_CLI", "true".into()),
            ("GIT_CONFIG_GLOBAL", format!("{cargo_home}\\gitconfig")),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_COUNT", "0".into()),
            ("GIT_ALLOW_PROTOCOL", "file".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("CK_REMOTE_SOURCE_COMMIT", self.source_commit.clone()),
            ("SYSTEMROOT", "C:\\Windows".into()),
            ("SYSTEMDRIVE", "C:".into()),
        ];
        let mut request = BTreeMap::new();
        for (key, value) in &self.env {
            if key.to_ascii_uppercase().starts_with("GIT_CONFIG_")
                || key.eq_ignore_ascii_case("GIT_CONFIG")
                || !forced
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(key))
            {
                let canonical = key.to_uppercase();
                if request.insert(canonical, value.clone()).is_some() {
                    return Err(invalid(format!(
                        "duplicate case-insensitive environment key: {key}"
                    )));
                }
            }
        }
        env.extend(request);
        for (key, value) in [
            ("CARGO_TARGET_DIR", CACHE_MOUNT.trim_end_matches('\\')),
            ("CARGO_BUILD_JOBS", "4"),
            ("RUST_TEST_THREADS", "4"),
            ("NEXTEST_TEST_THREADS", "4"),
        ] {
            env.entry(key.into()).or_insert_with(|| value.into());
        }
        for (key, value) in forced {
            env.insert(key.into(), value);
        }
        #[cfg(windows)]
        match git_rewrite_environment(std::path::Path::new(&cargo_home)) {
            Ok(git) => env.extend(git),
            Err(error) if cfg!(test) && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(env)
    }
}

// Git dependency rewrites only apply inside the Windows job VM; tests exercise
// them on every platform.
#[cfg(any(windows, test))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitRewriteManifest {
    schema: u32,
    routes: Vec<GitRewrite>,
}

#[cfg(any(windows, test))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitRewrite {
    url: String,
    mirror: String,
}

#[cfg(any(windows, test))]
fn local_git_file_url(path: &std::path::Path) -> io::Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| invalid("Git mirror path is not UTF-8"))?
        .replace('\\', "/");
    if path.starts_with("//") {
        return Err(invalid("UNC Git mirror path is not supported"));
    }
    let absolute = if path.starts_with('/') {
        path
    } else if path.as_bytes().get(1) == Some(&b':') {
        format!("/{path}")
    } else {
        return Err(invalid("Git mirror path is not absolute"));
    };
    let mut encoded = String::with_capacity(absolute.len());
    for byte in absolute.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(format!("file://{encoded}"))
}

/// The job rewrites each declared Git source to a local file mirror, so the
/// original URL is only an identifier that is never contacted. Accept the
/// same schemes as the runner (https, http, ssh, git), including an ssh
/// username such as `git@`, and refuse any password or control characters.
#[cfg(any(windows, test))]
fn job_git_rewrite_url_allowed(url: &str) -> bool {
    if url.chars().any(char::is_control) {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "https" | "http" | "ssh" | "git") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return false;
    }
    if let Some((userinfo, host)) = authority.rsplit_once('@') {
        if userinfo.is_empty() || host.is_empty() || userinfo.contains(':') {
            return false;
        }
    }
    true
}

#[cfg(any(windows, test))]
fn git_rewrite_environment(cargo_home: &std::path::Path) -> io::Result<BTreeMap<String, String>> {
    safe_directory(cargo_home)?;
    let path = cargo_home.join("git-rewrites.json");
    let manifest_metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let manifest: GitRewriteManifest = if let Some(metadata) = &manifest_metadata {
        if !metadata.is_file() || is_reparse(metadata) {
            return Err(invalid("job Git rewrite manifest is not a regular file"));
        }
        serde_json::from_slice(&std::fs::read(&path)?)
            .map_err(|error| invalid(error.to_string()))?
    } else {
        // A non-Cargo snapshot has no packaged dependency routes.
        GitRewriteManifest {
            schema: 1,
            routes: Vec::new(),
        }
    };
    if manifest.schema != 1 {
        return Err(invalid("unsupported job Git rewrite manifest schema"));
    }
    let global = cargo_home.join("gitconfig");
    match std::fs::symlink_metadata(&global) {
        Ok(metadata) if metadata.is_file() && !is_reparse(&metadata) => {}
        // Git accepts a missing global config. Keep its path pinned to the job
        // home instead of inheriting the service account's configuration.
        Err(error) if manifest_metadata.is_none() && error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
        Ok(_) => return Err(invalid("job Git config is not a regular file")),
    }
    let mut env = BTreeMap::from([
        (
            "GIT_CONFIG_GLOBAL".to_owned(),
            global.to_string_lossy().into_owned(),
        ),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ("GIT_ALLOW_PROTOCOL".to_owned(), "file".to_owned()),
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("CARGO_NET_GIT_FETCH_WITH_CLI".to_owned(), "true".to_owned()),
        (
            "GIT_CONFIG_COUNT".to_owned(),
            manifest.routes.len().to_string(),
        ),
    ]);
    for (index, route) in manifest.routes.into_iter().enumerate() {
        if !job_git_rewrite_url_allowed(&route.url) {
            return Err(invalid(
                "job Git rewrite URL is not an allowed credential-free URL",
            ));
        }
        let relative = std::path::Path::new(&route.mirror);
        if relative.is_absolute()
            || relative.components().next()
                != Some(std::path::Component::Normal(std::ffi::OsStr::new(
                    "git-mirrors",
                )))
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(invalid(
                "job Git rewrite path is outside cargo-home/git-mirrors",
            ));
        }
        let mut mirror = cargo_home.to_path_buf();
        for component in relative.components() {
            mirror.push(component);
            let metadata = std::fs::symlink_metadata(&mirror)?;
            if !metadata.is_dir() || is_reparse(&metadata) {
                return Err(invalid(format!(
                    "job Git mirror is not a real directory: {}",
                    mirror.display()
                )));
            }
        }
        let local = local_git_file_url(&mirror)?;
        env.insert(
            format!("GIT_CONFIG_KEY_{index}"),
            format!("url.{local}.insteadOf"),
        );
        env.insert(format!("GIT_CONFIG_VALUE_{index}"), route.url);
    }
    Ok(env)
}

/// Populate Cargo's own Git cache from the shipped mirrors before an offline
/// command. Cargo's offline switch refuses even a file-only Git rewrite when
/// its `git/db` is empty, so only these fetches temporarily lift that switch.
/// They run inside the job's networkless VM, with Git restricted to file URLs,
/// under the same unprivileged token and process bounds as the caller command.
#[cfg(any(windows, test))]
pub fn command_with_git_cache(
    command: &str,
    cargo_home: &std::path::Path,
    source_root: &std::path::Path,
    files: &Inventory,
) -> io::Result<String> {
    if git_rewrite_environment(cargo_home)?["GIT_CONFIG_COUNT"] == "0" {
        return Ok(command.to_owned());
    }
    let mut script = String::new();
    for lock in files.keys().filter(|name| name.ends_with("/Cargo.lock")) {
        let manifest = format!("{}Cargo.toml", lock.strip_suffix("Cargo.lock").unwrap());
        if !files.contains_key(&manifest) {
            continue;
        }
        let path = source_root.join(
            manifest
                .strip_prefix("m/")
                .ok_or_else(|| invalid("manifest outside source root"))?,
        );
        let path = path
            .to_str()
            .ok_or_else(|| invalid("manifest path is not UTF-8"))?
            .replace('\\', "/")
            .replace('\'', "'\\''");
        // Nested manifests are best-effort, as on the QEMU preparation path.
        // The caller's offline command still fails if a needed source is absent.
        script.push_str(&format!("CARGO_NET_OFFLINE=false cargo fetch --locked --manifest-path '{path}' || printf '%s\\n' 'warning: Git cache preparation failed' >&2;\n"));
    }
    script.push_str(command);
    Ok(script)
}

/// Preflight the complete archive before writing any files. Directory prefixes
/// participate in collision detection, so `a/x` and `A/y` cannot merge silently.
pub fn validate_tree(reader: &mut (impl Read + io::Seek)) -> io::Result<()> {
    validate_archive(reader, false)
}

/// Collected links are inert archive records, never extraction instructions.
/// Keep this separate from the stricter host-to-guest staging format.
pub fn validate_collection(reader: &mut (impl Read + io::Seek)) -> io::Result<()> {
    validate_archive(reader, true)
}

fn validate_archive(reader: &mut (impl Read + io::Seek), collection: bool) -> io::Result<()> {
    reader.rewind()?;
    let mut archive = tar::Archive::new(&mut *reader);
    let mut names = BTreeMap::<String, String>::new();
    let mut entries = std::collections::BTreeSet::new();
    let mut bun_seed = false;
    let mut bun_manifest = false;
    for entry in archive.entries()? {
        let entry = entry?;
        let bytes = entry.path_bytes();
        let path = std::str::from_utf8(&bytes).map_err(|_| invalid("tar path is not UTF-8"))?;
        let kind = entry.header().entry_type();
        let link = collection && kind.is_symlink();
        if !kind.is_file() && !kind.is_dir() && !link {
            return Err(invalid(format!(
                "tar links and non-file members are forbidden: {path}"
            )));
        }
        validate_archive_path(path)?;
        if path.trim_end_matches('/') == "bun-seed" || path.starts_with("bun-seed/") {
            bun::validate_seed_member(path, kind.is_dir())?;
            bun_seed = true;
            bun_manifest |= path == "bun-seed/manifest.json";
        }
        if collection && !path.starts_with("m/") {
            return Err(invalid(format!(
                "collect member is not a source path: {path}"
            )));
        }
        if link {
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| invalid("link has no target"))?;
            std::str::from_utf8(&target).map_err(|_| invalid("link target is not UTF-8"))?;
            if target.is_empty() || entry.size() != 0 {
                return Err(invalid("collected link must have a target and no payload"));
            }
        }
        let path = path.trim_end_matches('/');
        if !entry.header().entry_type().is_dir() && matches!(path, "m" | "cargo-home" | "bun-seed")
        {
            return Err(invalid(format!(
                "archive prefix must be a directory: {path}"
            )));
        }
        if !entries.insert(path.to_owned()) {
            return Err(invalid(format!("duplicate tar path: {path}")));
        }
        let mut prefix = String::new();
        for component in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if let Some(previous) = names.insert(prefix.to_uppercase(), prefix.clone())
                && previous != prefix
            {
                return Err(invalid(format!(
                    "case-colliding tar paths: {previous} and {prefix}"
                )));
            }
        }
    }
    if bun_seed && !bun_manifest {
        return Err(invalid("Bun seed digest manifest is missing"));
    }
    reader.rewind()
}

fn safe_directory(path: &std::path::Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !is_reparse(&metadata) => Ok(()),
        Ok(_) => Err(invalid(format!(
            "extraction directory is not a real directory: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::fs::create_dir(path),
        Err(error) => Err(error),
    }
}

fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn open_source_file(path: &std::path::Path) -> io::Result<std::fs::File> {
    if is_reparse(&std::fs::symlink_metadata(path)?) {
        return Err(invalid("source file is a reparse point"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000).share_mode(1); // OPEN_REPARSE_POINT, FILE_SHARE_READ
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_reparse(&metadata) {
        return Err(invalid(
            "source file changed into a reparse point or non-file",
        ));
    }
    Ok(file)
}

fn source_directory(path: &std::path::Path) -> io::Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || is_reparse(&metadata) {
        return Err(invalid("source directory is not a physical directory"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Deny write/delete sharing while resolving children, so an ancestor
        // cannot be renamed or turned into a reparse point during the walk.
        options.custom_flags(0x02200000).share_mode(1); // OPEN_REPARSE_POINT | BACKUP_SEMANTICS
    }
    let directory = options.open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || is_reparse(&metadata) {
        return Err(invalid("source directory changed into a reparse point"));
    }
    Ok(directory)
}

/// Roots must already exist and be owned by the job user. Extraction never uses
/// tar-supplied permissions, ownership, symlinks, or absolute paths.
pub fn unpack_tree(
    reader: &mut (impl Read + io::Seek),
    source_root: &std::path::Path,
    cargo_home: &std::path::Path,
) -> io::Result<()> {
    validate_tree(reader)?;
    safe_directory(source_root)?;
    safe_directory(cargo_home)?;
    for entry in tar::Archive::new(reader).entries()? {
        let mut entry = entry?;
        let bytes = entry.path_bytes();
        let path = std::str::from_utf8(&bytes).map_err(|_| invalid("tar path is not UTF-8"))?;
        let mut components = path.trim_end_matches('/').split('/');
        let mut dest = match components.next() {
            Some("m") => source_root.to_path_buf(),
            Some("cargo-home") => cargo_home.to_path_buf(),
            Some("bun-seed") => continue,
            _ => unreachable!(),
        };
        let components: Vec<_> = components.collect();
        for (index, component) in components.iter().enumerate() {
            dest.push(component);
            if index + 1 < components.len() || entry.header().entry_type().is_dir() {
                safe_directory(&dest)?;
            }
        }
        if entry.header().entry_type().is_file() {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest)?;
            io::copy(&mut entry, &mut file)?;
        }
    }
    Ok(())
}

pub fn rewrite_local_registry_config(cargo_home: &std::path::Path) -> io::Result<()> {
    safe_directory(cargo_home)?;
    let config_path = cargo_home.join("config.toml");
    let metadata = match std::fs::symlink_metadata(&config_path) {
        Ok(metadata) => metadata,
        // QEMU archives and non-Cargo snapshots need not include a generated
        // Cargo config. Only relocate the native preparer's config when present.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || is_reparse(&metadata) {
        return Err(invalid(format!(
            "job Cargo config is not a regular file: {}",
            config_path.display()
        )));
    }
    let original = std::fs::read_to_string(&config_path)?;
    if !original
        .lines()
        .any(|line| line.trim() == "[source.motor-local]")
    {
        return Ok(());
    }

    let local_registry = cargo_home.join("local-registry");
    safe_directory(&local_registry)?;
    if !local_registry.is_absolute() {
        return Err(invalid("guest local Cargo registry path is not absolute"));
    }
    let escaped_path = local_registry
        .to_str()
        .ok_or_else(|| invalid("guest local Cargo registry path is not UTF-8"))?
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let mut section = String::new();
    let mut replacements = 0;
    let mut rewritten = String::with_capacity(original.len());
    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.to_owned();
        }
        if section == "[source.motor-local]"
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "local-registry")
        {
            replacements += 1;
            let indentation = &line[..line.len() - line.trim_start().len()];
            rewritten.push_str(indentation);
            rewritten.push_str(&format!("local-registry = \"{escaped_path}\"\n"));
        } else {
            rewritten.push_str(line);
            rewritten.push('\n');
        }
    }
    if replacements != 1 {
        return Err(invalid(format!(
            "expected one local-registry path in job Cargo config, found {replacements}"
        )));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&config_path)?;
    file.write_all(rewritten.as_bytes())?;
    file.sync_all()
}

pub type Inventory = BTreeMap<String, [u8; 32]>;

/// Target directories live on the cache disk and must never enter collected
/// source changes. Hash link text, never the contents of a link's target.
pub fn inventory(root: &std::path::Path) -> io::Result<Inventory> {
    use sha2::{Digest, Sha256};
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        files: &mut Inventory,
    ) -> io::Result<()> {
        let _directory = source_directory(dir)?;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("source path is not UTF-8"))?;
            validate_component(&name)?;
            if name.eq_ignore_ascii_case("target") {
                continue;
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if !is_reparse(&metadata) && metadata.is_dir() {
                walk(root, &path, files)?;
            } else {
                let mut digest = Sha256::new();
                if is_reparse(&metadata) {
                    // read_link understands symbolic links and directory junctions;
                    // unsupported reparse tags fail rather than becoming traversal.
                    let target = std::fs::read_link(&path)?;
                    let target = target
                        .to_str()
                        .ok_or_else(|| invalid("link target is not UTF-8"))?;
                    digest.update(b"link\0");
                    digest.update(target.as_bytes());
                } else if metadata.is_file() {
                    digest.update(b"file\0");
                    io::copy(&mut open_source_file(&path)?, &mut digest)?;
                } else {
                    return Err(invalid(format!("non-file source path: {}", path.display())));
                }
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("/");
                files.insert(format!("m/{relative}"), digest.finalize().into());
            }
        }
        Ok(())
    }
    let mut files = Inventory::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

/// Collection is a tar of new/changed files plus an explicit deletion list.
/// Bound the tar builder as well as the host's incoming collection counter.
pub fn collect_changes_to(
    root: &std::path::Path,
    before: &Inventory,
    writer: impl Write,
) -> io::Result<Vec<String>> {
    struct CollectWriter<W> {
        writer: W,
        count: usize,
    }
    impl<W: Write> Write for CollectWriter<W> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_COLLECT_BYTES - self.count {
                return Err(invalid("collect output exceeds 16 MiB"));
            }
            let n = self.writer.write(bytes)?;
            self.count += n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.writer.flush()
        }
    }
    let after = inventory(root)?;
    let mut builder = tar::Builder::new(CollectWriter { writer, count: 0 });
    builder.follow_symlinks(false);
    for (path, hash) in &after {
        if before.get(path) != Some(hash) {
            let relative = std::path::Path::new(path.strip_prefix("m/").unwrap());
            let mut parents = vec![source_directory(root)?];
            let mut parent = root.to_path_buf();
            for component in relative.parent().unwrap().components() {
                parent.push(component);
                parents.push(source_directory(&parent)?);
            }
            let source = root.join(relative);
            if is_reparse(&std::fs::symlink_metadata(&source)?) {
                let target = std::fs::read_link(&source)?;
                let target = target
                    .to_str()
                    .ok_or_else(|| invalid("link target is not UTF-8"))?;
                append_source_link(&mut builder, path, target)?;
            } else {
                builder.append_file(path, &mut open_source_file(&source)?)?;
            }
        }
    }
    let deleted = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .cloned()
        .collect();
    builder.into_inner()?.flush()?;
    Ok(deleted)
}

#[cfg(test)]
fn collect_changes(
    root: &std::path::Path,
    before: &Inventory,
) -> io::Result<(Vec<u8>, Vec<String>)> {
    let mut bytes = Vec::new();
    let deleted = collect_changes_to(root, before, &mut bytes)?;
    Ok((bytes, deleted))
}

fn append_source_link(
    builder: &mut tar::Builder<impl Write>,
    path: &str,
    target: &str,
) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_mode(0o777);
    header.set_size(0);
    // Path-based tar helpers normalize Windows separators. Preserve literal
    // link text, including absolute and UNC targets, without resolving it.
    if target.len() <= 100 {
        header.set_link_name_literal(target.as_bytes())?;
    } else {
        builder.append_pax_extensions([("linkpath", target.as_bytes())])?;
    }
    builder.append_data(&mut header, path, io::empty())
}

#[cfg(test)]
mod macos_profile_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn non_cargo_workspace_needs_no_local_registry_config() {
        let scratch = Scratch::new();
        let cargo_home = scratch.0.join("profile/.cargo");
        std::fs::create_dir_all(&cargo_home).unwrap();
        let source_root = scratch.0.join("sources");
        let mut archive = tar::Builder::new(Vec::new());
        let script = b"printf started\n";
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(script.len() as u64);
        header.set_cksum();
        archive
            .append_data(&mut header, "m/workspace/run.sh", &script[..])
            .unwrap();
        let bytes = archive.into_inner().unwrap();
        unpack_tree(&mut Cursor::new(bytes), &source_root, &cargo_home).unwrap();

        rewrite_local_registry_config(&cargo_home).unwrap();

        let env = git_rewrite_environment(&cargo_home).unwrap();
        assert_eq!(env["GIT_CONFIG_COUNT"], "0");
        assert_eq!(env["GIT_ALLOW_PROTOCOL"], "file");
        assert_eq!(
            env["GIT_CONFIG_GLOBAL"],
            cargo_home.join("gitconfig").to_str().unwrap()
        );
        assert_eq!(
            command_with_git_cache(
                "sh run.sh",
                &cargo_home,
                &source_root,
                &inventory(&source_root).unwrap()
            )
            .unwrap(),
            "sh run.sh"
        );
        assert_eq!(
            std::fs::read(source_root.join("workspace/run.sh")).unwrap(),
            script
        );

        assert!(!cargo_home.join("config.toml").exists());
        // Absence is optional; an existing non-file is still invalid.
        std::fs::create_dir(cargo_home.join("config.toml")).unwrap();
        assert!(rewrite_local_registry_config(&cargo_home).is_err());
    }

    #[test]
    fn local_registry_config_is_rewritten_to_absolute_guest_cargo_home() {
        let scratch = Scratch::new();
        let cargo_home = scratch.0.join("profile/.cargo");
        let registry = cargo_home.join("local-registry");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(
            cargo_home.join("config.toml"),
            "[source.crates-io]\nreplace-with = \"motor-local\"\n\n[source.motor-local]\nlocal-registry = \"C:\\\\host\\\\staging\\\\cargo-home\\\\local-registry\"\n\n[net]\ngit-fetch-with-cli = true\n",
        )
        .unwrap();

        rewrite_local_registry_config(&cargo_home).unwrap();

        let config = std::fs::read_to_string(cargo_home.join("config.toml")).unwrap();
        let expected = registry
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        assert!(config.contains(&format!("local-registry = \"{expected}\"")));
        assert!(std::path::Path::new(&registry).is_absolute());
        assert!(!config.contains("C:\\\\host\\\\staging\\\\cargo-home\\\\local-registry"));
    }

    #[test]
    fn cache_timings_are_bounded_service_frames_not_command_output() {
        let frame = Frame::CacheMounted {
            timings: CacheTimings {
                disk_selection_ms: 17,
                ..Default::default()
            },
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).unwrap();
        assert!(bytes.len() < 1024);
        assert_eq!(read_frame(&mut &bytes[..]).unwrap(), Some(frame.clone()));
        let mut protocol = Protocol::new("boot-a").unwrap();
        protocol.accept(Peer::Agent, &hello()).unwrap();
        protocol.accept(Peer::Agent, &frame).unwrap();
        assert_eq!(protocol.state(), State::ReceivingTree);
        assert!(
            protocol.accept(Peer::Agent, &frame).is_err(),
            "duplicate diagnostics must not extend setup indefinitely"
        );
        let mut protocol = Protocol::new("boot-a").unwrap();
        assert!(protocol.accept(Peer::Agent, &frame).is_err());
        let mut protocol = Protocol::new("boot-a").unwrap();
        protocol.accept(Peer::Agent, &hello()).unwrap();
        assert!(protocol.accept(Peer::Host, &frame).is_err());
    }

    #[test]
    fn native_cache_timings_have_a_distinct_field_and_protocol_generation() {
        let value = serde_json::to_value(CacheTimings::default()).unwrap();
        assert!(value["native_attributes_ms"].is_u64());
        assert_eq!(PROTOCOL_VERSION, 6);
    }

    #[test]
    fn cache_failure_is_a_bounded_terminal_pre_start_diagnostic() {
        use cache_steps::{CacheFailure, CacheStep};
        let frame = Frame::CacheFailed {
            failure: CacheFailure {
                step: CacheStep::DiskLayout,
                elapsed_ms: 3_000,
                message: "DiskLayout: device not ready (os error 21)".into(),
                completed: vec![],
            },
        };
        for tree_end in [false, true] {
            let mut protocol = Protocol::new("boot-a").unwrap();
            protocol.accept(Peer::Agent, &hello()).unwrap();
            if tree_end {
                protocol
                    .accept(Peer::Host, &Frame::TreeEnd { command: command() })
                    .unwrap();
            }
            protocol.accept(Peer::Agent, &frame).unwrap();
            assert_eq!(protocol.state(), State::Failed);
            assert!(protocol.accept(Peer::Host, &Frame::Start {}).is_err());
        }
        for case in 0..3 {
            let mut invalid_frame = frame.clone();
            if let Frame::CacheFailed { failure } = &mut invalid_frame {
                match case {
                    0 => failure.message = "x".repeat(1025),
                    1 => failure.message = "forged\nlog line".into(),
                    _ => {
                        failure.completed = vec![
                            cache_steps::StepTiming {
                                step: CacheStep::DiskOpen,
                                elapsed_ms: 0
                            };
                            33
                        ]
                    }
                }
            }
            let mut protocol = Protocol::new("boot-a").unwrap();
            protocol.accept(Peer::Agent, &hello()).unwrap();
            assert!(protocol.accept(Peer::Agent, &invalid_frame).is_err());
        }
    }

    #[test]
    fn cache_heartbeats_are_bounded_and_cannot_arrive_after_mount() {
        use cache_steps::CacheStep;
        let frame = Frame::CacheProgress {
            step: CacheStep::DiskLayout,
            elapsed_ms: 1_000,
        };
        let mut protocol = Protocol::new("boot-a").unwrap();
        protocol.accept(Peer::Agent, &hello()).unwrap();
        for _ in 0..64 {
            protocol.accept(Peer::Agent, &frame).unwrap();
        }
        assert!(protocol.accept(Peer::Agent, &frame).is_err());
        let mut protocol = Protocol::new("boot-a").unwrap();
        protocol.accept(Peer::Agent, &hello()).unwrap();
        protocol
            .accept(
                Peer::Agent,
                &Frame::CacheMounted {
                    timings: CacheTimings::default(),
                },
            )
            .unwrap();
        assert!(protocol.accept(Peer::Agent, &frame).is_err());
    }

    fn command() -> Command {
        Command {
            command: "cargo test --locked".into(),
            cwd: "/work/project".into(),
            workspace: "/work/project".into(),
            env: BTreeMap::new(),
            source_commit: "abc123".into(),
        }
    }
    fn hello() -> Frame {
        Frame::Hello {
            version: PROTOCOL_VERSION,
            nonce: "boot-a".into(),
        }
    }
    #[test]
    fn requested_returns_are_negotiated_before_start_and_bound_the_stream() {
        fn ready() -> Protocol {
            let mut protocol = Protocol::new("boot-a").unwrap();
            for (peer, frame) in transcript().into_iter().take(4) {
                protocol.accept(peer, &frame).unwrap();
            }
            protocol
        }
        let request = Frame::RequestReturns {
            request: returned::Request {
                paths: vec!["artifact".into()],
                max_bytes: 2,
            },
        };
        let mut protocol = ready();
        protocol.accept(Peer::Host, &request).unwrap();
        for (peer, frame) in [
            (Peer::Host, Frame::Start {}),
            (Peer::Agent, Frame::Started {}),
            (Peer::Agent, Frame::Exit { code: 0 }),
            (Peer::Host, Frame::CollectReturns {}),
            (Peer::Agent, Frame::ReturnChunk { bytes: vec![1, 2] }),
        ] {
            protocol.accept(peer, &frame).unwrap();
        }
        assert!(
            protocol
                .accept(Peer::Agent, &Frame::ReturnChunk { bytes: vec![3] })
                .is_err()
        );
        let mut protocol = ready();
        protocol.accept(Peer::Host, &Frame::Start {}).unwrap();
        assert!(
            protocol.accept(Peer::Host, &request).is_err(),
            "cap cannot change after Start"
        );
        let mut protocol = ready();
        protocol.accept(Peer::Host, &request).unwrap();
        assert!(
            protocol.accept(Peer::Host, &request).is_err(),
            "request cannot be replaced"
        );
        let mut protocol = ready();
        protocol.accept(Peer::Host, &Frame::Start {}).unwrap();
        protocol.accept(Peer::Agent, &Frame::Started {}).unwrap();
        protocol
            .accept(Peer::Agent, &Frame::Exit { code: 0 })
            .unwrap();
        assert!(
            protocol
                .accept(Peer::Host, &Frame::CollectReturns {})
                .is_err(),
            "no implicit default cap"
        );
    }

    #[test]
    fn requested_returns_discarded_chunks_still_spend_the_wire_allowance() {
        let mut protocol = Protocol::new("boot-a").unwrap();
        for (peer, frame) in transcript().into_iter().take(4) {
            protocol.accept(peer, &frame).unwrap();
        }
        for (peer, frame) in [
            (
                Peer::Host,
                Frame::RequestReturns {
                    request: returned::Request {
                        paths: vec!["one".into(), "two".into()],
                        max_bytes: 2,
                    },
                },
            ),
            (Peer::Host, Frame::Start {}),
            (Peer::Agent, Frame::Started {}),
            (Peer::Agent, Frame::Exit { code: 0 }),
            (Peer::Host, Frame::CollectReturns {}),
            (Peer::Agent, Frame::ReturnChunk { bytes: vec![1, 2] }),
            (
                Peer::Agent,
                Frame::ReturnFileEnd {
                    outcome: returned::Outcome::Failed {
                        cause: returned::Failure::ReadError,
                        size: Some(2),
                    },
                },
            ),
        ] {
            protocol.accept(peer, &frame).unwrap();
        }
        assert!(
            protocol
                .accept(Peer::Agent, &Frame::ReturnChunk { bytes: vec![3] })
                .is_err()
        );
        assert_eq!(protocol.state(), State::Failed);
        assert!(protocol.accept(Peer::Agent, &Frame::ReturnEnd {}).is_err());
    }
    fn transcript() -> Vec<(Peer, Frame)> {
        vec![
            (Peer::Agent, hello()),
            (
                Peer::Host,
                Frame::ReceiveTree {
                    bytes: vec![0, 255],
                },
            ),
            (Peer::Host, Frame::TreeEnd { command: command() }),
            (Peer::Agent, Frame::TreeReceived {}),
            (Peer::Host, Frame::Start {}),
            (Peer::Agent, Frame::Started {}),
            (
                Peer::Agent,
                Frame::Output {
                    stream: Stream::Stdout,
                    bytes: vec![0, 255],
                },
            ),
            (
                Peer::Agent,
                Frame::Output {
                    stream: Stream::Stderr,
                    bytes: vec![10],
                },
            ),
            (Peer::Agent, Frame::Exit { code: 0xc0000005 }),
            (Peer::Host, Frame::Collect {}),
            (Peer::Agent, Frame::Collected { bytes: vec![1, 2] }),
            (
                Peer::Agent,
                Frame::CollectEnd {
                    deleted: vec!["m/work/project/gone.rs".into()],
                },
            ),
            (Peer::Host, Frame::Dismount {}),
            (Peer::Agent, Frame::Dismounted { clean: true }),
        ]
    }
    fn advance(protocol: &mut Protocol, count: usize) {
        for (peer, frame) in transcript().into_iter().take(count) {
            protocol.accept(peer, &frame).unwrap();
        }
    }

    #[test]
    fn roundtrip_and_full_width_exit_status() {
        for (_, frame) in transcript() {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &frame).unwrap();
            let expected_length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
            assert_eq!(expected_length, bytes.len() - 4);
            assert_eq!(read_frame(&mut Cursor::new(bytes)).unwrap(), Some(frame));
        }
        for code in [0, 256, u32::MAX, 0xc0000005] {
            let frame = Frame::Exit { code };
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &frame).unwrap();
            assert_eq!(read_frame(&mut &bytes[..]).unwrap(), Some(frame));
        }
    }

    #[test]
    fn oversized_length_is_rejected_before_payload_read_or_allocation() {
        struct HeaderOnly(Cursor<Vec<u8>>);
        impl Read for HeaderOnly {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert!(
                    self.0.position() < 4,
                    "oversized payload was read after its header"
                );
                self.0.read(buffer)
            }
        }
        for length in [0, MAX_FRAME_BYTES as u32 + 1, u32::MAX] {
            let error = read_frame(&mut HeaderOnly(Cursor::new(length.to_be_bytes().to_vec())))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn exact_frame_cap_is_allowed_and_oversized_write_is_atomic() {
        let empty = serde_json::to_vec(&Frame::Hello {
            version: 1,
            nonce: String::new(),
        })
        .unwrap()
        .len();
        let frame = Frame::Hello {
            version: 1,
            nonce: "a".repeat(MAX_FRAME_BYTES - empty),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).unwrap();
        assert_eq!(bytes.len(), MAX_FRAME_BYTES + 4);
        assert_eq!(read_frame(&mut &bytes[..]).unwrap(), Some(frame));
        let mut bytes = vec![7];
        assert!(
            write_frame(
                &mut bytes,
                &Frame::Hello {
                    version: 1,
                    nonce: "a".repeat(MAX_FRAME_BYTES)
                }
            )
            .is_err()
        );
        assert_eq!(bytes, vec![7]);
    }

    #[test]
    fn eof_truncation_and_malformed_frames() {
        assert_eq!(read_frame(&mut &[][..]).unwrap(), None);
        for bytes in [vec![0], vec![0, 0, 0], vec![0, 0, 0, 2, b'{']] {
            assert_eq!(
                read_frame(&mut &bytes[..]).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
        for json in [
            "{}",
            "null",
            "{\"kind\":\"unknown\"}",
            "{\"kind\":\"start\",\"extra\":1}",
            "{\"kind\":\"exit\",\"code\":4294967296}",
        ] {
            let mut bytes = (json.len() as u32).to_be_bytes().to_vec();
            bytes.extend(json.as_bytes());
            assert_eq!(
                read_frame(&mut &bytes[..]).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn stale_nonce_poisoned_session() {
        let mut protocol = Protocol::new("different-boot").unwrap();
        assert!(
            protocol
                .accept(Peer::Agent, &hello())
                .unwrap_err()
                .to_string()
                .contains("nonce")
        );
        assert_eq!(protocol.state(), State::Failed);
        assert!(
            protocol
                .accept(
                    Peer::Agent,
                    &Frame::Hello {
                        version: PROTOCOL_VERSION,
                        nonce: "different-boot".into()
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn wrong_version_poisoned_session() {
        let mut protocol = Protocol::new("boot-a").unwrap();
        assert!(
            protocol
                .accept(
                    Peer::Agent,
                    &Frame::Hello {
                        version: PROTOCOL_VERSION + 1,
                        nonce: "boot-a".into()
                    }
                )
                .unwrap_err()
                .to_string()
                .contains("version")
        );
        assert_eq!(protocol.state(), State::Failed);
    }

    #[test]
    fn ordered_conversation_and_cancellation() {
        let mut host = Session::new(Peer::Host, "boot-a").unwrap();
        let mut agent = Session::new(Peer::Agent, "boot-a").unwrap();
        for (sender, frame) in transcript() {
            let (send, receive) = if sender == Peer::Host {
                (&mut host, &mut agent)
            } else {
                (&mut agent, &mut host)
            };
            let mut bytes = Vec::new();
            send.send(&mut bytes, &frame).unwrap();
            assert_eq!(receive.receive(&mut &bytes[..]).unwrap(), Some(frame));
            assert_eq!(host.state(), agent.state());
        }
        assert_eq!(host.state(), State::Complete);
        assert!(host.receive(&mut &[][..]).unwrap().is_none());
        let mut protocol = Protocol::new("boot-a").unwrap();
        advance(&mut protocol, 5);
        protocol.accept(Peer::Host, &Frame::Cancel {}).unwrap();
        protocol.accept(Peer::Agent, &Frame::Started {}).unwrap();
        protocol.accept(Peer::Host, &Frame::Cancel {}).unwrap();
        protocol
            .accept(Peer::Agent, &Frame::Exit { code: 1 })
            .unwrap();
    }

    #[test]
    fn output_cannot_open_the_execution_gate() {
        let mut protocol = Protocol::new("boot-a").unwrap();
        advance(&mut protocol, 4);
        assert_eq!(protocol.state(), State::Ready);
        assert!(
            protocol
                .accept(
                    Peer::Agent,
                    &Frame::Output {
                        stream: Stream::Stdout,
                        bytes: vec![]
                    }
                )
                .is_err()
        );
        assert_eq!(protocol.state(), State::Failed);
    }

    #[test]
    fn message_order_and_direction_are_enforced_at_every_phase() {
        let messages = transcript();
        for (step, (sender, frame)) in messages.iter().enumerate() {
            let wrong_sender = if *sender == Peer::Host {
                Peer::Agent
            } else {
                Peer::Host
            };
            let mut protocol = Protocol::new("boot-a").unwrap();
            advance(&mut protocol, step);
            assert!(
                protocol.accept(wrong_sender, frame).is_err(),
                "direction at step {step}"
            );
            let mut protocol = Protocol::new("boot-a").unwrap();
            advance(&mut protocol, step);
            assert!(
                protocol.accept(Peer::Host, &Frame::Dismount {}).is_err() || step == 12,
                "early dismount at step {step}"
            );
        }
        for (count, sender, frame) in [
            (0, Peer::Host, Frame::Start {}),
            (2, Peer::Host, Frame::Start {}),
            (5, Peer::Agent, Frame::Exit { code: 0 }),
            (
                9,
                Peer::Agent,
                Frame::Output {
                    stream: Stream::Stdout,
                    bytes: vec![],
                },
            ),
            (12, Peer::Host, Frame::Start {}),
            (14, Peer::Agent, hello()),
        ] {
            let mut protocol = Protocol::new("boot-a").unwrap();
            advance(&mut protocol, count);
            assert!(protocol.accept(sender, &frame).is_err());
        }
    }

    #[test]
    fn collection_bound_and_unclean_dismount_fail_closed() {
        let mut protocol = Protocol::new("boot-a").unwrap();
        advance(&mut protocol, 10);
        let chunk = Frame::Collected {
            bytes: vec![0; CHUNK_BYTES],
        };
        for _ in 0..MAX_COLLECT_BYTES / CHUNK_BYTES {
            protocol.accept(Peer::Agent, &chunk).unwrap();
        }
        assert_eq!(protocol.collected_bytes(), MAX_COLLECT_BYTES);
        assert!(
            protocol
                .accept(Peer::Agent, &Frame::Collected { bytes: vec![0] })
                .is_err()
        );
        let mut protocol = Protocol::new("boot-a").unwrap();
        advance(&mut protocol, 13);
        assert!(
            protocol
                .accept(Peer::Agent, &Frame::Dismounted { clean: false })
                .is_err()
        );
    }

    #[test]
    fn session_eof_and_transport_errors_are_terminal() {
        let mut session = Session::new(Peer::Host, "boot-a").unwrap();
        assert!(session.receive(&mut &[][..]).is_err());
        assert_eq!(session.state(), State::Failed);
        let mut session = Session::new(Peer::Agent, "boot-a").unwrap();
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(session.send(&mut Broken, &hello()).is_err());
        assert_eq!(session.state(), State::Failed);
    }

    #[test]
    fn every_invalid_windows_path_rule_is_refused() {
        for name in [
            "..", ".", "", "NUL", "con.rs", "COM1", "lpt9.txt", "COM¹", "x.", "x ", "x:y", "a\\b",
            "a/b", "\u{1}", "x<y", "x>y", "x\"y", "x|y", "x?y", "x*y",
        ] {
            assert!(validate_component(name).is_err(), "accepted {name:?}");
        }
        for name in ["nulled", "cargo-home", "auxiliary.rs", "COM10", "résumé.rs"] {
            validate_component(name).unwrap();
        }
        assert_eq!(
            mapped_cwd("/Users/me/project").unwrap(),
            "C:\\m\\Users\\me\\project"
        );
        for path in ["relative", "/a/../b", "/a//b", "/a/AUX.txt"] {
            assert!(mapped_cwd(path).is_err());
        }
        for path in ["/m/x", "m/../x", "other/x", "m/x\\y", "cargo-home/x:ads"] {
            assert!(validate_archive_path(path).is_err());
        }
    }

    fn base_environment() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("Path".into(), "C:\\Windows\\System32;C:\\Windows\\System32\\WindowsPowerShell\\v1.0;C:\\Windows;C:\\Users\\motor-job\\bin".into()),
            ("PATHEXT".into(), ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC".into()),
            ("APPDATA".into(), "C:\\Users\\motor-job\\AppData\\Roaming".into()),
            ("LOCALAPPDATA".into(), "C:\\Users\\motor-job\\AppData\\Local".into()),
            ("USERNAME".into(), "motor-job".into()),
            ("PSModulePath".into(), "%ProgramFiles%\\WindowsPowerShell\\Modules".into()),
        ])
    }

    #[test]
    fn standard_user_environment_survives_job_overrides() {
        let base = base_environment();
        let env = command()
            .environment(
                "C:\\Users\\motor-job",
                base.clone(),
                "C:\\Program Files\\Git\\ucrt64\\bin",
            )
            .unwrap();
        for (key, value) in base {
            if key != "Path" {
                assert_eq!(env[&key.to_uppercase()], value, "lost base variable {key}");
            }
        }
        assert_eq!(env["TEMP"], "C:\\Users\\motor-job\\AppData\\Local\\Temp");
        assert_eq!(env["TMP"], env["TEMP"]);
        assert_eq!(env["HOME"], "C:\\Users\\motor-job");
        assert_eq!(env["USERPROFILE"], env["HOME"]);
        assert!(!env.contains_key("HOMEDRIVE"));
        assert!(!env.contains_key("HOMEPATH"));
    }

    #[test]
    fn request_overrides_base_but_not_agent_owned_values() {
        let mut base = base_environment();
        base.insert("rust_test_threads".into(), "9".into());
        base.insert("TEMP".into(), "C:\\Windows\\Temp".into());
        let mut command = command();
        command.env = BTreeMap::from([
            ("RUST_TEST_THREADS".into(), "2".into()),
            ("CI".into(), "request-marker".into()),
            ("temp".into(), "C:\\untrusted".into()),
        ]);
        let env = command
            .environment(
                "C:\\Users\\motor-job",
                base,
                "C:\\Program Files\\Git\\ucrt64\\bin",
            )
            .unwrap();
        assert_eq!(env["RUST_TEST_THREADS"], "2");
        assert_eq!(env["CI"], "request-marker");
        assert_eq!(env["TEMP"], "C:\\Users\\motor-job\\AppData\\Local\\Temp");
        assert_eq!(env["CK_REMOTE_SOURCE_COMMIT"], command.source_commit);
    }

    #[test]
    fn defaults_only_fill_missing_merged_values_and_request_duplicates_are_rejected() {
        let mut base = base_environment();
        base.insert("CARGO_BUILD_JOBS".into(), "3".into());
        let mut command = command();
        let env = command
            .environment(
                "C:\\Users\\motor-job",
                base,
                "C:\\Program Files\\Git\\ucrt64\\bin",
            )
            .unwrap();
        assert_eq!(env["CARGO_BUILD_JOBS"], "3");
        assert_eq!(env["NEXTEST_TEST_THREADS"], "4");
        command.env = BTreeMap::from([
            ("RUST_TEST_THREADS".into(), "1".into()),
            ("rust_test_threads".into(), "2".into()),
        ]);
        assert!(
            command
                .environment(
                    "C:\\Users\\motor-job",
                    base_environment(),
                    "C:\\Program Files\\Git\\ucrt64\\bin"
                )
                .is_err()
        );
    }

    #[test]
    fn forced_environment_wins_case_insensitively_and_thread_counts_survive() {
        let mut command = command();
        command.env = BTreeMap::from([
            ("Cargo_Net_Offline".into(), "false".into()),
            ("path".into(), "bad".into()),
            ("rust_test_threads".into(), "1".into()),
        ]);
        let env = command
            .environment(
                "C:\\Users\\motor-job",
                base_environment(),
                "C:\\Program Files\\Git\\mingw64\\bin",
            )
            .unwrap();
        assert_eq!(env["CARGO_NET_OFFLINE"], "true");
        assert_eq!(env["RUST_TEST_THREADS"], "1");
        assert_eq!(env["CARGO_BUILD_JOBS"], "4");
        assert!(!env.contains_key("path"));
        assert!(env["PATH"].starts_with("C:\\rust\\cargo\\bin"));
        for directory in [
            "C:\\Windows\\System32",
            "C:\\Windows\\System32\\WindowsPowerShell\\v1.0",
            "C:\\Windows",
        ] {
            assert!(
                env["PATH"].split(';').any(|entry| entry == directory),
                "missing Windows tools directory {directory}: {}",
                env["PATH"]
            );
        }
    }

    #[test]
    fn job_path_has_powershell_7_before_windows_powershell() {
        let env = command()
            .environment(
                "C:\\Users\\motor-job",
                base_environment(),
                "C:\\Program Files\\Git\\mingw64\\bin",
            )
            .unwrap();
        let paths: Vec<_> = env["PATH"].split(';').collect();
        let pwsh = paths
            .iter()
            .position(|path| *path == "C:\\Program Files\\PowerShell\\7")
            .expect("PowerShell 7 must be available to the job user");
        let legacy = paths
            .iter()
            .position(|path| *path == "C:\\Windows\\System32\\WindowsPowerShell\\v1.0")
            .expect("Windows PowerShell 5.1 must remain available");
        assert!(pwsh < legacy);
    }

    #[test]
    fn job_target_dir_defaults_to_short_cache_mount() {
        let env = command()
            .environment(
                "C:\\Users\\motor-job",
                base_environment(),
                "C:\\Program Files\\Git\\mingw64\\bin",
            )
            .unwrap();
        assert_eq!(
            env.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("C:\\t")
        );
    }

    #[test]
    fn caller_target_dir_wins_case_insensitively() {
        let mut command = command();
        assert_eq!(
            command
                .environment(
                    "C:\\Users\\motor-job",
                    base_environment(),
                    "C:\\Program Files\\Git\\mingw64\\bin"
                )
                .unwrap()
                .get("CARGO_TARGET_DIR")
                .map(String::as_str),
            Some("C:\\t")
        );
        for key in ["CARGO_TARGET_DIR", "cargo_target_dir", "Cargo_Target_Dir"] {
            command.env = BTreeMap::from([(key.into(), "C:\\custom-target".into())]);
            let env = command
                .environment(
                    "C:\\Users\\motor-job",
                    base_environment(),
                    "C:\\Program Files\\Git\\mingw64\\bin",
                )
                .unwrap();
            assert_eq!(env["CARGO_TARGET_DIR"], "C:\\custom-target");
            assert_eq!(
                env.keys()
                    .filter(|key| key.eq_ignore_ascii_case("CARGO_TARGET_DIR"))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn git_source_rewrite_fetches_from_the_packaged_bundle_mirror() {
        use std::{fs, process::Command as ProcessCommand};

        fn git(root: &std::path::Path, config: &std::path::Path, args: &[&str]) {
            let output = ProcessCommand::new("git")
                .current_dir(root)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", config)
                .env("GIT_AUTHOR_NAME", "Motor Test")
                .env("GIT_AUTHOR_EMAIL", "motor@example.invalid")
                .env("GIT_COMMITTER_NAME", "Motor Test")
                .env("GIT_COMMITTER_EMAIL", "motor@example.invalid")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let scratch = Scratch::new();
        let config = scratch.0.join("gitconfig");
        fs::write(&config, "").unwrap();
        let source = scratch.0.join("source");
        fs::create_dir_all(&source).unwrap();
        git(&source, &config, &["init", "--quiet"]);
        fs::write(source.join("dependency.txt"), "from bundled git source\n").unwrap();
        git(&source, &config, &["add", "dependency.txt"]);
        git(&source, &config, &["commit", "--quiet", "-m", "fixture"]);
        let bundle = scratch.0.join("dependency.bundle");
        git(
            &source,
            &config,
            &["bundle", "create", bundle.to_str().unwrap(), "--all"],
        );

        let cargo_home = scratch.0.join("cargo-home");
        let mirror = cargo_home.join("git-mirrors/0");
        fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        git(
            &scratch.0,
            &config,
            &[
                "clone",
                "--bare",
                bundle.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        fs::write(cargo_home.join("gitconfig"), "").unwrap();
        fs::write(
            cargo_home.join("git-rewrites.json"),
            serde_json::to_vec(&serde_json::json!({"schema":1,"routes":[{
                "url":"https://example.invalid/dependency.git",
                "mirror":"git-mirrors/0"
            }]}))
            .unwrap(),
        )
        .unwrap();

        let env = git_rewrite_environment(&cargo_home).unwrap();
        assert_eq!(env["GIT_ALLOW_PROTOCOL"], "file");
        assert_eq!(env["GIT_CONFIG_COUNT"], "1");
        assert_eq!(
            env["GIT_CONFIG_VALUE_0"],
            "https://example.invalid/dependency.git"
        );
        let checkout = scratch.0.join("checkout");
        let output = ProcessCommand::new("git")
            .current_dir(&scratch.0)
            .env("GIT_CONFIG_NOSYSTEM", &env["GIT_CONFIG_NOSYSTEM"])
            .env("GIT_CONFIG_GLOBAL", &env["GIT_CONFIG_GLOBAL"])
            .env("GIT_ALLOW_PROTOCOL", &env["GIT_ALLOW_PROTOCOL"])
            .env("GIT_CONFIG_COUNT", &env["GIT_CONFIG_COUNT"])
            .env("GIT_CONFIG_KEY_0", &env["GIT_CONFIG_KEY_0"])
            .env("GIT_CONFIG_VALUE_0", &env["GIT_CONFIG_VALUE_0"])
            .args([
                "clone",
                "https://example.invalid/dependency.git",
                checkout.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git bundle rewrite: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(checkout.join("dependency.txt")).unwrap(),
            "from bundled git source\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tagged_git_mirror_reaches_cargo_build_offline_locked() {
        use std::{fs, process::Command as ProcessCommand};

        let scratch = Scratch::new();
        let source = scratch.0.join("dependency");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(
            source.join("Cargo.toml"),
            "[package]\nname='tagged-dep'\nversion='1.0.11'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(source.join("src/lib.rs"), "pub fn value() -> u8 { 11 }\n").unwrap();
        let git = |args: &[&str]| {
            let output = ProcessCommand::new("git")
                .current_dir(&source)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@invalid",
            "commit",
            "-qm",
            "dependency",
        ]);
        git(&["tag", "1.0.11"]);
        let rev = git(&["rev-parse", "HEAD"]);
        let cargo_home = scratch.0.join("cargo-home");
        fs::create_dir_all(cargo_home.join("git-mirrors")).unwrap();
        let mirror = cargo_home.join("git-mirrors/0");
        git(&[
            "clone",
            "--bare",
            "--no-local",
            source.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ]);
        fs::write(cargo_home.join("gitconfig"), "[core]\nsymlinks=false\n").unwrap();
        fs::write(
            cargo_home.join("config.toml"),
            "[net]\ngit-fetch-with-cli=true\n",
        )
        .unwrap();
        let url = "https://dep.invalid/tagged-dep";
        fs::write(
            cargo_home.join("git-rewrites.json"),
            serde_json::to_vec(
                &serde_json::json!({"schema":1,"routes":[{"url":url,"mirror":"git-mirrors/0"}]}),
            )
            .unwrap(),
        )
        .unwrap();
        let sources = scratch.0.join("sources with ' quote");
        let workspace = sources.join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("Cargo.toml"), format!("[package]\nname='offline-root'\nversion='0.1.0'\nedition='2021'\n[dependencies]\ntagged-dep={{git='{url}',tag='1.0.11'}}\n")).unwrap();
        fs::write(
            workspace.join("src/lib.rs"),
            "pub fn value() -> u8 { tagged_dep::value() }\n",
        )
        .unwrap();
        let lock = format!(
            "version=4\n[[package]]\nname='offline-root'\nversion='0.1.0'\ndependencies=['tagged-dep']\n[[package]]\nname='tagged-dep'\nversion='1.0.11'\nsource='git+{url}?tag=1.0.11#{rev}'\n"
        );
        fs::write(workspace.join("Cargo.lock"), &lock).unwrap();
        assert!(!cargo_home.join("git/db").exists());
        let script = command_with_git_cache(
            "test \"$CARGO_NET_OFFLINE\" = true && cargo build --offline --locked",
            &cargo_home,
            &sources,
            &inventory(&sources).unwrap(),
        )
        .unwrap();
        let output = ProcessCommand::new("bash")
            .arg("-c")
            .arg(script)
            .current_dir(&workspace)
            .envs(git_rewrite_environment(&cargo_home).unwrap())
            .env("CARGO_HOME", &cargo_home)
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_TARGET_DIR", scratch.0.join("target"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "offline build: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(workspace.join("Cargo.lock")).unwrap(),
            lock
        );
        assert!(cargo_home.join("git/db").is_dir());
        assert!(cargo_home.join("git/checkouts").is_dir());
    }

    #[test]
    fn job_git_rewrite_urls_accept_runner_schemes_and_refuse_passwords() {
        // The guest rewrite is only an identifier: accept the same schemes as
        // the runner, including an ssh username such as `git@`.
        for url in [
            "https://example.invalid/dependency.git",
            "http://example.invalid/dependency.git",
            "ssh://git@example.invalid/dependency.git",
            "ssh://example.invalid/dependency.git",
            "git://example.invalid/dependency.git",
            "https://user@example.invalid/dependency.git",
        ] {
            assert!(
                job_git_rewrite_url_allowed(url),
                "job rewrite refused bundled source: {url}"
            );
        }
        for url in [
            "file:///tmp/dependency.git",
            "git@example.invalid:dependency.git",
            "https://user:secret@example.invalid/dependency.git",
            "ssh://user:secret@example.invalid/dependency.git",
            "https://example.invalid/dependency\u{7}.git",
        ] {
            assert!(
                !job_git_rewrite_url_allowed(url),
                "job rewrite accepted forbidden source: {url:?}"
            );
        }
    }

    #[test]
    fn cache_grant_requires_exact_explicit_inheritable_job_modify_ace() {
        let job_sid = vec![1, 1, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0];
        let exact = CacheAce {
            allow: true,
            mask: 0x0013_01bf,
            flags: 0x03,
            sid: job_sid.clone(),
        };
        assert!(!cache_has_modify_grant(&[], &job_sid));
        assert!(cache_has_modify_grant(
            std::slice::from_ref(&exact),
            &job_sid
        ));
        for changed in [
            CacheAce {
                allow: false,
                ..exact.clone()
            },
            CacheAce {
                mask: 0x001f_01ff,
                ..exact.clone()
            },
            CacheAce {
                mask: 0x0012_0089,
                ..exact.clone()
            },
            CacheAce {
                flags: 0x01,
                ..exact.clone()
            },
            CacheAce {
                flags: 0x02,
                ..exact.clone()
            },
            CacheAce {
                flags: 0x0b,
                ..exact.clone()
            },
            CacheAce {
                flags: 0x13,
                ..exact.clone()
            },
            CacheAce {
                flags: 0x07,
                ..exact.clone()
            },
            CacheAce {
                sid: vec![1, 1, 0, 0, 0, 0, 0, 5, 22, 0, 0, 0],
                ..exact.clone()
            },
        ] {
            assert!(
                !cache_has_modify_grant(std::slice::from_ref(&changed), &job_sid),
                "accepted {changed:?}"
            );
            assert!(cache_has_modify_grant(&[changed, exact.clone()], &job_sid));
        }
    }

    fn archive(members: &[(&str, tar::EntryType)]) -> Cursor<Vec<u8>> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(if kind.is_file() { 1 } else { 0 });
            header.set_mode(0o644);
            header.set_entry_type(*kind);
            // Raw names let this fixture exercise paths the tar builder normally rejects.
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder
                .append(&header, if kind.is_file() { &b"x"[..] } else { &[][..] })
                .unwrap();
        }
        Cursor::new(builder.into_inner().unwrap())
    }

    #[test]
    fn tar_safety_checks_paths_links_duplicates_and_parent_case_collisions() {
        for members in [
            vec![("m/a", tar::EntryType::Symlink)],
            vec![("m/a", tar::EntryType::Link)],
            vec![("m/../outside", tar::EntryType::Regular)],
            vec![("/m/a", tar::EntryType::Regular)],
            vec![("other/a", tar::EntryType::Regular)],
            vec![
                ("m/a/x", tar::EntryType::Regular),
                ("m/A/y", tar::EntryType::Regular),
            ],
            vec![
                ("m/a", tar::EntryType::Regular),
                ("m/a", tar::EntryType::Regular),
            ],
        ] {
            assert!(
                validate_tree(&mut archive(&members)).is_err(),
                "accepted {members:?}"
            );
        }
        validate_tree(&mut archive(&[
            ("m/a", tar::EntryType::Regular),
            ("cargo-home/registry/index/a", tar::EntryType::Regular),
        ]))
        .unwrap();
    }

    #[test]
    fn deadlines_are_anchored_reset_only_for_chunks_and_allow_silent_commands() {
        let mut bounds = Bounds::new(100);
        bounds.check(120_099).unwrap();
        assert_eq!(
            bounds.check(120_100).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(bounds.enter(Phase::Hello, 101).is_err());
        for phase in [Phase::Transfer, Phase::Collect] {
            let mut bounds = Bounds::new(0);
            bounds.enter(phase, 1_000).unwrap();
            bounds.received(60_999).unwrap();
            assert_eq!(bounds.deadline_ms(), Some(120_999));
            assert!(bounds.received(120_999).is_err());
        }
        for phase in [
            Phase::StartAcknowledgement,
            Phase::Dismount,
            Phase::UnitExit,
        ] {
            let mut bounds = Bounds::new(0);
            bounds.enter(phase, 1_000).unwrap();
            bounds.received(60_000).unwrap();
            assert_eq!(bounds.deadline_ms(), Some(61_000));
            assert!(bounds.check(61_000).is_err());
        }
        let mut bounds = Bounds::new(0);
        bounds.enter(Phase::Running, 1_000).unwrap();
        bounds.check(u64::MAX).unwrap();
        assert_eq!(bounds.deadline_ms(), None);
    }

    #[test]
    fn windows_argument_quoting_preserves_quotes_and_trailing_backslashes() {
        assert_eq!(quote_windows_arg("simple"), "\"simple\"");
        assert_eq!(quote_windows_arg("a b"), "\"a b\"");
        assert_eq!(quote_windows_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_windows_arg("a\\"), "\"a\\\\\"");
    }

    #[test]
    fn collection_builder_rejects_oversized_output() {
        let scratch = Scratch::new();
        std::fs::write(scratch.0.join("large"), vec![1; MAX_COLLECT_BYTES]).unwrap();
        assert!(
            collect_changes(&scratch.0, &Inventory::new())
                .unwrap_err()
                .to_string()
                .contains("16 MiB")
        );
    }

    #[cfg(unix)]
    #[test]
    fn extraction_and_inventory_never_follow_symlinks() {
        let scratch = Scratch::new();
        let outside = scratch.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let source = scratch.0.join("m");
        let home = scratch.0.join("cargo-home");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::os::unix::fs::symlink(&outside, source.join("escape")).unwrap();
        assert!(
            unpack_tree(
                &mut archive(&[("m/escape/touched", tar::EntryType::Regular)]),
                &source,
                &home
            )
            .is_err()
        );
        assert!(!outside.join("touched").exists());
        assert_eq!(inventory(&source).unwrap().len(), 1);
        assert!(inventory(&source.join("escape")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn collection_returns_file_directory_and_dangling_links_without_target_bytes() {
        use std::os::unix::fs::symlink;
        let scratch = Scratch::new();
        let source = scratch.0.join("m");
        let outside = scratch.0.join("outside");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), "outside target bytes").unwrap();
        let links = [
            ("file", outside.join("secret")),
            ("directory", outside.clone()),
            ("dangling", outside.join("absent")),
            ("unc", std::path::PathBuf::from(r"\\server\share\absent")),
        ];
        for (name, target) in &links {
            symlink(target, source.join(name)).unwrap();
        }
        assert!(open_source_file(&source.join("file")).is_err());
        assert!(source_directory(&source.join("directory")).is_err());
        let (bytes, deleted) = collect_changes(&source, &Inventory::new()).unwrap();
        assert!(deleted.is_empty());
        validate_collection(&mut Cursor::new(&bytes)).unwrap();
        assert!(validate_tree(&mut Cursor::new(&bytes)).is_err());
        let mut found = BTreeMap::new();
        for entry in tar::Archive::new(&bytes[..]).entries().unwrap() {
            let mut entry = entry.unwrap();
            assert!(entry.header().entry_type().is_symlink());
            let mut payload = Vec::new();
            entry.read_to_end(&mut payload).unwrap();
            assert!(payload.is_empty());
            found.insert(
                entry.path().unwrap().into_owned(),
                entry.link_name().unwrap().unwrap().into_owned(),
            );
        }
        assert_eq!(found.len(), links.len());
        for (name, target) in links {
            assert_eq!(
                found[&std::path::PathBuf::from(format!("m/{name}"))],
                target
            );
        }
        let before = inventory(&source).unwrap();
        std::fs::write(outside.join("secret"), "changed target bytes").unwrap();
        assert_eq!(inventory(&source).unwrap(), before);
        std::fs::remove_file(source.join("file")).unwrap();
        std::fs::write(
            source.join("file"),
            outside.join("secret").to_str().unwrap(),
        )
        .unwrap();
        assert_ne!(inventory(&source).unwrap()["m/file"], before["m/file"]);
    }

    #[test]
    fn collection_validation_does_not_admit_hardlinks_or_non_source_paths() {
        for members in [
            vec![("m/a", tar::EntryType::Link)],
            vec![("cargo-home/a", tar::EntryType::Regular)],
            vec![("m/../outside", tar::EntryType::Regular)],
        ] {
            assert!(validate_collection(&mut archive(&members)).is_err());
        }
    }

    #[test]
    fn collected_link_text_preserves_windows_absolute_unc_and_long_targets() {
        for target in [
            r"C:\Windows\System32".to_owned(),
            r"\\server\share\file".to_owned(),
            format!(r"C:\{}\file", "long".repeat(80)),
        ] {
            let mut builder = tar::Builder::new(Vec::new());
            append_source_link(&mut builder, "m/link", &target).unwrap();
            let bytes = builder.into_inner().unwrap();
            validate_collection(&mut Cursor::new(&bytes)).unwrap();
            let mut archive = tar::Archive::new(&bytes[..]);
            let entry = archive.entries().unwrap().next().unwrap().unwrap();
            assert_eq!(entry.link_name_bytes().unwrap().as_ref(), target.as_bytes());
            assert!(entry.header().entry_type().is_symlink());
        }
    }

    pub(super) struct Scratch(pub(super) std::path::PathBuf);
    impl Scratch {
        pub(super) fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/test-scratch")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn extraction_and_collection_include_only_changed_sources() {
        let scratch = Scratch::new();
        let source = scratch.0.join("m");
        let home = scratch.0.join("cargo-home");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&home).unwrap();
        unpack_tree(
            &mut archive(&[
                ("m/work/project/a.rs", tar::EntryType::Regular),
                ("m/work/project/gone.rs", tar::EntryType::Regular),
                ("cargo-home/registry/index/a", tar::EntryType::Regular),
            ]),
            &source,
            &home,
        )
        .unwrap();
        let before = inventory(&source).unwrap();
        std::fs::write(source.join("work/project/a.rs"), "changed").unwrap();
        std::fs::remove_file(source.join("work/project/gone.rs")).unwrap();
        std::fs::create_dir(source.join("work/project/target")).unwrap();
        std::fs::write(source.join("work/project/target/ignored"), "artifact").unwrap();
        let (bytes, deleted) = collect_changes(&source, &before).unwrap();
        assert_eq!(deleted, vec!["m/work/project/gone.rs"]);
        let mut archive = tar::Archive::new(&bytes[..]);
        let members = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            members,
            vec![std::path::PathBuf::from("m/work/project/a.rs")]
        );
    }
}

/// Quote one argument for Windows' command-line parser, including trailing
/// backslashes before the closing quote. Git Bash receives the command as one
/// `-c` argument rather than letting Windows parse pieces of the shell program.
pub fn quote_windows_arg(argument: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in argument.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        result.push(c);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Hello,
    Transfer,
    BunPreparation,
    StartAcknowledgement,
    Running,
    Collect,
    Dismount,
    UnitExit,
    Finished,
}

pub const HELLO_TIMEOUT_MS: u64 = 120_000;
pub const REPLY_TIMEOUT_MS: u64 = 60_000;

/// Host-side deadlines use a monotonic clock supplied by the caller. A fake
/// clock can exercise stalled peers without sleeping or booting a VM.
#[derive(Debug)]
pub struct Bounds {
    phase: Phase,
    deadline_ms: Option<u64>,
}
impl Bounds {
    pub fn new(unit_started_ms: u64) -> Self {
        Self {
            phase: Phase::Hello,
            deadline_ms: Some(unit_started_ms.saturating_add(HELLO_TIMEOUT_MS)),
        }
    }
    pub fn enter(&mut self, phase: Phase, now_ms: u64) -> io::Result<()> {
        self.check(now_ms)?;
        if phase == Phase::Hello {
            return Err(invalid("hello deadline is anchored to the unit start"));
        }
        self.phase = phase;
        self.deadline_ms = match phase {
            Phase::Running | Phase::Finished => None,
            // Offline extraction/installation is a bounded preparation phase,
            // not transport inactivity. Guest messages cannot extend this budget.
            Phase::BunPreparation => Some(now_ms.saturating_add(600_000)),
            _ => Some(now_ms.saturating_add(REPLY_TIMEOUT_MS)),
        };
        Ok(())
    }
    pub fn received(&mut self, now_ms: u64) -> io::Result<()> {
        self.check(now_ms)?;
        if matches!(self.phase, Phase::Transfer | Phase::Collect) {
            self.deadline_ms = Some(now_ms.saturating_add(REPLY_TIMEOUT_MS));
        }
        Ok(())
    }
    pub fn check(&self, now_ms: u64) -> io::Result<()> {
        if self.deadline_ms.is_some_and(|deadline| now_ms >= deadline) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("agent deadline exceeded in {:?}", self.phase),
            ));
        }
        Ok(())
    }
    pub fn deadline_ms(&self) -> Option<u64> {
        self.deadline_ms
    }
}
