//! Shared sandbox paths and Cargo inputs for dependency cache keys.
//! Matching these inputs between motor-cache fills and motor-job readers is necessary for dependency
//! hits; otherwise identical sources produce unrelated cache entries.
//!
//! sccache 0.18.0's Rust key inputs, in order (src/compiler/rust.rs:1493-1597):
//! 1. Cache format version.
//! 2. Compiler shared-library digests.
//! 3. Compiler arguments (cfgs sorted; extern, search paths, check-cfg, out-dir,
//!    diagnostic width and a JSON target's filename excluded).
//! 4. Source-file digests from dep-info.
//! 5. Extern-file digests.
//! 6. Static-library digests.
//! 7. Target JSON contents.
//! 8. Dep-info environment dependencies and CARGO_* variables, except jobserver
//!    flags, registry settings, build-job count and encoded rustflags.
//! 9. Compile cwd, which also winds up in the rlib.
//! 10. Compiler version.
//!
//! Registry/git dependencies compile from their package directory under the
//! shared CARGO_HOME, not from the workspace. Both accounts must see that same
//! cwd and Cargo environment. The writer forces Cargo's local crates incremental,
//! including release builds. The trusted shim strips CARGO_INCREMENTAL and
//! CARGO_BUILD_INCREMENTAL after Cargo has emitted per-crate incremental flags:
//! sccache rejects a literal `1` in either variable even on cacheable dependencies. This also
//! matches reader keys when a job explicitly enables incremental compilation.
//! OUT_DIR matters when dep-info names it
//! as an environment dependency, not simply because the variable is present.
//! Source: <https://github.com/mozilla/sccache/blob/v0.18.0/src/compiler/rust.rs>.
//!
//! Reader misses are aggregate, not per-unit. They request at most one refill per
//! repository/default-branch-tip/descriptor, only when the submitted snapshot's
//! descriptor matches. The persisted bound prevents features or profiles omitted
//! by the warm command from causing endless fills. If the same tip is evicted
//! again after that refill, it stays partly cold until a default-branch change.
//! Descriptors include the repository's warm command, not the job's command,
//! in-command features, rustflags or submitted environment.
use std::{collections::BTreeMap, fs, io, path::Path};

pub(crate) const TARGET: &str = "/run/motor/target";
pub(crate) const CACHE: &str = "/motor-cache";
pub(crate) const CARGO_HOME: &str = "/motor-home/cargo";

/// Format-1 build-base environment key. Frame names as well as values so
/// swapped values, empty values and missing variables cannot alias one another.
#[cfg(target_os = "linux")]
pub(crate) fn environment_fingerprint(env: &BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for (name, value) in env {
        if (name.starts_with("CARGO_")
            || matches!(
                name.as_str(),
                "RUSTFLAGS" | "RUSTDOCFLAGS" | "RUSTC_WRAPPER"
            ))
            && !matches!(name.as_str(), "CARGO_BUILD_JOBS" | "CARGO_INCREMENTAL")
        {
            for input in [name, value] {
                hash.update(
                    u32::try_from(input.len())
                        .expect("environment input exceeds u32")
                        .to_be_bytes(),
                );
                hash.update(input.as_bytes());
            }
        }
    }
    format!("{:x}", hash.finalize())
}

#[cfg(all(test, target_os = "linux"))]
mod fingerprint_tests {
    use super::*;

