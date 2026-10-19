//! Operator-only named-pipe probe. The production Linux host never invokes this.
use motor_winagent::{Command, Frame, Peer, Session, Stream};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io, time::Instant};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn trace(event: &str, detail: serde_json::Value) {
    let unix_ms = unix_ms();
    eprintln!(
        "EVIDENCE transport {}",
        serde_json::json!({"event": event, "unix_ms": unix_ms, "detail": detail})
    );
}

struct TracedIo<T> {
    inner: T,
    read: usize,
    written: usize,
    first_com_byte_unix_ms: Option<u128>,
}
impl<T: io::Read> io::Read for TracedIo<T> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        trace(
            "read-start",
            serde_json::json!({"requested":bytes.len(), "total":self.read}),
        );
        let started = Instant::now();
        let result = self.inner.read(bytes);
        match &result {
            Ok(count) => {
                if *count > 0 && self.first_com_byte_unix_ms.is_none() {
                    let first = unix_ms();
                    self.first_com_byte_unix_ms = Some(first);
                    trace(
                        "first-com-byte",
                        serde_json::json!({"first_com_byte_unix_ms":first}),
                    );
                }
                self.read += count;
                trace(
                    "read-done",
                    serde_json::json!({"bytes":count,"total":self.read,"elapsed_ms":started.elapsed().as_millis()}),
                );
            }
            Err(error) => trace(
                "read-error",
                serde_json::json!({"error":error.to_string(),"os_code":error.raw_os_error()}),
            ),
        }
        result
    }
}
impl<T: io::Write> io::Write for TracedIo<T> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        trace(
            "write-start",
            serde_json::json!({"requested":bytes.len(),"total":self.written}),
        );
        let started = Instant::now();
        let result = self.inner.write(bytes);
        match &result {
            Ok(count) => {
                self.written += count;
                trace(
                    "write-done",
                    serde_json::json!({"bytes":count,"total":self.written,"elapsed_ms":started.elapsed().as_millis()}),
                );
            }
            Err(error) => trace(
                "write-error",
                serde_json::json!({"error":error.to_string(),"os_code":error.raw_os_error()}),
            ),
        }
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        trace("flush-start", serde_json::json!({"written":self.written}));
        let result = self.inner.flush();
        trace("flush-done", serde_json::json!({"ok":result.is_ok()}));
        result
    }
}

/// How the host reaches the guest agent: the named pipe Hyper-V maps to the
/// VM's COM1, or a Hyper-V socket to the VM's ID and Motor's service ID.
#[derive(Debug, PartialEq)]
enum Channel {
    Pipe(String),
    HvSock(u128),
}

#[derive(Debug, PartialEq)]
struct Args {
    channel: Channel,
    nonce: String,
    canary: String,
    host: String,
    result: String,
    stress_bytes: usize,
}

fn arguments(values: &[String]) -> io::Result<Args> {
    if values.len() != 10 && values.len() != 12 {
        return Err(invalid(
            "expected --pipe NAME|--vm-id GUID --nonce UUID --canary PATH --host NAME --result PATH",
        ));
    }
    let mut pairs = BTreeMap::new();
    for pair in values.as_chunks::<2>().0 {
        if pairs.insert(pair[0].as_str(), pair[1].clone()).is_some() {
            return Err(invalid("duplicate option"));
        }
    }
    let stress_bytes = match pairs.remove("--stress-bytes").as_deref() {
        None | Some("0") => 0,
        Some("65536") => 65536,
        _ => return Err(invalid("stress bytes must be 0 or 65536")),
    };
    let channel = match (pairs.remove("--pipe"), pairs.remove("--vm-id")) {
        (Some(pipe), None) => {
            if !pipe.starts_with("motor-hv-")
                || !pipe.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(invalid("unsafe pipe name"));
            }
            Channel::Pipe(pipe)
        }
        (None, Some(vm_id)) => Channel::HvSock(motor_winagent::transport::parse_guid(&vm_id)?),
        _ => return Err(invalid("exactly one of --pipe or --vm-id is required")),
    };
    let mut take = |key| {
        pairs
            .remove(key)
            .ok_or_else(|| invalid(format!("missing {key}")))
    };
    let args = Args {
        channel,
        nonce: take("--nonce")?,
        canary: take("--canary")?,
        host: take("--host")?,
        result: take("--result")?,
        stress_bytes,
    };
    if !pairs.is_empty() {
        return Err(invalid("unknown option"));
    }
    motor_winagent::transport::hyperv_nonce(&args.nonce)?;
    if !args.canary.starts_with("C:\\")
        || args.canary.contains(['\r', '\n', '\0'])
        || args.result.is_empty()
        || args.host.is_empty()
        || !args
            .host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(invalid("unsafe host/canary/result"));
    }
    Ok(args)
}

