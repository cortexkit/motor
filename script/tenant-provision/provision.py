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
import stat
import struct
import subprocess
import sys
import tempfile
import time
import traceback
import uuid

ROOT = Path("/var/lib/motor-vm")
REGISTRY = Path("/etc/motor-vm-helper/tenants")
BINDINGS = Path("/etc/motor-host/tenants.json")
PROVISION = Path("/etc/motor-host/provision")
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


def _atomic_restore(path, value, mode):
    tmp = path.with_name(f".{path.name}.{uuid.uuid4().hex}.pending")
    fd = os.open(tmp, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "wb") as out:
            os.fchmod(out.fileno(), mode)
            out.write(value)
            out.flush()
            os.fsync(out.fileno())
        os.replace(tmp, path)
        sync_dir(path.parent)
    finally:
        tmp.unlink(missing_ok=True)


def atomic_pair(updates):
    """Write each file atomically and restore files already replaced on failure."""
    staged = []
    replaced = []
    try:
        for path, value in updates:
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode):
                raise ValueError(f"refusing to replace a non-regular file: {path}")
            original = path.read_bytes()
            mode = stat.S_IMODE(info.st_mode)
            tmp = path.with_name(f".{path.name}.{uuid.uuid4().hex}.pending")
            fd = os.open(tmp, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
            try:
                with os.fdopen(fd, "wb") as out:
                    os.fchmod(out.fileno(), mode)
                    out.write(value.encode() if isinstance(value, str) else value)
                    out.flush()
                    os.fsync(out.fileno())
            except BaseException:
                tmp.unlink(missing_ok=True)
                raise
            staged.append((path, tmp, original, mode))

        for path, tmp, original, mode in staged:
            os.replace(tmp, path)
            replaced.append((path, original, mode))
            sync_dir(path.parent)
    except BaseException as error:
        restore_errors = []
        for path, original, mode in reversed(replaced):
            try:
                _atomic_restore(path, original, mode)
            except Exception as restore_error:
                restore_errors.append(f"{path}: {restore_error}")
        if restore_errors:
            raise RuntimeError(f"record update failed ({error}); rollback also failed: {'; '.join(restore_errors)}") from error
        raise
    finally:
        for _, tmp, _, _ in staged:
            tmp.unlink(missing_ok=True)


def systemd_active_state(unit):
    try:
        return run("systemctl", "show", "--property=ActiveState", "--value", unit).stdout.strip()
    except subprocess.CalledProcessError as error:
        raise ValueError(f"could not verify that {unit} is stopped") from error


def inventory_image(image):
    builder = Path(__file__).resolve().parents[1] / "tenant-image" / "build.py"
    subprocess.run([sys.executable, str(builder), "inventory", str(image)], check=True)


def suspend_snapshot_to_move(suspend, manifest):
    if not suspend.exists():
        return None
    if suspend.is_symlink() or not suspend.is_dir():
        raise ValueError(f"suspend path is not a directory: {suspend}")
    key_path = suspend / "key.json"
    snapshot_files = (suspend / "suspend.vmstate", suspend / "suspend.memory")
    if not key_path.exists() and not key_path.is_symlink():
        if any(path.exists() or path.is_symlink() for path in snapshot_files):
            raise ValueError(f"suspend snapshot has no image key; refusing to switch: {suspend}")
        return None
    try:
        if not stat.S_ISREG(key_path.lstat().st_mode):
            raise ValueError("snapshot key is not a regular file")
        key = json.loads(key_path.read_text())
        mismatch = any(key[name] != manifest[name] for name in ("version", "kernel_sha256", "rootfs_sha256"))
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise ValueError(f"cannot verify suspend snapshot image key: {key_path}") from error
    return suspend if mismatch else None


def move_stale_suspend(suspend, old_version):
    moved = suspend.with_name(f"{suspend.name}.stale-{old_version}-{time.time_ns()}")
    os.rename(suspend, moved)
    sync_dir(suspend.parent)
    try:
        suspend.mkdir(mode=0o700)
        sync_dir(suspend.parent)
    except BaseException:
        os.rename(moved, suspend)
        sync_dir(suspend.parent)
        raise
    return moved


def switch_image(tenant, image_version, make_base=None, check_service=None, validate_image=None, output=print):
    tenant = component(tenant)
    image_version = component(image_version)
    lock_path = ROOT / "provision.lock"
    with lock_path.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        check_service = systemd_active_state if check_service is None else check_service
        validate_image = inventory_image if validate_image is None else validate_image
        make_base = build_base if make_base is None else make_base
        for unit in ("motor-host.service", "motor-vm-helper.service"):
            state = check_service(unit)
            if state != "inactive":
                raise ValueError(f"{unit} must be stopped before switching a tenant image (ActiveState={state})")

        provision_path = PROVISION / f"{tenant}.json"
        registry_path = REGISTRY / f"{tenant}.json"
        original_config = json.loads(provision_path.read_text())
        current = checked(original_config)
        if current["tenant"] != tenant:
            raise ValueError(f"provisioning record tenant does not match {tenant}")
        record = json.loads(registry_path.read_text())
        if not isinstance(record, dict) or record.get("image_version") != current["image_version"]:
            raise ValueError("provisioning record and helper registry image versions differ")
        for name, value in record.items():
            if name in current and value != current[name]:
                raise ValueError(f"provisioning record and helper registry differ at {name}")

        image = ROOT / "tenant-images" / image_version
        if not image.is_dir():
            raise ValueError(f"tenant image does not exist: {image}")
        validate_image(image)
        manifest = json.loads((image / "manifest.json").read_text())
        if manifest.get("version") != image_version:
            raise ValueError("image manifest version mismatch")

        updated = checked({**original_config, "image_version": image_version})
        base = base_path(updated, image)
        if base.exists() or base.is_symlink():
            if base.is_symlink() or not base.is_dir():
                raise ValueError("size base is not a directory; refusing to reuse it")
            for name in ("base.vmstate", "base.memory"):
                artifact = base / name
                if not stat.S_ISREG(artifact.lstat().st_mode) or artifact.stat().st_size == 0:
                    raise ValueError("incomplete size base; refusing to reuse it")
        else:
            make_base(updated, image, base)

        suspend = ROOT / "tenants" / tenant / "suspend"
        stale = suspend_snapshot_to_move(suspend, manifest)
        old_version = current["image_version"]
        moved = move_stale_suspend(stale, old_version) if stale is not None else None
        record["image_version"] = image_version
        updated_config = dict(original_config)
        updated_config["image_version"] = image_version
        if old_version != image_version:
            try:
                atomic_pair([
                    (registry_path, json.dumps(record, indent=2) + "\n"),
                    (provision_path, json.dumps(updated_config, indent=2) + "\n"),
                ])
            except BaseException:
                if moved is not None:
                    suspend.rmdir()
                    os.rename(moved, suspend)
                    sync_dir(suspend.parent)
                raise

        output(f"{tenant}: {old_version} -> {image_version}")
        if moved is not None:
            output(f"moved stale suspend snapshot to {moved}")
        output(f"rollback: sudo python3 {Path(__file__).resolve()} switch-image {tenant} {old_version}")
        return moved


# Registry fields provision() derives from the tenant's home directory rather
# than copying from the provisioning record.
REGISTRY_PATHS = {"disk": "disk.img", "generation": "generation", "suspend": "suspend"}
# Provisioning fields provision() leaves out of the helper registry.
PROVISION_ONLY = {"tenant", "sub", "vm", "resolvers"}


def repair_from_registry(tenant, validate_image=None, check_trusted=None, output=print):
    """Adopt the helper registry's image_version into the provisioning record.

    The registry is what the VM helper boots. An image swapped by hand, before
    switch-image existed, left the provisioning record naming the old image,
    and switch-image refuses records that disagree. This repairs only that one
    disagreement: the registry's image must exist and validate, and every other
    registry field must be exactly what provision() would have written from
    the provisioning record. The record is replaced atomically with its mode
    kept and its key order unchanged, so it stays byte-identical to what
    hosted-host/install.sh renders for the adopted image.
    """
    tenant = component(tenant)
    validate_image = inventory_image if validate_image is None else validate_image
    check_trusted = trusted if check_trusted is None else check_trusted
    with (ROOT / "provision.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        provision_path = PROVISION / f"{tenant}.json"
        registry_path = REGISTRY / f"{tenant}.json"
        original_config = json.loads(provision_path.read_text())
        current = checked(original_config)
        if current["tenant"] != tenant:
            raise ValueError(f"provisioning record tenant does not match {tenant}")
        record = json.loads(registry_path.read_text())
        if not isinstance(record, dict) or "image_version" not in record:
            raise ValueError("helper registry has no image_version")
        adopted = component(record["image_version"])
        expected = {name: value for name, value in current.items() if name not in PROVISION_ONLY}
        home = ROOT / "tenants" / tenant
        expected.update({name: str(home / leaf) for name, leaf in REGISTRY_PATHS.items()})
        for name in sorted((set(record) | set(expected)) - {"image_version"}):
            if name not in record or name not in expected or record[name] != expected[name]:
                raise ValueError(f"provisioning record and helper registry differ at {name}; "
                                 "only image_version can be repaired from the registry")

        image = ROOT / "tenant-images" / adopted
        if not image.is_dir():
            raise ValueError(f"registry image does not exist: {image}")
        for path in (image / "manifest.json", image / "vmlinux", image / "rootfs"):
            check_trusted(path)
        validate_image(image)
        if json.loads((image / "manifest.json").read_text()).get("version") != adopted:
            raise ValueError("registry image manifest version mismatch")

        old_version = current["image_version"]
        if old_version == adopted:
            output(f"{provision_path}: already names the registry image {adopted}; nothing changed")
            return False
        updated_config = dict(original_config)
        updated_config["image_version"] = adopted
        checked(updated_config)
        atomic_pair([(provision_path, json.dumps(updated_config, indent=2) + "\n")])
        output(f"{provision_path}: image_version {old_version} -> {adopted} (from {registry_path}); no other field changed")
        return True


def trusted(path):
    for p in [path, *path.parents]:
        st = p.lstat()
        if p.is_symlink() or st.st_uid != 0 or st.st_mode & 0o022:
            raise ValueError(f"path must be root-owned, non-writable and not a symlink: {p}")


# motor-runner's default free-space floor (crates/motor-runner/src/engine.rs):
# below it the runner refuses jobs. Between it and the runner's higher eviction
# threshold the runner deletes idle workspaces and caches by design; nothing
# fails there.
RUNNER_DEFAULT_FLOOR = 128 * 1024**3
# The runner's options that take no value; every other option takes one.
RUNNER_FLAGS = {"--tenant-vm", "--boat-profile"}


def runner_processes(proc=Path("/proc")):
    """(pid, argv, environment) of every live `motor-runner serve` on this host.

    A runner whose arguments or environment cannot be read is still returned,
    with None in place of what was unreadable, so the caller fails closed."""
    found = []
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            argv = [a.decode(errors="replace") for a in (entry / "cmdline").read_bytes().split(b"\0")[:-1]]
        except OSError:
            continue  # exited, or a kernel thread
        name = os.path.basename(argv[0]) if argv else ""
        # deploy.sh installs instance binaries as motor-runner-<instance>; serve
        # is the default operation only when no operation is named.
        serving = argv[1:2] == ["serve"] or (name == "motor-runner" and len(argv) == 1)
        if not (name == "motor-runner" or name.startswith("motor-runner-")) or not serving:
            continue
        try:
            pairs = (entry / "environ").read_bytes().split(b"\0")
            environ = dict(p.decode(errors="replace").split("=", 1) for p in pairs if b"=" in p)
        except OSError:
            environ = None
        found.append((int(entry.name), argv, environ))
    return found


def runner_floor(argv, environ):
    """(state root, refusal floor bytes) of one runner, from its argv."""
    options = {}
    rest = argv[2:]
    while rest:
        if rest[0] in RUNNER_FLAGS:
            rest = rest[1:]
            continue
        if len(rest) < 2:
            raise ValueError(f"cannot parse motor-runner arguments: {argv}")
        options[rest[0]] = rest[1]
        rest = rest[2:]
    floor = int(options.get("--free-space-floor-bytes", RUNNER_DEFAULT_FLOOR))
    if "--state-root" in options:
        return Path(options["--state-root"]), floor
    if environ is None or not (environ.get("XDG_DATA_HOME") or environ.get("HOME")):
        raise ValueError(f"cannot find the state root of motor-runner {argv}: its environment is unreadable")
    data = Path(environ["XDG_DATA_HOME"]) if environ.get("XDG_DATA_HOME") else Path(environ["HOME"]) / ".local/share"
    return data / "cortexkit/motor-runner", floor


def device(path):
    """Filesystem of the nearest existing ancestor of path."""
    for p in [path, *path.parents]:
        try:
            return p.stat().st_dev
        except FileNotFoundError:
            continue
    raise ValueError(f"no existing ancestor of {path}")


def runner_reserve(root, runners):
    """(floor, pid) of the motor-runner with the highest refusal floor whose state
    shares root's filesystem, or (0, None). A tenant must never push free space
    there below it: that runner would refuse every job."""
    reserve, owner = 0, None
    for pid, argv, environ in runners:
        state, floor = runner_floor(argv, environ)
        if device(state) == device(root) and floor > reserve:
            reserve, owner = floor, pid
    return reserve, owner


class HTTP(http.client.HTTPConnection):
    def __init__(self, path, timeout=300):
        super().__init__("localhost", timeout=timeout)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def api(root, method, path, body=None, record=None, timeout=300):
    SCHEMA.validate(method, path, body)
    if record is not None:
        record("api_request", method=method, path=path, body=body)
    conn = HTTP(root / "api.sock", timeout=timeout)
    try:
        conn.request(method, path, None if body is None else json.dumps(body), {"Content-Type": "application/json"})
        response = conn.getresponse()
        data = response.read()
        if record is not None:
            record("api_response", method=method, path=path, status=response.status, body=data.decode(errors="replace"))
        if response.status not in (200, 204):
            raise RuntimeError(f"Firecracker {path}: {response.status} {data!r}")
        return json.loads(data) if data else None
    finally:
        conn.close()


class AgentNotReady(ConnectionError):
    """Transport failed before any guest control request was submitted."""


def agent(root, request):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(1)
        try:
            sock.connect(str(root / "vsock.sock"))
            sock.sendall(b"CONNECT 54\n")
            answer = b""
            while len(answer) < 128 and not answer.endswith(b"\n"):
                chunk = sock.recv(1)
                if not chunk:
                    raise AgentNotReady(f"vsock CONNECT 54 closed before handshake completed: {answer!r}")
                answer += chunk
        except AgentNotReady:
            raise
        except OSError as error:
            raise AgentNotReady(f"vsock CONNECT 54: {type(error).__name__}: {error}") from error
        if not answer.startswith(b"OK ") or not answer.endswith(b"\n"):
            raise RuntimeError(f"vsock handshake refused: {answer!r}")
        sock.settimeout(30)
        body = json.dumps(request).encode()
        sock.sendall(struct.pack(">I", len(body)) + body)
        def exact(n):
            data = b""
            while len(data) < n:
                chunk = sock.recv(n - len(data))
                if not chunk:
                    raise EOFError(f"guest control reply closed after request submission ({len(data)}/{n} bytes)")
                data += chunk
            return data
        size, = struct.unpack(">I", exact(4))
        if not 0 < size <= 1048576:
            raise ValueError("agent frame length")
        reply = json.loads(exact(size))
        if reply.get("ok") is not True:
            raise RuntimeError(f"guest refused base preparation: {reply}")


def wait_for_base_agent(root, proc, record, timeout=60, probe=agent, clock=time.monotonic, pause=time.sleep):
    deadline = clock() + timeout
    attempt = 0
    while True:
        status = proc.poll()
        if status is not None:
            raise RuntimeError(f"base VMM exited before guest readiness: returncode={status}")
        attempt += 1
        try:
            probe(root, {"op": "prepare_snapshot", "kind": "base"})
        except AgentNotReady as error:
            record("agent_not_ready", attempt=attempt, exception_type=type(error).__name__, error=str(error))
            status = proc.poll()
            if status is not None:
                raise RuntimeError(f"base VMM exited during guest readiness: returncode={status}; {error}") from error
            if clock() >= deadline:
                raise TimeoutError(f"base agent readiness exceeded {timeout}s while VMM was running; last transport error: {error}") from error
            pause(.02)
        else:
            record("agent_ready", attempt=attempt)
            return


def boot_args(image):
    args = json.loads((image / "manifest.json").read_text())["boot_args"]
    if not isinstance(args, str) or not args.strip() or any(ch in args for ch in "\r\n\0"):
        raise ValueError("manifest boot_args must be a nonempty single line")
    return args


class BootEvidence:
    """Optional operator artifacts that survive removal of a failed base jail."""
    def __init__(self, directory):
        self.directory = directory
        self.config = {}
        if directory is not None:
            directory.mkdir(mode=0o700, parents=True, exist_ok=False)

    def record(self, kind, **fields):
        if self.directory is None:
            return
        event = dict(kind=kind, monotonic_s=time.monotonic(), **fields)
        with (self.directory / "events.jsonl").open("a") as output:
            output.write(json.dumps(event) + "\n")
        if kind == "api_request" and fields["method"] == "PUT":
            self.config[fields["path"]] = fields["body"]
            (self.directory / "submitted-config.json").write_text(json.dumps(self.config, indent=2) + "\n")

    def cgroup(self, group):
        if self.directory is None:
            return
        values = {}
        for name in ("memory.events", "memory.current", "memory.peak", "pids.current", "cgroup.events", "cpu.stat", "io.stat"):
            path = group / name
            if path.exists():
                values[name] = path.read_text()
        self.record("cgroup_before_stop", path=str(group), values=values)

    def preserve(self, state):
        if self.directory is not None:
            for name in ("firecracker.log", "metrics.jsonl"):
                source = state / name
                if source.exists():
                    shutil.copyfile(source, self.directory / name)

    def stop(self, proc, group, state, vmm_api, failure):
        if proc is not None:
            before = proc.poll()
            self.record("vmm_before_cleanup", pid=proc.pid, returncode=before,
                        reason=type(failure).__name__ if failure is not None else "base_snapshot_complete")
            self.cgroup(group)
            if self.directory is not None and before is None:
                for method, path, body in [("GET", "/vm/config", None), ("PUT", "/actions", {"action_type": "FlushMetrics"})]:
                    try:
                        reply = vmm_api(method, path, body, timeout=1)
                        if path == "/vm/config":
                            (self.directory / "observed-config.json").write_text(json.dumps(reply, indent=2) + "\n")
                    except Exception as error:
                        self.record("diagnostic_error", path=path, exception_type=type(error).__name__, error=str(error) or repr(error))
            current = proc.poll()
            self.record("vmm_after_diagnostics", pid=proc.pid, returncode=current)
            if current is None:
                self.record("host_kill", pid=proc.pid, signal="SIGKILL", reason="base_boot_cleanup")
                proc.kill()
            after = proc.wait(timeout=10)
            self.record("vmm_after_cleanup", pid=proc.pid, returncode=after)
        self.preserve(state)


def base_requests(c, args):
    return [("/machine-config", {"vcpu_count": c["vcpu_ceiling"], "mem_size_mib": c["guest_ram_mib"], "smt": False}),
            ("/boot-source", {"kernel_image_path": "/kernel", "boot_args": args}),
            ("/drives/rootfs", {"drive_id": "rootfs", "path_on_host": "/rootfs.ext4", "is_root_device": True, "is_read_only": True}),
            ("/drives/tenant", {"drive_id": "tenant", "path_on_host": "/tenant.img", "is_root_device": False, "is_read_only": False}),
            ("/network-interfaces/eth0", {"iface_id": "eth0", "host_dev_name": "base0", "guest_mac": "06:00:ac:10:00:02"}),
            ("/vsock", {"guest_cid": 3, "uds_path": "/vsock.sock"}),
            ("/actions", {"action_type": "InstanceStart"})]


def build_base(c, image, base, console=None, diagnostics=None):
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
    evidence = BootEvidence(diagnostics)
    stderr = None
    failure = None
    def vmm_api(method, path, body=None, timeout=300):
        return api(root, method, path, body, record=evidence.record, timeout=timeout)
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
        if diagnostics is not None:
            for name in ("firecracker.log", "metrics.jsonl"):
                path = state / name
                path.touch(mode=0o600)
                os.chown(path, uid, uid)
            stderr = (diagnostics / "vmm-stderr.log").open("ab", buffering=0)
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
        evidence.record("jailer_launch", argv=[str(a) for a in args])
        proc = subprocess.Popen([str(a) for a in args], stdin=subprocess.DEVNULL,
                                stdout=console if console is not None else subprocess.DEVNULL,
                                stderr=stderr if stderr is not None else (console if console is not None else None))
        deadline = time.monotonic() + 10
        while True:
            try:
                vmm_api("GET", "/version", timeout=1)
                break
            except OSError as error:
                evidence.record("api_not_ready", exception_type=type(error).__name__, error=str(error) or repr(error))
                if proc.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(f"base Firecracker API not ready: returncode={proc.poll()}; {type(error).__name__}: {error}") from error
                time.sleep(.02)
        if diagnostics is not None:
            vmm_api("PUT", "/logger", {"log_path": "/state/firecracker.log", "level": "Debug", "show_level": True, "show_log_origin": True})
            vmm_api("PUT", "/metrics", {"metrics_path": "/state/metrics.jsonl"})
        for path, body in base_requests(c, args_from_manifest):
            vmm_api("PUT", path, body)
        wait_for_base_agent(root, proc, evidence.record)
        vmm_api("PATCH", "/vm", {"state": "Paused"})
        high = group / "memory.high"
        old = high.read_text()
        high.write_text(str(min(c["snapshot_memory_high_mib"], c["guest_ram_mib"]) * 1048576))
        try:
            vmm_api("PUT", "/snapshot/create", {"snapshot_type": "Full", "snapshot_path": "/state/base.vmstate", "mem_file_path": "/state/base.memory", "sync_snapshot_files": False})
        finally:
            high.write_text(old)
        for name in ("base.vmstate", "base.memory"):
            with (state / name).open("rb") as file:
                os.fsync(file.fileno())
            os.chown(state / name, 0, 0)
            (state / name).chmod(0o444)
        sync_dir(state)
    except BaseException as error:
        failure = error
        evidence.record("base_boot_failure", exception_type=type(error).__name__, error=str(error) or repr(error), traceback=traceback.format_exc())
        raise
    finally:
        cleanup_errors = []
        def cleanup(label, action):
            try:
                action()
                return True
            except Exception as error:
                cleanup_errors.append(f"{label}: {type(error).__name__}: {str(error) or repr(error)}")
                return False
        cleanup("diagnostics and VMM stop", lambda: evidence.stop(proc, group, state, vmm_api, failure))
        # A diagnostic write failure must not leave the VMM alive or prevent
        # independent unmount and namespace cleanup attempts.
        if proc is not None and proc.poll() is None:
            cleanup("fallback VMM kill", proc.kill)
            cleanup("fallback VMM wait", lambda: proc.wait(timeout=10))
        if stderr is not None:
            cleanup("stderr close", stderr.close)
        unmounted = True
        for target in reversed(mounts):
            unmounted = cleanup(f"unmount {target}", lambda target=target: run("umount", target)) and unmounted
        if group.exists():
            cleanup("cgroup removal", group.rmdir)
        if namespace_created:
            cleanup("namespace removal", lambda: run("ip", "netns", "del", netns))
        if unmounted:
            cleanup("jail removal", lambda: shutil.rmtree(boundary))
        if cleanup_errors:
            original = f"{type(failure).__name__}: {str(failure) or repr(failure)}; " if failure is not None else ""
            raise RuntimeError(original + "base cleanup failed: " + "; ".join(cleanup_errors)) from failure
    placeholder.unlink()
    os.chown(state, 0, 0)
    state.chmod(0o755)
    sync_dir(state)
    os.rename(state, base)
    sync_dir(base.parent)


def provision(c, make_base=build_base, execute=run, stage=None, runners=None):
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
    free = shutil.disk_usage(ROOT).free
    if free < needed:
        raise ValueError("insufficient host free bytes")
    # On a host that also runs motor-runner, the bytes this tenant can consume
    # (its disk, a suspend snapshot and any new size base) must leave free space
    # at or above that runner's refusal floor. Between the floor and its
    # eviction threshold the runner evicts idle workspaces, which is expected.
    consumed = needed - c["host_floor_bytes"]
    reserve, pid = runner_reserve(ROOT, runner_processes() if runners is None else runners)
    # The free figure is rounded down, so a shortfall never prints as equal.
    if reserve and free - consumed < reserve:
        raise ValueError(
            f"tenant would leave {(free - consumed) * 10 // 1024**3 / 10:.1f} GiB free on {ROOT} ({free} free, tenant needs {consumed} bytes), "
            f"below motor-runner pid {pid}'s refusal floor {reserve / 1024**3:.1f} GiB; "
            "lower disk_quota_bytes or free space first")
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
    parser.add_argument("--config", type=Path)
    commands = parser.add_subparsers(dest="command")
    switch = commands.add_parser("switch-image", help="change an existing tenant's immutable image")
    switch.add_argument("tenant")
    switch.add_argument("image_version")
    repair = commands.add_parser(
        "repair-from-registry",
        help="set the provisioning record's image_version to the helper registry's, when nothing else differs")
    repair.add_argument("tenant")
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("provisioning requires root on the dedicated VM host")
    if args.command == "repair-from-registry":
        if args.config is not None:
            parser.error("repair-from-registry takes a tenant, not --config")
        tenant = component(args.tenant)
        for directory in (ROOT, REGISTRY, PROVISION, ROOT / "tenants", ROOT / "tenants" / tenant):
            trusted(directory)
        trusted(PROVISION / f"{tenant}.json")
        trusted(REGISTRY / f"{tenant}.json")
        repair_from_registry(tenant)
        return
    if args.command == "switch-image":
        if args.config is not None:
            parser.error("switch-image takes a tenant and image version, not --config")
        tenant = component(args.tenant)
        image_version = component(args.image_version)
        image = ROOT / "tenant-images" / image_version
        for directory in (ROOT, REGISTRY, PROVISION, ROOT / "tenants", ROOT / "tenants" / tenant):
            trusted(directory)
        trusted(PROVISION / f"{tenant}.json")
        trusted(REGISTRY / f"{tenant}.json")
        trusted(image / "manifest.json")
        for name in ("vmlinux", "rootfs"):
            trusted(image / name)
        for binary in (BIN / "firecracker", BIN / "jailer"):
            trusted(binary)
            if "1.17.0" not in run(binary, "--version").stdout:
                raise ValueError("Firecracker and jailer must both be 1.17.0")
        switch_image(tenant, image_version)
        return
    if args.command is not None:
        parser.error(f"unknown command: {args.command}")
    if args.config is None:
        parser.error("--config is required when provisioning a new tenant")
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
