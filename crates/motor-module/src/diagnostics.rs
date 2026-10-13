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

pub(crate) fn health_detail() -> String {
    let counts = UnreachableStep::ALL
        .into_iter()
        .map(|step| format!("{}={}", step.label(), process_count(step)))
        .collect::<Vec<_>>()
        .join(",");
    format!("unreachable_refusals_since_boot: {counts}")
}
