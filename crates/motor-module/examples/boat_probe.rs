//! Operator-only client over an already API-pinned, loopback SSH forward.
//! Motor owns snapshot capture, bundle upload, negotiation and terminal handling.
use motor_module::{Config, Connector, Duplex, Motor};
use motor_protocol::{OutputStream, RunRequest, StreamRecord, Uuid};
use std::{future::Future, io, path::Path, pin::Pin, sync::Arc};

struct Forward(std::net::SocketAddr);
impl Connector for Forward {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async move {
            Ok(Box::new(tokio::net::TcpStream::connect(self.0).await?) as Box<dyn Duplex>)
        })
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    // stdout carries only the final JSON result. stderr records the client's
    // own log lines and when each stage was reached, so a probe that kills a
    // client with no terminal can tell whether the job was never accepted,
    // accepted but never started, or started and never finished.
    let _ = motor_module::logging::init();
    let began = std::time::Instant::now();
    let stage = |text: std::fmt::Arguments| {
        eprintln!("boat_probe +{:.3}s {text}", began.elapsed().as_secs_f64());
    };
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !(4..=5).contains(&args.len()) {
        return Err(io::Error::other(
            "usage: boat_probe LOOPBACK_ADDRESS REPOSITORY STORAGE COMMAND [VCPUS]",
        ));
    }
    // The probe passes the vCPUs the sandbox really has, so a job fits every Boat size;
    // 8 (a whole large sandbox) keeps older probe invocations unchanged.
    let vcpus: u32 = match args.get(4) {
        Some(text) => text.parse().map_err(io::Error::other)?,
        None => 8,
    };
    let address: std::net::SocketAddr = args[0].parse().map_err(io::Error::other)?;
    if !address.ip().is_loopback() {
        return Err(io::Error::other("probe requires a loopback SSH forward"));
    }
    let motor = Motor::with_connector(
        Config::default(),
        Path::new(&args[2]),
        Arc::new(Forward(address)),
    )?;
    let request = RunRequest::new(&args[1], &args[1], &args[1], &args[3])
        .with_timeout(120)
        .with_weight_hint(vcpus);
    let mut stream = motor.run(request)?;
    stage(format_args!("job submitted"));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output_records = 0u64;
    while let Some(record) = stream.records.recv().await {
        match record? {
            StreamRecord::Accepted(accepted) => stage(format_args!(
                "accepted job_id={} queue_position={}",
                accepted.job_id, accepted.queue_position
            )),
            StreamRecord::Started(started) => stage(format_args!(
                "started queue_wait_ms={}",
                started.queue_wait_ms
            )),
            StreamRecord::Output(output) => {
                output_records += 1;
                if output_records == 1 {
                    stage(format_args!("first output seq={}", output.seq));
                }
                match output.stream {
                    OutputStream::Stdout => stdout.extend_from_slice(&output.bytes.0),
                    OutputStream::Stderr => stderr.extend_from_slice(&output.bytes.0),
                    _ => return Err(io::Error::other("unknown job output stream")),
                }
            }
            StreamRecord::Terminal(terminal) => {
                stage(format_args!(
                    "terminal after {output_records} output records"
                ));
                stream.acknowledge_terminal()?;
                println!(
                    "{}",
                    serde_json::json!({
                        "code": motor_module::cli::exit_code(&terminal), "terminal": terminal,
                        "stdout": String::from_utf8_lossy(&stdout), "stderr": String::from_utf8_lossy(&stderr)
                    })
                );
                return Ok(());
            }
            other => stage(format_args!("other record {}", record_kind(&other))),
        }
    }
    Err(io::Error::other("job stream ended without a terminal"))
}

/// The record's variant name, for the stage log.
fn record_kind(record: &StreamRecord) -> &'static str {
    match record {
        StreamRecord::ReturnResult(_) => "return_result",
        StreamRecord::ReturnChunk(_) => "return_chunk",
        StreamRecord::ReturnManifest(_) => "return_manifest",
        _ => "unrecognized",
    }
}