fn ps_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn isolation_script(args: &Args) -> String {
    let unc = format!("\\\\{}\\C$\\{}", args.host, &args.canary[3..]);
    include_str!("isolation.ps1")
        .replace("__CANARY__", &ps_literal(&args.canary))
        .replace("__UNC__", &ps_literal(&unc))
}

fn tree(args: &Args) -> io::Result<Vec<u8>> {
    let script = isolation_script(args);
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "m/spike/check.ps1", script.as_bytes())?;
    if args.stress_bytes != 0 {
        let padding = vec![0u8; args.stress_bytes];
        let mut header = tar::Header::new_gnu();
        header.set_size(padding.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "m/spike/serial-stress.bin", padding.as_slice())?;
    }
    tar.into_inner()
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Row {
    create_ms: f64,
    boot_hello_ms: f64,
    command_ms: f64,
    teardown_ms: f64,
    start_vm_call_ms: f64,
    start_to_first_byte_ms: f64,
    first_byte_to_hello_ms: f64,
}

fn summary(rows: &[Row]) -> io::Result<serde_json::Value> {
    if rows.len() != 5 {
        return Err(invalid(
            "five successful runs required; skips are not samples",
        ));
    }
    let mut result = serde_json::Map::new();
    for (name, values) in [
        (
            "create_ms",
            rows.iter().map(|r| r.create_ms).collect::<Vec<_>>(),
        ),
        (
            "boot_hello_ms",
            rows.iter().map(|r| r.boot_hello_ms).collect(),
        ),
        ("command_ms", rows.iter().map(|r| r.command_ms).collect()),
        ("teardown_ms", rows.iter().map(|r| r.teardown_ms).collect()),
        (
            "start_vm_call_ms",
            rows.iter().map(|r| r.start_vm_call_ms).collect(),
        ),
        (
            "start_to_first_byte_ms",
            rows.iter().map(|r| r.start_to_first_byte_ms).collect(),
        ),
        (
            "first_byte_to_hello_ms",
            rows.iter().map(|r| r.first_byte_to_hello_ms).collect(),
        ),
    ] {
        if values.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return Err(invalid("invalid duration"));
        }
        let mut sorted = values.clone();
        sorted.sort_by(f64::total_cmp);
        result.insert(
            name.into(),
            serde_json::json!({"raw": values, "median": sorted[2]}),
        );
    }
    Ok(serde_json::Value::Object(result))
}

#[derive(Serialize)]
struct Probe {
    hello_unix_ms: u128,
    first_com_byte_unix_ms: u128,
    command_ms: f64,
    command_start_unix_ms: u128,
    command_exit_unix_ms: u128,
    protocol_complete_unix_ms: u128,
}

