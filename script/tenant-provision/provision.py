#!/usr/bin/env python3
"""Root-only, one-shot provisioning of a persistent tenant and size base snapshot."""
import argparse
import fcntl
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import socket
import struct
import subprocess
import tempfile
import time
import uuid

ROOT = Path("/var/lib/motor-vm")
REGISTRY = Path("/etc/motor-vm-helper/tenants")
BINDINGS = Path("/etc/motor-host/tenants.json")
BIN = Path("/usr/local/lib/firecracker/1.17.0")
SCHEMA_PATH = Path(__file__).resolve().parents[1] / "tenant-vm-spike/api_schema.py"
spec = importlib.util.spec_from_file_location("tenant_firecracker_schema", SCHEMA_PATH)
assert spec is not None and spec.loader is not None
schema_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(schema_module)
SCHEMA = schema_module.SCHEMA


def run(*args):
    return subprocess.run([str(a) for a in args], check=True, capture_output=True, text=True, timeout=300)


def component(value):
    if not isinstance(value, str) or not value or value in (".", "..") or "/" in value or "\0" in value or len(value.encode()) > 128:
        raise ValueError("expected a single nonempty path component")
    return value


def checked(config):
    allowed = {"tenant", "sub", "vm", "image_version", "vcpu_ceiling", "guest_ram_mib", "disk_quota_bytes", "cpuset", "tap_index",
               "idle_suspend_s", "host_floor_bytes", "memory_headroom_mib", "snapshot_memory_high_mib", "io_weight", "resolvers", "capture_console"}
    if set(config) - allowed:
        raise ValueError("unknown provisioning key")
    c = dict(config)
    if "capture_console" in c and type(c["capture_console"]) is not bool:
        raise ValueError("capture_console must be boolean")
    for name in ("tenant", "vm", "image_version"):
        component(c[name])
    if not re.fullmatch(r"(?:acct|org):[0-9A-HJKMNP-TV-Z]{26}", c["sub"]):
        raise ValueError("invalid tenant subject")
    for name, default in (("idle_suspend_s", 600), ("host_floor_bytes", 128 * 1024**3),
                          ("memory_headroom_mib", 512), ("snapshot_memory_high_mib", 16384), ("io_weight", 100)):
        c.setdefault(name, default)
    for name in ("vcpu_ceiling", "guest_ram_mib", "disk_quota_bytes", "tap_index", "idle_suspend_s", "memory_headroom_mib", "snapshot_memory_high_mib", "io_weight"):
        if type(c[name]) is not int or c[name] <= 0:
            raise ValueError(f"{name} must be a positive integer")
    if type(c["host_floor_bytes"]) is not int or c["host_floor_bytes"] < 0:
        raise ValueError("host floor must be nonnegative bytes")
    if c["vcpu_ceiling"] > 256 or c["tap_index"] > 254 or c["io_weight"] > 10000 or c["memory_headroom_mib"] < 512:
        raise ValueError("invalid tenant limits")
    if not re.fullmatch(r"[0-9]+(?:-[0-9]+)?(?:,[0-9]+(?:-[0-9]+)?)*", c["cpuset"]):
        raise ValueError("invalid cpuset")
    for part in c["cpuset"].split(","):
        bounds = [int(n) for n in part.split("-")]
        if bounds[0] > bounds[-1]:
            raise ValueError("reversed cpuset range")
    if not c.get("resolvers"):
        raise ValueError("public IPv4 resolvers are required")
    import ipaddress
    if any(not isinstance(ipaddress.ip_address(r), ipaddress.IPv4Address) or not ipaddress.ip_address(r).is_global for r in c["resolvers"]):
        raise ValueError("resolvers must be public IPv4 addresses")
    if c["disk_quota_bytes"] + c["guest_ram_mib"] * 1048576 + c["host_floor_bytes"] >= 2**64:
        raise ValueError("space requirement overflows u64")
    return c


def runner_args(c):
    return (f"--backend host --tenant-vm --job-network veth --state-root /tenant/state "
            f"--max-weight {c['vcpu_ceiling']} --rustc-slots {c['vcpu_ceiling']} "
            f"--sccache-size-gib {min(200, c['disk_quota_bytes'] // (4 * 1024**3))} "
            f"--free-space-floor-bytes {c['disk_quota_bytes'] // 10} "
            "--workspace-root /Users --owned-workspace-root /Users "
            "--workspace-root /home --owned-workspace-root /home\n")


