/// Request environment names permitted at both submission and execution.
/// Runner-owned paths and credentials must not be overridden by a request.
pub fn allowed_env(name: &str) -> bool {
    (name.starts_with("RUST_")
        || matches!(name, "RUSTFLAGS" | "RUSTDOCFLAGS")
        || name.starts_with("CARGO_")
        || name.starts_with("NEXTEST_")
        || matches!(
            name,
            "CI" | "NO_COLOR" | "CLICOLOR" | "CLICOLOR_FORCE" | "MOTOR_RUNNER_INSTANCE"
        ))
        && !matches!(
            name,
            "CARGO_HOME" | "CARGO_INSTALL_ROOT" | "CARGO_TARGET_DIR"
        )
        && (!name.starts_with("CARGO_BUILD_") || name == "CARGO_BUILD_JOBS")
        && !["TOKEN", "SECRET", "PASSWORD", "KEY", "CREDENTIAL"]
            .iter()
            .any(|s| name.contains(s))
}
