//! Durable, instance-scoped remote execution. The Unix transport is also usable
//! in-process: bind a scratch socket and pass a cloned [`Runner`] to [`serve`].
mod backend;
#[cfg(target_os = "linux")]
mod boat_quota;
#[cfg(target_os = "linux")]
mod boat_quota_events;
mod caller_path;
mod cleanup;
#[cfg(any(target_os = "linux", test))]
mod compile_cache;
mod cpu;
mod engine;
mod filesystem;
#[cfg(target_os = "linux")]
mod isolation;
pub mod local_transport;
#[cfg(unix)]
pub mod macos;
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
// Cache records and publication policy are portable; the sandbox transport is
// Unix-only.
#[cfg_attr(any(not(test), not(target_os = "linux")), allow(dead_code))]
mod remote_cache;
mod server;
mod setup_log;
// Nothing in a production build uses sibling snapshots yet: production builds
// keep sibling snapshot mode off, so only tests reach this module.
mod job_memory_budget;
#[cfg_attr(not(test), allow(dead_code))]
mod sibling_snapshots;
mod spool;
mod state_lock;
mod storage;
#[cfg(any(target_os = "linux", test))]
mod systemd;

/// systemd-run properties for every job unit. PID 1 starts these system
/// services, independently of the user manager's protected runner processes.
/// OOMPolicy also stops surviving job descendants. Kept outside `systemd` so
/// the Unix-hosted Windows VM argument builders and portable tests can share
/// the properties, although only a Linux host runs these units.
#[cfg(any(unix, test))]
pub(crate) const JOB_MEMORY_PROPERTIES: [&str; 3] = [
    "--property=Slice=motor-jobs.slice",
    "--property=OOMScoreAdjust=900",
    "--property=OOMPolicy=kill",
];
mod tenant;
#[cfg(target_os = "linux")]
mod tenant_agent;
#[cfg(any(target_os = "linux", test))]
mod tenant_local;
#[cfg(test)]
mod test_cleanup;
#[cfg(all(test, unix))]
mod test_helpers;
#[cfg(test)]
#[path = "../tests/support/process.rs"]
mod test_process;
mod tree_access;
#[cfg(target_os = "linux")]
mod vsock;
mod windows;
mod workspace;
mod workspace_access;
mod writer;
#[cfg(target_os = "linux")]
pub use writer::CacheFill;

pub use backend::BackendKind;
pub use engine::{Config, Runner, BOAT_DISK_RESERVE_BYTES, DEFAULT_FREE_SPACE_FLOOR_BYTES};
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
/// Runs the trusted JavaScript dependency setup with the tools of the role
/// `config` selects (`--tenant-vm`, `--boat-profile`) on an embedded fixture
/// locked with bun.lock, package-lock.json and pnpm-lock.yaml, in the new
/// directory `scratch`. Image builds and qualification run it so that a setup
/// tool missing from where the runner looks fails there, not in a job.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn check_javascript_setup(
    config: &Config,
    scratch: &std::path::Path,
) -> std::io::Result<String> {
    isolation::check_javascript_setup(&isolation::JavascriptTools::for_config(config), scratch)
}
/// Runs a cold-cache online fetch in the tenant's trusted snapshot helper.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn check_tenant_cargo_fetch(scratch: &std::path::Path) -> std::io::Result<()> {
    isolation::check_tenant_cargo_fetch(scratch)
}
/// PID 1 of a Boat job's PID namespace; the job unit
/// execs the installed runner in this mode. Never returns.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub const JOB_PID_INIT_MODE: &str = isolation::pid_init::MODE;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn job_pid_init(args: Vec<std::ffi::OsString>) -> ! {
    isolation::pid_init::run(args)
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
pub fn hyperv_qualify_prep(config: Config) -> std::io::Result<()> {
    let runner = Runner::open_for_qualification(config)?;
    windows::qualify_prep(runner.config())
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
pub fn hyperv_qualify_disconnect(config: Config) -> std::io::Result<()> {
    let runner = Runner::open_for_qualification(config)?;
    windows::qualify_disconnect(runner.config())
}
