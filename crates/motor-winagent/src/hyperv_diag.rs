//! Private diagnostics for the opt-in Hyper-V serial experiment.
use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    sync::{Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

static LOG: OnceLock<Mutex<File>> = OnceLock::new();
// A second handle to the same log, used only for flushing to disk. Flushing
// through it does not hold the writers' mutex, so a slow disk flush cannot
// stall a thread that logs from inside its I/O path.
static SYNC: OnceLock<File> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

pub fn init() -> io::Result<()> {
    use sha2::Digest;
    let _ = START.set(Instant::now());
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(r"C:\ProgramData\Motor\hyperv-agent.log")?;
    file.seek(SeekFrom::End(0))?;
    let executable = std::env::current_exe()?;
    let hash = sha2::Sha256::digest(std::fs::read(&executable)?);
    writeln!(
        file,
        "agent_diag=3 feature=hyperv-spike transport={} exe={} sha256={hash:x}",
        if cfg!(feature = "hyperv-hvsock") {
            "hvsock"
        } else {
            "com"
        },
        executable.display()
    )?;
    // Flush file creation and startup identity before any serial operation.
    // A forced VM stop otherwise need not preserve an open cached log file.
    file.sync_all()?;
    let _ = SYNC.set(file.try_clone()?);
    LOG.set(Mutex::new(file))
        .map_err(|_| io::Error::other("Hyper-V diagnostic log already initialized"))?;
    event("service starting");
    sync();
    Ok(())
}

pub fn sync() {
    if let Some(file) = SYNC.get() {
        let _ = file.sync_data();
    }
}

pub fn event(message: &str) {
    if let Some(log) = LOG.get()
        && let Ok(mut file) = log.lock()
    {
        // Opened as SYSTEM before impersonation; never expose this handle to a job.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let elapsed = START
            .get()
            .map(|start| start.elapsed().as_millis())
            .unwrap_or_default();
        let _ = writeln!(file, "{now} elapsed_ms={elapsed} {message}");
        let _ = file.flush();
    }
}