fn conversation<T: io::Read + io::Write>(pipe: T, args: &Args) -> io::Result<Probe> {
    let mut pipe = TracedIo {
        inner: pipe,
        read: 0,
        written: 0,
        first_com_byte_unix_ms: None,
    };
    let mut session = Session::new(Peer::Host, &args.nonce)?;
    let receive = |session: &mut Session, pipe: &mut _| -> io::Result<Frame> {
        trace(
            "await-frame",
            serde_json::json!({"state":format!("{:?}",session.state())}),
        );
        let frame = session
            .receive(pipe)?
            .ok_or_else(|| invalid("unexpected EOF"))?;
        trace(
            "received-frame",
            serde_json::json!({"state":format!("{:?}",session.state())}),
        );
        Ok(frame)
    };
    match receive(&mut session, &mut pipe)? {
        frame @ Frame::Hello { .. } => println!("PASS Hello {}", serde_json::to_string(&frame)?),
        frame => return Err(invalid(format!("expected Hello: {frame:?}"))),
    }
    let hello_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    trace(
        "hello-validated",
        serde_json::json!({"hello_unix_ms":hello_unix_ms}),
    );
    loop {
        match receive(&mut session, &mut pipe)? {
            Frame::CacheMounted { timings } => {
                println!(
                    "PASS disposable-scratch {}",
                    serde_json::to_string(&timings)?
                );
                break;
            }
            Frame::CacheProgress { step, elapsed_ms } => {
                println!("EVIDENCE cache {step:?} {elapsed_ms}ms")
            }
            frame => return Err(invalid(format!("cache preparation failed: {frame:?}"))),
        }
    }
    trace("tree-build-start", serde_json::json!({}));
    let bytes = tree(args)?;
    use sha2::Digest;
    trace(
        "tree-built",
        serde_json::json!({"bytes":bytes.len(),"sha256":format!("{:x}",sha2::Sha256::digest(&bytes))}),
    );
    trace(
        "send-receive-tree-start",
        serde_json::json!({"bytes":bytes.len()}),
    );
    session.send(&mut pipe, &Frame::ReceiveTree { bytes })?;
    trace("send-receive-tree-done", serde_json::json!({}));
    trace("send-tree-end-start", serde_json::json!({}));
    session.send(
        &mut pipe,
        &Frame::TreeEnd {
            command: Command {
                command: "pwsh.exe -NoProfile -NonInteractive -File C:/m/spike/check.ps1".into(),
                cwd: "/spike".into(),
                workspace: "/spike".into(),
                env: BTreeMap::new(),
                source_commit: "hyperv-spike".into(),
            },
        },
    )?;
    trace("send-tree-end-done", serde_json::json!({}));
    if receive(&mut session, &mut pipe)? != (Frame::TreeReceived {}) {
        return Err(invalid("expected tree_received"));
    }
    let command = Instant::now();
    let command_start_unix_ms = unix_ms();
    trace("command-start", serde_json::json!({}));
    session.send(&mut pipe, &Frame::Start {})?;
    if receive(&mut session, &mut pipe)? != (Frame::Started {}) {
        return Err(invalid("expected started"));
    }
    let mut stdout = Vec::new();
    loop {
        match receive(&mut session, &mut pipe)? {
            Frame::Output { stream, bytes } => {
                if stdout.len() + bytes.len() > motor_winagent::MAX_COLLECT_BYTES {
                    return Err(invalid("excess command output"));
                }
                println!("EVIDENCE {stream:?} {}", String::from_utf8_lossy(&bytes));
                if stream == Stream::Stdout {
                    stdout.extend(bytes);
                }
            }
            Frame::Exit { code: 0 } => break,
            frame => return Err(invalid(format!("command did not succeed: {frame:?}"))),
        }
    }
    let command_ms = command.elapsed().as_secs_f64() * 1000.0;
    let command_exit_unix_ms = unix_ms();
    trace("command-exit", serde_json::json!({"command_ms":command_ms}));
    let stdout = String::from_utf8(stdout).map_err(io::Error::other)?;
    for required in [
        "PASS standard-account",
        "PASS no-network-route",
        "PASS host-files-inaccessible",
        "PASS trivial-command 4",
    ] {
        if !stdout.lines().any(|line| line.trim() == required) {
            return Err(invalid(format!("missing {required}")));
        }
    }
    session.send(&mut pipe, &Frame::Collect {})?;
    loop {
        match receive(&mut session, &mut pipe)? {
            Frame::Collected { .. } => (),
            Frame::CollectEnd { .. } => break,
            frame => return Err(invalid(format!("unexpected collection: {frame:?}"))),
        }
    }
    session.send(&mut pipe, &Frame::Dismount {})?;
    if receive(&mut session, &mut pipe)? != (Frame::Dismounted { clean: true }) {
        return Err(invalid("unclean dismount"));
    }
    println!("PASS protocol-complete");
    let protocol_complete_unix_ms = unix_ms();
    Ok(Probe {
        hello_unix_ms,
        first_com_byte_unix_ms: pipe
            .first_com_byte_unix_ms
            .ok_or_else(|| invalid("Hello without a first byte"))?,
        command_ms,
        command_start_unix_ms,
        command_exit_unix_ms,
        protocol_complete_unix_ms,
    })
}

#[cfg(windows)]
fn probe(args: &Args) -> io::Result<Probe> {
    use std::os::windows::fs::OpenOptionsExt;
    let pipe_name = match &args.channel {
        Channel::Pipe(name) => name,
        Channel::HvSock(vm_id) => return conversation(hvsock::dial(*vm_id)?, args),
    };
    let path = format!(r"\\.\pipe\{pipe_name}");
    let deadline = Instant::now() + std::time::Duration::from_secs(120);
    trace("pipe-connect-start", serde_json::json!({"pipe":pipe_name}));
    let pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
        {
            Ok(pipe) => break pipe,
            Err(error)
                if matches!(error.raw_os_error(), Some(2 | 231)) && Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            Err(error) => return Err(error),
        }
    };
    trace("pipe-connected", serde_json::json!({"pipe":pipe_name}));
    conversation(pipe, args)
}

