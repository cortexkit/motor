use super::*;

pub(super) const CLIPPY_STEP: &str = concat!(
    "(clippy_status=0; cargo clippy --workspace --all-targets || clippy_status=$?; ",
    "printf 'cargo clippy warm-up exited with status %s\\n' \"$clippy_status\"; exit 0)"
);

pub(super) fn with_clippy(command: &str) -> String {
    format!("{command} && {CLIPPY_STEP}")
}

#[test]
fn warm_command_is_derived_from_the_repository_tree() {
    let f = Fixture::new();
    let bare = git(&f.main, &["rev-parse", "HEAD"]);
    assert_eq!(workspace::warm_command(&f.main, &bare), "true");
    let plain = declare(&f, "[workspace]\nmembers = [\"a\"]\n");
    assert_eq!(
        workspace::warm_command(&f.main, &plain),
        with_clippy("cargo build --workspace --all-targets")
    );
    let declared = declare(
        &f,
        "[workspace]\nmembers = [\"a\"]\n[workspace.metadata.motor]\nwarm-commands = [\"cargo nextest run --workspace --no-run\", \"cargo nextest run -p a --no-run\"]\n",
    );
    assert_eq!(
        workspace::warm_command(&f.main, &declared),
        with_clippy("cargo nextest run --workspace --no-run && cargo nextest run -p a --no-run")
    );
    let package = declare(
        &f,
        "[package]\nname = \"a\"\n[package.metadata.motor]\nwarm-commands = [\"cargo build\"]\n",
    );
    assert_eq!(
        workspace::warm_command(&f.main, &package),
        with_clippy("cargo build")
    );
    let malformed = declare(
        &f,
        "[workspace]\n[workspace.metadata.motor]\nwarm-commands = \"cargo build\"\n",
    );
    assert_eq!(
        workspace::warm_command(&f.main, &malformed),
        with_clippy("cargo build --workspace --all-targets")
    );
    // The original commits keep their own commands: derivation reads the
    // commit being warmed, not whatever the repository holds now.
    assert_eq!(workspace::warm_command(&f.main, &bare), "true");
    assert_eq!(
        workspace::warm_command(&f.main, &plain),
        with_clippy("cargo build --workspace --all-targets")
    );
}

/// Motor declares the workspace and package-scoped commands that precompile
/// its test binaries; the runner reads them instead of hardcoding package names.
#[test]
fn this_repository_declares_its_warm_build() {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml")).unwrap();
    let f = Fixture::new();
    let tip = declare(&f, &manifest);
    assert_eq!(
        workspace::warm_command(&f.main, &tip),
        with_clippy(concat!(
            "cargo nextest run --workspace --no-run && ",
            "cargo nextest run -p motor-module --lib --bins --no-run && ",
            "cargo nextest run -p motor-runner --lib --bins --no-run"
        ))
    );
}

fn run_plan(command: &str, warm_status: i32, clippy_status: i32) -> std::process::Output {
    // A shell function records Cargo invocations without compiling fixture code.
    // Errexit also checks that the best-effort step handles failure explicitly.
    Command::new("/bin/bash")
        .args([
            "-eu",
            "-c",
            &format!(
                "cargo() {{ printf 'cargo %s\\n' \"$*\"; if [ \"$1\" = clippy ]; then return {clippy_status}; fi; return {warm_status}; }}\n{command}"
            ),
        ])
        .output()
        .unwrap()
}

