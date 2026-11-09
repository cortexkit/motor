//! Unprivileged tenant admission and durable relay ownership.

pub mod billing;
pub mod broker;
pub mod capacity;
pub mod claim_log;
pub mod closure;
pub mod config;
pub mod enroll;
pub mod helper;
pub mod ingress;
pub mod jobs;
pub mod journal;
mod lost_diagnostics;
pub mod metrics;
mod relay;
mod replay;
pub mod sandbox;
pub mod server;
pub mod state;
pub mod store;
pub mod wire;

#[cfg(test)]
extern crate self as motor_host;
#[cfg(test)]
#[path = "../tests/it/mod.rs"]
mod tests;