#[cfg(not(windows))]
fn probe(args: &Args) -> io::Result<Probe> {
    // Keep the protocol client typechecked on portable hosts without pretending
    // that an in-memory stream proves Hyper-V connectivity.
    let _ = conversation::<io::Cursor<Vec<u8>>>;
    let channel = match &args.channel {
        Channel::Pipe(name) => format!("pipe {name}"),
        Channel::HvSock(vm_id) => format!("hvsock {vm_id:032x}"),
    };
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("measurement over {channel} requires Windows Hyper-V"),
    ))
}

/// Host side of the Hyper-V socket: dial the job VM's ID on Motor's service ID.
#[cfg(windows)]
mod hvsock {
    use super::trace;
    use std::{
        collections::BTreeSet,
        io,
        mem::{size_of, zeroed},
        time::{Duration, Instant},
    };
    use windows_sys::{
        Win32::{
            Networking::WinSock::{
                AF_HYPERV, INVALID_SOCKET, SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ERROR, WSADATA,
                WSAGetLastError, WSAStartup, closesocket, connect, recv, send, socket,
            },
            System::Hypervisor::{HV_PROTOCOL_RAW, SOCKADDR_HV},
        },
        core::GUID,
    };

    pub struct Stream(SOCKET);
    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe {
                closesocket(self.0);
            }
        }
    }
    impl io::Read for Stream {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let length = bytes.len().min(i32::MAX as usize) as i32;
            match unsafe { recv(self.0, bytes.as_mut_ptr(), length, 0) } {
                SOCKET_ERROR => Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() })),
                count => Ok(count as usize),
            }
        }
    }
    impl io::Write for Stream {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let length = bytes.len().min(i32::MAX as usize) as i32;
            match unsafe { send(self.0, bytes.as_ptr(), length, 0) } {
                SOCKET_ERROR => Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() })),
                count => Ok(count as usize),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Retry until the agent's listener is up or 120 s pass (the same budget as
    /// the named-pipe path). Every distinct error code is traced once, so a
    /// permanent refusal such as a missing service registration stays visible.
    pub fn dial(vm_id: u128) -> io::Result<Stream> {
        let service =
            motor_winagent::transport::parse_guid(motor_winagent::transport::HVSOCK_SERVICE_ID)?;
        let started = unsafe {
            let mut data: WSADATA = zeroed();
            WSAStartup(0x0202, &mut data)
        };
        if started != 0 {
            return Err(io::Error::from_raw_os_error(started));
        }
        let address = SOCKADDR_HV {
            Family: AF_HYPERV,
            Reserved: 0,
            VmId: GUID::from_u128(vm_id),
            ServiceId: GUID::from_u128(service),
        };
        let deadline = Instant::now() + Duration::from_secs(120);
        trace(
            "hvsock-connect-start",
            serde_json::json!({"vm_id":format!("{vm_id:032x}"),"service_id":format!("{service:032x}")}),
        );
        let mut attempts = 0u32;
        let mut seen = BTreeSet::new();
        loop {
            attempts += 1;
            let error = unsafe {
                let candidate = socket(i32::from(AF_HYPERV), SOCK_STREAM, HV_PROTOCOL_RAW as i32);
                if candidate == INVALID_SOCKET {
                    return Err(io::Error::from_raw_os_error(WSAGetLastError()));
                }
                if connect(
                    candidate,
                    (&raw const address).cast::<SOCKADDR>(),
                    size_of::<SOCKADDR_HV>() as i32,
                ) == 0
                {
                    trace("hvsock-connected", serde_json::json!({"attempts":attempts}));
                    return Ok(Stream(candidate));
                }
                let error = WSAGetLastError();
                closesocket(candidate);
                error
            };
            if seen.insert(error) {
                trace(
                    "hvsock-connect-error",
                    serde_json::json!({"os_code":error,"attempt":attempts,"error":io::Error::from_raw_os_error(error).to_string()}),
                );
            }
            if Instant::now() >= deadline {
                return Err(io::Error::from_raw_os_error(error));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn run() -> io::Result<()> {
    let values: Vec<_> = std::env::args().skip(1).collect();
    if values == ["--version"] {
        println!(
            "motor Hyper-V spike protocol {} trace=3 transports=pipe,hvsock",
            motor_winagent::PROTOCOL_VERSION
        );
        return Ok(());
    }
    if let [option, path] = values.as_slice()
        && option == "--summary"
    {
        let rows: Vec<Row> = serde_json::from_slice(&std::fs::read(path)?)?;
        println!("PASS five-run-medians {}", summary(&rows)?);
        return Ok(());
    }
    let args = arguments(&values)?;
    let result = probe(&args)?;
    std::fs::write(&args.result, serde_json::to_vec(&result)?)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("FAIL {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args() -> Args {
        Args {
            channel: Channel::Pipe("motor-hv-test".into()),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            canary: "C:\\motor spike\\it's private.txt".into(),
            host: "HOST-1".into(),
            result: "C:\\result.json".into(),
            stress_bytes: 0,
        }
    }

    #[test]
    fn tracing_preserves_stream_bytes_counts_and_eof_errors() {
        let mut stream = TracedIo {
            inner: io::Cursor::new(vec![1, 2, 3]),
            read: 0,
            written: 0,
            first_com_byte_unix_ms: None,
        };
        assert_eq!(io::Read::read(&mut stream, &mut []).unwrap(), 0);
        assert!(stream.first_com_byte_unix_ms.is_none());
        let mut bytes = [0; 2];
        io::Read::read_exact(&mut stream, &mut bytes).unwrap();
        assert_eq!(bytes, [1, 2]);
        io::Write::write_all(&mut stream, &[9, 8, 7]).unwrap();
        io::Write::flush(&mut stream).unwrap();
        assert_eq!((stream.read, stream.written), (2, 3));
        assert!(stream.first_com_byte_unix_ms.is_some());
        let first = stream.first_com_byte_unix_ms;
        assert_eq!(stream.inner.get_ref(), &[1, 2, 9, 8, 7]);
        assert_eq!(
            io::Read::read_exact(&mut stream, &mut bytes)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(stream.first_com_byte_unix_ms, first);
    }

    struct ScriptedPipe {
        read: io::Cursor<Vec<u8>>,
        written: Vec<u8>,
    }
    impl io::Read for ScriptedPipe {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            io::Read::read(&mut self.read, buffer)
        }
    }
    impl io::Write for ScriptedPipe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn replies(nonce: &str, output: &str, code: u32) -> ScriptedPipe {
        let mut bytes = Vec::new();
        for frame in [
            Frame::Hello {
                version: motor_winagent::PROTOCOL_VERSION,
                nonce: nonce.into(),
            },
            Frame::CacheMounted {
                timings: Default::default(),
            },
            Frame::TreeReceived {},
            Frame::Started {},
            Frame::Output {
                stream: Stream::Stdout,
                bytes: output.as_bytes().into(),
            },
            Frame::Exit { code },
            Frame::CollectEnd { deleted: vec![] },
            Frame::Dismounted { clean: true },
        ] {
            motor_winagent::write_frame(&mut bytes, &frame).unwrap();
        }
        ScriptedPipe {
            read: io::Cursor::new(bytes),
            written: Vec::new(),
        }
    }
    #[test]
    fn probe_requires_isolation_evidence_zero_exit_and_matching_hello() {
        let a = args();
        let output = "PASS standard-account\nPASS no-network-route\nPASS host-files-inaccessible\nPASS trivial-command 4\n";
        let mut pipe = replies(&a.nonce, output, 0);
        assert!(conversation(&mut pipe, &a).is_ok());
        let mut host = io::Cursor::new(pipe.written);
        let mut frames = Vec::new();
        while let Some(frame) = motor_winagent::read_frame(&mut host).unwrap() {
            frames.push(frame);
        }
        assert!(matches!(
            frames.as_slice(),
            [
                Frame::ReceiveTree { .. },
                Frame::TreeEnd { .. },
                Frame::Start {},
                Frame::Collect {},
                Frame::Dismount {}
            ]
        ));
        assert!(conversation(replies(&a.nonce, output, 1), &a).is_err());
        assert!(conversation(replies("ffffffffffffffffffffffffffffffff", output, 0), &a).is_err());
        assert!(conversation(replies(&a.nonce, "PASS trivial-command 4", 0), &a).is_err());
    }
    #[test]
    fn arguments_preserve_paths_and_refuse_unknown_duplicate_and_unsafe_options() {
        let a = args();
        let Channel::Pipe(pipe) = &a.channel else {
            unreachable!()
        };
        let values = [
            "--pipe", pipe, "--nonce", &a.nonce, "--canary", &a.canary, "--host", &a.host,
            "--result", &a.result,
        ]
        .map(str::to_owned);
        assert_eq!(arguments(&values).unwrap(), a);
        let mut stress = values.to_vec();
        stress.extend(["--stress-bytes".to_owned(), "65536".to_owned()]);
        assert_eq!(arguments(&stress).unwrap().stress_bytes, 65536);
        stress[11] = "1".into();
        assert!(arguments(&stress).is_err());
        stress[10] = "--unknown".into();
        assert!(arguments(&stress).is_err());
        for (index, value) in [
            (0, "--other"),
            (2, "--pipe"),
            (1, "../pipe"),
            (3, "bad nonce"),
            (5, "relative"),
            (7, "host'command"),
        ] {
            let mut bad = values.clone();
            bad[index] = value.into();
            assert!(arguments(&bad).is_err());
        }
    }
    #[test]
    fn hvsock_channel_takes_a_vm_guid_and_never_both_or_neither_channel() {
        let a = args();
        let mut values = [
            "--vm-id",
            "{4AE3C7F2-1D39-4C6B-9E58-0123456789AB}",
            "--nonce",
            &a.nonce,
            "--canary",
            &a.canary,
            "--host",
            &a.host,
            "--result",
            &a.result,
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            arguments(&values).unwrap().channel,
            Channel::HvSock(0x4ae3c7f2_1d39_4c6b_9e58_0123456789ab)
        );
        values[1] = "not-a-guid".into();
        assert!(arguments(&values).is_err());
        values[1] = "4ae3c7f2-1d39-4c6b-9e58-0123456789ab".into();
        values.extend(["--pipe".to_owned(), "motor-hv-test".to_owned()]);
        assert!(
            arguments(&values)
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
        values.truncate(10);
        values[0] = "--stress-bytes".into();
        values[1] = "0".into();
        assert!(
            arguments(&values)
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }
    #[test]
    fn tar_contains_only_script_and_quoted_canary_not_host_content() {
        let a = args();
        let bytes = tree(&a).unwrap();
        motor_winagent::validate_tree(&mut io::Cursor::new(&bytes)).unwrap();
        let script = isolation_script(&a);
        assert!(script.contains("'C:\\motor spike\\it''s private.txt'"));
        assert!(script.contains("'\\\\HOST-1\\C$\\motor spike\\it''s private.txt'"));
        assert!(script.contains("$Evidence.groups -contains 'S-1-5-32-544'"));
    }
    #[test]
    fn serial_stress_tree_is_bounded_and_has_no_extra_command() {
        let mut a = args();
        a.stress_bytes = 65536;
        let bytes = tree(&a).unwrap();
        assert!(bytes.len() < motor_winagent::CHUNK_BYTES);
        motor_winagent::validate_tree(&mut io::Cursor::new(&bytes)).unwrap();
        let entries: Vec<_> = tar::Archive::new(bytes.as_slice())
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, ["m/spike/check.ps1", "m/spike/serial-stress.bin"]);
        assert_eq!(isolation_script(&a), isolation_script(&args()));
    }
    #[test]
    fn medians_use_all_five_samples_not_average_and_reject_skips() {
        let rows: Vec<_> = [1000., 3., 1., 5., 2.]
            .iter()
            .map(|v| Row {
                create_ms: *v,
                boot_hello_ms: v * 2.,
                command_ms: v * 3.,
                teardown_ms: v * 4.,
                start_vm_call_ms: v / 4.,
                start_to_first_byte_ms: v / 2.,
                first_byte_to_hello_ms: v * 1.5,
            })
            .collect();
        let result = summary(&rows).unwrap();
        assert_eq!(result["create_ms"]["median"], 3.);
        assert_eq!(result["boot_hello_ms"]["median"], 6.);
        assert_eq!(result["command_ms"]["median"], 9.);
        assert_eq!(result["teardown_ms"]["median"], 12.);
        assert_eq!(result["start_vm_call_ms"]["median"], 0.75);
        assert_eq!(result["start_to_first_byte_ms"]["median"], 1.5);
        assert_eq!(result["first_byte_to_hello_ms"]["median"], 4.5);
        assert_eq!(
            result["create_ms"]["raw"],
            serde_json::json!([1000., 3., 1., 5., 2.])
        );
        assert!(summary(&rows[..4]).is_err());
        let mut bad = rows;
        bad[0].command_ms = f64::NAN;
        assert!(summary(&bad).is_err());
        bad[0].command_ms = -1.;
        assert!(summary(&bad).is_err());
    }
}
