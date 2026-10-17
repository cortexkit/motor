#!/usr/bin/env python3
"""Throwaway root-only vsock probe; never install this agent in a tenant image."""

import fcntl
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import stat
import subprocess
import sys
import threading
import time

GIB = 1024**3
RUNNER = "/tenant/state/spike/runner.sock"
PROBE_REPOSITORY = "/Users/spike/probe"
_tenant_frozen = False


def command(*args):
    return subprocess.check_output(args, stderr=subprocess.STDOUT, text=True).strip()


def meminfo():
    return {line.split(":")[0]: int(line.split()[1]) * 1024
            for line in Path("/proc/meminfo").read_text().splitlines()}


def ready():
    return {"uptime": Path("/proc/uptime").read_text().split()[0],
            "cached_bytes": meminfo()["Cached"],
            "modules_disabled": Path("/proc/sys/kernel/modules_disabled").read_text().strip(),
            "tenant_mounted": os.path.ismount("/tenant"),
            "runner_active": command("systemctl", "is-active", "spike-runner")
            if Path(RUNNER).exists() else "not-started"}


def network(profile):
    address, gateway = {"a": ("192.0.2.2/30", "192.0.2.1"),
                        "b": ("192.0.2.6/30", "192.0.2.5")}[profile]
    command("ip", "link", "set", "eth0", "up")
    command("ip", "addr", "flush", "dev", "eth0")
    command("ip", "addr", "add", address, "dev", "eth0")
    return {"address": address, "ping": command("ping", "-n", "-c", "1", "-W", "2", gateway)}


def listening_runner_socket(pid, path=RUNNER):
    sockets = set()
    for line in Path("/proc/net/unix").read_text().splitlines()[1:]:
        fields = line.split(maxsplit=7)
        if len(fields) == 8 and fields[7] == path and fields[4] == "0001" and int(fields[3], 16) & 0x10000:
            sockets.add(f"socket:[{fields[6]}]")
    if not sockets:
        return None
    try:
        for fd in Path(f"/proc/{pid}/fd").iterdir():
            try:
                target = os.readlink(fd)
                if target in sockets:
                    return {"pid": pid, "socket_inode": target, "path": path}
            except FileNotFoundError:
                pass
    except FileNotFoundError:
        pass
    return None


def runner_diagnostics():
    result = subprocess.run(["journalctl", "-b", "--no-pager", "-n", "160",
                             "-u", "spike-runner", "-u", "spike-agent"],
                            capture_output=True, text=True, timeout=10)
    return {"journal": result.stdout[-32000:], "stderr": result.stderr,
            "unit": command("systemctl", "show", "spike-runner", "--property=MainPID",
                            "--property=ActiveState", "--property=SubState", "--property=ExecMainStatus")}


def verify_state(require_existing=True, state_dir=Path("/tenant/state/spike"),
                 command_prefix=("runuser", "-u", "motor-spike", "--")):
    mirror = state_dir / "mirrors" / hashlib.sha256(PROBE_REPOSITORY.encode()).hexdigest()
    if not mirror.exists():
        if require_existing:
            raise RuntimeError(f"persisted probe mirror is missing: {mirror}")
        return {"mirror": str(mirror), "present": False}
    try:
        fsck = command(*command_prefix, "git", "--no-optional-locks", "-C", str(mirror),
                       "fsck", "--full", "--no-dangling")
        refs = command(*command_prefix, "git", "--no-optional-locks", "-C", str(mirror),
                       "for-each-ref", "--format=%(refname) %(objectname)", "refs/snapshots/", "refs/tips/")
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"persisted mirror fsck failed: {mirror}: {error.output}") from error
    if require_existing and not refs:
        raise RuntimeError(f"persisted probe mirror has no pinned snapshot refs: {mirror}")
    return {"mirror": str(mirror), "present": True, "fsck": fsck, "refs": refs.splitlines()}


