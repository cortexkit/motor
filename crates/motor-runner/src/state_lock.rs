//! The runner's global state lock, measured. Every job admission, status
//! reply and cancel waits for this lock, so one caller holding it across slow
//! work (an fsync, a process spawn, a directory removal) delays the whole
//! fleet. Each guard records where it was taken and how long it was held; the
//! longest hold since the last report is kept so the scheduler can log it once
//! a minute, and every hold over `SLOW_HOLD` is logged at debug level.

use std::{
    ops::{Deref, DerefMut},
    panic::Location,
    sync::{
        atomic::{AtomicU64, Ordering},
        LockResult, Mutex, MutexGuard, PoisonError, TryLockError, TryLockResult,
    },
    time::{Duration, Instant},
};

/// Holds at least this long are logged and counted.
pub(crate) const SLOW_HOLD: Duration = Duration::from_millis(100);

pub(crate) struct StateLock<T> {
    mutex: Mutex<T>,
    /// Microseconds of the longest hold since the last report. Read without
    /// `longest` so the common, short hold costs one atomic load on release.
    longest_us: AtomicU64,
    longest: Mutex<Option<LongestHold>>,
    slow_holds: AtomicU64,
    /// Every hold by call site, for load tests that rank the holders.
    #[cfg(test)]
    sites: Mutex<std::collections::BTreeMap<(&'static str, u32), SiteHolds>>,
}

/// Hold times of one call site, kept only in test builds.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SiteHolds {
    pub(crate) count: u64,
    pub(crate) total: Duration,
    pub(crate) longest: Duration,
}

/// The longest hold of the state lock in a reporting period.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LongestHold {
    /// Where the guard was taken (file and line of the `lock` call).
    pub(crate) site: &'static Location<'static>,
    pub(crate) held: Duration,
    /// Holds of at least `SLOW_HOLD` in the same period.
    pub(crate) slow_holds: u64,
}

pub(crate) struct StateGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    lock: &'a StateLock<T>,
    site: &'static Location<'static>,
    acquired: Instant,
}

impl<T> StateLock<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            mutex: Mutex::new(value),
            longest_us: AtomicU64::new(0),
            longest: Mutex::new(None),
            slow_holds: AtomicU64::new(0),
            #[cfg(test)]
            sites: Mutex::default(),
        }
    }

    /// Same contract as `Mutex::lock`; the guard remembers the caller.
    #[track_caller]
    pub(crate) fn lock(&self) -> LockResult<StateGuard<'_, T>> {
        let site = Location::caller();
        match self.mutex.lock() {
            Ok(guard) => Ok(self.guard(guard, site)),
            Err(poisoned) => Err(PoisonError::new(self.guard(poisoned.into_inner(), site))),
        }
    }

    /// Same contract as `Mutex::try_lock`.
    #[track_caller]
    pub(crate) fn try_lock(&self) -> TryLockResult<StateGuard<'_, T>> {
        let site = Location::caller();
        match self.mutex.try_lock() {
            Ok(guard) => Ok(self.guard(guard, site)),
            Err(TryLockError::Poisoned(poisoned)) => Err(TryLockError::Poisoned(PoisonError::new(
                self.guard(poisoned.into_inner(), site),
            ))),
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
        }
    }

    fn guard<'a>(
        &'a self,
        guard: MutexGuard<'a, T>,
        site: &'static Location<'static>,
    ) -> StateGuard<'a, T> {
        StateGuard {
            guard,
            lock: self,
            site,
            acquired: Instant::now(),
        }
    }

    /// The longest hold since the previous call, if any hold reached
    /// `SLOW_HOLD`, and start a new period.
    pub(crate) fn take_longest(&self) -> Option<LongestHold> {
        let mut longest = self.longest.lock().unwrap();
        self.longest_us.store(0, Ordering::Relaxed);
        let slow_holds = self.slow_holds.swap(0, Ordering::Relaxed);
        longest
            .take()
            .filter(|hold| hold.held >= SLOW_HOLD)
            .map(|hold| LongestHold { slow_holds, ..hold })
    }

    /// Hold times by call site (file, line), longest total first.
    #[cfg(test)]
    pub(crate) fn site_holds(&self) -> Vec<((&'static str, u32), SiteHolds)> {
        let mut sites: Vec<_> = self
            .sites
            .lock()
            .unwrap()
            .iter()
            .map(|(site, holds)| (*site, *holds))
            .collect();
        sites.sort_by_key(|(_, holds)| std::cmp::Reverse(holds.total));
        sites
    }

    fn record(&self, site: &'static Location<'static>, held: Duration) {
        #[cfg(test)]
        {
            let mut sites = self.sites.lock().unwrap();
            let holds = sites.entry((site.file(), site.line())).or_default();
            holds.count += 1;
            holds.total += held;
            holds.longest = holds.longest.max(held);
        }
        if held >= SLOW_HOLD {
            self.slow_holds.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(%site, held_ms = held.as_millis() as u64, "state lock held");
        }
        let micros = held.as_micros().min(u64::MAX as u128) as u64;
        if micros <= self.longest_us.load(Ordering::Relaxed) {
            return;
        }
        let mut longest = self.longest.lock().unwrap();
        if longest.is_none_or(|current| held > current.held) {
            *longest = Some(LongestHold {
                site,
                held,
                slow_holds: 0,
            });
            self.longest_us.store(micros, Ordering::Relaxed);
        }
    }
}

impl<T> Deref for StateGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for StateGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for StateGuard<'_, T> {
    fn drop(&mut self) {
        // Runs before the inner guard unlocks, so the measured hold includes
        // everything the caller did with the lock.
        self.lock.record(self.site, self.acquired.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_hold_names_the_call_site_and_resets_each_period() {
        let lock = StateLock::new(0u32);
        let short = lock.lock().unwrap();
        drop(short);
        assert!(
            lock.take_longest().is_none(),
            "a short hold is not reported"
        );
        let line = line!() + 1;
        let mut held = lock.lock().unwrap();
        *held += 1;
        // Back-date the acquisition instead of sleeping: the guard measures
        // from `acquired`, so this is a hold of exactly that length.
        held.acquired -= SLOW_HOLD * 3;
        drop(held);
        let mut other = lock.lock().unwrap();
        other.acquired -= SLOW_HOLD;
        drop(other);
        let longest = lock.take_longest().expect("slow hold reported");
        assert_eq!(longest.site.line(), line);
        assert_eq!(longest.site.file(), file!());
        assert!(longest.held >= SLOW_HOLD * 3);
        assert_eq!(longest.slow_holds, 2);
        assert!(lock.take_longest().is_none(), "each period starts empty");
        assert_eq!(*lock.lock().unwrap(), 1);
    }

    #[test]
    fn try_lock_keeps_the_mutex_contract() {
        let lock = StateLock::new(());
        let held = lock.lock().unwrap();
        assert!(matches!(lock.try_lock(), Err(TryLockError::WouldBlock)));
        drop(held);
        assert!(lock.try_lock().is_ok());
    }
}
