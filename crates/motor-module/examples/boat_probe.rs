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
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err(io::Error::other(
            "usage: boat_probe LOOPBACK_ADDRESS REPOSITORY STORAGE COMMAND",
        ));
    }
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
        .with_weight_hint(8);
    let mut stream = motor.run(request)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(record) = stream.records.recv().await {
        match record? {
            StreamRecord::Accepted(_) => {}
            StreamRecord::Output(output) => match output.stream {
                OutputStream::Stdout => stdout.extend_from_slice(&output.bytes.0),
                OutputStream::Stderr => stderr.extend_from_slice(&output.bytes.0),
                _ => return Err(io::Error::other("unknown job output stream")),
            },
            StreamRecord::Terminal(terminal) => {
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
            _ => {}
        }
    }
    Err(io::Error::other("job stream ended without a terminal"))
}