def prepare_snapshot(kind):
    global _tenant_frozen
    mounted = os.path.ismount("/tenant")
    if kind == "base":
        if mounted or os.path.ismount("/Users"):
            raise RuntimeError("base snapshot must not contain a mounted tenant filesystem")
        os.sync()
        return {"tenant_mounted": False, "synced": True, "frozen": False}
    if kind != "suspend" or not mounted or _tenant_frozen:
        raise RuntimeError("suspend requires a mounted, not-yet-frozen tenant filesystem")
    state = verify_state()
    os.sync()
    command("fsfreeze", "--freeze", "/tenant")
    _tenant_frozen = True
    return {"tenant_mounted": True, "synced": True, "frozen": True, "state_check": state}


def post_restore():
    global _tenant_frozen
    if not _tenant_frozen or not os.path.ismount("/tenant"):
        raise RuntimeError("suspend restore must contain the frozen original tenant filesystem")
    command("fsfreeze", "--unfreeze", "/tenant")
    _tenant_frozen = False
    return {"thawed": True, "state_check": verify_state()}


def detach():
    if _tenant_frozen or not os.path.ismount("/tenant"):
        raise RuntimeError("cold detach requires a mounted, thawed tenant filesystem")
    drained = command("runuser", "-u", "motor-spike", "--", "/usr/local/bin/motor-runner", "drain",
                      "--instance", "spike", "--state-root", "/tenant/state", "--drain-timeout-s", "30",
                      "--exit-after-drain", "true")
    command("systemctl", "stop", "spike-runner")
    state = verify_state()
    os.sync()
    command("umount", "/Users")
    command("umount", "/tenant")
    os.sync()
    tenant_mounted, users_mounted = os.path.ismount("/tenant"), os.path.ismount("/Users")
    if tenant_mounted or users_mounted:
        raise RuntimeError("tenant filesystem is still mounted after cold detach")
    return {"drain": drained, "synced": True, "tenant_mounted": tenant_mounted,
            "users_mounted": users_mounted, "state_check": state}


def attach(expected_bytes):
    size = int(command("blockdev", "--getsize64", "/dev/vdb"))
    if size != expected_bytes:
        raise RuntimeError(f"/dev/vdb has {size} bytes, expected {expected_bytes}")
    if os.path.ismount("/tenant"):
        raise RuntimeError("refusing to mount an already attached tenant")
    command("mount", "-t", "xfs", "/dev/vdb", "/tenant")
    for name in ["home", "home/.cargo", "home/.cargo/bin", "home/.cargo/registry",
                 "home/.cargo/git", "home/.rustup", "state", "state/workspaces", "work", "probe-a", "probe-b"]:
        Path("/tenant", name).mkdir(parents=True, exist_ok=True)
    command("chown", "-R", "motor-spike:motor-spike", "/tenant")
    Path("/Users").mkdir(exist_ok=True)
    command("mount", "--bind", "/tenant/state/workspaces", "/Users")
    command("runuser", "-u", "motor-spike", "--", "test", "-w", "/Users")
    marker = Path("/var/lib/motor-spike/host-only")
    marker.parent.chmod(0o755)
    marker.write_text("not visible to an isolated job\n")
    marker.chmod(0o644)
    command("runuser", "-u", "motor-job", "--", "test", "-r", str(marker))
    Path("/tenant/persistent-marker").write_text("tenant-vm-spike\n")
    os.sync()
    active = subprocess.run(["systemctl", "is-active", "--quiet", "spike-runner"]).returncode == 0
    if active:
        raise RuntimeError("refusing to replace a socket while the runner is active")
    try:
        old = Path(RUNNER).lstat()
    except FileNotFoundError:
        old = None
    if old is not None:
        if not stat.S_ISSOCK(old.st_mode):
            raise RuntimeError("refusing non-socket runner.sock in the tenant state")
        Path(RUNNER).unlink()
    command("systemctl", "start", "spike-runner")
    pid = int(command("systemctl", "show", "spike-runner", "--property=MainPID", "--value"))
    deadline = time.monotonic() + 30
    listener = None
    while listener is None:
        listener = listening_runner_socket(pid)
        if not Path(f"/proc/{pid}").exists() or time.monotonic() > deadline:
            raise RuntimeError(json.dumps(runner_diagnostics()))
        if listener is not None:
            break
        time.sleep(0.05)
    return {"device": "/dev/vdb", "bytes": size,
            "filesystem": command("findmnt", "-n", "-o", "FSTYPE", "/tenant"),
            "owned_workspace_root": "/Users", "workspace_source": "/tenant/state/workspaces",
            "workspace_owner_uid": Path("/Users").stat().st_uid,
            "runner_listener": listener,
            "stale_runner_socket_removed": old is not None,
            "runner": command("systemctl", "is-active", "spike-runner")}


