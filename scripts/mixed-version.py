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
        health_bin = ""
        if label == "oldest":
            # This decoder links the archived protocol, not a reconstruction of
            # its serde rules or the current protocol in the integration suite.
            (helper / "health.rs").write_text('''fn main() {
    let frame: motor_protocol::Frame = serde_json::from_reader(std::io::stdin()).unwrap();
    let motor_protocol::Frame::StatusReply(reply) = frame else { panic!("not status_reply") };
    let health = reply.health.expect("archived client discarded health");
    assert_eq!(health.runner_version, "health-compat-fixture");
    assert_eq!(health.counts.accepted, 7);
    println!("archived health decoded");
}
''')
            health_bin = '''[[bin]]
name = "motor-compat-health"
path = "health.rs"
'''
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
{health_bin}
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
        if label == "oldest":
            shutil.copy2(target / "debug/motor-compat-health", build_root / "oldest-health")
        return client, runner


def check_health_compatibility():
    fixture = {
        "type": "status_reply", "queue_depth": 0, "running_jobs": [],
        "server_reachable": True, "repositories": [], "rustc_version": "",
        "health": {
            "runner_version": "health-compat-fixture", "since_ms": 0,
            "window_minutes": 60, "free_bytes": None, "queue_depth": 0,
            "running": [], "counts": {
                "accepted": 7, "refusals": {}, "evictions": 0,
                "eviction_bytes": 0, "eviction_failures": 0,
                "soft_goal_unmet_passes": 0, "skipped_recent": 0,
            },
        },
    }
    tool = {"hits": 1, "probes": 2, "misses": 3, "breaker_open": 0,
            "errors": 0, "refusals": {"read": 0, "write": 0}}
    for present in (False, True):
        if present:
            fixture["health"]["oldest_wait_s"] = 42
            fixture["health"]["held_workspaces"] = [{
                "job_id": "00000000-0000-7000-8000-000000000001", "used_ms": 42,
            }]
            fixture["health"]["remote_cache"] = {
                "state": "available", "sccache": tool, "ccache": tool,
            }
        subprocess.run([str(ROOT / "target/compat-client/oldest-health")],
                       input=json.dumps(fixture), text=True, check=True, env=CARGO_ENV)
        print(f"Oldest production client {REVISION}: remote_cache, held_workspaces and oldest_wait_s {'present' if present else 'absent'} decoded", flush=True)


def main():
    run("rustc", "--version")
    run("cargo", "--version")
    run("cargo", "nextest", "--version")
    client, runner = build_revision(REVISION, "oldest")
    check_health_compatibility()
    rollback, _ = build_revision(ROLLBACK_REVISION, "rollback")
    test_env = CARGO_ENV.copy()
    test_env["MOTOR_COMPAT_CLIENT"] = str(client)
    test_env["MOTOR_COMPAT_RUNNER"] = str(runner)
    test_env["MOTOR_ROLLBACK_CLIENT"] = str(rollback)
    print(f"Testing unchanged modules at {REVISION} and {ROLLBACK_REVISION}", flush=True)
    subprocess.run(["cargo", "nextest", "run", "-p", "motor-module", "--lib",
                    "--locked", "--run-ignored", "only", "--no-tests", "fail",
                    "-E", "test(tests::compatibility::) | test(storage::journal_tests::rollback_) | test(motor::late_grant_tests::rollback_)"],
                   check=True, cwd=ROOT, env=test_env)


if __name__ == "__main__":
    main()
