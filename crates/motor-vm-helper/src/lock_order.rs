//! Debug-only lock ordering catches inversions before they can deadlock.
use std::marker::PhantomData;

// G drains job lifecycle requests before shutdown; H protects tenant/legacy
// Helper state. Acquire those before reservation admission A, job writer J,
// network flock N and view registry R, in that order. R is a memory-only leaf.
// The separate connection gate C protects peer authorization, not VM work;
// drop it before entering an ordered lock. Check before waiting, because an
// inverted acquisition can deadlock before it wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Lock {
    G,
    H,
    A,
    J,
    N,
    R,
    C,
}

#[cfg(debug_assertions)]
thread_local! {
    static HELD: std::cell::RefCell<Vec<Lock>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) struct Held {
    #[cfg(debug_assertions)]
    lock: Lock,
    // A held lock belongs to this thread, including the network flock guard.
    _thread: PhantomData<*mut ()>,
}

pub(crate) fn enter(lock: Lock) -> Held {
    #[cfg(debug_assertions)]
    HELD.with(|held| {
        let mut held = held.borrow_mut();
        assert!(
            held.iter()
                .all(|previous| *previous < lock && *previous != Lock::C && lock != Lock::C),
            "lock order violation: holding {held:?}, acquiring {lock:?}"
        );
        held.push(lock);
    });
    let _ = lock;
    Held {
        #[cfg(debug_assertions)]
        lock,
        _thread: PhantomData,
    }
}

pub(crate) fn assert_held(lock: Lock) {
    #[cfg(debug_assertions)]
    HELD.with(|held| {
        assert!(
            held.borrow().contains(&lock),
            "required lock {lock:?} is not held"
        )
    });
    let _ = lock;
}

impl Drop for Held {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            let index = held.iter().position(|lock| *lock == self.lock).unwrap();
            held.remove(index);
        });
    }
}

pub(crate) struct Mutex<T> {
    inner: std::sync::Mutex<T>,
    order: Lock,
}

impl<T> Mutex<T> {
    pub(crate) fn new(order: Lock, value: T) -> Self {
        Self {
            inner: std::sync::Mutex::new(value),
            order,
        }
    }

    pub(crate) fn lock(&self) -> Guard<'_, T> {
        let held = enter(self.order);
        let guard = self.inner.lock().unwrap();
        Guard { guard, _held: held }
    }

    pub(crate) fn try_lock(&self) -> Option<Guard<'_, T>> {
        let held = enter(self.order);
        match self.inner.try_lock() {
            Ok(guard) => Some(Guard { guard, _held: held }),
            Err(std::sync::TryLockError::Poisoned(error)) => panic!("{error}"),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

pub(crate) struct Guard<'a, T> {
    guard: std::sync::MutexGuard<'a, T>,
    _held: Held,
}

pub(crate) fn mutex<T>(mutex: &std::sync::Mutex<T>, order: Lock) -> Guard<'_, T> {
    let held = enter(order);
    let guard = mutex.lock().unwrap();
    Guard { guard, _held: held }
}

pub(crate) fn mutex_recover<T>(mutex: &std::sync::Mutex<T>, order: Lock) -> Guard<'_, T> {
    let held = enter(order);
    let guard = mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Guard { guard, _held: held }
}

impl<T> std::ops::Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}
impl<T> std::ops::DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}
