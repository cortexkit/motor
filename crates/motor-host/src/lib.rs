//! Unprivileged tenant admission and durable relay ownership.

pub mod billing;
pub mod broker;
pub mod closure;
pub mod enroll;
pub mod helper;
pub mod ingress;
pub mod journal;
mod relay;
pub mod sandbox;
pub mod server;
pub mod state;
pub mod wire;

#[cfg(test)]
extern crate self as motor_host;
#[cfg(test)]
#[path = "../tests/it/mod.rs"]
mod tests;
