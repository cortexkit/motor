use crate::state::Host;
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError, TryLockError, TryLockResult};
use std::time::{Duration, Instant};

/// Shared host state. Reports long holds, not time spent waiting for a holder.
pub struct HostMutex(Mutex<Host>);

impl HostMutex {
    pub fn new(host: Host) -> Self {
        Self(Mutex::new(host))
    }

    pub fn is_poisoned(&self) -> bool {
        self.0.is_poisoned()
    }

    #[track_caller]
    pub fn lock(&self) -> LockResult<HostGuard<'_>> {
        self.lock_named(None)
    }

    #[track_caller]
    pub(super) fn lock_for(&self, operation: &'static str) -> LockResult<HostGuard<'_>> {
        self.lock_named(Some(operation))
    }

    #[track_caller]
    fn lock_named(&self, operation: Option<&'static str>) -> LockResult<HostGuard<'_>> {
        let caller = Location::caller();
        let wrap = |guard| HostGuard::new(guard, operation, caller);
        self.0
            .lock()
            .map(wrap)
            .map_err(|err| PoisonError::new(wrap(err.into_inner())))
    }

    #[track_caller]
    pub fn try_lock(&self) -> TryLockResult<HostGuard<'_>> {
        let caller = Location::caller();
        match self.0.try_lock() {
            Ok(guard) => Ok(HostGuard::new(guard, None, caller)),
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
            Err(TryLockError::Poisoned(err)) => Err(TryLockError::Poisoned(PoisonError::new(
                HostGuard::new(err.into_inner(), None, caller),
            ))),
        }
    }
}

pub struct HostGuard<'a> {
    guard: Option<MutexGuard<'a, Host>>,
    held_since: Instant,
    operation: Option<&'static str>,
    caller: &'static Location<'static>,
}

impl<'a> HostGuard<'a> {
    fn new(
        guard: MutexGuard<'a, Host>,
        operation: Option<&'static str>,
        caller: &'static Location<'static>,
    ) -> Self {
        Self {
            guard: Some(guard),
            held_since: Instant::now(),
            operation,
            caller,
        }
    }
}

impl Deref for HostGuard<'_> {
    type Target = Host;

    fn deref(&self) -> &Host {
        self.guard.as_deref().expect("host guard")
    }
}

impl DerefMut for HostGuard<'_> {
    fn deref_mut(&mut self) -> &mut Host {
        self.guard.as_deref_mut().expect("host guard")
    }
}

impl Drop for HostGuard<'_> {
    fn drop(&mut self) {
        let held = self.held_since.elapsed();
        // Logging must not extend the hold it reports.
        drop(self.guard.take());
        if held > Duration::from_secs(5) {
            let operation = self.operation.map_or_else(
                || format_args!("{}", self.caller).to_string(),
                str::to_owned,
            );
            tracing::warn!(
                operation,
                held_ms = held.as_millis() as u64,
                "host_mutex_long_hold"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_one_warning_per_long_hold_and_none_for_short_holds() {
        let scratch = cortexkit_test_support::ScratchDir::new("host-mutex");
        let (journal, snapshot) = crate::journal::FileJournal::open(scratch.path()).unwrap();
        let host = HostMutex::new(Host::new(Default::default(), snapshot, Box::new(journal)));
        let lines = crate::test_logs::capture(|| {
            drop(host.lock_for("short operation").unwrap());
            let mut guard = host.lock_for("journal commit").unwrap();
            guard.held_since -= Duration::from_secs(6);
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("WARN "), "{lines:?}");
        assert!(lines[0].contains("host_mutex_long_hold"), "{lines:?}");
        assert!(
            lines[0].contains("operation=\"journal commit\""),
            "{lines:?}"
        );
        let held_ms: u64 = lines[0]
            .split_whitespace()
            .find_map(|field| field.strip_prefix("held_ms="))
            .unwrap()
            .parse()
            .unwrap();
        assert!(held_ms >= 6000, "{lines:?}");
    }
}
