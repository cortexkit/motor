#!/usr/bin/python3
"""Root qualification payload, staged only on a disposable tenant disk."""
import os
from pathlib import Path
import subprocess
import time

TEST = "isolation::tests::hosted_job_cannot_open_vsock"


def run(*args, **kwargs):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, timeout=180, **kwargs)
    print(result.stdout, end="", flush=True)
    if result.returncode:
        raise RuntimeError(f"{args[0]} exited {result.returncode}")
    return result.stdout


def main():
    deadline = time.monotonic() + 120
    while subprocess.run(["systemctl", "is-active", "--quiet", "motor-tenant-runner.service"]).returncode:
        if time.monotonic() > deadline:
            raise RuntimeError("tenant runner did not become active after attachment")
        time.sleep(.1)
    run("/usr/local/libexec/hosted_image_check.py", "--tenant")
    root = Path("/tenant/qualification")
    scratch = Path("/tenant/qualification-scratch")
    scratch.mkdir(exist_ok=True)
    env = dict(os.environ, TMPDIR=str(scratch), RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="")
    for key in list(env):
        if key.startswith("GIT_CONFIG_"):
            del env[key]
    output = run(str(root / "runner-tests"), TEST, "--ignored", "--exact", "--nocapture",
                 env=env, cwd=root)
    if f"test {TEST} ... ok" not in output or "1 passed; 0 failed; 0 ignored;" not in output:
        raise RuntimeError("the exact ignored isolation test did not run once successfully")
    print("PASS guest vsock isolation: 1 passed; 0 failed; 0 ignored", flush=True)
    # The final real job creates this file. Checking units afterward includes
    # failures from attachment, isolation and the post-restore job, not just boot.
    deadline = time.monotonic() + 600
    finish = Path("/Users/qualification/probe/qualification-finish")
    while not finish.is_file():
        if time.monotonic() > deadline:
            raise RuntimeError("post-restore completion marker did not arrive")
        time.sleep(.1)
    failed = run("systemctl", "--failed", "--no-legend", "--plain", "--no-pager").strip()
    if failed:
        raise RuntimeError(f"failed guest units: {failed}")
    print("PASS guest failed units: none", flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"FAIL guest qualification: {error}", flush=True)
        raise
