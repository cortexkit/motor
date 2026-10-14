//! Mac-side executor. Submitted commands only ever reach the remote runner;
//! transport loss after submission is recovered by attach, never by rerunning.
mod agent;
#[cfg(feature = "bench-client")]
pub mod benchmark;
pub mod cli;
mod config;
mod default_tip;
mod diagnostics;
pub mod logging;
pub mod module;
mod motor;
mod snapshot;
mod storage;
mod transport;

pub use config::Config;
pub use motor::{JobStream, Motor};
pub use transport::{Connector, Duplex, SshConnector};
pub use transport::{RUNNER_CREDENTIAL_CATEGORY, RUNNER_CREDENTIAL_ID};

pub use motor_protocol::allowed_env;

#[cfg(test)]
#[path = "../tests/it/mod.rs"]
mod tests;
