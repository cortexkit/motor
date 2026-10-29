//! Hosted runner credentials and a deterministic broker test control.
//!
//! The library performs no I/O. Callers supply clocks, keys, nonces and request
//! bytes; fetching JWKS and persisting counters belong to the host agent. Only
//! the `motor-fake-broker` test executable opens a network listener.

pub mod auth;
pub mod fake;
pub mod metrics;
pub mod types;

pub use auth::*;
pub use types::*;

#[cfg(test)]
extern crate self as motor_lease;
#[cfg(test)]
#[path = "../tests/it/mod.rs"]
mod tests;