def ficlone():
    for name in ["a", "b"]:
        target = Path(f"/mnt/spike-{name}")
        target.mkdir(parents=True, exist_ok=True)
        command("mount", "--bind", f"/tenant/probe-{name}", str(target))
    try:
        data = os.urandom(65536)
        source, target = Path("/mnt/spike-a/source"), Path("/mnt/spike-b/clone")
        source.write_bytes(data)
        with source.open("rb") as src, target.open("wb") as dst:
            fcntl.ioctl(dst.fileno(), 0x40049409, src.fileno())  # Linux FICLONE, without a copy fallback.
        if target.read_bytes() != data or source.stat().st_ino == target.stat().st_ino:
            raise RuntimeError("FICLONE content/inode check failed")
        return {"ficlone": "passed", "bytes": len(data), "source_inode": source.stat().st_ino,
                "clone_inode": target.stat().st_ino, "device": source.stat().st_dev}
    finally:
        for name in ["b", "a"]:
            command("umount", f"/mnt/spike-{name}")


def cache_fill():
    path = Path("/tenant/cache-10g")
    with path.open("wb") as out:
        getattr(os, "posix_fallocate")(out.fileno(), 0, 10 * GIB)
        out.write(b"tenant-vm-spike-cache\n")
        out.seek(10 * GIB - 1)
        out.write(b"!")
        out.flush()
        os.fsync(out.fileno())
    # Count actual cached file pages, not merely an allocated 10 GiB file.
    with path.open("rb") as src:
        while src.read(4 * 1024**2):
            pass
    import ctypes
    import mmap
    with path.open("rb") as src, mmap.mmap(src.fileno(), 0, access=mmap.ACCESS_COPY) as region:
        pages = (10 * GIB) // mmap.PAGESIZE
        residency = (ctypes.c_ubyte * pages)()
        address = ctypes.addressof(ctypes.c_char.from_buffer(region))
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.mincore(ctypes.c_void_p(address), ctypes.c_size_t(10 * GIB), residency):
            raise OSError(ctypes.get_errno(), "mincore")
        resident = sum(value & 1 for value in residency) * mmap.PAGESIZE
    if resident != 10 * GIB:
        raise RuntimeError(f"10 GiB cache workload is not resident: {resident} bytes")
    os.sync()
    return {"file_bytes": path.stat().st_size, "resident_file_bytes": resident,
            "cached_bytes": meminfo()["Cached"]}


def cache_verify():
    path = Path("/tenant/cache-10g")
    with path.open("rb") as src:
        if src.read(len(b"tenant-vm-spike-cache\n")) != b"tenant-vm-spike-cache\n":
            raise RuntimeError("cache marker lost")
        while src.read(4 * 1024**2):
            pass
        src.seek(-1, os.SEEK_END)
        if src.read(1) != b"!":
            raise RuntimeError("cache final byte lost")
    if Path("/tenant/persistent-marker").read_text() != "tenant-vm-spike\n":
        raise RuntimeError("persistent tenant file lost")
    return {"verified_bytes": path.stat().st_size, "cached_bytes": meminfo()["Cached"]}


