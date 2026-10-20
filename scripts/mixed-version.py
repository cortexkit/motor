#!/usr/bin/env python3
"""Build client/runner from compat-client-revision; test old/new wire compatibility."""
from pathlib import Path
import json
import os
import subprocess
import tempfile
import shutil

ROOT = Path(__file__).resolve().parent.parent
REVISION = (ROOT / "scripts/compat-client-revision").read_text().strip()
ROLLBACK_REVISION = "222d833f063b7ba1d34163fa6df1e816ebfb34e9"
CARGO_ENV = os.environ.copy()
# Nested Cargo must not rediscover the host's sandbox-inaccessible wrapper.
CARGO_ENV["RUSTC_WRAPPER"] = ""
CARGO_ENV["CARGO_BUILD_RUSTC_WRAPPER"] = ""
for key in ("GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"):
    CARGO_ENV.pop(key, None)


def run(*args, **kwargs):
    subprocess.run(args, check=True, env=CARGO_ENV, **kwargs)


def build_revision(revision, label):
    run("git", "cat-file", "-e", f"{revision}^{{commit}}", cwd=ROOT)
    # Cargo output is regenerable; the archived source is never patched.
    build_root = ROOT / "target/compat-client"
    build_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="source-", dir=build_root) as directory:
        scratch = Path(directory)
        old = scratch / "old"
        old.mkdir()
        archive = scratch / "old.tar"
        with archive.open("wb") as output:
            run("git", "archive", revision, cwd=ROOT, stdout=output)
        run("tar", "-xf", str(archive), "-C", str(old))
        helper = scratch / "helper"
        helper.mkdir()
        (helper / "main.rs").write_bytes((ROOT / "scripts/compat-client.rs").read_bytes())
        (helper / "runner.rs").write_bytes((ROOT / "scripts/compat-runner.rs").read_bytes())
        # A separate binary avoids linking two identically named protocol crates
        # and tests the real legacy transport rather than a reconstructed decoder.
        (helper / "Cargo.toml").write_text(f'''[package]
name = "motor-compat-client"
version = "0.0.0"
edition = "2021"
[workspace]
[[bin]]
name = "motor-compat-client"
path = "main.rs"
[[bin]]
name = "motor-compat-runner"
path = "runner.rs"
[dependencies]
motor-module = {{ path = {json.dumps(str(old / "crates/motor-module"))} }}
motor-protocol = {{ path = {json.dumps(str(old / "crates/motor-protocol"))} }}
motor-runner = {{ path = {json.dumps(str(old / "crates/motor-runner"))} }}
serde_json = "1"
tokio = {{ version = "1", features = ["rt-multi-thread", "macros", "net", "io-std"] }}
uuid = "1"
''')
        # Reuse the production revision's locked registry dependency versions.
        (helper / "Cargo.lock").write_bytes((old / "Cargo.lock").read_bytes())
        target = build_root / "target"
        run("cargo", "build", "--manifest-path", str(helper / "Cargo.toml"),
            "--target-dir", str(target), cwd=ROOT)
        client = build_root / f"{label}-client"
        runner = build_root / f"{label}-runner"
        shutil.copy2(target / "debug/motor-compat-client", client)
        shutil.copy2(target / "debug/motor-compat-runner", runner)
        return client, runner


def main():
    run("rustc", "--version")
    run("cargo", "--version")
    run("cargo", "nextest", "--version")
    client, runner = build_revision(REVISION, "oldest")
    rollback, _ = build_revision(ROLLBACK_REVISION, "rollback")
    test_env = CARGO_ENV.copy()
    test_env["MOTOR_COMPAT_CLIENT"] = str(client)
    test_env["MOTOR_COMPAT_RUNNER"] = str(runner)
    test_env["MOTOR_ROLLBACK_CLIENT"] = str(rollback)
    print(f"Testing unchanged modules at {REVISION} and {ROLLBACK_REVISION}", flush=True)
    subprocess.run(["cargo", "nextest", "run", "-p", "motor-module", "--lib",
                    "--locked", "--run-ignored", "only", "--no-tests", "fail",
                    "-E", "test(tests::compatibility::) | test(storage::journal_tests::rollback_)"],
                   check=True, cwd=ROOT, env=test_env)


if __name__ == "__main__":
    main()
