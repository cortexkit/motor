//! Client-side executor. Submitted commands only ever reach the remote runner;
//! transport loss after submission is recovered by attach, never by rerunning.
mod agent;
mod agent_route;
#[cfg(feature = "bench-client")]
pub mod benchmark;
pub mod cli;
mod config;
mod default_tip;
mod diagnostics;
mod engram_catalog;
mod health_cache;
mod hosted;
pub mod logging;
pub mod module;
mod motor;
mod return_paths;
mod snapshot;
mod storage;
pub use return_paths::ReturnOptions;
mod transport;
mod wsl;

pub use config::{BrokerTokenSource, Config, MacosRunner, WindowsRunner};
pub use hosted::account::AccountTokens;
pub use hosted::{BillingStatus, HostedJobState};
pub use motor::{JobStream, Motor};
pub use transport::{Connector, Duplex, SshConnector};
pub use transport::{RUNNER_CREDENTIAL_CATEGORY, RUNNER_CREDENTIAL_ID};

pub use motor_protocol::allowed_env;

mod direct_secret;
#[cfg(test)]
mod macos_tests;
#[cfg(test)]
#[path = "../tests/it/mod.rs"]
mod tests;
