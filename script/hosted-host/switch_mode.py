#!/usr/bin/env python3
"""Switch an already drained hosted host; never resume placement automatically."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

import hosted_host

HOST_CONFIG = Path("/etc/motor-host/host.json")
COUNTS = ("active_leases", "active_job_vms", "active_fills", "active_recoveries")


def run(*args):
    subprocess.run([str(arg) for arg in args], check=True)


def read_json(path):
    return json.loads(Path(path).read_text())


def wait_empty(path, mode, timeout):
    deadline = time.monotonic() + timeout
    fields = COUNTS if mode == "per_job" else COUNTS[:1]
    while True:
        status = hosted_host.admin_exchange({"op": "host_status"}, path, timeout=max(0.001, deadline - time.monotonic()))
        if status.get("ok") is not True or status.get("draining") is not True:
            raise ValueError("host must remain fully drained during the mode switch")
        counts = {field: status.get(field) for field in fields}
        if any(type(value) is not int or value < 0 for value in counts.values()):
            raise ValueError(f"host status lacks mode-switch work counts: {counts}")
        if not any(counts.values()):
            return status
        if time.monotonic() >= deadline:
            raise ValueError(f"mode-switch wait expired; still running: {counts}; host remains drained")
        time.sleep(min(1, max(0, deadline - time.monotonic())))


def replace_json(path, value):
    # Keep a failed switch drained, with each complete config installed atomically.
    path = Path(path)
    mode = path.stat().st_mode & 0o777
    fd, staged = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as output:
            os.fchmod(output.fileno(), mode)
            output.write(json.dumps(value, indent=2) + "\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(staged, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(staged):
            os.unlink(staged)


def wait_started(path, timeout):
    deadline = time.monotonic() + timeout
    while True:
        try:
            status = hosted_host.admin_exchange({"op": "host_status"}, path, timeout=max(0.001, deadline - time.monotonic()))
            if status.get("ok") is not True or status.get("draining") is not True:
                raise ValueError("restarted host did not retain its drain")
            return
        except OSError as error:
            if time.monotonic() >= deadline:
                raise ValueError("restarted host admin socket did not become ready; inspect motor-host logs") from error
            time.sleep(min(1, max(0, deadline - time.monotonic())))


def helper_exchange(path, request, timeout):
    request = json.dumps(request).encode() + b"\n"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(timeout)
        connection.connect(str(path))
        connection.sendall(request)
        reply = connection.makefile("rb").readline(4097)
    if len(reply) > 4096 or not reply.endswith(b"\n"):
        raise ValueError("helper reply is truncated or oversized")
    return json.loads(reply)


def helper_request(path, request, timeout, *, wait=False):
    # The helper authenticates the connecting uid, even for a root operator.
    # Use the installed host account instead of granting root a new socket role.
    result = subprocess.run([
        "runuser", "-u", "motor-host", "--", sys.executable, str(Path(__file__).resolve()),
        "--wait-helper" if wait else "--helper-request", str(path), json.dumps(request), str(timeout),
    ], check=True, capture_output=True, text=True)
    return json.loads(result.stdout)


def wait_helper_exchange(path, request, timeout):
    deadline = time.monotonic() + timeout
    while True:
        try:
            return helper_exchange(path, request, max(0.001, deadline - time.monotonic()))
        except (FileNotFoundError, ConnectionRefusedError, PermissionError) as error:
            if time.monotonic() >= deadline:
                raise ValueError("helper socket did not become ready; host remains drained") from error
            time.sleep(min(1, max(0, deadline - time.monotonic())))


def wait_helper(path, tenant, timeout):
    # systemd's simple service can be active before the helper has rebound its
    # socket. A stale socket left by the old process is not evidence of readiness.
    reply = helper_request(path, {"op": "adopt", "tenant": tenant}, timeout, wait=True)
    if reply.get("result") != "tenant" or reply.get("running") is not False:
        raise ValueError(f"restarted helper did not confirm tenant {tenant} stopped: {reply}")


def publish_to_helper(path, generation):
    reply = helper_request(path, {"op": "publish_job_images", "generation": generation}, 3600)
    if reply.get("result") != "ok":
        raise ValueError(f"helper image publication refused: {reply}")


def clean_stop_tenants(path, registries, timeout):
    for tenant, registry in registries.items():
        for op in ("stop_tenant", "adopt"):
            reply = helper_request(path, {"op": op, "tenant": tenant}, timeout)
            if reply.get("result") != "tenant" or reply.get("running") is not False:
                raise ValueError(f"tenant {tenant} did not report stopped: {reply}")
        home = Path(registry["generation"]).parent
        if any((home / name).exists() for name in ("running.json", "unclean-stop.json")):
            raise ValueError(f"tenant {tenant} stop was not clean; helper remains running; inspect {home}")


def switch(mode, *, per_job=None, job_vms=None, generation=None, timeout=300,
           host_path=HOST_CONFIG, helper_path=hosted_host.HELPER_CONFIG):
    host = read_json(host_path)
    helper = read_json(helper_path)
    old_mode = host.get("mode", "tenant")
    if old_mode == mode:
        raise ValueError(f"host is already in {mode} mode; image rollouts use running admin publication")
    bindings = read_json(host["bindings"])
    if not bindings or any(not binding.get("tenant") or not binding.get("sub") for binding in bindings.values()):
        raise ValueError("tenant account bindings must be retained for rollback")
    registries = {}
    for binding in bindings.values():
        registry = read_json(Path(host["registry_dir"]) / f"{binding['tenant']}.json")
        if registry.get("account") != binding["sub"] or not Path(registry["disk"]).is_file():
            raise ValueError("tenant disk and account binding must be retained for rollback")
        registries[binding["tenant"]] = registry
    staged_host = dict(host, mode=mode)
    staged_helper = dict(helper)
    if mode == "per_job":
        if per_job is None or job_vms is None or not generation:
            raise ValueError("per_job requires --per-job-settings, --job-vms-settings and --generation")
        staged_host["per_job"] = read_json(per_job)
        staged_helper["job_vms"] = read_json(job_vms)
        if not isinstance(staged_helper["job_vms"], dict):
            raise ValueError("job_vms settings must be an object")
        maximum = staged_helper["job_vms"].get("workspace_disk_max_bytes")
        if type(maximum) is not int or maximum < staged_host["per_job"]["workspace_disk_cap_bytes"]:
            raise ValueError("helper workspace_disk_max_bytes must cover the host workspace disk cap")
    else:
        staged_host.pop("per_job", None)
        # Retain job_vms while the existing rollback stops job VMs and deletes their disks.
        # Removing it early would prevent helper reconciliation of old job records.
    scratch = Path(os.environ.get("TMPDIR", "/srv/build/tmp"))
    with tempfile.TemporaryDirectory(prefix="motor-switch-mode-", dir=scratch) as directory:
        staged = Path(directory) / "host.json"
        staged.write_text(json.dumps(staged_host))
        run(hosted_host.HOST_BINARY, "check-config", staged)
    admin = Path(host["admin_socket"])
    status = hosted_host.admin_exchange({"op": "host_status"}, admin)
    if status.get("draining") is not True:
        raise ValueError("drain the host before switching mode")
    hosted_host.drain_host(timeout, admin)
    wait_empty(admin, old_mode, timeout)
    if mode == "per_job":
        publish_to_helper(host["helper_socket"], generation)
    run("systemctl", "stop", "motor-host.service")
    # Prove the tenant stop finished before the helper loses its control channel.
    # A timeout or unclean-stop.json recording a failed guest filesystem sync
    # leaves the helper running and placement drained.
    clean_stop_tenants(host["helper_socket"], registries, timeout)
    run("systemctl", "stop", "motor-vm-helper.service")
    replace_json(host_path, staged_host)
    if staged_helper != helper:
        replace_json(helper_path, staged_helper)
    run("systemctl", "start", "motor-vm-helper.service")
    wait_helper(host["helper_socket"], next(iter(registries)), timeout)
    if mode == "per_job":
        run("runuser", "-u", "motor-host", "--", hosted_host.HOST_BINARY,
            "select-job-images", host_path, generation)
    run(hosted_host.HOST_BINARY, "check-config", host_path)
    run("systemctl", "start", "motor-host.service")
    wait_started(admin, timeout)
    print(f"Started {mode} mode; host remains drained. Run a canary lease with "
          f"placement_hint.host={host['host_id']!r}; only after success use hosted_host.py undrain-host.")


def main():
    if sys.argv[1:2] in (["--helper-request"], ["--wait-helper"]):
        if len(sys.argv) != 5:
            raise ValueError("helper request needs socket, JSON request and timeout")
        exchange = wait_helper_exchange if sys.argv[1] == "--wait-helper" else helper_exchange
        print(json.dumps(exchange(sys.argv[2], json.loads(sys.argv[3]), float(sys.argv[4]))))
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("tenant", "per_job"))
    parser.add_argument("--per-job-settings", type=Path)
    parser.add_argument("--job-vms-settings", type=Path)
    parser.add_argument("--generation")
    parser.add_argument("--timeout", type=float, default=300)
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run as root on the drained host")
    if not 0 < args.timeout < float("inf"):
        parser.error("--timeout must be positive and finite")
    try:
        switch(args.mode, per_job=args.per_job_settings, job_vms=args.job_vms_settings,
               generation=args.generation, timeout=args.timeout)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"mode switch failed: {error}; do not undrain the host\n")


if __name__ == "__main__":
    main()
