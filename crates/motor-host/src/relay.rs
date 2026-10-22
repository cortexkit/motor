use anyhow::Result;
use serde_json::Value;
use std::sync::Mutex;
use tokio::io::{AsyncWrite, AsyncWriteExt};

#[derive(Default)]
struct Progress {
    client_read: u64,
    guest_written: u64,
    input_pending: usize,
    client_written: u64,
    frame_type: &'static str,
    frame_seq: Option<u64>,
    frame_bytes: usize,
    frame_written: usize,
    frame_flushed: bool,
}

pub(crate) struct RelayTrace {
    jti: String,
    progress: Mutex<Progress>,
}

impl RelayTrace {
    pub fn new(jti: &str) -> Self {
        Self {
            jti: jti.into(),
            progress: Mutex::new(Progress::default()),
        }
    }

    pub async fn guest_input(
        &self,
        writer: &mut (impl AsyncWrite + Unpin),
        bytes: &[u8],
    ) -> std::io::Result<()> {
        {
            let mut p = self.progress.lock().expect("relay progress");
            p.client_read += bytes.len() as u64;
            p.input_pending = bytes.len();
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let n = writer.write(remaining).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            remaining = &remaining[n..];
            let mut p = self.progress.lock().expect("relay progress");
            p.guest_written += n as u64;
            p.input_pending = remaining.len();
        }
        Ok(())
    }

    pub async fn client_frame(
        &self,
        writer: &mut (impl AsyncWrite + Unpin),
        frame: &Value,
    ) -> Result<()> {
        let body = serde_json::to_vec(frame)?;
        anyhow::ensure!(
            body.len() <= crate::helper::FRAME_LIMIT,
            "invalid frame length"
        );
        let mut bytes = Vec::with_capacity(4 + body.len());
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&body);
        {
            let mut p = self.progress.lock().expect("relay progress");
            // Never copy arbitrary guest text into the host journal.
            p.frame_type = match frame["type"].as_str() {
                Some("accepted") => "accepted",
                Some("started") => "started",
                Some("event") => "event",
                Some("output") => "output",
                Some("terminal") => "terminal",
                Some("hosted_terminal") => "hosted_terminal",
                Some("pong") => "pong",
                Some("heartbeat") => "heartbeat",
                _ => "other",
            };
            p.frame_seq = frame["seq"].as_u64();
            p.frame_bytes = bytes.len();
            p.frame_written = 0;
            p.frame_flushed = false;
        }
        let mut offset = 0;
        while offset < bytes.len() {
            let n = writer.write(&bytes[offset..]).await?;
            anyhow::ensure!(n != 0, "relay write returned zero");
            {
                let mut p = self.progress.lock().expect("relay progress");
                p.client_written += n as u64;
                p.frame_written += n;
            }
            offset += n;
        }
        writer.flush().await?;
        self.progress.lock().expect("relay progress").frame_flushed = true;
        Ok(())
    }
}

impl Drop for RelayTrace {
    fn drop(&mut self) {
        let p = self.progress.lock().expect("relay progress");
        tracing::info!(
            jti = self.jti,
            client_read_bytes = p.client_read,
            guest_written_bytes = p.guest_written,
            guest_input_pending_bytes = p.input_pending,
            client_written_bytes = p.client_written,
            client_frame = p.frame_type,
            client_frame_seq = p.frame_seq,
            client_frame_bytes = p.frame_bytes,
            client_frame_written_bytes = p.frame_written,
            client_frame_flushed = p.frame_flushed,
            "relay_finished"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_client_frame_records_the_exact_partial_write_without_payload() {
        let trace = RelayTrace::new("job");
        let (mut writer, _reader) = tokio::io::duplex(9);
        let frame = serde_json::json!({"type":"output", "seq":7, "data":"private-output"});
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            trace.client_frame(&mut writer, &frame)
        )
        .await
        .is_err());
        let p = trace.progress.lock().unwrap();
        assert_eq!(
            (p.frame_type, p.frame_seq, p.frame_written, p.client_written),
            ("output", Some(7), 9, 9)
        );
        assert_eq!(p.frame_bytes, serde_json::to_vec(&frame).unwrap().len() + 4);
    }

    #[tokio::test]
    async fn broken_guest_input_records_pending_bytes_and_keeps_the_io_error() {
        let trace = RelayTrace::new("job");
        let (mut writer, reader) = tokio::io::duplex(9);
        drop(reader);
        assert_eq!(
            trace
                .guest_input(&mut writer, b"heartbeat")
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::BrokenPipe
        );
        let p = trace.progress.lock().unwrap();
        assert_eq!((p.client_read, p.guest_written, p.input_pending), (9, 0, 9));
    }
}
