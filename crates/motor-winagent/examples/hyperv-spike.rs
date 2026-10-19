//! Operator-only named-pipe probe. The production Linux host never invokes this.
use motor_winagent::{Command, Frame, Peer, Session, Stream};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io, time::Instant};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Debug, PartialEq)]
struct Args {
    pipe: String,
    nonce: String,
    canary: String,
    host: String,
    result: String,
}

fn arguments(values: &[String]) -> io::Result<Args> {
    if values.len() != 10 {
        return Err(invalid(
            "expected --pipe NAME --nonce UUID --canary PATH --host NAME --result PATH",
        ));
    }
    let mut pairs = BTreeMap::new();
    for pair in values.as_chunks::<2>().0 {
        if pairs.insert(pair[0].as_str(), pair[1].clone()).is_some() {
            return Err(invalid("duplicate option"));
        }
    }
    let mut take = |key| {
        pairs
            .remove(key)
            .ok_or_else(|| invalid(format!("missing {key}")))
    };
    let args = Args {
        pipe: take("--pipe")?,
        nonce: take("--nonce")?,
        canary: take("--canary")?,
        host: take("--host")?,
        result: take("--result")?,
    };
    motor_winagent::transport::hyperv_nonce(&args.nonce)?;
    if !args.pipe.starts_with("motor-hv-")
        || !args
            .pipe
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(invalid("unsafe pipe name"));
    }
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
    tar.into_inner()
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Row {
    create_ms: f64,
    boot_hello_ms: f64,
    command_ms: f64,
    teardown_ms: f64,
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
    command_ms: f64,
}

fn conversation<T: io::Read + io::Write>(mut pipe: T, args: &Args) -> io::Result<Probe> {
    let mut session = Session::new(Peer::Host, &args.nonce)?;
    let receive = |session: &mut Session, pipe: &mut _| -> io::Result<Frame> {
        session
            .receive(pipe)?
            .ok_or_else(|| invalid("unexpected EOF"))
    };
    match receive(&mut session, &mut pipe)? {
        frame @ Frame::Hello { .. } => println!("PASS Hello {}", serde_json::to_string(&frame)?),
        frame => return Err(invalid(format!("expected Hello: {frame:?}"))),
    }
    let hello_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
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
    session.send(&mut pipe, &Frame::ReceiveTree { bytes: tree(args)? })?;
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
    if receive(&mut session, &mut pipe)? != (Frame::TreeReceived {}) {
        return Err(invalid("expected tree_received"));
    }
    let command = Instant::now();
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
    Ok(Probe {
        hello_unix_ms,
        command_ms,
    })
}

#[cfg(windows)]
fn probe(args: &Args) -> io::Result<Probe> {
    use std::os::windows::fs::OpenOptionsExt;
    let path = format!(r"\\.\pipe\{}", args.pipe);
    let deadline = Instant::now() + std::time::Duration::from_secs(120);
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
    conversation(pipe, args)
}

#[cfg(not(windows))]
fn probe(_args: &Args) -> io::Result<Probe> {
    // Keep the protocol client typechecked on portable hosts without pretending
    // that an in-memory stream proves Hyper-V connectivity.
    let _ = conversation::<io::Cursor<Vec<u8>>>;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "measurement requires Windows Hyper-V",
    ))
}

fn run() -> io::Result<()> {
    let values: Vec<_> = std::env::args().skip(1).collect();
    if values == ["--version"] {
        println!(
            "motor Hyper-V spike protocol {}",
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
            pipe: "motor-hv-test".into(),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            canary: "C:\\motor spike\\it's private.txt".into(),
            host: "HOST-1".into(),
            result: "C:\\result.json".into(),
        }
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
        let values = [
            "--pipe", &a.pipe, "--nonce", &a.nonce, "--canary", &a.canary, "--host", &a.host,
            "--result", &a.result,
        ]
        .map(str::to_owned);
        assert_eq!(arguments(&values).unwrap(), a);
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
    fn medians_use_all_five_samples_not_average_and_reject_skips() {
        let rows: Vec<_> = [1000., 3., 1., 5., 2.]
            .iter()
            .map(|v| Row {
                create_ms: *v,
                boot_hello_ms: v * 2.,
                command_ms: v * 3.,
                teardown_ms: v * 4.,
            })
            .collect();
        let result = summary(&rows).unwrap();
        assert_eq!(result["create_ms"]["median"], 3.);
        assert_eq!(result["boot_hello_ms"]["median"], 6.);
        assert_eq!(result["command_ms"]["median"], 9.);
        assert_eq!(result["teardown_ms"]["median"], 12.);
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