#[test]
fn default_and_declared_plans_end_with_best_effort_clippy() {
    let f = Fixture::new();
    for (manifest, warm_output, explicit) in [
        ("[workspace]\n", "cargo build --workspace --all-targets\n", false),
        (
            "[workspace.metadata.motor]\nwarm-commands = ['cargo nextest run --workspace --no-run', 'cargo build -p a']\n",
            "cargo nextest run --workspace --no-run\ncargo build -p a\n",
            true,
        ),
        (
            "[package.metadata.motor]\nwarm-commands = ['cargo build -p a']\n",
            "cargo build -p a\n",
            true,
        ),
    ] {
        let tip = declare(&f, manifest);
        let (steps, requested) = workspace::warm_steps(&f.main, &tip);
        assert_eq!(requested, explicit);
        assert_eq!(steps.last().map(String::as_str), Some(CLIPPY_STEP));
        for clippy_status in [0, 23, 101] {
            let output = run_plan(&steps.join(" && "), 0, clippy_status);
            assert_eq!(output.status.code(), Some(0), "{output:?}");
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{warm_output}cargo clippy --workspace --all-targets\ncargo clippy warm-up exited with status {clippy_status}\n")
            );
        }
        let failed = run_plan(&steps.join(" && "), 17, 23);
        assert_eq!(failed.status.code(), Some(17), "{failed:?}");
        assert_eq!(
            String::from_utf8(failed.stdout).unwrap(),
            format!("{}\n", warm_output.lines().next().unwrap()),
            "a failed warm build must not run clippy or mask the warm failure"
        );
    }
}

async fn warm_and_tick(f: &Fixture, runner: &Runner) {
    let (request, bytes) = f.run("true");
    f.submit(runner, &request, &bytes);
    until_with_diagnostics(
        || {
            runner.tick().unwrap();
            let inner = runner.core.inner.lock().unwrap();
            inner.jobs[&request.job_id].terminal.is_some()
                && inner
                    .jobs
                    .values()
                    .all(|job| !job.base || job.terminal.is_some())
                && inner.base_finishes.is_empty()
        },
        || runner_diagnostics(runner),
    )
    .await;
    assert_eq!(
        terminal(runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
}

#[tokio::test]
async fn clippy_failure_keeps_fill_success_and_descriptor_reuse() {
    let f = Fixture::new();
    // The declared build succeeds and supplies a Cargo function for the trailing
    // clippy pass, making its failure deterministic without changing permissions.
    let manifest = r#"[workspace]
members = []
[workspace.metadata.motor]
warm-commands = ['cargo() { echo "$*" >> clippy-marker; return 23; }; printf warm > command-marker']
"#;
    let tip = declare(&f, manifest);
    let mut config = f.config();
    config.base_commands.clear();
    config.manual_tick = true;
    config.development_allow_host_backend = true;
    let runner = Runner::open(config).unwrap();
    warm_and_tick(&f, &runner).await;
    let checkout = {
        let inner = runner.core.inner.lock().unwrap();
        let writers: Vec<_> = inner.jobs.values().filter(|job| job.base).collect();
        assert_eq!(writers.len(), 1);
        PathBuf::from(&writers[0].request.as_ref().unwrap().cwd)
    };
    assert_eq!(fs::read(checkout.join("command-marker")).unwrap(), b"warm");
    assert_eq!(
        fs::read_to_string(checkout.join("clippy-marker")).unwrap(),
        "clippy --workspace --all-targets\n"
    );
    let status = &runner.status().repositories[0];
    assert_eq!(status.last_rebuild_result, Some(RebuildResult::Ok));
    assert_eq!(status.published_commit, Some(tip));
    let descriptor = {
        let inner = runner.core.inner.lock().unwrap();
        let record = &inner.catalog.repositories[&f.root];
        assert!(record.failed_descriptor.is_none());
        assert_eq!(record.filled_descriptor, record.requested_descriptor);
        record.filled_descriptor.clone().unwrap()
    };

    commit(&f.main, "ordinary source change\n");
    warm_and_tick(&f, &runner).await;
    assert_eq!(
        base_commands(&runner).len(),
        1,
        "a clippy failure caused a refill"
    );
    assert_eq!(
        runner.core.inner.lock().unwrap().catalog.repositories[&f.root].filled_descriptor,
        Some(descriptor.clone())
    );

    declare(&f, &format!("{manifest}\n# changed build input\n"));
    warm_and_tick(&f, &runner).await;
    assert_eq!(
        base_commands(&runner).len(),
        2,
        "a changed manifest did not refill"
    );
    let inner = runner.core.inner.lock().unwrap();
    let record = &inner.catalog.repositories[&f.root];
    assert_eq!(record.result, Some(RebuildResult::Ok));
    assert!(record.failed_descriptor.is_none());
    assert_ne!(record.filled_descriptor, Some(descriptor));
}
