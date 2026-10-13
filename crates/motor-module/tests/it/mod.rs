// In-process tests live here so they can exercise private transfer machinery
// without exposing fault injection in the production module's configuration.
use std::{io, time::Duration};

use crate::Motor;

// A spawned task can outlive its Motor's drop and release the storage lease
// later, so reopening that same module state must briefly wait for the lease.
pub(crate) async fn reopen_motor(open: impl FnMut() -> io::Result<Motor>) -> Motor {
    tokio::time::timeout(Duration::from_secs(5), async move {
        let mut open = open;
        loop {
            match open() {
                Ok(motor) => break motor,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("reopen module storage: {error}"),
            }
        }
    })
    .await
    .expect("timed out waiting for the previous Motor's storage lease")
}

mod compatibility;
mod detach;
mod git_dependencies;
mod health;
mod operator_cli;
pub(crate) mod placement;
mod recovery;
mod routing;
mod safety;
mod started;
mod workspace_capture;
