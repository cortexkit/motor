//! Rust reader environment contracts exercised through the public job protocol.
//! Ignored operator tests require MOTOR_READER_SCCACHE_SIZE_GIB for the fixture's cache size.
#![cfg(target_os = "linux")]
use super::javascript::cleanup::OperatorScratch;
use super::{
    connect::SocketScratch,
    javascript::{
        checked_restore_command, drain_fixture_runner, git, snapshot_bundle, snapshot_run,
        submit_with_capability,
    },
};
use motor_protocol::{Outcome, Uuid};
use motor_runner::{Config, Runner};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};
use tokio::net::UnixListener;

fn write(path: impl AsRef<Path>, text: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn script(path: &Path, text: &str) {
    write(path, text);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn rust_source(root: &Path, supervisor: bool, configured: bool) {
    write(
        root.join("Cargo.toml"),
        "[package]\nname='reader-fixture'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(
        root.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = 'reader-fixture'\nversion = '0.1.0'\n",
    );
    write(
        root.join("src/main.rs"),
        "fn main() { println!(\"built\"); }\n",
    );
    for label in ["configured", "exported"] {
        script(
            &root.join(format!("{label}.sh")),
            &format!("#!/bin/bash\nprintf '%s\\n' {label} >> target/wrappers.log\nexec \"$@\"\n"),
        );
    }
    if supervisor {
        // Only presence is submitted. The runner compiles its own embedded
        // supervisor source rather than executing uploaded Rust as the daemon.
        write(
            root.join("crates/prefrontal-build-slot/src/main.rs"),
            "presence sentinel\n",
        );
    }
    if configured {
        write(
            root.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper='./configured.sh'\n",
        );
    }
}

struct ClientFixture {
    scratch: OperatorScratch,
    source: PathBuf,
    socket_scratch: SocketScratch,
    socket: PathBuf,
    runner: Runner,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    jobs: Vec<Uuid>,
}

impl ClientFixture {
    fn writer_diagnostics(&self) -> String {
        fs::read_to_string(self.runner.config().state_dir().join("catalog.json"))
            .unwrap_or_else(|e| format!("cannot read writer catalog: {e}"))
    }
    async fn new() -> Self {
        Self::with_warm_command("true").await
    }

    async fn with_warm_command(warm: &str) -> Self {
        let mut scratch = OperatorScratch::new("rust-reader-public");
        let source = scratch.join("source");
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q"]);
        let mut config = Config::new(scratch.join("state"), "reader");
        config.workspace_roots = vec![scratch.join("workspaces")];
        config.owned_workspace_roots = vec![scratch.path().to_path_buf()];
        config.free_space_floor_bytes = 0;
        config.isolate_jobs = true;
        config.base_commands = vec![warm.into()];
        config.build_slot_root = scratch.join("slots");
        config.rustc_slots = 1;
        config.sccache_size_gib = std::env::var("MOTOR_READER_SCCACHE_SIZE_GIB")
            .expect("set MOTOR_READER_SCCACHE_SIZE_GIB for the operator fixture")
            .parse()
            .expect("cache size must be an integer GiB value");
        assert!(config.sccache_size_gib > 0, "cache size must be positive");
        let socket_scratch = SocketScratch::new();
        let socket = socket_scratch.path().join("reader.sock");
        let runner = Runner::open(config).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(motor_runner::serve(runner.clone(), listener));
        scratch.attach(&runner, server.abort_handle());
        Self {
            scratch,
            source,
            socket_scratch,
            socket,
            runner,
            server,
            jobs: Vec::new(),
        }
    }

    async fn run(&mut self, name: &str, command: &str, expected: i32) -> String {
        self.run_capable(name, command, expected, false).await
    }

    async fn run_capable(
        &mut self,
        name: &str,
        command: &str,
        expected: i32,
        capable: bool,
    ) -> String {
        git(&self.source, &["add", "."]);
        git(
            &self.source,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                name,
                "--allow-empty",
            ],
        );
        let commit = git(&self.source, &["rev-parse", "HEAD"]);
        let tree = git(&self.source, &["rev-parse", "HEAD^{tree}"]);
        let bundle = snapshot_bundle(&self.source, &self.scratch.join("snapshot.bundle"));
        let workspace = self.scratch.join("workspaces").join(name);
        let run = snapshot_run(&workspace, &self.source, &commit, &tree, command, &bundle);
        self.jobs.push(run.job_id);
        let (terminal, output) = submit_with_capability(&self.socket, run, &bundle, capable).await;
        eprintln!("{name}: {:?}\n{output}", terminal.outcome);
        assert_eq!(
            terminal.outcome,
            Outcome::Exit { code: expected },
            "{name}: {output}"
        );
        output
    }

    async fn finish(mut self) {
        let _instance_lock = drain_fixture_runner(
            self.runner,
            self.server,
            &self.jobs,
            Some(&mut self.scratch),
        )
        .await;
        checked_restore_command(
            Command::new("sudo")
                .args(["-n", "chown", "-R", "-P", "--"])
                .arg(format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
                    libc::getegid()
                }))
                .arg(self.scratch.path()),
        );
        checked_restore_command(
            Command::new("sudo")
                .args(["-n", "chmod", "-R", "-P", "u+rwX", "--"])
                .arg(self.scratch.path()),
        );
        drop(self.socket_scratch);
    }
}

