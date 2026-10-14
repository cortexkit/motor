//! The shared host/guest protocol for a single disposable Windows job VM.
//!
//! Frames are JSON preceded by a four-byte big-endian payload length. Both peers
//! must validate each incoming and outgoing frame with the same state machine.
//! Transport deadlines, persistence of Started, and VM cleanup belong to the host.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_COLLECT_BYTES: usize = 16 * 1024 * 1024;

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

/// Tree and collection payloads are chunks of tar archives, not guest paths.
/// Chunk sizes must account for JSON encoding overhead within the frame cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Hello { version: u32, nonce: String },
    ReceiveTree { bytes: Vec<u8> },
    TreeEnd { command: Command },
    TreeReceived {},
    Start {},
    Started {},
    Output { stream: Stream, bytes: Vec<u8> },
    Exit { code: u32 },
    Cancel {},
    Collect {},
    Collected { bytes: Vec<u8> },
    CollectEnd { deleted: Vec<String> },
    Dismount {},
    Dismounted { clean: bool },
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    AwaitHello,
    ReceivingTree,
    AwaitTreeReceived,
    Ready,
    AwaitStarted,
    Running,
    Exited,
    Collecting,
    CollectionComplete,
    AwaitDismounted,
    Complete,
    Failed,
}

/// Tracks both directions of one boot's conversation. Any rejected message
/// poisons the conversation: neither peer may resume or start a second command.
#[derive(Debug)]
pub struct Protocol {
    nonce: String,
    state: State,
    collected_bytes: usize,
}