def base_path(c, image):
    return image / "base" / f"{c['vcpu_ceiling']}-{c['guest_ram_mib']}"


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def atomic(path, value):
    tmp = path.with_name(path.name + ".pending")
    fd = os.open(tmp, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "w") as out:
            out.write(value)
            out.flush()
            os.fsync(out.fileno())
        os.replace(tmp, path)
        sync_dir(path.parent)
    finally:
        tmp.unlink(missing_ok=True)


def trusted(path):
    for p in [path, *path.parents]:
        st = p.lstat()
        if p.is_symlink() or st.st_uid != 0 or st.st_mode & 0o022:
            raise ValueError(f"path must be root-owned, non-writable and not a symlink: {p}")


class HTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=300)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def api(root, method, path, body=None):
    SCHEMA.validate(method, path, body)
    conn = HTTP(root / "api.sock")
    try:
        conn.request(method, path, None if body is None else json.dumps(body), {"Content-Type": "application/json"})
        response = conn.getresponse()
        data = response.read()
        if response.status not in (200, 204):
            raise RuntimeError(f"Firecracker {path}: {response.status} {data!r}")
    finally:
        conn.close()


def agent(root, request):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(30)
        sock.connect(str(root / "vsock.sock"))
        sock.sendall(b"CONNECT 54\n")
        answer = b""
        while len(answer) < 128 and not answer.endswith(b"\n"):
            answer += sock.recv(1) or (_ for _ in ()).throw(EOFError())
        if not answer.startswith(b"OK ") or not answer.endswith(b"\n"):
            raise RuntimeError("vsock handshake refused")
        body = json.dumps(request).encode()
        sock.sendall(struct.pack(">I", len(body)) + body)
        def exact(n):
            data = b""
            while len(data) < n:
                chunk = sock.recv(n - len(data))
                if not chunk:
                    raise EOFError()
                data += chunk
            return data
        size, = struct.unpack(">I", exact(4))
        if not 0 < size <= 1048576:
            raise ValueError("agent frame length")
        reply = json.loads(exact(size))
        if reply.get("ok") is not True:
            raise RuntimeError(f"guest refused base preparation: {reply}")


def boot_args(image):
    args = json.loads((image / "manifest.json").read_text())["boot_args"]
    if not isinstance(args, str) or not args.strip() or any(ch in args for ch in "\r\n\0"):
        raise ValueError("manifest boot_args must be a nonempty single line")
    return args


