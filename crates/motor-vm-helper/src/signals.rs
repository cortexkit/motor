//! A service stop must not kill the helper before its tenant VMs are stopped
//! cleanly. SIGTERM and SIGINT are blocked in every helper thread and taken
//! synchronously by one thread, which runs the clean stop before the process
//! exits. Child processes do not inherit the mask: the standard library
//! resets it before exec.
use crate::{Error, Helper, Machine, Result};
use std::sync::{Mutex, PoisonError};

pub struct StopSignals(libc::sigset_t);

impl StopSignals {
    /// Blocks SIGTERM and SIGINT in the calling thread. Threads it spawns
    /// later inherit the mask, so call this before starting any thread;
    /// otherwise a thread without the mask takes the default action and the
    /// process dies with its VMs still running.
    pub fn block() -> Result<Self> {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGINT);
        }
        let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error).into());
        }
        Ok(Self(set))
    }
    /// Waits until SIGTERM or SIGINT is pending and returns its number.
    pub fn wait(&self) -> Result<i32> {
        let mut signal = 0;
        let error = unsafe { libc::sigwait(&self.0, &mut signal) };
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error).into());
        }
        Ok(signal)
    }
}

/// Waits for a stop signal, then stops tenant VMs cleanly. The caller exits
/// the process after this returns. Waiting for the lock lets a request that is
/// in progress, such as a suspend, finish first. A poisoned lock still runs
/// the stop: a panicked request is no reason to cut a guest's power.
pub fn stop_on_signal<M: Machine>(helper: &Mutex<Helper<M>>, signals: &StopSignals) -> Result<i32> {
    let signal = signals.wait()?;
    helper
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .shutdown()
        .map_err(|e| Error(format!("clean stop after signal {signal}: {e}")))?;
    Ok(signal)
}
