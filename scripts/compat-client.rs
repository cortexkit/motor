// This executable links only the archived client and its archived protocol.
// The current runner lives in the separate integration-test process.
use motor_module::{Config, Connector, Duplex, Motor};
use motor_protocol::{AttachRequest, PrepareRequest, RunRequest, StreamRecord};
use std::{future::Future, io, path::Path, pin::Pin, sync::Arc};
use tokio::net::TcpStream;
use uuid::Uuid;

struct TcpConnector(String);
impl Connector for TcpConnector {
    fn connect(
        &self,
        _id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async move {
            Ok(Box::new(TcpStream::connect(&self.0).await?) as Box<dyn Duplex>)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let config: Config = serde_json::from_slice(&std::fs::read(&args[2])?)?;
    let motor = Motor::with_connector(
        config,
        Path::new(&args[3]),
        Arc::new(TcpConnector(args[1].clone())),
    )?;
    if args[4] == "status" {
        let status = motor.status().await;
        if !status.server_reachable {
            return Err("old client could not decode runner status".into());
        }
        println!("{}", serde_json::to_string(&status)?);
        return Ok(());
    }
    if args[4] == "prepare" {
        let request: PrepareRequest = serde_json::from_slice(&std::fs::read(&args[5])?)?;
        let reply = motor.prepare(request).await?;
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(());
    }
    let mut stream = if args[4] == "run" {
        let request: RunRequest = serde_json::from_slice(&std::fs::read(&args[5])?)?;
        motor.run(request)?
    } else {
        motor.attach(AttachRequest::new(args[5].parse()?, args[6].parse()?))?
    };
    while let Some(record) = stream.records.recv().await {
        let record = record?;
        println!("{}", serde_json::to_string(&record)?);
        if matches!(record, StreamRecord::Terminal(_)) {
            return Ok(());
        }
    }
    Err("legacy stream closed without Terminal".into())
}