def build_base(c, image, base, console=None):
    """Kernel boot happens only here, with an unmounted placeholder /dev/vdb."""
    args_from_manifest = boot_args(image)
    base.parent.mkdir(parents=True, exist_ok=True)
    state = Path(tempfile.mkdtemp(prefix=".base-", dir=base.parent))
    boundary = Path(tempfile.mkdtemp(prefix="provision-", dir=ROOT / "jail"))
    identifier = "base-" + uuid.uuid4().hex[:10]
    netns = identifier
    root = boundary / "firecracker" / identifier / "root"
    root.mkdir(parents=True)
    uid = 400000 + c["tap_index"]
    proc = None
    mounts = []
    parent_group = Path("/sys/fs/cgroup/motor-tenant-provision")
    group = parent_group / identifier
    namespace_created = False
    try:
        os.chown(root, uid, uid)
        parent_group.mkdir(exist_ok=True)
        (parent_group.parent / "cgroup.subtree_control").write_text("+cpu +cpuset +memory +io +pids")
        (parent_group / "cgroup.subtree_control").write_text("+cpu +cpuset +memory +io +pids")
        # A network namespace containing only this tap makes base boot incapable
        # of reaching the host; the load API substitutes each real tenant tap.
        run("ip", "netns", "add", netns)
        namespace_created = True
        run("ip", "-n", netns, "tuntap", "add", "dev", "base0", "mode", "tap", "user", uid)
        run("ip", "-n", netns, "link", "set", "base0", "up")
        placeholder = state / "placeholder.img"
        run("fallocate", "-l", 512 * 1048576, placeholder)
        os.chown(placeholder, uid, uid)
        os.chown(state, uid, uid)
        for source, name, readonly, directory in [(image / "vmlinux", "kernel", True, False), (image / "rootfs", "rootfs.ext4", True, False),
                                                   (placeholder, "tenant.img", False, False), (state, "state", False, True)]:
            target = root / name
            target.mkdir() if directory else target.touch()
            run("mount", "--bind", source, target)
            mounts.append(target)
            if readonly:
                run("mount", "-o", "remount,bind,ro", target)
        args = [BIN / "jailer", "--id", identifier, "--exec-file", BIN / "firecracker", "--uid", uid, "--gid", uid,
                "--chroot-base-dir", boundary, "--netns", f"/run/netns/{netns}", "--cgroup-version", "2", "--parent-cgroup", "motor-tenant-provision",
                "--cgroup", "cpu.max=100000 100000", "--cgroup", f"cpuset.cpus={c['cpuset']}",
                "--cgroup", f"memory.max={(c['guest_ram_mib'] + c['memory_headroom_mib']) * 1048576}",
                "--cgroup", f"pids.max={c['vcpu_ceiling'] + 64}", "--cgroup", f"io.weight=default {c['io_weight']}",
                "--", "--api-sock", "/api.sock"]
        proc = subprocess.Popen([str(a) for a in args], stdin=subprocess.DEVNULL,
                                stdout=console if console is not None else subprocess.DEVNULL,
                                stderr=console if console is not None else None)
        deadline = time.monotonic() + 10
        while True:
            try:
                api(root, "GET", "/version")
                break
            except OSError:
                if proc.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("base Firecracker did not become ready")
                time.sleep(.02)
        for path, body in [("/machine-config", {"vcpu_count": c["vcpu_ceiling"], "mem_size_mib": c["guest_ram_mib"], "smt": False}),
                           ("/boot-source", {"kernel_image_path": "/kernel", "boot_args": args_from_manifest}),
                           ("/drives/rootfs", {"drive_id": "rootfs", "path_on_host": "/rootfs.ext4", "is_root_device": True, "is_read_only": True}),
                           ("/drives/tenant", {"drive_id": "tenant", "path_on_host": "/tenant.img", "is_root_device": False, "is_read_only": False}),
                           ("/network-interfaces/eth0", {"iface_id": "eth0", "host_dev_name": "base0", "guest_mac": "06:00:ac:10:00:02"}),
                           ("/vsock", {"guest_cid": 3, "uds_path": "/vsock.sock"}),
                           ("/actions", {"action_type": "InstanceStart"})]:
            api(root, "PUT", path, body)
        deadline = time.monotonic() + 60
        while True:
            try:
                agent(root, {"op": "prepare_snapshot", "kind": "base"})
                break
            except OSError:
                if proc.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("base guest agent did not become ready")
                time.sleep(.02)
        api(root, "PATCH", "/vm", {"state": "Paused"})
        high = group / "memory.high"
        old = high.read_text()
        high.write_text(str(min(c["snapshot_memory_high_mib"], c["guest_ram_mib"]) * 1048576))
        try:
            api(root, "PUT", "/snapshot/create", {"snapshot_type": "Full", "snapshot_path": "/state/base.vmstate", "mem_file_path": "/state/base.memory", "sync_snapshot_files": False})
        finally:
            high.write_text(old)
        for name in ("base.vmstate", "base.memory"):
            with (state / name).open("rb") as file:
                os.fsync(file.fileno())
            os.chown(state / name, 0, 0)
            (state / name).chmod(0o444)
        sync_dir(state)
    finally:
        if proc is not None:
            proc.kill()
            proc.wait(timeout=10)
        for target in reversed(mounts):
            run("umount", target)
        if group.exists():
            group.rmdir()
        if namespace_created:
            run("ip", "netns", "del", netns)
        shutil.rmtree(boundary)
    placeholder.unlink()
    os.chown(state, 0, 0)
    state.chmod(0o755)
    sync_dir(state)
    os.rename(state, base)
    sync_dir(base.parent)


