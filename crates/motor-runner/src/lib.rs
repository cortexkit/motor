//! Durable, instance-scoped remote execution. The Unix transport is also usable
//! in-process: bind a scratch socket and pass a cloned [`Runner`] to [`serve`].
mod backend;
mod cleanup;
#[cfg(any(target_os = "linux", test))]
mod compile_cache;
mod cpu;
mod engine;
#[cfg(target_os = "linux")]
mod isolation;
pub mod network;
mod platform;
mod server;
mod spool;
mod storage;
#[cfg(any(target_os = "linux", test))]
mod systemd;
#[cfg(test)]
mod test_cleanup;
#[cfg(test)]
#[path = "../tests/support/process.rs"]
mod test_process;
mod tree_access;
mod windows;
mod workspace;
mod writer;
#[cfg(target_os = "linux")]
pub use writer::CacheFill;

pub use backend::BackendKind;
pub use engine::{Config, Runner, DEFAULT_FREE_SPACE_FLOOR_BYTES};
pub use server::serve;
pub use storage::{JobState, REPLAY_BYTES, RETENTION_MS};

#[doc(hidden)]
pub fn windows_prepare(path: &std::path::Path) -> std::io::Result<()> {
    windows::prepare_entry(path)
}

pub use motor_protocol::allowed_env;
