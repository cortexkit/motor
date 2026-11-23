//! The log sink shared by both ends of exec-remote/v1: ck-motor on the client
//! machine and motor-runner on the Linux runner. Both report through
//! `tracing`, which discards every event until a subscriber is installed, so
//! each binary's `main` installs this one. It formats INFO and above as one
//! `<prefix> LEVEL target: message field=value ...` line per event and writes
//! it to stderr. The caller can supply a timestamp when the sink does not add
//! one. The SubC daemon adds timestamps for ck-motor (`ck module logs
//! ck-motor`), and journald adds them for the runner's systemd unit.
//!
//! This is deliberately a small `tracing::Subscriber` rather than another
//! logging framework: both binaries only need leveled single-line events, and
//! spans are accepted but not rendered.
use std::{
    fmt::{self, Write as _},
    io::{self, Write},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
use tracing::{
    field::{Field, Visit},
    span, Event, Level, Metadata, Subscriber,
};

/// Events below this level are dropped. DEBUG and TRACE stay off so routine
/// transport chatter (for example detached channels) does not flood the log.
pub const MAX_LEVEL: Level = Level::INFO;

/// Install the stderr sink as the process-wide subscriber. Call it once, as
/// early in `main` as possible; a second call returns an error and leaves the
/// first sink in place. `prefix` names the binary on every line, for example
/// `[ck-motor]`.
pub fn init(prefix: &'static str) -> Result<(), tracing::dispatcher::SetGlobalDefaultError> {
    init_with_writer(prefix, io::stderr())
}

/// Install the stderr sink with a caller-supplied clock for a plain stderr sink.
pub fn init_with_timestamp(
    prefix: &'static str,
    timestamp: fn() -> String,
) -> Result<(), tracing::dispatcher::SetGlobalDefaultError> {
    let mut subscriber = LineSubscriber::new(prefix, io::stderr());
    subscriber.timestamp = Some(timestamp);
    tracing::subscriber::set_global_default(subscriber)
}

/// Install a sink that writes to `writer` instead of stderr. Tests use this to
/// capture what `init` would have printed.
pub fn init_with_writer(
    prefix: &'static str,
    writer: impl Write + Send + 'static,
) -> Result<(), tracing::dispatcher::SetGlobalDefaultError> {
    tracing::dispatcher::set_global_default(tracing::Dispatch::new(LineSubscriber::new(
        prefix, writer,
    )))
}

/// One formatted line per event, written to the wrapped writer.
pub struct LineSubscriber {
    prefix: &'static str,
    writer: Mutex<Box<dyn Write + Send>>,
    next_span: AtomicU64,
    timestamp: Option<fn() -> String>,
}

impl LineSubscriber {
    pub fn new(prefix: &'static str, writer: impl Write + Send + 'static) -> Self {
        Self {
            prefix,
            writer: Mutex::new(Box::new(writer)),
            next_span: AtomicU64::new(1),
            timestamp: None,
        }
    }
}

impl Subscriber for LineSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // Level ordering in `tracing`: TRACE > DEBUG > INFO > WARN > ERROR.
        *metadata.level() <= MAX_LEVEL
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::from_level(MAX_LEVEL))
    }
    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }
    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}
    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut line = format!(
            "{} {} {}: {}{}",
            self.prefix,
            metadata.level(),
            metadata.target(),
            fields.message,
            fields.rest
        );
        // Append rather than prefix so readers can still recognize the
        // binary, severity and target at the start of the line.
        if let Some(timestamp) = self.timestamp {
            let _ = write!(line, " timestamp={}", timestamp());
        }
        // One line per event: a field value carrying a newline must not be able
        // to forge a second log line.
        let mut line: String = line
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
        line.push('\n');
        // A poisoned or failing sink must never take the process down with it.
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.flush();
        }
    }
    fn enter(&self, _span: &span::Id) {}
    fn exit(&self, _span: &span::Id) {}
}

#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.rest, " {}={value}", field.name());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.rest, " {}={value:?}", field.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn timestamp_is_optional_and_leaves_the_event_prefix_intact() {
        for (clock, suffix) in [
            (None, ""),
            (
                Some((|| "2000-02-29T01:02:03.004Z".to_string()) as fn() -> String),
                " timestamp=2000-02-29T01:02:03.004Z",
            ),
        ] {
            let capture = Capture::default();
            let mut subscriber = LineSubscriber::new("[runner]", capture.clone());
            subscriber.timestamp = clock;
            let _guard = tracing::subscriber::set_default(subscriber);
            tracing::info!(target: "test_runner", version = "123", "serving");
            assert_eq!(
                *capture.0.lock().unwrap(),
                format!("[runner] INFO test_runner: serving version=123{suffix}\n").as_bytes()
            );
        }
    }
}
