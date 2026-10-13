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

/// Apply the outbound allow-list again at the execution boundary.
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
