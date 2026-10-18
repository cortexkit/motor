//! Unprivileged tenant admission and durable relay ownership.

pub mod billing;
pub mod broker;
pub mod helper;
pub mod journal;
pub mod server;
pub mod state;

#[cfg(test)]
extern crate self as motor_host;
#[cfg(test)]
#[path = "../tests/it/mod.rs"]
mod tests;