#[tokio::test]
#[ignore = "requires trusted operator, provisioned cache/job accounts, sccache 0.18.0, ACLs, sudo and systemd"]
async fn isolated_public_reader_wrapper_precedence_matrix() {
    for supervisor in [false, true] {
        for configured in [false, true] {
            let mut f = ClientFixture::new().await;
            rust_source(&f.source, supervisor, configured);
            for export in ["none", "exported", "empty"] {
                let cache_size = f.runner.config().sccache_size_gib;
                let mut command = format!("set -e\ntest \"$CARGO_TARGET_DIR\" = /run/motor/target\ntest ./target -ef /run/motor/target\ntest \"$SCCACHE_DIR\" = /motor-cache\ntest \"$SCCACHE_CACHE_SIZE\" = {cache_size}G\ntest \"$SCCACHE_LOCAL_RW_MODE\" = READ_ONLY\n");
                if supervisor {
                    command.push_str("test \"$RUSTC_WRAPPER\" = /motor-tools/prefrontal-build-slot\ntest \"$(cat \"$RUSTC_WRAPPER.cache\")\" = \"$(command -v sccache)\"\n");
                } else if configured {
                    command.push_str("test -z \"${RUSTC_WRAPPER+x}\"\n");
                } else {
                    command.push_str("test \"$RUSTC_WRAPPER\" = sccache\n");
                }
                match export {
                    "exported" => command.push_str("export RUSTC_WRAPPER=$PWD/exported.sh\n"),
                    "empty" => command.push_str("export RUSTC_WRAPPER=\n"),
                    _ => {}
                }
                command.push_str("cargo build --locked --offline\ntest \"$(./target/debug/reader-fixture)\" = built\n");
                if export == "exported" || (export == "none" && configured && !supervisor) {
                    let sentinel = if export == "exported" {
                        "exported"
                    } else {
                        "configured"
                    };
                    command.push_str(&format!("test -s target/wrappers.log\ntest \"$(sort -u target/wrappers.log)\" = {sentinel}\n"));
                } else {
                    command.push_str("test ! -e target/wrappers.log\n");
                }
                if export == "none" && (supervisor || !configured) {
                    command.push_str("sccache --show-stats > target/stats\ncat target/stats\nrequests=$(awk '/^Compile requests +[0-9]+$/ {print $NF}' target/stats)\ntest \"$requests\" -gt 0\nsccache --stop-server >/dev/null\n");
                }

                let name = format!("supervisor-{supervisor}-config-{configured}-export-{export}");
                f.run(&name, &command, 0).await;
            }
            f.finish().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires trusted operator, provisioned accounts, sccache 0.18.0, ACLs, sudo and systemd"]
async fn isolated_public_reader_slot_precedes_sccache_compile() {
    let mut f = ClientFixture::new().await;
    rust_source(&f.source, true, false);
    f.run("slot-order", r#"set -e
exec 9<> /motor-slots/0.lock
flock -x 9
printf 'pub fn value() -> u32 { 42 }\n' > dep.rs
"$RUSTC_WRAPPER" "$(rustup which rustc)" --crate-type=rlib --crate-name dep --emit=link --out-dir "$CARGO_TARGET_DIR" dep.rs 9>&- &
compiler=$!
trap 'kill "$compiler" 2>/dev/null || true; sccache --stop-server >/dev/null 2>&1 || true' EXIT
exec 8<> /motor-slots/queue.lock
queued=false
for i in $(seq 1 500); do
    kill -0 "$compiler"
    if ! flock -n 8; then queued=true; break; fi
    flock -u 8
    sleep 0.02
done
test "$queued" = true
sccache --show-stats > target/before
cat target/before
test "$(awk '/^Compile requests +[0-9]+$/ {print $NF}' target/before)" = 0
test ! -e target/libdep.rlib
flock -u 9
wait "$compiler"
sccache --show-stats > target/after
cat target/after
test "$(awk '/^Compile requests +[0-9]+$/ {print $NF}' target/after)" = 1
test -f target/libdep.rlib
"#, 0).await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires trusted operator, provisioned accounts, sccache 0.18.0, ACLs, sudo and systemd"]
async fn isolated_public_reader_missing_cache_eviction_and_server_fallback() {
    let mut f = ClientFixture::new().await;
    rust_source(&f.source, false, false);
    let cache = f.runner.config().sccache_dir();
    let trash = cache.with_file_name("evicted");
    checked_restore_command(
        Command::new("sudo")
            .args(["-n", "mv", "--"])
            .arg(&cache)
            .arg(&trash),
    );
    f.run(
        "during-eviction",
        "set -e; cargo build --locked --offline; test -f ./target/debug/reader-fixture",
        0,
    )
    .await;
    checked_restore_command(
        Command::new("sudo")
            .args(["-n", "rm", "-rf", "--"])
            .arg(&trash),
    );
    f.run(
        "missing-cache",
        "set -e; cargo build --locked --offline; test -f ./target/debug/reader-fixture",
        0,
    )
    .await;
    checked_restore_command(
        Command::new("sudo")
            .args([
                "-n",
                "install",
                "-d",
                "-o",
                "motor-cache",
                "-g",
                "motor-cache",
                "-m",
                "0755",
                "--",
            ])
            .arg(&cache),
    );
    f.run(
        "after-eviction",
        "set -e; cargo build --locked --offline; test -f ./target/debug/reader-fixture",
        0,
    )
    .await;
    let compiler_sentinel = r#"set -e
export REAL_RUSTC=$(rustup which rustc)
cat > count-rustc.sh <<'SH'
#!/bin/bash
for argument in "$@"; do
    if [[ "$argument" == --emit=* ]]; then printf 'compile\n' >> "$CARGO_TARGET_DIR/compile-count"; break; fi
done
exec "$REAL_RUSTC" "$@"
SH
chmod +x count-rustc.sh
# A socket beneath the read-only cache cannot be created by this job.
export SCCACHE_SERVER_UDS=/motor-cache/unavailable/server.sock
"#;
    let success = format!("{compiler_sentinel}\nexport RUSTC=$PWD/count-rustc.sh\ncargo build --locked --offline\ntest \"$(wc -l < target/compile-count)\" = 1\ntest -f target/debug/reader-fixture\n");
    f.run("unavailable-server-build", &success, 0).await;
    let error = format!("{compiler_sentinel}\nprintf 'compile_error!(\"genuine compiler error\");\\n' > error.rs\nset +e\nsccache $PWD/count-rustc.sh --crate-type=rlib --crate-name error --emit=link --out-dir \"$CARGO_TARGET_DIR\" error.rs\ncode=$?\nset -e\ntest \"$(wc -l < target/compile-count)\" = 1\nexit \"$code\"\n");
    let output = f.run("compiler-error-once", &error, 1).await;
    assert!(output.contains("genuine compiler error"), "{output}");
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires trusted operator, provisioned accounts, ACLs, sudo and systemd"]
async fn isolated_public_non_rust_job_has_no_sccache_environment() {
    let mut f = ClientFixture::new().await;
    write(f.source.join("README.md"), "not a Rust workspace\n");
    f.run(
        "non-rust",
        "set -e; if env | grep '^SCCACHE_'; then exit 40; fi; test -z \"${RUSTC_WRAPPER+x}\"",
        0,
    )
    .await;
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires motor-cache, motor-job, patched sccache, sudo, ACLs and systemd"]
async fn isolated_public_reader_exported_incremental_keeps_dependency_hits() {
    let mut f = ClientFixture::with_warm_command("cargo build --locked && cargo build --release --locked && test \"$CARGO_INCREMENTAL\" = 1 && test -n \"$(ls -A /run/motor/target/release/incremental)\" && sccache --show-stats").await;
    write(f.source.join("Cargo.toml"), "[package]\nname='incremental-reader'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nitoa='=1.0.18'\n");
    write(
        f.source.join("src/lib.rs"),
        "pub fn value() -> String { itoa::Buffer::new().format(42).to_owned() }\n",
    );
    let generated = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&f.source)
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0")
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    f.run_capable("prime-writer", "true", 0, true).await;
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            let status = f.runner.status();
            if status
                .repositories
                .first()
                .is_some_and(|r| r.last_rebuild_result == Some(motor_protocol::RebuildResult::Ok))
            {
                break;
            }
            if status.repositories.first().is_some_and(|r| {
                r.last_rebuild_result == Some(motor_protocol::RebuildResult::Failed)
            }) {
                panic!("writer fill failed: {}", f.writer_diagnostics());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("writer did not complete");
    for variable in ["CARGO_INCREMENTAL", "CARGO_BUILD_INCREMENTAL"] {
        // Cargo's config boolean accepts true/false, while CARGO_INCREMENTAL
        // uses 1/0. sccache rejects literal 1; both exports must still hit with
        // the shim removing incremental variables from dependency cache keys.
        let value = if variable == "CARGO_BUILD_INCREMENTAL" {
            "true"
        } else {
            "1"
        };
        let output = f.run(&format!("export-{variable}"), &format!("export {variable}={value}; cargo build --locked && sccache --show-stats && test -n \"$(ls -A /run/motor/target/debug/incremental)\""), 0).await;
        let hits = output
            .lines()
            .find(|l| l.starts_with("Cache hits "))
            .unwrap();
        let misses = output
            .lines()
            .find(|l| l.starts_with("Cache misses "))
            .unwrap();
        assert!(
            hits.split_whitespace()
                .last()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0,
            "{output}"
        );
        assert_eq!(misses.split_whitespace().last().unwrap(), "0", "{output}");
    }
    f.finish().await;
}