def diagnostics(job_id):
    import uuid
    job_id = str(uuid.UUID(job_id))

    def probe(*args):
        result = subprocess.run(args, capture_output=True, text=True, timeout=10)
        return {"status": result.returncode, "stdout": result.stdout, "stderr": result.stderr}

    properties = ["LoadState", "ActiveState", "MainPID", "PrivateNetwork", "NetworkNamespacePath",
                  "PrivateDevices", "DevicePolicy", "DeviceAllow", "RestrictNamespaces",
                  "RestrictAddressFamilies", "RestrictNetworkInterfaces", "IPAddressDeny",
                  "IPAddressAllow", "SocketBindAllow", "SocketBindDeny", "NoNewPrivileges"]
    candidates = probe("systemctl", "list-units", "--all", "--plain", "--no-legend", "--type=service", "motor-job-*.service")
    unit = None
    for line in candidates["stdout"].splitlines():
        candidate = line.split()[0]
        root = probe("systemctl", "show", candidate, "--property=RootDirectory", "--value")["stdout"].strip()
        if root.endswith(f"/sandboxes/{job_id}/root"):
            unit = candidate
            break
    shown = probe("systemctl", "show", unit, *(f"--property={name}" for name in properties)) if unit else {
        "status": None, "stdout": "", "stderr": "job unit no longer present; no effective-property claim"}
    main_pid = probe("systemctl", "show", unit, "--property=MainPID", "--value")["stdout"].strip() if unit else "0"
    namespaces = {}
    for name, pid in [("guest_init", "1"), ("agent", str(os.getpid())), ("job_main", main_pid)]:
        try:
            namespaces[name] = os.readlink(f"/proc/{pid}/ns/net")
        except OSError as error:
            namespaces[name] = str(error)
    journal = probe("journalctl", "-b", "--no-pager", "-n", "2000")["stdout"]
    related = [line for line in journal.splitlines()
               if any(word in line.lower() for word in ["namespace", "bpf", "firewall", "device", (unit or "motor-job-").lower()])]
    return {"unit": unit, "unit_candidates": candidates, "properties": shown, "network_namespaces": namespaces,
            "guest_interfaces": socket.if_nameindex(), "namespace_journal": "\n".join(related)[-24000:]}


def control(conn):
    with conn:
        conn.settimeout(120)
        try:
            request = json.loads(conn.makefile("rb").readline(4096))
            op = request["op"]
            if op == "ready":
                result = ready()
            elif op == "network":
                result = network(request["profile"])
            elif op == "attach":
                result = attach(request["expected_bytes"])
            elif op == "ficlone":
                result = ficlone()
            elif op == "cache_fill":
                result = cache_fill()
            elif op == "cache_verify":
                result = cache_verify()
            elif op == "diagnostics":
                result = diagnostics(request["job_id"])
            elif op == "runner_diagnostics":
                result = runner_diagnostics()
            elif op == "verify_state":
                result = verify_state(request.get("require_existing", True))
            elif op == "prepare_snapshot":
                result = prepare_snapshot(request["kind"])
            elif op == "post_restore":
                result = post_restore()
            elif op == "detach":
                result = detach()
            elif op == "sync":
                os.sync()
                result = ready()
            else:
                raise ValueError(f"unknown spike operation: {op}")
            reply = {"ok": True, "result": result}
        except Exception as error:
            detail = f"{error}: {error.output}" if isinstance(error, subprocess.CalledProcessError) else str(error)
            reply = {"ok": False, "error": detail}
        conn.sendall(json.dumps(reply).encode() + b"\n")


def bridge(conn):
    with conn, socket.socket(socket.AF_UNIX) as runner:
        try:
            runner.connect(RUNNER)
            runner_bytes = 0
            while True:
                readers, _, _ = select.select([conn, runner], [], [], 120)
                if not readers:
                    return
                for source in readers:
                    data = source.recv(65536)
                    if not data:
                        if source is runner and runner_bytes == 0:
                            print(json.dumps({"runner_bridge_eof_before_reply": RUNNER}), file=sys.stderr, flush=True)
                        return
                    if source is runner:
                        runner_bytes += len(data)
                    (runner if source is conn else conn).sendall(data)
        except (OSError, TimeoutError) as error:
            print(json.dumps({"runner_bridge_error": str(error), "path": RUNNER}), file=sys.stderr, flush=True)
            return


def listen(port, handler):
    with socket.socket(getattr(socket, "AF_VSOCK"), socket.SOCK_STREAM) as server:
        server.bind((getattr(socket, "VMADDR_CID_ANY"), port))
        server.listen(16)
        while True:
            conn, _ = server.accept()
            threading.Thread(target=handler, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    threading.Thread(target=listen, args=(52, bridge), daemon=True).start()
    listen(54, control)
