//! A credential-free protocol capture, separate from the CLI's diagnostic stderr.
use motor_protocol::StreamRecord;
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

pub(super) struct Capture(File);
impl Capture {
    /// Creates a new file only, so an existing file or a planted link is never
    /// written through. On Unix it is owner-only; on Windows it inherits the
    /// folder's access rules, as the caller's own files do.
    pub(super) fn create(path: &Path) -> io::Result<Self> {
        let mut options = File::options();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        Ok(Self(options.open(path)?))
    }

    pub(super) fn record(&mut self, record: &StreamRecord) -> io::Result<()> {
        // Serialize the typed protocol record, never the direct-route envelope:
        // that envelope also carries the bearer secret used for reattachment.
        if matches!(
            record,
            StreamRecord::Accepted(_)
                | StreamRecord::Started(_)
                | StreamRecord::Output(_)
                | StreamRecord::Terminal(_)
        ) {
            serde_json::to_writer(&mut self.0, record)?;
            self.0.write_all(b"\n")?;
            self.0.flush()?;
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn capture_preserves_platform_binary_output_and_excludes_route_secrets() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-records");
        let path = scratch.join("records.jsonl");
        let mut capture = Capture::create(&path).unwrap();
        let id = motor_protocol::Uuid::now_v7();
        let envelope = serde_json::json!({"type":"accepted", "job_id":id, "queue_position":0,
            "platform":"macos", "direct_job":{"job_id":id,"secret":"must-not-leak"}});
        let accepted: StreamRecord = serde_json::from_value(envelope).unwrap();
        capture.record(&accepted).unwrap();
        let bytes = serde_json::json!({"type":"output", "job_id":id,
            "seq":1,"stream":"stderr","bytes":"AP8K"});
        let output: StreamRecord = serde_json::from_value(bytes.clone()).unwrap();
        capture.record(&output).unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(!data.contains("secret") && !data.contains("must-not-leak"));
        let rows: Vec<serde_json::Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["platform"], "macos");
        assert_eq!(rows[1]["bytes"], bytes["bytes"]);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(Capture::create(&path).is_err());
        let link = scratch.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Capture::create(&link).is_err());
    }
}
