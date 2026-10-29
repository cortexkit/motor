//! Windows service stderr has no journal. Retain logs/runner.log and one rotated
//! backup under the instance state directory, each at most 10 MiB.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_LINE: usize = 64 * 1024;

struct BoundedLog {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
    limit: u64,
}

impl BoundedLog {
    fn open(path: PathBuf, limit: u64) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata()?.len().min(limit);
        file.set_len(bytes)?;
        Ok(Self {
            path,
            file: Some(file),
            bytes,
            limit,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Close before rename: Windows does not allow renaming an open log.
        self.file.take();
        let previous = self.path.with_extension("log.1");
        match fs::remove_file(&previous) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(&self.path, previous)?;
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        self.bytes = 0;
        Ok(())
    }
}

impl Write for BoundedLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis();
        let mut line = format!("{timestamp} ").into_bytes();
        let available = MAX_LINE
            .min(self.limit as usize)
            .saturating_sub(line.len() + 1);
        line.extend_from_slice(&bytes[..bytes.len().min(available)]);
        if !line.ends_with(b"\n") {
            line.push(b'\n');
        }
        if self.bytes + line.len() as u64 > self.limit {
            self.rotate()?;
        }
        if self.file.is_none() {
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        self.file.as_mut().unwrap().write_all(&line)?;
        self.bytes += line.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &self.file {
            file.sync_data()?;
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(super) fn init(state: &std::path::Path) -> io::Result<()> {
    static INITIALIZED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if INITIALIZED.get().is_some() {
        return Ok(());
    }
    let directory = state.join("logs");
    fs::create_dir_all(&directory)?;
    let writer = BoundedLog::open(directory.join("runner.log"), MAX_BYTES)?;
    motor_protocol::logging::init_with_writer(super::LOG_PREFIX, writer)
        .map_err(io::Error::other)?;
    let _ = INITIALIZED.set(());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_log_is_timestamped_bounded_and_survives_reopen() {
        let scratch = cortexkit_test_support::ScratchDir::new("windows-service-log");
        let path = scratch.path().join("runner.log");
        let mut log = BoundedLog::open(path.clone(), 128).unwrap();
        for n in 0..10 {
            log.write_all(format!("event {n} {}\n", "x".repeat(60)).as_bytes())
                .unwrap();
        }
        log.flush().unwrap();
        drop(log);
        for file in [&path, &path.with_extension("log.1")] {
            assert!(file.metadata().unwrap().len() <= 128);
            let line = fs::read_to_string(file).unwrap();
            assert!(line.split_once(' ').unwrap().0.parse::<u128>().unwrap() > 0);
        }
        let mut log = BoundedLog::open(path.clone(), 128).unwrap();
        log.write_all(b"restarted\n").unwrap();
        log.flush().unwrap();
        assert!(fs::read_to_string(path).unwrap().contains("restarted"));
    }

    #[test]
    fn service_log_caps_an_oversized_event() {
        let scratch = cortexkit_test_support::ScratchDir::new("windows-service-log-large");
        let path = scratch.path().join("runner.log");
        let mut log = BoundedLog::open(path.clone(), MAX_BYTES).unwrap();
        log.write_all(&vec![b'x'; 2 * MAX_LINE]).unwrap();
        assert!(path.metadata().unwrap().len() <= MAX_LINE as u64);
    }
}