def provision(c, make_base=build_base, execute=run, stage=None):
    home = ROOT / "tenants" / c["tenant"]
    image = ROOT / "tenant-images" / c["image_version"]
    base = base_path(c, image)
    registry = REGISTRY / (c["tenant"] + ".json")
    if home.exists() or registry.exists():
        raise ValueError("tenant already exists; refusing to change or delete its disk")
    if base.exists() and any(not (base / name).is_file() for name in ("base.vmstate", "base.memory")):
        raise ValueError("incomplete size base; refusing to reuse it")
    bindings = json.loads(BINDINGS.read_text()) if BINDINGS.exists() else {}
    if c["vm"] in bindings or any(v["tenant"] == c["tenant"] for v in bindings.values()):
        raise ValueError("tenant binding already exists")
    for entry in REGISTRY.glob("*.json"):
        if json.loads(entry.read_text())["tap_index"] == c["tap_index"]:
            raise ValueError("tap index already assigned")
    manifest = json.loads((image / "manifest.json").read_text())
    if manifest["version"] != c["image_version"]:
        raise ValueError("image version mismatch")
    for name, key in (("vmlinux", "kernel_sha256"), ("rootfs", "rootfs_sha256")):
        with (image / name).open("rb") as file:
            hasher = hashlib.sha256()
            for chunk in iter(lambda: file.read(1048576), b""):
                hasher.update(chunk)
            digest = hasher.hexdigest()
        if digest != manifest[key]:
            raise ValueError(f"image digest mismatch: {name}")
    # Byte accounting includes suspend room AND a new size base. Neither a disk
    # nor a directory is created if the available byte count is one byte short.
    needed = c["disk_quota_bytes"] + c["guest_ram_mib"] * 1048576 + c["host_floor_bytes"]
    if not base.exists():
        needed += c["guest_ram_mib"] * 1048576 + 512 * 1048576
    if shutil.disk_usage(ROOT).free < needed:
        raise ValueError("insufficient host free bytes")
    if not base.exists():
        make_base(c, image, base)
    home.mkdir(mode=0o700)
    (home / "suspend").mkdir(mode=0o700)
    disk = home / "disk.img"
    execute("fallocate", "-l", c["disk_quota_bytes"], disk)
    disk.chmod(0o600)
    execute("mkfs.xfs", "-m", "reflink=1", disk)
    # This is the only host mount of the tenant filesystem, before any VM can
    # attach it. Later cleanup never mounts, reflinks or deletes a tenant disk.
    with tempfile.TemporaryDirectory(prefix="tenant-init-", dir=ROOT) as mount:
        execute("mount", "-o", "loop", disk, mount)
        try:
            atomic(Path(mount) / "runner-args", runner_args(c))
            if stage is not None:
                stage(Path(mount))
        finally:
            execute("umount", mount)
    with disk.open("rb") as file:
        os.fsync(file.fileno())
    atomic(home / "generation", "0\n")
    record = {k: v for k, v in c.items() if k not in {"tenant", "sub", "vm", "resolvers"}}
    record.update(disk=str(disk), generation=str(home / "generation"), suspend=str(home / "suspend"))
    atomic(registry, json.dumps(record, indent=2) + "\n")
    registry.chmod(0o644)
    bindings[c["vm"]] = {"tenant": c["tenant"], "sub": c["sub"]}
    atomic(BINDINGS, json.dumps(bindings, indent=2) + "\n")
    # The host agent can read the binding map, but only root may replace it.
    BINDINGS.chmod(0o644)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("provisioning requires root on the dedicated VM host")
    trusted(args.config.resolve())
    c = checked(json.loads(args.config.read_text()))
    for directory in (ROOT, REGISTRY, BINDINGS.parent, ROOT / "jail", ROOT / "tenants"):
        directory.mkdir(parents=True, exist_ok=True)
        trusted(directory)
    trusted(ROOT / "tenant-images" / c["image_version"] / "manifest.json")
    for name in ("vmlinux", "rootfs"):
        trusted(ROOT / "tenant-images" / c["image_version"] / name)
    base = base_path(c, ROOT / "tenant-images" / c["image_version"])
    if base.exists():
        for name in ("base.vmstate", "base.memory"):
            trusted(base / name)
            if (base / name).stat().st_size == 0:
                raise ValueError("empty size base snapshot")
    for binary in (BIN / "firecracker", BIN / "jailer"):
        trusted(binary)
        if "1.17.0" not in run(binary, "--version").stdout:
            raise ValueError("Firecracker and jailer must both be 1.17.0")
    with (ROOT / "provision.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        provision(c)


if __name__ == "__main__":
    main()
