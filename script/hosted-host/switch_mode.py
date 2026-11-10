#!/usr/bin/env python3
"""Switch an already drained hosted host; never resume placement automatically."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import socket
import stat
import subprocess
import sys
import tempfile
import time

import hosted_host

HOST_CONFIG = Path("/etc/motor-host/host.json")
REMOTE_CACHE_STORE = Path("/var/lib/motor-vm/remote-cache")
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


def atomic_file(path, write, metadata):
    path = Path(path)
    fd, staged = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as output:
            if (os.geteuid(), os.getegid()) != (metadata.st_uid, metadata.st_gid):
                os.fchown(output.fileno(), metadata.st_uid, metadata.st_gid)
            os.fchmod(output.fileno(), stat.S_IMODE(metadata.st_mode))
            write(output)
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


def replace_json(path, value):
    payload = (json.dumps(value, indent=2) + "\n").encode()
    atomic_file(path, lambda output: output.write(payload), Path(path).stat())


class SavedConfig:
    def __init__(self, path):
        self.path = Path(path)
        self.metadata = self.path.stat()
        self.original = self.path.read_bytes()

    def restore(self):
        atomic_file(self.path, lambda output: output.write(self.original), self.metadata)

    def describe(self):
        try:
            current = self.path.read_bytes()
            state = "original" if current == self.original else "changed"
            parsed = json.loads(current)
            mode = parsed.get("mode", "tenant") if isinstance(parsed, dict) and self.path.name == "host.json" else "helper"
            return f"{self.path}: {state} ({mode}) sha256={hashlib.sha256(current).hexdigest()}"
        except (OSError, ValueError) as error:
            return f"{self.path}: unreadable ({error})"


def digest_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as incoming:
        for chunk in iter(lambda: incoming.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.digest()


def copy_atomic(source, target, metadata, *, expected_digest=None):
    def copy(output):
        with Path(source).open("rb") as incoming:
            digest = hashlib.sha256()
            for chunk in iter(lambda: incoming.read(1024 * 1024), b""):
                digest.update(chunk)
                output.write(chunk)
        if expected_digest is not None and digest.digest() != expected_digest:
            raise ValueError(f"binary changed since validation: {source}")
    atomic_file(target, copy, metadata)


class BinaryInstall:
    def __init__(self, directory, expected_commit):
        if not expected_commit or not re.fullmatch(r"[0-9a-f]{40}", expected_commit):
            raise ValueError("--install-binaries requires --expected-commit with a full lowercase Git SHA")
        self.files = []
        for name, target in [("motor-host", hosted_host.HOST_BINARY), ("motor-vm-helper", hosted_host.HELPER_BINARY)]:
            source, target = Path(directory) / name, Path(target)
            for path in (source, target):
                if not stat.S_ISREG(path.lstat().st_mode) or not os.access(path, os.X_OK):
                    raise ValueError(f"binary must be an executable regular file: {path}")
            digest = digest_file(source)
            result = subprocess.run([str(source), "--version"], check=True, capture_output=True, text=True, timeout=10)
            if not re.fullmatch(rf"{name} \S+ {expected_commit}", result.stdout.strip()):
                raise ValueError(f"{name} is unstamped or does not report expected commit {expected_commit}: {result.stdout.strip()!r}")
            if digest_file(source) != digest:
                raise ValueError(f"binary changed during version validation: {source}")
            self.files.append((source, target, target.stat(), digest))
        self.backups = []
        for source, target, metadata, digest in self.files:
            fd, name = tempfile.mkstemp(prefix=f"{target.name}.bak-", dir=target.parent)
            os.close(fd)
            backup = Path(name)
            original_digest = digest_file(target)
            copy_atomic(target, backup, metadata, expected_digest=original_digest)
            self.backups.append((source, target, backup, metadata, digest, original_digest))
            print(f"binary rollback copy: {target} -> {backup}")

    def install(self):
        for source, target, _backup, metadata, digest, _original_digest in self.backups:
            copy_atomic(source, target, metadata, expected_digest=digest)

    def restore(self):
        errors = []
        for _source, target, backup, metadata, _digest, original_digest in self.backups:
            try:
                copy_atomic(backup, target, metadata, expected_digest=original_digest)
            except Exception as error:
                errors.append(f"restore binary {target} from {backup}: {error}")
        if errors:
            raise ValueError("; ".join(errors))

    def describe(self):
        states = []
        for _source, target, backup, _metadata, _digest, _original_digest in self.backups:
            try:
                current = target.read_bytes()
                state = "original" if current == backup.read_bytes() else "changed"
                states.append(f"{target}: {state} sha256={hashlib.sha256(current).hexdigest()} (rollback {backup})")
            except OSError as error:
                states.append(f"{target}: unreadable ({error}); rollback {backup}")
        return "; ".join(states)


def service_state(unit):
    result = subprocess.run(["systemctl", "show", "--property=ActiveState", "--value", unit],
                            check=True, capture_output=True, text=True, timeout=30)
    return result.stdout.strip()


def restore_previous(configs, *, replaced, host_start_attempted, binaries, admin, registries, helper_socket, timeout):
    errors = []

    def attempt(label, action):
        try:
            action()
            return True
        except Exception as error:
            errors.append(f"{label}: {error}")
            return False

    if replaced:
        host_stopped = attempt("stop replacement motor-host", lambda: run("systemctl", "stop", "motor-host.service"))
        # Recovery must not kill a tenant that the replacement mode already woke.
        if host_stopped:
            clean = not host_start_attempted or attempt("clean-stop replacement tenants", lambda: clean_stop_tenants(helper_socket, registries, timeout))
            if clean:
                attempt("stop replacement helper", lambda: run("systemctl", "stop", "motor-vm-helper.service"))
        else:
            errors.append("helper restart withheld because replacement motor-host did not stop")
        for config in configs:
            attempt(f"restore config {config.path}", config.restore)
    if binaries:
        attempt("restore binaries", binaries.restore)
    # start is idempotent when a failed stop left a service running. Always try
    # both starts, even if one recovery action failed, to avoid leaving an outage.
    attempt("start original helper", lambda: run("systemctl", "start", "motor-vm-helper.service"))
    attempt("wait for original helper", lambda: wait_helper_available(helper_socket, next(iter(registries)), timeout))
    attempt("start original motor-host", lambda: run("systemctl", "start", "motor-host.service"))
    attempt("wait for original drained host", lambda: wait_started(admin, timeout, restore_drain=True))
    states = {}
    for unit in ("motor-host.service", "motor-vm-helper.service"):
        try:
            states[unit] = service_state(unit)
        except Exception as error:
            states[unit] = f"unknown ({error})"
        if states[unit] != "active":
            errors.append(f"{unit} is {states[unit]}")
    if errors:
        details = "RESTORATION FAILED: " + "; ".join(errors) + "; services: " + str(states) + "; configs on disk: " + "; ".join(config.describe() for config in configs)
        return details + ("; binaries on disk: " + binaries.describe() if binaries else "")
    return "original configs/binaries restored; helper and motor-host started; host remains drained"


def wait_started(path, timeout, *, restore_drain=False):
    deadline = time.monotonic() + timeout
    while True:
        try:
            status = hosted_host.admin_exchange({"op": "host_status"}, path, timeout=max(0.001, deadline - time.monotonic()))
            if restore_drain and status.get("ok") is True and status.get("draining") is not True:
                status = hosted_host.admin_exchange({"op": "drain_host"}, path)
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
    try:
        result = subprocess.run([
            "runuser", "-u", "motor-host", "--", sys.executable, str(Path(__file__).resolve()),
            "--wait-helper" if wait else "--helper-request", str(path), json.dumps(request), str(timeout),
        ], check=True, capture_output=True, text=True)
    except subprocess.CalledProcessError as error:
        if not wait:
            raise
        # The socket probe uses the host uid, but only the root switch process
        # can reliably read the helper's system journal on a locked-down host.
        journal = subprocess.run([
            "journalctl", "-u", "motor-vm-helper.service", "-n", "20", "--no-pager",
        ], check=False, capture_output=True, text=True, timeout=30)
        print(journal.stdout or journal.stderr, file=sys.stderr, end="")
        raise ValueError(f"helper readiness failed: {error.stderr.strip()}") from error
    return json.loads(result.stdout)


def helper_startup_state():
    result = subprocess.run([
        "systemctl", "show", "--property=ActiveState,SubState,NRestarts", "motor-vm-helper.service",
    ], check=True, capture_output=True, text=True, timeout=30)
    return dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)


def wait_helper_exchange(path, request, timeout):
    deadline = time.monotonic() + timeout
    initial_restarts = None
    while True:
        state = helper_startup_state()
        restarts = int(state["NRestarts"])
        if initial_restarts is None:
            initial_restarts = restarts
        if state["ActiveState"] == "failed" or state["SubState"] == "auto-restart" or restarts > initial_restarts:
            raise ValueError(f"helper failed or is restarting: {state}; host remains drained")
        try:
            return helper_exchange(path, request, min(1, max(0.001, deadline - time.monotonic())))
        except (FileNotFoundError, ConnectionRefusedError, PermissionError, ConnectionResetError, BrokenPipeError, TimeoutError) as error:
            if time.monotonic() >= deadline:
                raise ValueError("helper socket did not become ready; host remains drained") from error
            time.sleep(min(1, max(0, deadline - time.monotonic())))


def wait_helper(path, tenant, timeout):
    # systemd's simple service can be active before the helper has rebound its
    # socket. A stale socket left by the old process is not evidence of readiness.
    reply = wait_helper_available(path, tenant, timeout)
    if reply.get("result") != "tenant" or reply.get("running") is not False:
        raise ValueError(f"restarted helper did not confirm tenant {tenant} stopped: {reply}")


def wait_helper_available(path, tenant, timeout):
    reply = helper_request(path, {"op": "adopt", "tenant": tenant}, timeout, wait=True)
    if reply.get("result") != "tenant" or type(reply.get("running")) is not bool:
        raise ValueError(f"helper did not inspect tenant {tenant}: {reply}")
    return reply


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


def check_credential_set(account):
    if not isinstance(account, str) or account in ("", ".", "..") or Path(account).name != account:
        raise ValueError(f"invalid rollback credential account: {account!r}")
    directory = REMOTE_CACHE_STORE / account
    paths = [(REMOTE_CACHE_STORE, True), (directory, True),
             (directory / "config.json", False), (directory / "read-key", False)]
    write_key = directory / "write-key"
    if write_key.exists() or write_key.is_symlink():
        paths.append((write_key, False))
    for path, is_directory in paths:
        try:
            metadata = path.lstat()
            valid = (stat.S_ISDIR(metadata.st_mode) if is_directory else stat.S_ISREG(metadata.st_mode))
            valid = valid and metadata.st_uid == 0 and not metadata.st_mode & 0o077
        except FileNotFoundError:
            valid = False
        if not valid:
            raise ValueError(
                f"rollback credential set for account {account!r} is missing or insecure at {path}; install it with: "
                f"sudo /usr/local/libexec/motor-vm-helper set-remote-cache --account {shlex.quote(account)} "
                "--config /absolute/root-only-dir/config.json --read-key /absolute/root-only-dir/read-key "
                "[--write-key /absolute/root-only-dir/write-key] (docs/hosted-host.md, root credential command)"
            )


def switch(mode, *, per_job=None, job_vms=None, generation=None, timeout=300,
           host_path=HOST_CONFIG, helper_path=hosted_host.HELPER_CONFIG,
           install_binaries=None, expected_commit=None):
    configs = (SavedConfig(host_path), SavedConfig(helper_path))
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
        if registry.get("account") != binding["sub"]:
            raise ValueError(f"tenant {binding['tenant']} rollback registry account {registry.get('account')!r} "
                             f"does not match binding sub {binding['sub']!r}; restore the matching registry and binding")
        if not Path(registry["disk"]).is_file():
            raise ValueError(f"tenant {binding['tenant']} rollback disk missing: {registry['disk']}; restore the retained tenant disk")
        if mode == "tenant":
            check_credential_set(binding["sub"])
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
    if expected_commit and install_binaries is None:
        raise ValueError("--expected-commit requires --install-binaries")
    binaries = BinaryInstall(install_binaries, expected_commit) if install_binaries is not None else None
    # The helper rejects writable ancestors, including the system temp folder.
    # Stage beside its root-controlled installed config, not under TMPDIR.
    with tempfile.TemporaryDirectory(prefix=".motor-switch-mode-", dir=Path(helper_path).parent) as directory:
        staged = Path(directory) / "host.json"
        staged.write_text(json.dumps(staged_host))
        validator = Path(install_binaries) / "motor-host" if install_binaries is not None else hosted_host.HOST_BINARY
        run(validator, "check-config", staged)
        staged = Path(directory) / "helper.json"
        staged.write_text(json.dumps(staged_helper))
        validator = Path(install_binaries) / "motor-vm-helper" if install_binaries is not None else hosted_host.HELPER_BINARY
        run(validator, "check-config", staged)
    admin = Path(host["admin_socket"])
    status = hosted_host.admin_exchange({"op": "host_status"}, admin)
    if status.get("draining") is not True:
        raise ValueError("drain the host before switching mode")
    hosted_host.drain_host(timeout, admin)
    wait_empty(admin, old_mode, timeout)
    replaced = host_start_attempted = False
    try:
        if binaries:
            binaries.install()
        run("systemctl", "stop", "motor-host.service")
        # Prove tenant clean stop before helper shutdown. Any failed transition
        # restores the original configuration and resumes the drained services.
        clean_stop_tenants(host["helper_socket"], registries, timeout)
        run("systemctl", "stop", "motor-vm-helper.service")
        replaced = True
        replace_json(host_path, staged_host)
        if staged_helper != helper:
            replace_json(helper_path, staged_helper)
        run("systemctl", "start", "motor-vm-helper.service")
        wait_helper(host["helper_socket"], next(iter(registries)), timeout)
        if mode == "per_job":
            # Publish with the helper that will run the job VMs. The helper
            # running before the switch may be an older build with different
            # image checks, so its verdict would not hold for the new one.
            publish_to_helper(host["helper_socket"], generation)
            run("runuser", "-u", "motor-host", "--", hosted_host.HOST_BINARY,
                "select-job-images", host_path, generation)
        run(hosted_host.HOST_BINARY, "check-config", host_path)
        host_start_attempted = True
        run("systemctl", "start", "motor-host.service")
        wait_started(admin, timeout)
    except BaseException as error:
        restored = restore_previous(configs, replaced=replaced, host_start_attempted=host_start_attempted,
                                    binaries=binaries, admin=admin, registries=registries,
                                    helper_socket=host["helper_socket"], timeout=timeout)
        raise ValueError(f"{error}; {restored}") from error
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
    parser.add_argument("--install-binaries", type=Path)
    parser.add_argument("--expected-commit")
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run as root on the drained host")
    if not 0 < args.timeout < float("inf"):
        parser.error("--timeout must be positive and finite")
    try:
        switch(args.mode, per_job=args.per_job_settings, job_vms=args.job_vms_settings,
               generation=args.generation, timeout=args.timeout,
               install_binaries=args.install_binaries, expected_commit=args.expected_commit)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        parser.exit(1, f"mode switch failed: {error}; do not undrain the host\n")


if __name__ == "__main__":
    main()
