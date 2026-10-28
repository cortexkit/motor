#[cfg(test)]
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(crate) enum UnreachableStep {
    Connect,
    HelloSend,
    HelloRecv,
    UnexpectedFrame,
    Timeout15s,
    HaveExchange,
    Prepare,
}

impl UnreachableStep {
    pub(crate) const ALL: [Self; 7] = [
        Self::Connect,
        Self::HelloSend,
        Self::HelloRecv,
        Self::UnexpectedFrame,
        Self::Timeout15s,
        Self::HaveExchange,
        Self::Prepare,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::HelloSend => "hello_send",
            Self::HelloRecv => "hello_recv",
            Self::UnexpectedFrame => "unexpected_frame",
            Self::Timeout15s => "timeout_15s",
            Self::HaveExchange => "have_exchange",
            Self::Prepare => "prepare",
        }
    }
}

static COUNTS: [AtomicU64; 7] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

#[cfg(test)]
thread_local! {
    // Libtest gives each test worker its own thread, and these tests use Tokio's
    // current-thread runtime. Keeping a parallel test-only count here prevents
    // another test worker's diagnostic from contaminating before/after checks.
    static TEST_COUNTS: RefCell<[u64; 7]> = const { RefCell::new([0; 7]) };
}

#[cfg(test)]
pub(crate) fn count(step: UnreachableStep) -> u64 {
    TEST_COUNTS.with(|counts| counts.borrow()[step as usize])
}

fn process_count(step: UnreachableStep) -> u64 {
    COUNTS[step as usize].load(Ordering::Relaxed)
}

pub(crate) fn record_unreachable(id: Uuid, step: UnreachableStep, cause: impl std::fmt::Display) {
    COUNTS[step as usize].fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    TEST_COUNTS.with(|counts| counts.borrow_mut()[step as usize] += 1);

    let error: String = cause
        .to_string()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(300)
        .collect();
    tracing::warn!(
        job_or_workspace_id = %id,
        step = step.label(),
        error = %error,
        "remote executor refused as unreachable"
    );
}

/// Per-step unreachable refusal counts since ck-motor started, for the health
/// report's metrics: diagnostic detail for `--verbose`, never the summary line.
pub(crate) fn health_metrics() -> serde_json::Value {
    UnreachableStep::ALL
        .into_iter()
        .map(|step| (step.label().to_owned(), process_count(step).into()))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// Keep the end of a refusal for logging without splitting UTF-8 characters.
pub(crate) fn detail_tail(detail: &str) -> String {
    let mut start = detail.len().saturating_sub(16 * 1024);
    while !detail.is_char_boundary(start) {
        start += 1;
    }
    detail[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_log_tail_is_bounded_and_preserves_utf8_and_the_end() {
        for detail in ["", "short refusal", &"a".repeat(16 * 1024)] {
            assert_eq!(detail_tail(detail), detail);
        }
        assert_eq!(
            detail_tail(&format!("x{}", "a".repeat(16 * 1024))),
            "a".repeat(16 * 1024)
        );
        let detail = format!("{}!", "é".repeat(16 * 1024));
        assert_eq!(detail_tail(&detail), format!("{}!", "é".repeat(8191)));
    }
}