    #[test]
    fn environment_fingerprint_uses_sorted_length_framed_names_and_values() {
        let env = BTreeMap::from([
            ("RUSTFLAGS".into(), "-C debuginfo=1".into()),
            ("CARGO_Z".into(), "".into()),
            ("CARGO_A".into(), "x".into()),
        ]);
        // Independently encoded as u32 big-endian byte lengths, in name order.
        assert_eq!(
            environment_fingerprint(&env),
            "d781bb0df880ed0548e7bda8d7b04640f855ee280e2e2bd40be953ff63b2c945"
        );
        let mut changed = env.clone();
        changed.insert("CARGO_A".into(), "".into());
        changed.insert("CARGO_Z".into(), "x".into());
        assert_ne!(
            environment_fingerprint(&env),
            environment_fingerprint(&changed)
        );
        changed = env.clone();
        changed.remove("CARGO_Z");
        assert_ne!(
            environment_fingerprint(&env),
            environment_fingerprint(&changed)
        );
        for name in [
            "CARGO_FUTURE_VARIABLE",
            "RUSTFLAGS",
            "RUSTDOCFLAGS",
            "RUSTC_WRAPPER",
        ] {
            changed = env.clone();
            changed.insert(name.into(), "é=value".into());
            assert_ne!(
                environment_fingerprint(&env),
                environment_fingerprint(&changed),
                "{name}"
            );
        }
        for name in [
            "CARGO_BUILD_JOBS",
            "CARGO_INCREMENTAL",
            "RUST_TEST_THREADS",
            "NEXTEST_TEST_THREADS",
            "SCCACHE_BUCKET",
        ] {
            changed = env.clone();
            changed.insert(name.into(), "different".into());
            assert_eq!(
                environment_fingerprint(&env),
                environment_fingerprint(&changed),
                "{name}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn build_directory() -> String {
    format!("{TARGET}/bd/{{workspace-path-hash}}")
}

/// Set the output layout shared by isolated Linux jobs and cache fills.
pub(crate) fn target_environment(env: &mut BTreeMap<String, String>) {
    env.insert("CARGO_TARGET_DIR".into(), TARGET.into());
    // macOS and Windows keep Cargo's existing intermediate-output layout.
    #[cfg(target_os = "linux")]
    env.insert("CARGO_BUILD_BUILD_DIR".into(), build_directory());
}

/// Keep profile defaults identical for fills and readers: sccache hashes CARGO_* values.
/// Explicit job or repository settings win over the space-saving Linux default.
#[cfg(target_os = "linux")]
pub(crate) fn profile_environment(
    env: &mut BTreeMap<String, String>,
    repository: &Path,
    tree: &str,
) {
    if !use_line_only_debug(env, repository, tree) {
        return;
    }
    for key in ["CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"] {
        env.insert(key.into(), "line-tables-only".into());
    }
}

#[cfg(target_os = "linux")]
fn use_line_only_debug(env: &BTreeMap<String, String>, repository: &Path, tree: &str) -> bool {
    if ["CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"]
        .iter()
        .any(|key| env.contains_key(*key))
    {
        return false;
    }
    for file in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
        let Some(text) = snapshot_profile(repository, tree, file) else {
            continue;
        };
        let config: toml::Value = match toml::from_str(&text) {
            Ok(config) => config,
            Err(error) => {
                profile_lookup_failed(file, "invalid_toml");
                tracing::warn!(file, tree, %error, "cannot parse Cargo profile settings; treating file as unset");
                continue;
            }
        };
        // Package-specific debug settings still apply on top of the profile default.
        if ["dev", "test"].iter().any(|profile| {
            config
                .get("profile")
                .and_then(|profiles| profiles.get(*profile))
                .and_then(|profile| profile.get("debug"))
                .is_some()
        }) {
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn profile_lookup_failed(file: &str, reason: &'static str) {
    let source = match file {
        "Cargo.toml" => "manifest",
        ".cargo/config.toml" => "config_toml",
        _ => "legacy_config",
    };
    tracing::info!(source, reason, "Cargo profile lookup failed");
}

#[cfg(target_os = "linux")]
fn snapshot_profile(repository: &Path, tree: &str, file: &str) -> Option<String> {
    const MAX_BYTES: usize = 1024 * 1024;
    let entry = crate::workspace::git_bytes(repository, &["ls-tree", "-lz", tree, "--", file])
        .inspect_err(|_| profile_lookup_failed(file, "git_tree"))
        .ok()?;
    let entry = std::str::from_utf8(&entry)
        .inspect_err(|_| profile_lookup_failed(file, "invalid_utf8"))
        .ok()?;
    let metadata = entry.strip_suffix(&format!("\t{file}\0"))?;
    let mut fields = metadata.split_whitespace();
    let mode = fields.next()?;
    let kind = fields.next()?;
    let _object = fields.next()?;
    let size = fields.next()?.parse::<usize>().ok()?;
    if !matches!(mode, "100644" | "100755")
        || kind != "blob"
        || size > MAX_BYTES
        || fields.next().is_some()
    {
        return None;
    }
    // Immutable regular blobs cannot follow checkout symlinks or grow between the size check
    // and the read. Never open caller-controlled profile paths on the host filesystem.
    let blob = crate::workspace::git_bytes(repository, &["show", &format!("{tree}:{file}")])
        .inspect_err(|_| profile_lookup_failed(file, "git_blob"))
        .ok()?;
    String::from_utf8(blob)
        .inspect_err(|_| profile_lookup_failed(file, "invalid_utf8"))
        .ok()
}

/// sccache hashes CARGO_HOME, CARGO_TARGET_DIR, CARGO_BUILD_BUILD_DIR and
/// CARGO_NET_OFFLINE, as well as profile debug defaults, even for non-incremental dependencies.
/// Changing their presence or value between a fill and a reader loses every hit.
/// Writers enable incremental in Cargo,
/// and the trusted compile shim strips incremental environment variables before
/// sccache sees them, preserving Cargo's per-crate flags.
pub(crate) fn cargo_environment(env: &mut BTreeMap<String, String>, repository: &Path, tree: &str) {
    env.remove("CARGO_INCREMENTAL");
    target_environment(env);
    #[cfg(target_os = "linux")]
    profile_environment(env, repository, tree);
    #[cfg(not(target_os = "linux"))]
    let _ = (repository, tree);
    for (key, value) in [("CARGO_HOME", CARGO_HOME), ("CARGO_NET_OFFLINE", "true")] {
        env.insert(key.into(), value.into());
    }
}

pub(crate) fn reader_environment(
    env: &mut BTreeMap<String, String>,
    repository: &Path,
    tree: &str,
    size_gib: u64,
) {
    cargo_environment(env, repository, tree);
    for (key, value) in [
        ("SCCACHE_DIR", CACHE.to_string()),
        ("SCCACHE_CACHE_SIZE", format!("{size_gib}G")),
        ("SCCACHE_LOCAL_RW_MODE", "READ_ONLY".into()),
        // Only transport/startup failure falls back locally. A compiler's
        // nonzero exit status remains a compiler error, not a retry request.
        ("SCCACHE_IGNORE_SERVER_IO_ERROR", "1".into()),
    ] {
        env.insert(key.into(), value);
    }
}

/// Whether the shared compiler cache at `fleet` may hold compiled entries:
/// whether any one-hex-digit path exists at its top level. sccache 0.18.0
/// stores each entry under such a directory (see `writer::cache`), and the
/// only other directory the runner creates there, `preprocessor/`, holds no
/// Rust entries, so a cache without one is empty. A missing cache is empty.
/// Only the Linux runner has a shared cache to inspect.
#[cfg(target_os = "linux")]
pub(crate) fn holds_entries(fleet: &Path) -> io::Result<bool> {
    for shard in "0123456789abcdef".chars() {
        match fs::symlink_metadata(fleet.join(shard.to_string())) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// Follow Cargo's ancestor search and its preference for config over config.toml.
/// Read data only: never invoke Cargo or a workspace wrapper as the runner.
pub(crate) fn configured_wrapper(workspace: &Path, cwd: &Path) -> io::Result<bool> {
    for directory in cwd
        .ancestors()
        .take_while(|path| path.starts_with(workspace))
    {
        let cargo = directory.join(".cargo");
        let path = if cargo.join("config").is_file() {
            cargo.join("config")
        } else {
            cargo.join("config.toml")
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let config: toml::Value = match toml::from_str(&text) {
            Ok(config) => config,
            // Do not mask a malformed configuration by installing an override;
            // Cargo will report the original configuration error in the job.
            Err(_) => return Ok(true),
        };
        if let Some(wrapper) = config.get("build").and_then(|v| v.get("rustc-wrapper")) {
            return Ok(wrapper.as_str().is_none_or(|value| !value.is_empty()));
        }
    }
    Ok(false)
}

#[cfg(all(test, target_os = "linux"))]
mod profile_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn reader_and_writer_have_identical_hashed_cargo_inputs() {
        let scratch = ScratchDir::new("cargo-input-parity");
        let mut writer = BTreeMap::new();
        cargo_environment(&mut writer, scratch.path(), "");
        let mut reader = BTreeMap::from([
            ("CARGO_HOME".into(), "/operator/cargo".into()),
            ("CARGO_TARGET_DIR".into(), "/workspace/target".into()),
            #[cfg(target_os = "linux")]
            ("CARGO_BUILD_BUILD_DIR".into(), "/workspace/build".into()),
            ("CARGO_NET_OFFLINE".into(), "false".into()),
            ("CARGO_INCREMENTAL".into(), "1".into()),
        ]);
        reader_environment(&mut reader, scratch.path(), "", 17);
        reader.retain(|key, _| key.starts_with("CARGO_"));
        assert_eq!(reader, writer);
        assert_eq!(reader["CARGO_TARGET_DIR"], "/run/motor/target");
        #[cfg(target_os = "linux")]
        assert_eq!(
            reader["CARGO_BUILD_BUILD_DIR"],
            "/run/motor/target/bd/{workspace-path-hash}"
        );
        assert_eq!(reader["CARGO_HOME"], "/motor-home/cargo");
        assert_eq!(reader["CARGO_NET_OFFLINE"], "true");
        assert!(!reader.contains_key("CARGO_INCREMENTAL"));
    }

    #[test]
    fn reader_disk_configuration_is_complete_and_read_only() {
        let scratch = ScratchDir::new("cargo-reader-environment");
        let mut env = BTreeMap::new();
        reader_environment(&mut env, scratch.path(), "", 17);
        assert_eq!(env["SCCACHE_DIR"], "/motor-cache");
        assert_eq!(env["SCCACHE_CACHE_SIZE"], "17G");
        assert_eq!(env["SCCACHE_LOCAL_RW_MODE"], "READ_ONLY");
        assert_eq!(env["SCCACHE_IGNORE_SERVER_IO_ERROR"], "1");
    }

    #[test]
    fn cargo_wrapper_configuration_obeys_ancestor_and_legacy_precedence() {
        let scratch = ScratchDir::new("cargo-wrapper-config");
        let root = scratch.path();
        let nested = root.join("member");
        fs::create_dir_all(nested.join(".cargo")).unwrap();
        fs::create_dir_all(root.join(".cargo")).unwrap();
        assert!(!configured_wrapper(root, &nested).unwrap());
        fs::write(
            root.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper='root'\n",
        )
        .unwrap();
        assert!(configured_wrapper(root, &nested).unwrap());
        fs::write(
            nested.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper=''\n",
        )
        .unwrap();
        assert!(!configured_wrapper(root, &nested).unwrap());
        fs::write(
            nested.join(".cargo/config"),
            "[build]\nrustc-wrapper='legacy'\n",
        )
        .unwrap();
        assert!(configured_wrapper(root, &nested).unwrap());
        fs::write(nested.join(".cargo/config"), "not valid toml").unwrap();
        assert!(configured_wrapper(root, &nested).unwrap());
    }
}

#[cfg(all(test, unix))]
mod wrapper_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::process::Command;

    fn run(
        available: bool,
        compiler_exit: i32,
        administrative: bool,
        already_running: bool,
    ) -> (i32, String) {
        let scratch = ScratchDir::new("sccache-startup-fallback");
        let wrapper = scratch.join("sccache");
        let backend = scratch.join("backend");
        let compiler = scratch.join("rustc");
        let events = scratch.join("events");
        crate::test_helpers::write_test_executable(
            &wrapper,
            include_str!("sccache-wrapper.sh").as_bytes(),
        )
        .unwrap();
        fs::write(
            wrapper.with_extension("backend"),
            backend.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let ready = wrapper.with_extension("ready");
        let state = scratch.join("running");
        if already_running {
            fs::write(&state, "").unwrap();
        }
        crate::test_helpers::write_test_executable(&ready, b"#!/bin/bash\ntest -f \"$STATE\"\n")
            .unwrap();
        crate::test_helpers::write_test_executable(
            &backend,
            format!("#!/bin/bash\ncase \"$1\" in\n--start-server) echo startup >> \"$EVENTS\"; {}; exit {};;\n--show-stats) echo stats >> \"$EVENTS\"; exit 0;;\n*) if [[ -n ${{CARGO_INCREMENTAL+x}} || -n ${{CARGO_BUILD_INCREMENTAL+x}} ]]; then exit 77; fi; echo cached >> \"$EVENTS\"; exec \"$@\";;\nesac\n", if available {"touch \"$STATE\""} else {":"}, if available {0} else {2}).as_bytes(),
        )
        .unwrap();
        crate::test_helpers::write_test_executable(
            &compiler,
            format!("#!/bin/bash\necho compiler >> \"$EVENTS\"\nexit {compiler_exit}\n").as_bytes(),
        )
        .unwrap();
        let mut command = Command::new(&wrapper);
        command
            .env("EVENTS", &events)
            .env("STATE", &state)
            .env("CARGO_INCREMENTAL", "1")
            .env("CARGO_BUILD_INCREMENTAL", "1");
        if administrative {
            command.arg("--show-stats");
        } else {
            command.arg(&compiler);
        }
        let output = command.output().unwrap();
        (
            output.status.code().unwrap(),
            fs::read_to_string(events).unwrap(),
        )
    }

    #[test]
    fn unavailable_cache_server_runs_compiler_once() {
        assert_eq!(
            run(false, 0, false, false),
            (0, "startup\ncompiler\n".into())
        );
    }

    #[test]
    fn genuine_cached_compiler_failure_is_not_retried() {
        assert_eq!(
            run(true, 1, false, false),
            (1, "startup\ncached\ncompiler\n".into())
        );
    }

    #[test]
    fn genuine_uncached_compiler_failure_is_not_retried() {
        assert_eq!(
            run(false, 1, false, false),
            (1, "startup\ncompiler\n".into())
        );
    }

    #[test]
    fn running_server_is_reused_without_restarting() {
        assert_eq!(run(true, 0, false, true), (0, "cached\ncompiler\n".into()));
    }

    #[test]
    fn incremental_variables_are_stripped_for_cached_compiles_only() {
        assert_eq!(run(true, 0, false, true), (0, "cached\ncompiler\n".into()));
        assert_eq!(
            run(true, 0, false, false),
            (0, "startup\ncached\ncompiler\n".into())
        );
    }

    #[test]
    fn cache_administration_is_forwarded_without_compiler_or_startup() {
        assert_eq!(run(true, 0, true, false), (0, "stats\n".into()));
    }
}
