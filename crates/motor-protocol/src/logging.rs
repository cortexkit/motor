//! The log sink shared by both ends of exec-remote/v1: ck-motor on the client
//! machine and motor-runner on the Linux runner. Both report through
//! `tracing`, which discards every event until a subscriber is installed, so
//! each binary's `main` installs this one. It formats INFO and above as one
//! `<prefix> LEVEL target: message field=value ...` line per event and writes
//! it to stderr. Whoever captures that stderr adds the timestamp: the subc
//! daemon for ck-motor (`ck module logs ck-motor`), and the systemd user unit
//! for motor-runner (the journal).
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
}

impl LineSubscriber {
    pub fn new(prefix: &'static str, writer: impl Write + Send + 'static) -> Self {
        Self {
            prefix,
            writer: Mutex::new(Box::new(writer)),
            next_span: AtomicU64::new(1),
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
        let line = format!(
            "{} {} {}: {}{}",
            self.prefix,
            metadata.level(),
            metadata.target(),
            fields.message,
            fields.rest
        );
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
