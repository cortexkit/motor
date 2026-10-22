//! Durable, instance-scoped remote execution. The Unix transport is also usable
//! in-process: bind a scratch socket and pass a cloned [`Runner`] to [`serve`].
mod backend;
mod cleanup;
#[cfg(any(target_os = "linux", test))]
mod compile_cache;
mod cpu;
mod engine;
mod filesystem;
#[cfg(target_os = "linux")]
mod isolation;
pub mod local_transport;
#[cfg(windows)]
mod native_process;
pub mod network;
#[cfg(target_os = "linux")]
mod permissions;
mod platform;
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod privilege;
mod return_paths;
mod server;
mod setup_log;
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
#[cfg(target_os = "linux")]
pub use privilege::run_helpers_directly_as_root;
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

/// Serves one tenant job connection whose first frame is motor-host's
/// `verified_lease`, exactly as the vsock and local tenant listeners do.
/// Those listeners need a tenant VM or root-owned socket directories, so
/// hosted-client tests use this to drive the real tenant boundary in process.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub async fn serve_tenant_connection<S>(runner: Runner, socket: S) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    server::tenant_channel(runner, socket).await
}

#[doc(hidden)]
#[cfg(unix)]
#[cfg(unix)]
pub fn windows_prepare(path: &std::path::Path) -> std::io::Result<()> {
    windows::prepare_entry(path)
}

pub use motor_protocol::allowed_env;

#[cfg(windows)]
#[doc(hidden)]
pub fn request_windows_service_stop() {
    windows::request_service_stop();
}

#[cfg(windows)]
#[doc(hidden)]
pub fn hyperv_probe_peers() -> std::io::Result<()> {
    windows::probe_peers()
}

#[cfg(windows)]
#[doc(hidden)]
pub fn hyperv_qualify_prep_offline_cargo(config: Config) -> std::io::Result<()> {
    windows::qualify_prep_offline_cargo(config)
}
#[cfg(windows)]
#[doc(hidden)]
pub fn hyperv_qualify_prep_local_registry(config: Config) -> std::io::Result<()> {
    windows::qualify_prep_local_registry(config)
}
#[cfg(windows)]
#[doc(hidden)]
pub fn hyperv_qualify_disconnect(config: &Config) -> std::io::Result<()> {
    windows::qualify_disconnect(config)
}