impl Protocol {
    pub fn new(nonce: impl Into<String>) -> io::Result<Self> {
        let nonce = nonce.into();
        if nonce.is_empty() {
            return Err(invalid("boot nonce must not be empty"));
        }
        Ok(Self {
            nonce,
            state: State::AwaitHello,
            collected_bytes: 0,
        })
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
        match self.transition(sender, frame) {
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

    fn transition(&mut self, sender: Peer, frame: &Frame) -> io::Result<State> {
        use Frame::*;
        use Peer::{Agent, Host};
        use State::*;

        let next = match (self.state, sender, frame) {
            (AwaitHello, Agent, Hello { version, nonce }) => {
                if *version != PROTOCOL_VERSION {
                    return Err(invalid(format!(
                        "unsupported agent protocol version {version}"
                    )));
                }
                if nonce != &self.nonce {
                    return Err(invalid("agent hello boot nonce mismatch"));
                }
                ReceivingTree
            }
            (ReceivingTree, Host, ReceiveTree { .. }) => ReceivingTree,
            (ReceivingTree, Host, TreeEnd { command }) => {
                command.validate()?;
                AwaitTreeReceived
            }
            (AwaitTreeReceived, Agent, TreeReceived {}) => Ready,
            (Ready, Host, Start {}) => AwaitStarted,
            (AwaitStarted, Agent, Started {}) => Running,
            (AwaitStarted | Running, Host, Cancel {}) => self.state,
            (Running, Agent, Output { .. }) => Running,
            (Running, Agent, Exit { .. }) => Exited,
            (Exited, Host, Collect {}) => Collecting,
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
    pub fn new(peer: Peer, nonce: impl Into<String>) -> io::Result<Self> {
        Ok(Self {
            peer,
            protocol: Protocol::new(nonce)?,
        })
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
                "agent session closed before dismount",
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
    if !matches!(components.next(), Some("m" | "cargo-home")) {
        return Err(invalid(format!(
            "archive path must be under m/ or cargo-home/: {path:?}"
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

    /// Windows environment keys are case-insensitive; discard case variants of
    /// protected keys before setting the agent-owned values.
    pub fn environment(&self, profile: &str) -> io::Result<BTreeMap<String, String>> {
        self.validate()?;
        let forced = [
            (
                "PATH",
                "C:\\rust\\cargo\\bin;C:\\Program Files\\Git\\cmd;C:\\Program Files\\Git\\usr\\bin;C:\\Windows\\System32;C:\\Windows\\System32\\WindowsPowerShell\\v1.0;C:\\Windows"
                    .to_string(),
            ),
            ("RUSTUP_HOME", "C:\\rust\\rustup".into()),
            ("HOME", profile.into()),
            ("USERPROFILE", profile.into()),
            ("CARGO_HOME", format!("{profile}\\.cargo")),
            ("TMP", format!("{profile}\\AppData\\Local\\Temp")),
            ("TEMP", format!("{profile}\\AppData\\Local\\Temp")),
            ("CARGO_NET_OFFLINE", "true".into()),
            ("CK_REMOTE_SOURCE_COMMIT", self.source_commit.clone()),
            ("SystemRoot", "C:\\Windows".into()),
            ("SystemDrive", "C:".into()),
        ];
        let mut env = BTreeMap::new();
        for (key, value) in &self.env {
            if !forced
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(key))
            {
                let canonical = key.to_uppercase();
                if env.insert(canonical, value.clone()).is_some() {
                    return Err(invalid(format!(
                        "duplicate case-insensitive environment key: {key}"
                    )));
                }
            }
        }
        for (key, value) in forced {
            env.insert(key.into(), value);
        }
        for key in [
            "CARGO_BUILD_JOBS",
            "RUST_TEST_THREADS",
            "NEXTEST_TEST_THREADS",
        ] {
            env.entry(key.into()).or_insert_with(|| "4".into());
        }
        Ok(env)
    }
}

/// Preflight the complete archive before writing any files. Directory prefixes
/// participate in collision detection, so `a/x` and `A/y` cannot merge silently.
pub fn validate_tree(reader: &mut (impl Read + io::Seek)) -> io::Result<()> {
    reader.rewind()?;
    let mut archive = tar::Archive::new(&mut *reader);
    let mut names = BTreeMap::<String, String>::new();
    let mut entries = std::collections::BTreeSet::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let bytes = entry.path_bytes();
        let path = std::str::from_utf8(&bytes).map_err(|_| invalid("tar path is not UTF-8"))?;
        if !entry.header().entry_type().is_file() && !entry.header().entry_type().is_dir() {
            return Err(invalid(format!(
                "tar links and non-file members are forbidden: {path}"
            )));
        }
        validate_archive_path(path)?;
        let path = path.trim_end_matches('/');
        if !entry.header().entry_type().is_dir() && matches!(path, "m" | "cargo-home") {
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
    reader.rewind()
}

fn safe_directory(path: &std::path::Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(invalid(format!(
            "extraction directory is not a real directory: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::fs::create_dir(path),
        Err(error) => Err(error),
    }
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

pub type Inventory = BTreeMap<String, [u8; 32]>;

/// Target directories live on the cache disk and must never enter collected
/// source changes. Reject generated links instead of following them out of m/.
pub fn inventory(root: &std::path::Path) -> io::Result<Inventory> {
    use sha2::{Digest, Sha256};
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        files: &mut Inventory,
    ) -> io::Result<()> {
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
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(invalid(format!(
                    "source symlink is forbidden: {}",
                    path.display()
                )));
            }
            if kind.is_dir() {
                walk(root, &path, files)?;
            } else if kind.is_file() {
                let mut file = std::fs::File::open(&path)?;
                let mut digest = Sha256::new();
                io::copy(&mut file, &mut digest)?;
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("/");
                files.insert(format!("m/{relative}"), digest.finalize().into());
            } else {
                return Err(invalid(format!("non-file source path: {}", path.display())));
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
pub fn collect_changes(
    root: &std::path::Path,
    before: &Inventory,
) -> io::Result<(Vec<u8>, Vec<String>)> {
    struct CollectBuffer(Vec<u8>);
    impl Write for CollectBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_COLLECT_BYTES - self.0.len() {
                return Err(invalid("collect output exceeds 16 MiB"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let after = inventory(root)?;
    let mut builder = tar::Builder::new(CollectBuffer(Vec::new()));
    for (path, hash) in &after {
        if before.get(path) != Some(hash) {
            builder.append_path_with_name(root.join(path.strip_prefix("m/").unwrap()), path)?;
        }
    }
    let deleted = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .cloned()
        .collect();
    Ok((builder.into_inner()?.0, deleted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
                        version: 1,
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

    #[test]
    fn forced_environment_wins_case_insensitively_and_thread_counts_survive() {
        let mut command = command();
        command.env = BTreeMap::from([
            ("Cargo_Net_Offline".into(), "false".into()),
            ("path".into(), "bad".into()),
            ("rust_test_threads".into(), "1".into()),
        ]);
        let env = command.environment("C:\\Users\\motor-job").unwrap();
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
        assert!(inventory(&source).is_err());
    }

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
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
