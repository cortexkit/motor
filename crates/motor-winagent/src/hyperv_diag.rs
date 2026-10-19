//! Private diagnostics for the opt-in Hyper-V serial experiment.
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

static LOG: OnceLock<Mutex<File>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

pub fn init() {
    let _ = START.set(Instant::now());
    if let Ok(file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\ProgramData\Motor\hyperv-agent.log")
    {
        let _ = LOG.set(Mutex::new(file));
    }
    event("service starting");
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
