#!/usr/bin/python3
"""Root qualification payload, staged only on a disposable tenant disk."""
import os
import json
from pathlib import Path
import socket
import struct
import subprocess
import threading
import time
from typing import Any

TEST = "isolation::tests::hosted_job_cannot_open_vsock"


def runner_diagnostics():
    state: dict[str, Any] = {}
    for name, args in (
        ("unit", ["systemctl", "show", "motor-tenant-runner.service", "-p", "MainPID", "-p", "ActiveState", "-p", "SubState", "-p", "Result", "-p", "ExecMainStatus", "-p", "ExecStart"]),
        ("journal", ["journalctl", "-b", "-u", "motor-tenant-runner.service", "--no-pager", "-n", "80", "-o", "short-monotonic"]),
    ):
        try:
            result = subprocess.run(args, capture_output=True, text=True, timeout=2)
            state[name] = dict(code=result.returncode, stdout=result.stdout, stderr=result.stderr)
        except Exception as error:
            state[name] = dict(error=f"{type(error).__name__}: {error}")
    # The guest keeps no journal, so a runner that never binds its socket leaves
    # no log. Record what its threads and children are blocked in instead.
    state["processes"] = process_snapshot(state)
    path = Path("/tenant/state/main/runner.sock")
    try:
        info = path.lstat()
        socket_state: dict[str, Any] = dict(path=str(path), mode=oct(info.st_mode), uid=info.st_uid, inode=info.st_ino)
        state["socket"] = socket_state
        with socket.socket(socket.AF_UNIX) as peer:
            peer.settimeout(.25)
            peer.connect(str(path))
            pid, uid, gid = struct.unpack("3i", peer.getsockopt(socket.SOL_SOCKET, getattr(socket, "SO_PEERCRED"), 12))
            socket_state["peer"] = dict(pid=pid, uid=uid, gid=gid)
    except Exception as error:
        state["socket_error"] = f"{type(error).__name__}: {error}"
    print("GUEST_RUNNER_DIAGNOSTIC " + json.dumps(state), flush=True)


def process_snapshot(state):
    """Each thread's wait channel for the runner and its descendants."""
    try:
        main = int(state["unit"]["stdout"].split("MainPID=", 1)[1].split()[0])
    except Exception as error:
        return dict(error=f"no MainPID: {type(error).__name__}: {error}")
    children: dict[int, list[int]] = {}
    try:
        entries = list(Path("/proc").iterdir())
    except OSError as error:
        return dict(error=f"no /proc: {type(error).__name__}: {error}")
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            parent = int((entry / "stat").read_text().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
        children.setdefault(parent, []).append(int(entry.name))
    found, pending = [], [main]
    while pending and len(found) < 64:
        pid = pending.pop()
        found.append(pid)
        pending.extend(children.get(pid, []))
    snapshot = []
    for pid in found:
        proc = Path(f"/proc/{pid}")
        try:
            cmdline = (proc / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")[:300]
            threads = []
            for task in sorted((proc / "task").iterdir(), key=lambda p: int(p.name))[:32]:
                stat = (task / "stat").read_text().rsplit(")", 1)[1].split()
                threads.append(dict(tid=int(task.name), comm=(task / "comm").read_text().strip(),
                                    state=stat[0], wchan=(task / "wchan").read_text().strip()))
            fds = {}
            for fd in sorted((proc / "fd").iterdir(), key=lambda p: int(p.name))[:32]:
                try:
                    fds[fd.name] = os.readlink(fd)
                except OSError as error:
                    fds[fd.name] = f"{type(error).__name__}"
            try:
                stack = (proc / "stack").read_text()[-1200:]
            except OSError as error:
                stack = f"{type(error).__name__}: {error}"
            snapshot.append(dict(pid=pid, cmdline=cmdline, threads=threads, fds=fds, stack=stack))
        except OSError as error:
            snapshot.append(dict(pid=pid, error=f"{type(error).__name__}: {error}"))
    return snapshot


def diagnostic_window(stop):
    for _ in range(8):
        runner_diagnostics()
        if stop.wait(4):
            return


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
    stop = threading.Event()
    diagnostics = threading.Thread(target=diagnostic_window, args=(stop,), daemon=True)
    diagnostics.start()
    try:
        main()
    except Exception as error:
        print(f"FAIL guest qualification: {error}", flush=True)
        raise
    finally:
        stop.set()
        diagnostics.join(timeout=5)
