#!/usr/bin/env python3
"""Throwaway root-only vsock probe; never install this agent in a tenant image."""

import fcntl
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import threading
import time

GIB = 1024**3
RUNNER = "/tenant/state/spike/runner.sock"


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
    Path("/var/lib/motor-spike/host-only").write_text("not visible to an isolated job\n")
    Path("/tenant/persistent-marker").write_text("tenant-vm-spike\n")
    os.sync()
    command("systemctl", "start", "spike-runner")
    deadline = time.monotonic() + 30
    while not Path(RUNNER).exists():
        if time.monotonic() > deadline:
            raise RuntimeError(command("journalctl", "-u", "spike-runner", "--no-pager", "-n", "60"))
        time.sleep(0.05)
    return {"device": "/dev/vdb", "bytes": size,
            "filesystem": command("findmnt", "-n", "-o", "FSTYPE", "/tenant"),
            "owned_workspace_root": "/Users", "workspace_source": "/tenant/state/workspaces",
            "workspace_owner_uid": Path("/Users").stat().st_uid,
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
            elif op == "sync":
                os.sync()
                result = ready()
            else:
                raise ValueError(f"unknown spike operation: {op}")
            reply = {"ok": True, "result": result}
        except Exception as error:
            reply = {"ok": False, "error": str(error)}
        conn.sendall(json.dumps(reply).encode() + b"\n")


def bridge(conn):
    with conn, socket.socket(socket.AF_UNIX) as runner:
        try:
            runner.connect(RUNNER)
            while True:
                readers, _, _ = select.select([conn, runner], [], [], 120)
                if not readers:
                    return
                for source in readers:
                    data = source.recv(65536)
                    if not data:
                        return
                    (runner if source is conn else conn).sendall(data)
        except (OSError, TimeoutError):
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
