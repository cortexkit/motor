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
#[cfg(target_os = "linux")]
mod permissions;
mod platform;
mod return_paths;
mod server;
mod spool;
mod storage;
#[cfg(any(target_os = "linux", test))]
mod systemd;
mod tenant;
#[cfg(target_os = "linux")]
mod tenant_agent;
#[cfg(any(target_os = "linux", test))]
mod tenant_local;
#[cfg(test)]
mod test_cleanup;
#[cfg(test)]
#[path = "../tests/support/process.rs"]
mod test_process;
mod tree_access;
#[cfg(target_os = "linux")]
mod vsock;
mod windows;
mod workspace;
mod writer;
#[cfg(target_os = "linux")]
pub use writer::CacheFill;

pub use backend::BackendKind;
pub use engine::{Config, Runner, DEFAULT_FREE_SPACE_FLOOR_BYTES};
pub use server::serve;
pub use storage::{JobState, REPLAY_BYTES, RETENTION_MS};
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn physical_chmod(args: &[String]) -> std::io::Result<()> {
    permissions::run(args)
}
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn permission_helper_digest() -> String {
    permissions::digest()
}
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn check_permissions_helper() -> std::io::Result<()> {
    permissions::check_install()
}
#[cfg(target_os = "linux")]
pub use tenant::serve_tenant;
#[cfg(target_os = "linux")]
pub use tenant_agent::serve_agent;
#[cfg(target_os = "linux")]
pub use tenant_local::serve_tenant_local;

#[doc(hidden)]
pub fn windows_prepare(path: &std::path::Path) -> std::io::Result<()> {
    windows::prepare_entry(path)
}

pub use motor_protocol::allowed_env;
