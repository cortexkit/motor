//! ck-motor's log sink. The subc daemon captures each module's stderr into
//! `ck module logs <id>` (and `run/logs/<id>.stderr.log`), and the other
//! modules write their diagnostics there as plain lines. ck-motor reports
//! through `tracing`, which discards every event until a subscriber is
//! installed, so `main` installs this one: it formats INFO and above as one
//! `[ck-motor] LEVEL target: message field=value ...` line per event and
//! writes it to stderr. The daemon adds the timestamp.
//!
//! The formatter itself lives in `motor_protocol::logging`, shared with
//! motor-runner, which installs the same sink under its own prefix.
use motor_protocol::logging as shared;
use std::io::{self, Write};
#[cfg(test)]
use std::sync::Mutex;

pub use shared::MAX_LEVEL;

const PREFIX: &str = "[ck-motor]";

/// Install the stderr sink as the process-wide subscriber. Call it once, as
/// early in `main` as possible; a second call returns an error and leaves the
/// first sink in place.
pub fn init() -> Result<(), tracing::dispatcher::SetGlobalDefaultError> {
    init_with_writer(io::stderr())
}

/// Install a sink that writes to `writer` instead of stderr. Tests use this to
/// capture what `init` would have printed.
pub fn init_with_writer(
    writer: impl Write + Send + 'static,
) -> Result<(), tracing::dispatcher::SetGlobalDefaultError> {
    shared::init_with_writer(PREFIX, writer)
}

/// A sink that keeps everything written to it, for tests that assert on the
/// exact lines ck-motor would print.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Captured(std::sync::Arc<Mutex<Vec<u8>>>);
#[cfg(test)]
impl Captured {
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}
#[cfg(test)]
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Route this thread's events to `writer` until the guard drops. Tests on a
/// current-thread runtime use it to read what the module logs without
/// installing a process-wide subscriber.
#[cfg(test)]
pub(crate) fn scoped(writer: impl Write + Send + 'static) -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(shared::LineSubscriber::new(PREFIX, writer))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process-wide install is what `main` runs, so this test exercises it
    /// rather than a thread-local dispatcher. Other tests can log concurrently,
    /// so assertions below identify this test's events by their unique ID.
    #[test]
    fn unreachable_warning_reaches_the_installed_sink() {
        let captured = Captured::default();
        init_with_writer(captured.clone()).expect("first install succeeds");
        let id = uuid::Uuid::now_v7();
        crate::diagnostics::record_unreachable(
            id,
            crate::diagnostics::UnreachableStep::HelloRecv,
            "channel detached\nforged line",
        );
        tracing::info!(test_event_id = %id, answer = 42, "info is kept");
        tracing::debug!(test_event_id = %id, "debug is dropped");
        let text = captured.text();
        let lines: Vec<&str> = text.lines().collect();
        let warning = lines
            .iter()
            .find(|line| line.contains(&format!("job_or_workspace_id={id}")))
            .expect("this diagnostic warning reaches the installed sink");
        assert!(
            warning.starts_with(
                "[ck-motor] WARN motor_module::diagnostics: remote executor refused as unreachable"
            ),
            "{warning:?}"
        );
        assert!(warning.contains("step=hello_recv"), "{warning:?}");
        assert!(
            warning.contains("error=channel detached forged line"),
            "{warning:?}"
        );
        let event_id = format!("test_event_id={id}");
        let info = lines
            .iter()
            .find(|line| line.contains(&event_id) && line.contains("info is kept"))
            .expect("this info event reaches the installed sink");
        assert!(
            info.contains("INFO") && info.contains("answer=42"),
            "{info:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| { line.contains(&event_id) && line.contains("debug is dropped") }),
            "the debug event must be filtered: {text:?}"
        );
        assert!(init().is_err(), "a second install is refused");
    }
}
