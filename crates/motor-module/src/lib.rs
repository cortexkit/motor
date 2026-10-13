//! Mac-side executor. Submitted commands only ever reach the remote runner;
//! transport loss after submission is recovered by attach, never by rerunning.
mod agent;
#[cfg(feature = "bench-client")]
pub mod benchmark;
pub mod cli;
mod config;
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

pub fn allowed_env(name: &str) -> bool {
    (name.starts_with("RUST_")
        || matches!(name, "RUSTFLAGS" | "RUSTDOCFLAGS")
        || name.starts_with("CARGO_")
        || name.starts_with("NEXTEST_")
        || matches!(name, "CI" | "NO_COLOR" | "CLICOLOR" | "CLICOLOR_FORCE"))
        && !matches!(
            name,
            "CARGO_HOME" | "CARGO_INSTALL_ROOT" | "CARGO_TARGET_DIR"
        )
        && (!name.starts_with("CARGO_BUILD_") || name == "CARGO_BUILD_JOBS")
        && !["TOKEN", "SECRET", "PASSWORD", "KEY", "CREDENTIAL"]
            .iter()
            .any(|s| name.contains(s))
}

#[cfg(test)]
#[path = "../tests/it/mod.rs"]
mod tests;
