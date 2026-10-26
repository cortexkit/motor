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
import types
import uuid

ROOT = Path("/var/lib/motor-vm")
REGISTRY = Path("/etc/motor-vm-helper/tenants")
BINDINGS = Path("/etc/motor-host/tenants.json")
PROVISION = Path("/etc/motor-host/provision")
BIN = Path("/usr/local/lib/firecracker/1.17.0")
CAPACITY = Path("/etc/motor-host/capacity.json")
MEMINFO = Path("/proc/meminfo")
JOBS_DROP_IN = Path("/etc/systemd/system/motor-jobs.slice.d/50-capacity.conf")
U64_MAX = 2**64 - 1
CAPACITY_KEYS = {"windows_vm_bytes", "host_reserve_bytes", "jobs_min_bytes", "storage_margin_bytes",
                 "growth_allowance_bytes", "tenant_memory_reserve_bytes"}
U32_MAX = 2**32 - 1
# motor-runner's default free-space floor (crates/motor-runner/src/engine.rs,
# DEFAULT_FREE_SPACE_FLOOR_BYTES): below it the runner refuses jobs.
# Between it and the runner's higher eviction threshold the runner deletes idle
# workspaces and caches by design; nothing fails there.
RUNNER_DEFAULT_FLOOR = 128 * 1024**3
# The VMM ranks below jobs (+900) and above runners (-900) with the kernel's
# OOM killer, so a host OOM prefers a job over the tenant's VM.
VMM_OOM_SCORE_ADJ = -500
# Room mkfs needs beyond the operating requirement when it writes a filesystem.
METADATA_ALLOWANCE_BYTES = 2 * 1024**3
SCHEMA_PATH = Path(__file__).resolve().parents[1] / "tenant-vm-spike/api_schema.py"
spec = importlib.util.spec_from_file_location("tenant_firecracker_schema", SCHEMA_PATH)
assert spec is not None and spec.loader is not None
schema_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(schema_module)
SCHEMA = schema_module.SCHEMA


def run(*args):
    return subprocess.run([str(a) for a in args], check=True, capture_output=True, text=True, timeout=300)


def reset_egress_counter(index):
    path = Path(__file__).resolve().parents[1] / 'runner/motor_network_policy.py'
    policy = types.ModuleType('tenant_network_policy')
    policy.__file__ = str(path)
    exec(compile(path.read_bytes(), str(path), 'exec'), policy.__dict__)
    policy.reset_tenant_counter(index)


def u64(value, name):
    if type(value) is not int or not 0 <= value <= U64_MAX:
        raise ValueError(f"{name} must be a nonnegative u64 integer")
    return value


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate capacity.json key: {key}")
        result[key] = value
    return result


def read_capacity():
    try:
        fd = os.open(CAPACITY, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd) as source:
            metadata = os.fstat(source.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o022:
                raise ValueError("capacity.json must be a root-owned regular file, not group/world-writable")
            capacity = json.load(source, object_pairs_hook=unique_object)
    except (OSError, ValueError) as error:
        raise ValueError(f"invalid capacity.json: {error}") from error
    if type(capacity) is not dict or set(capacity) != CAPACITY_KEYS:
        raise ValueError("capacity.json must contain exactly the six capacity keys")
    for key, value in capacity.items():
        u64(value, f"capacity.json {key}")
    jobs_slice_limits(capacity)
    return capacity


def memtotal_bytes():
    try:
        text = MEMINFO.read_text()
    except (OSError, ValueError) as error:
        raise ValueError(f"cannot read MemTotal: {error}") from error
    matches = re.findall(r"^MemTotal:\s+([0-9]+) kB\s*$", text, re.MULTILINE)
    if len(matches) != 1:
        raise ValueError("missing or malformed MemTotal in /proc/meminfo")
    return u64(int(matches[0]) * 1024, "MemTotal conversion")


def memory_fit(capacity, replacement=None):
    total = sum(capacity[key] for key in ("windows_vm_bytes", "host_reserve_bytes", "jobs_min_bytes"))
    tenants = []
    for entry in sorted(REGISTRY.glob("*.json")):
        if replacement is None or entry.stem != replacement["tenant"]:
            tenants.append(json.loads(entry.read_text()))
    if replacement is not None:
        tenants.append(replacement)
    for tenant in tenants:
        ram = u64(tenant["guest_ram_mib"], "guest_ram_mib")
        headroom = u64(tenant["memory_headroom_mib"], "memory_headroom_mib")
        total = u64(total + (ram + headroom) * 1048576, "memory fit sum")
    u64(total, "memory fit sum")
    available = memtotal_bytes()
    if total > available:
        raise ValueError(f"memory fit requires {total} bytes, MemTotal is {available} bytes")


def jobs_slice_limits(capacity):
    jobs = u64(capacity["jobs_min_bytes"], "capacity.json jobs_min_bytes")
    windows = u64(capacity["windows_vm_bytes"], "capacity.json windows_vm_bytes")
    if jobs == 0:
        raise ValueError("capacity.json jobs_min_bytes must be at least 1")
    # The slice holds Windows and direct Linux jobs. jobs_min_bytes remains
    # available for Linux when Windows uses its full allowance.
    maximum = u64(jobs + windows, "capacity.json jobs-slice MemoryMax")
    high = maximum - maximum // 6
    return maximum, high


def jobs_slice_render(capacity):
    maximum, high = jobs_slice_limits(capacity)
    return f"[Slice]\nMemoryMax={maximum}\nMemoryHigh={high}\n"


def slice_properties(names):
    try:
        result = run("systemctl", "show", "-p", ",".join(names), "motor-jobs.slice")
    except (OSError, subprocess.SubprocessError) as error:
        raise ValueError(f"could not read motor-jobs.slice {','.join(names)}: {error}") from error
    values = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
    if any(name not in values for name in names):
        raise ValueError(f"missing motor-jobs.slice properties: {','.join(names)}")
    return values


def slice_bytes(value, name):
    if not re.fullmatch(r"[0-9]+", value):
        raise ValueError(f"motor-jobs.slice {name} is not a byte value: {value}")
    return u64(int(value), f"motor-jobs.slice {name}")


def verify_jobs_slice(capacity):
    try:
        exact = JOBS_DROP_IN.read_text() == jobs_slice_render(capacity)
    except OSError as error:
        raise ValueError(f"missing or unreadable jobs-slice drop-in: {JOBS_DROP_IN}") from error
    if not exact:
        raise ValueError("jobs-slice drop-in differs from capacity.json; run apply-jobs-slice")
    values = slice_properties(("LoadState", "MemoryMax"))
    if values["LoadState"] == "not-found":
        return
    if values["LoadState"] != "loaded":
        raise ValueError(f"motor-jobs.slice LoadState={values['LoadState']}")
    maximum = slice_bytes(values["MemoryMax"], "MemoryMax")
    limit, _high = jobs_slice_limits(capacity)
    if maximum > limit:
        raise ValueError(f"motor-jobs.slice MemoryMax={maximum} exceeds {limit}")


def refuse_resize_journal():
    journals = sorted((ROOT / "tenants").glob("*/resize.json"))
    if journals:
        raise ValueError(f"pending resize journal: {journals[0]}")


def apply_jobs_slice():
    # Validate before creating even the lock file, then re-read under the lock.
    read_capacity()
    with (ROOT / "provision.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        capacity = read_capacity()
        refuse_resize_journal()
        memory_fit(capacity)
        rendered = jobs_slice_render(capacity)
        values = slice_properties(("LoadState", "MemoryCurrent", "ActiveState", "ControlGroup"))
        if values["LoadState"] == "not-found":
            current = 0
        elif values["LoadState"] == "loaded":
            # systemd synthesizes an unloaded slice as loaded but inactive,
            # without a cgroup or a memory counter. Only that empty state is zero.
            if (values["MemoryCurrent"] == "[not set]" and values["ActiveState"] == "inactive"
                    and values["ControlGroup"] == ""):
                current = 0
            else:
                current = slice_bytes(values["MemoryCurrent"], "MemoryCurrent")
        else:
            raise ValueError(f"motor-jobs.slice LoadState={values['LoadState']}")
        _maximum, high = jobs_slice_limits(capacity)
        if current > high:
            raise ValueError(f"motor-jobs.slice MemoryCurrent={current} exceeds rendered MemoryHigh={high}")
        JOBS_DROP_IN.parent.mkdir(parents=True, exist_ok=True)
        atomic(JOBS_DROP_IN, rendered)
        JOBS_DROP_IN.chmod(0o644)
        try:
            run("systemctl", "daemon-reload")
        except (OSError, subprocess.SubprocessError) as error:
            raise ValueError(f"failed to apply jobs-slice drop-in: {error}") from error
        verify_jobs_slice(capacity)


def storage_threshold(c, capacity):
    """Host free bytes below which motor-host refuses new leases.

    motor-host mirrors this formula over every registered tenant; the reserve
    keeps a suspend possible at the threshold, because a suspend needs room
    for the guest's RAM plus the host floor.
    """
    reserve = c["guest_ram_mib"] * 1048576 if c["idle_suspend_s"] > 0 else 0
    threshold = u64(c["host_floor_bytes"] + reserve, "host floor and suspend reserve")
    return u64(threshold + capacity["storage_margin_bytes"], "storage threshold")


def operating_requirement(c, capacity, threshold=None):
    """Free bytes provisioning and resize must leave for motor-host.

    threshold, when given, is the host-wide storage threshold over every
    tenant (see host_storage_threshold); otherwise only c's own term counts.
    """
    threshold = storage_threshold(c, capacity) if threshold is None else threshold
    return u64(threshold + capacity["storage_margin_bytes"]
               + capacity["growth_allowance_bytes"], "operating requirement")


def host_storage_threshold(capacity, replacement):
    """The storage threshold over every registered tenant, with replacement's
    record (which names its tenant) standing in for that tenant's registry
    entry. motor-host refuses leases below the largest per-tenant term."""
    records = [record for name, record in registered_tenants() if name != replacement["tenant"]]
    return max(storage_threshold(record, capacity) for record in records + [replacement])


def pre_operation_free_bytes(c, capacity, base_present, threshold=None):
    """Free host bytes the operation needs before it creates anything.

    A resize of the size base itself must fit: an absent base needs the
    guest's RAM plus the 512 MiB placeholder, and every operation needs 2 GiB
    for the filesystem metadata mkfs writes.
    """
    needed = u64(operating_requirement(c, capacity, threshold) + METADATA_ALLOWANCE_BYTES, "metadata allowance")
    if not base_present:
        needed = u64(needed + c["guest_ram_mib"] * 1048576 + 512 * 1048576, "size base space")
    return needed


def base_present(c, image):
    base = base_path(c, image)
    return base.exists() and all((base / name).is_file() for name in ("base.vmstate", "base.memory"))


def image_capability(manifest, name):
    """Whether an image manifest lists the named capability."""
    capabilities = manifest.get("capabilities", [])
    if not isinstance(capabilities, list):
        raise ValueError("image manifest capabilities must be a list")
    return name in capabilities


def sparse_v1(manifest):
    """Whether an image manifest lists the sparse-v1 capability."""
    return image_capability(manifest, "sparse-v1")


def component(value):
    if not isinstance(value, str) or not value or value in (".", "..") or "/" in value or "\0" in value or len(value.encode()) > 128:
        raise ValueError("expected a single nonempty path component")
    return value


def checked(config):
    allowed = {"tenant", "sub", "vm", "image_version", "vcpu_ceiling", "guest_ram_mib", "disk_quota_bytes", "cpuset", "tap_index",
               "idle_suspend_s", "host_floor_bytes", "memory_headroom_mib", "snapshot_memory_high_mib", "io_weight", "resolvers",
               "capture_console", "max_weight", "remote_cache"}
    if set(config) - allowed:
        raise ValueError("unknown provisioning key")
    c = dict(config)
    if "capture_console" in c and type(c["capture_console"]) is not bool:
        raise ValueError("capture_console must be boolean")
    if "remote_cache" in c and type(c["remote_cache"]) is not bool:
        raise ValueError("remote_cache must be boolean")
    for name in ("tenant", "vm", "image_version"):
        component(c[name])
    if not re.fullmatch(r"(?:acct|org):[0-9A-HJKMNP-TV-Z]{26}", c["sub"]):
        raise ValueError("invalid tenant subject")
    for name, default in (("idle_suspend_s", 600), ("host_floor_bytes", 128 * 1024**3),
                          ("memory_headroom_mib", 512), ("snapshot_memory_high_mib", 16384), ("io_weight", 100)):
        c.setdefault(name, default)
    for name in ("vcpu_ceiling", "guest_ram_mib", "disk_quota_bytes", "tap_index", "memory_headroom_mib", "snapshot_memory_high_mib", "io_weight"):
        if type(c[name]) is not int or c[name] <= 0:
            raise ValueError(f"{name} must be a positive integer")
    # Zero idle means the tenant never suspends; motor-host reads it that way.
    if type(c["idle_suspend_s"]) is not int or c["idle_suspend_s"] < 0:
        raise ValueError("idle_suspend_s must be a nonnegative integer")
    if type(c["host_floor_bytes"]) is not int or c["host_floor_bytes"] < 0:
        raise ValueError("host floor must be nonnegative bytes")
    if c["host_floor_bytes"] < RUNNER_DEFAULT_FLOOR:
        # The direct runner refuses jobs below its own free-space floor. A host
        # floor under that lets the threshold sit below the point where the
        # runner already stopped admitting.
        raise ValueError(f"host_floor_bytes must be at least the direct runner floor {RUNNER_DEFAULT_FLOOR}")
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
    if "max_weight" in c:
        # The guest's own parser reads max_weight as a u32, so a larger value
        # would boot a runner that refuses its arguments.
        weight = c["max_weight"]
        if type(weight) is not int or not 0 < weight <= U32_MAX:
            raise ValueError("max_weight must be an integer between 1 and 4294967295")
        if weight < c["vcpu_ceiling"]:
            raise ValueError("max_weight must be at least vcpu_ceiling")
    return c


def runner_args(c):
    # The permit budget is max_weight when the operator set it, and the vCPU
    # ceiling otherwise, so a record without the key produces the same bytes
    # it always did. The guest requires the two values to be equal.
    weight = c.get("max_weight", c["vcpu_ceiling"])
    # A tenant provisioned with remote compile cache files turns the runner's
    # cache switch on. A record without the key renders the same bytes it
    # always did, so resize keeps rendering what install wrote.
    cache = "--remote-cache true " if c.get("remote_cache") else ""
    return (f"--backend host --tenant-vm --job-network veth --state-root /tenant/state "
            f"--max-weight {weight} --rustc-slots {weight} "
            f"--sccache-size-gib {min(200, c['disk_quota_bytes'] // (4 * 1024**3))} "
            f"--free-space-floor-bytes {c['disk_quota_bytes'] // 10} "
            f"{cache}"
            "--workspace-root /Users --owned-workspace-root /Users "
            "--workspace-root /home --owned-workspace-root /home\n")


# The tenant runner's remote compile cache provisioning: the files motor-runner
# reads from <state root>/<instance>/cache-provisioning
# (crates/motor-runner/src/remote_cache/provisioning.rs), with the modes it
# requires. The guest runs the runner as root, so root-owned files written here
# are owned by the runner's account. A write key is optional: without it the
# tenant's jobs read the cache and the tenant never fills it.
REMOTE_CACHE_FILES = {"config.json": 0o444, "read-key": 0o400, "write-key": 0o400}
REMOTE_CACHE_REQUIRED = {"config.json", "read-key"}
REMOTE_CACHE_CONFIG_KEYS = {"generation", "tenant", "endpoint", "region", "bucket", "prefix", "storage_class", "enabled"}
REMOTE_CACHE_KEY_KEYS = {"generation", "access_key_id", "secret_access_key"}
# The directory under the tenant disk's root (the guest's /tenant): the
# runner's state root, its `main` instance, then the provisioning directory.
# The modes are the ones the guest agent and the runner create them with.
REMOTE_CACHE_DIRECTORIES = (("state", 0o755), ("main", 0o700), ("cache-provisioning", 0o700))
# A record of which cache files install wrote and their modes, never their
# contents, so check-installed can report it without mounting the tenant disk.
REMOTE_CACHE_MANIFEST = "remote-cache.json"
REMOTE_CACHE_MAX_FILE = 64 * 1024


def read_remote_cache(directory, sub):
    """Read the operator's cache provisioning directory for tenant subject
    `sub` and return each file's bytes by name.

    The checks catch operator mistakes before the disk is written; the runner
    checks the files again when it reads them. Errors name a file and the
    problem, never a file's contents: the key files hold secrets."""
    names = {entry.name for entry in directory.iterdir()}
    unknown = names - set(REMOTE_CACHE_FILES)
    if unknown:
        raise ValueError(f"unexpected file in {directory}: {sorted(unknown)[0]}")
    missing = REMOTE_CACHE_REQUIRED - names
    if missing:
        raise ValueError(f"{directory} lacks {', '.join(sorted(missing))}")
    files, parsed = {}, {}
    for name in sorted(names):
        path = directory / name
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode):
            raise ValueError(f"{name} is not a regular file")
        if info.st_size > REMOTE_CACHE_MAX_FILE:
            raise ValueError(f"{name} is larger than {REMOTE_CACHE_MAX_FILE} bytes")
        if name != "config.json" and info.st_mode & 0o077:
            raise ValueError(f"{name} must be readable by its owner alone")
        data = path.read_bytes()
        try:
            value = json.loads(data)
        except ValueError:
            # The parser's message can quote the text it choked on.
            raise ValueError(f"{name} is not valid JSON") from None
        expected = REMOTE_CACHE_CONFIG_KEYS if name == "config.json" else REMOTE_CACHE_KEY_KEYS
        if not isinstance(value, dict) or set(value) != expected:
            raise ValueError(f"{name} must be a JSON object with exactly the keys {', '.join(sorted(expected))}")
        if not isinstance(value["generation"], str) or not value["generation"]:
            raise ValueError(f"{name}: generation must be a non-empty string")
        files[name], parsed[name] = data, value
    config = parsed["config.json"]
    if config["tenant"] != sub:
        raise ValueError("config.json: tenant must be the tenant's subject")
    # The store policy confines the tenant's keys to this prefix; the runner
    # refuses any other.
    if config["prefix"] != f"tenant/{sub.replace(':', '-')}/":
        raise ValueError("config.json: prefix must be tenant/<subject with each ':' replaced by '-'>/")
    if any(value["generation"] != config["generation"] for value in parsed.values()):
        raise ValueError("the cache provisioning files carry different generations")
    return files


def install_remote_cache(mount, files):
    """Write the cache provisioning files into a freshly made tenant disk
    mounted at `mount`, with the runner's exact modes."""
    directory = mount
    for name, mode in REMOTE_CACHE_DIRECTORIES:
        directory = directory / name
        directory.mkdir(mode=mode, exist_ok=True)
        if directory.is_symlink() or not directory.is_dir():
            raise ValueError(f"not a directory on the new tenant disk: {directory}")
        os.chmod(directory, mode)
    for name, data in files.items():
        fd = os.open(directory / name, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as out:
            os.fchmod(out.fileno(), REMOTE_CACHE_FILES[name])
            out.write(data)
            out.flush()
            os.fsync(out.fileno())
    while directory != mount:
        sync_dir(directory)
        directory = directory.parent
    sync_dir(mount)


def remote_cache_manifest(files):
    """The manifest text for the installed files: names and modes only."""
    return json.dumps({name: f"{REMOTE_CACHE_FILES[name]:04o}" for name in sorted(files)}, indent=2) + "\n"


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
    # Integrity only: switch-image also goes back to an older installed image
    # for a rollback, whose pins rightly differ from today's versions.txt.
    builder = Path(__file__).resolve().parents[1] / "tenant-image" / "build.py"
    subprocess.run([sys.executable, str(builder), "verify", str(image)], check=True)


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
    """Point a stopped tenant at another image.

    The tenant's VM must not be running: it would keep the old image and be
    killed later. It runs only while motor-vm-helper.service is active, so both
    services must be stopped first, with STOP_COMMAND. Stopping the helper is
    the clean stop: on SIGTERM it has each running guest drain, sync and freeze
    its disk before the VM is stopped, and records any it cannot.
    """
    tenant = component(tenant)
    image_version = component(image_version)
    lock_path = ROOT / "provision.lock"
    with lock_path.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        refuse_resize_journal()
        check_service = systemd_active_state if check_service is None else check_service
        validate_image = inventory_image if validate_image is None else validate_image
        make_base = build_base if make_base is None else make_base
        for unit in ("motor-host.service", "motor-vm-helper.service"):
            state = check_service(unit)
            if state != "inactive":
                raise ValueError(f"{unit} must be stopped before switching a tenant image (ActiveState={state}). "
                                 f"Stop both with: {STOP_COMMAND} "
                                 "(the helper stops each running tenant VM cleanly before it exits)")

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
        if unclean_stop_evidence(tenant):
            output(f"warning: {tenant}'s last VM did not stop cleanly ({RUNNING_MARKER} is present); "
                   "its next start boots fresh and runs the in-guest recovery check")
        output(f"rollback: sudo python3 {Path(__file__).resolve()} switch-image {tenant} {old_version}")
        return moved


# The helper writes this before a tenant VM starts and removes it only when the
# guest has flushed its disk and the VM stopped. With the helper stopped, a
# marker means the last stop cut the guest's power.
RUNNING_MARKER = "running.json"
STOP_COMMAND = "sudo systemctl stop motor-host.service motor-vm-helper.service"


def unclean_stop_evidence(tenant):
    home = ROOT / "tenants" / tenant
    return (home / RUNNING_MARKER).exists()


# Registry fields provision() derives from the tenant's home directory rather
# than copying from the provisioning record.
REGISTRY_PATHS = {"disk": "disk.img", "generation": "generation", "suspend": "suspend"}
# Provisioning fields provision() leaves out of the helper registry. The helper
# and motor-host refuse unknown registry fields, and remote_cache matters only
# to the runner arguments, which are rendered from the provisioning record.
PROVISION_ONLY = {"tenant", "sub", "vm", "resolvers", "remote_cache"}


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
        refuse_resize_journal()
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


# How long a removed tenant's data waits in the quarantine folder before it is
# deleted. motor-vm-helper's purge (crates/motor-vm-helper/src/removal.rs,
# QUARANTINE_S) deletes such folders at this age, so a removal made here is
# purged on the same schedule as one the host made. The public privacy page
# promises a departed customer's disk is deleted within 30 days; 7 days of
# quarantine (time to undo a mistake) stays well inside that.
QUARANTINE_S = 7 * 24 * 60 * 60


def removal_layout():
    """What a tenant removal touches, with {tenant}, {unix_s} and {tap}
    placeholders. removal-layout.json pins this, and the helper's own removal
    (motor-vm-helper's removal::layout) is pinned to the same file, so the
    command and the host's automatic removal delete the same set."""
    return {
        "lock": str(ROOT / "provision.lock"),
        "quarantine": str(ROOT / "removed" / "{tenant}-{unix_s}"),
        "moved": {str(ROOT / "tenants" / "{tenant}"): "home"},
        "records": {
            str(REGISTRY / "{tenant}.json"): "records/registry.json",
            str(PROVISION / "{tenant}.json"): "records/provision.json",
        },
        "binding": {"file": str(BINDINGS), "saved_as": "records/binding.json"},
        "network": [
            "tap mtap{tap}",
            "nft chain tenant_mtap{tap}_input",
            "nft chain tenant_mtap{tap}_forward",
            "nft chain tenant_mtap{tap}_nat",
            "nft counter tenant_mtap{tap}_egress",
            "ufw route motor-tenant-mtap{tap}",
        ],
        "purge_age_s": QUARANTINE_S,
    }


def tap_exists(tap):
    return subprocess.run(["ip", "link", "show", "dev", tap], capture_output=True).returncode == 0


def tap_metadata(tap):
    result = subprocess.run(['ip', '-j', '-d', 'link', 'show', 'dev', tap], check=True,
                            capture_output=True, text=True, env={**os.environ, 'LC_ALL': 'C'})
    if len(result.stdout) > 65536:
        raise ValueError('tenant TAP metadata exceeds read bound')
    links = json.loads(result.stdout)
    if not isinstance(links, list) or len(links) != 1:
        raise ValueError('ambiguous tenant TAP metadata')
    return links[0]


def require_owned_tap(tap, tap_index, link):
    if not isinstance(link, dict):
        raise ValueError(f'refusing to delete foreign interface {tap}')
    info = link.get('linkinfo', {})
    data = info.get('info_data', {})
    if (link.get('ifname') != tap or info.get('info_kind') != 'tun' or data.get('type') != 'tap'
            or data.get('persist') is not True or type(data.get('user')) is not int
            or data['user'] != 400000 + tap_index):
        raise ValueError(f'refusing to delete foreign interface {tap}')


def remove_tenant(tenant, dry_run=False, purge_now=False, check_service=None, execute=run, has_tap=None,
                  now=time.time, output=print, tap_info=None):
    """Remove a departed customer's tenant from this host.

    Refuses unless motor-host.service and motor-vm-helper.service are both
    stopped, exactly as switch-image does: with the helper stopped no tenant VM
    runs, and with motor-host stopped no lease or session exists. Then, under
    the provision lock, it saves the tenant's records into a root-only
    quarantine folder, deletes the tap, drops the binding, provisioning record
    and registry entry, and moves the tenant's home (disk, generation, suspend
    snapshots, set-aside stale snapshots and stop markers) into the folder.
    The egress counter is reset before the index is released. The helper deletes
    the folder after QUARANTINE_S; --purge-now deletes it
    at once. The firewall exemption and UFW route grant are dropped by the
    helper's startup reconciliation, which keeps only registered tenants.
    The shared size base under the image is not per tenant and is kept.
    A dry run lists everything it would remove and changes nothing.
    """
    tenant = component(tenant)
    check_service = systemd_active_state if check_service is None else check_service
    has_tap = tap_exists if has_tap is None else has_tap
    tap_info = tap_metadata if tap_info is None else tap_info
    with (ROOT / "provision.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for unit in ("motor-host.service", "motor-vm-helper.service"):
            state = check_service(unit)
            if state != "inactive":
                raise ValueError(f"{unit} must be stopped before removing a tenant (ActiveState={state}). "
                                 f"Stop both with: {STOP_COMMAND} "
                                 "(the helper stops each running tenant VM cleanly before it exits)")
        registry_path = REGISTRY / f"{tenant}.json"
        provision_path = PROVISION / f"{tenant}.json"
        if not registry_path.exists():
            raise ValueError(f"no helper registry entry for tenant {tenant}: {registry_path}")
        record = json.loads(registry_path.read_text())
        tap_index = record.get("tap_index") if isinstance(record, dict) else None
        if type(tap_index) is not int or not 0 < tap_index < 255:
            raise ValueError("helper registry entry has no valid tap_index")
        if provision_path.exists():
            current = checked(json.loads(provision_path.read_text()))
            if current["tenant"] != tenant or current["tap_index"] != tap_index:
                raise ValueError("provisioning record and helper registry disagree; inspect them before removing")
        bindings = json.loads(BINDINGS.read_text()) if BINDINGS.exists() else {}
        binding = {vm: entry for vm, entry in bindings.items() if entry.get("tenant") == tenant}
        home = ROOT / "tenants" / tenant
        if home.is_symlink() or (home.exists() and not home.is_dir()):
            raise ValueError(f"tenant home is not a directory: {home}")
        unix_s = int(now())
        quarantine = ROOT / "removed" / f"{tenant}-{unix_s}"
        tap = f"mtap{tap_index}"
        destination = "delete" if purge_now else f"quarantine in {quarantine} until the helper purges it"
        plan = []
        if home.exists():
            plan += [f"{destination}: {path}" for path in [home, *sorted(home.rglob("*"))]]
        plan += [f"{destination}: {path}" for path in (registry_path, provision_path) if path.exists()]
        plan += [f"{destination}: binding {vm} in {BINDINGS}" for vm in sorted(binding)]
        if has_tap(tap):
            require_owned_tap(tap, tap_index, tap_info(tap))
            plan.append(f"delete: tap {tap}")
        plan += [f"dropped at the next helper start: {name}" for name in (
            f"nft chain tenant_{tap}_input", f"nft chain tenant_{tap}_forward", f"nft chain tenant_{tap}_nat",
            f"ufw route motor-tenant-{tap}")]
        plan.append(f"reset: nft counter tenant_{tap}_egress")
        if dry_run:
            for line in plan:
                output(f"would {line}")
            return plan
        if unclean_stop_evidence(tenant):
            output(f"note: {tenant}'s last VM did not stop cleanly ({RUNNING_MARKER} is present); removing it anyway")

        removed = ROOT / "removed"
        if removed.is_symlink() or (removed.exists() and not removed.is_dir()):
            raise ValueError(f"removed-tenant folder is not a directory: {removed}")
        if not removed.exists():
            removed.mkdir(mode=0o700)
            sync_dir(ROOT)
        quarantine.mkdir(mode=0o700)
        (quarantine / "records").mkdir(mode=0o700)
        try:
            metadata = home.lstat()
        except FileNotFoundError:
            identity = None
        else:
            if not stat.S_ISDIR(metadata.st_mode):
                raise ValueError('tenant home is not a directory')
            identity = {'dev': metadata.st_dev, 'ino': metadata.st_ino}
        atomic(quarantine / 'records/home-identity.json', json.dumps(identity))
        for source, saved in ((registry_path, "registry.json"), (provision_path, "provision.json")):
            if source.exists():
                atomic(quarantine / "records" / saved, source.read_text())
        if binding:
            atomic(quarantine / "records" / "binding.json", json.dumps(binding, indent=2))
        sync_dir(removed)
        if has_tap(tap):
            require_owned_tap(tap, tap_index, tap_info(tap))
            execute("ip", "link", "delete", "dev", tap)
        reset_egress_counter(tap_index)
        if not home.exists():
            atomic(quarantine / 'complete', '1\n')
        if binding:
            kept = {vm: entry for vm, entry in bindings.items() if vm not in binding}
            atomic(BINDINGS, json.dumps(kept, indent=2) + "\n")
            BINDINGS.chmod(0o644)
        # The registry entry goes before the home moves, as in the helper: a
        # registry entry naming a missing disk would stop the helper starting.
        for path in (provision_path, registry_path):
            if path.exists():
                path.unlink()
                sync_dir(path.parent)
        if home.exists():
            os.rename(home, quarantine / "home")
            sync_dir(home.parent)
            sync_dir(quarantine)
        atomic(quarantine / 'complete', '1\n')
        if purge_now:
            shutil.rmtree(quarantine)
            sync_dir(removed)
            output(f"{tenant}: removed and deleted")
        else:
            output(f"{tenant}: removed; data quarantined in {quarantine} and deleted by the helper "
                   f"{QUARANTINE_S // 86400} days after {unix_s}")
        output("start the services again; the helper's startup reconciliation drops the tenant's firewall rules")
        return plan


def trusted(path):
    for p in [path, *path.parents]:
        st = p.lstat()
        if p.is_symlink() or st.st_uid != 0 or st.st_mode & 0o022:
            raise ValueError(f"path must be root-owned, non-writable and not a symlink: {p}")


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


def protect_vmm(pid, proc=None, oom=None, reread=None):
    """Give the VMM an OOM score below jobs (+900) and above runners (-900).

    The jailer execs Firecracker in place, so the jailer's pid is the VMM's.
    The score is set right after spawn, often before that exec, so the process
    may still be named `jailer`; oom_score_adj survives execve, so setting it
    then protects the VMM all the same. The pid is our own unreaped child, so
    nothing else can hold it. A set or read-back failure is fatal: a VMM ranked
    like a job could be the kernel's first victim on a host whose memory is
    already over-committed. `proc` and `oom` are the process's own /proc
    directory; tests pass a scratch directory instead.
    """
    own = Path("/proc") / str(int(pid))
    proc = own if proc is None else proc
    oom = own if oom is None else oom
    try:
        comm = (proc / "comm").read_text().strip()
    except OSError as error:
        raise RuntimeError(f"cannot confirm the VMM process: {error}") from error
    if comm not in ("jailer", "firecracker"):
        raise RuntimeError(f"expected jailer or firecracker, comm={comm!r}")

    def read_back():
        return (oom / "oom_score_adj").read_text()

    try:
        (oom / "oom_score_adj").write_text(str(VMM_OOM_SCORE_ADJ))
        actual = (reread or read_back)().strip()
    except OSError as error:
        raise RuntimeError(f"cannot set the VMM oom_score_adj: {error}") from error
    if actual != str(VMM_OOM_SCORE_ADJ):
        raise RuntimeError(f"oom readback: expected {VMM_OOM_SCORE_ADJ}, got {actual!r}")
    return dict(pid=pid, comm=comm, oom_score_adj=int(actual))


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
            ("/drives/tenant", {"drive_id": "tenant", "path_on_host": "/tenant.img", "is_root_device": False, "is_read_only": False,
                                "discard": True, "io_engine": "Sync"}),
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
        evidence.record("vmm_oom_protection", **protect_vmm(proc.pid))
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


def provision(c, make_base=build_base, execute=run, stage=None, runners=None, remote_cache=None):
    """Create a tenant. `remote_cache` is the cache provisioning files from
    read_remote_cache, required exactly when the record sets remote_cache.

    The files reach the guest only here, through the one host mount of a new
    tenant disk, and stay on that persistent disk across switch-image and
    resize. Replacing them (a key rotation, or adding or withdrawing a write
    key) needs a reinstall of the tenant until a delivery channel exists that
    passes them through motor-vm-helper and the guest agent at a cold start,
    the way a resize's staged runner arguments travel."""
    if bool(c.get("remote_cache")) != (remote_cache is not None):
        raise ValueError("remote_cache in the record and --remote-cache-dir must be given together")
    capacity = read_capacity()
    # An unfinished resize holds host sizing fixed until it is resumed or aborted.
    refuse_resize_journal()
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
    memory_fit(capacity, replacement=c)
    verify_jobs_slice(capacity)
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
    # Byte accounting includes the size base, the filesystem metadata mkfs
    # writes, and the space motor-host needs to stay admitting and to let the
    # disk grow. Neither a disk nor a directory is created if the available byte
    # count is one byte short.
    needed = pre_operation_free_bytes(c, capacity, base_present(c, image))
    free = shutil.disk_usage(ROOT).free
    if free < needed:
        raise ValueError(f"provisioning needs {needed} free bytes on {ROOT}; {free} are available")

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
    if sparse_v1(manifest):
        # The guest sees the full quota, but only the blocks it writes occupy
        # the host. Its trim support returns freed blocks to the host filesystem.
        with disk.open("wb") as file:
            file.truncate(c["disk_quota_bytes"])
    else:
        execute("fallocate", "-l", c["disk_quota_bytes"], disk)
    disk.chmod(0o600)
    execute("mkfs.xfs", "-m", "reflink=1", disk)
    # This is the only host mount of the tenant filesystem, before any VM can
    # attach it. Later cleanup never mounts, reflinks or deletes a tenant disk.
    with tempfile.TemporaryDirectory(prefix="tenant-init-", dir=ROOT) as mount:
        execute("mount", "-o", "loop", disk, mount)
        try:
            atomic(Path(mount) / "runner-args", runner_args(c))
            if remote_cache is not None:
                install_remote_cache(Path(mount), remote_cache)
            if stage is not None:
                stage(Path(mount))
        finally:
            execute("umount", mount)
    with disk.open("rb") as file:
        os.fsync(file.fileno())
    if remote_cache is not None:
        atomic(home / REMOTE_CACHE_MANIFEST, remote_cache_manifest(remote_cache))
    atomic(home / "generation", "0\n")
    record = {k: v for k, v in c.items() if k not in PROVISION_ONLY}
    record.update(disk=str(disk), generation=str(home / "generation"), suspend=str(home / "suspend"))
    # Only an unallocated, stopped index reaches publication. Reset legacy
    # deferred counters before a replacement registry can expose old usage.
    reset_egress_counter(c["tap_index"])
    atomic(registry, json.dumps(record, indent=2) + "\n")
    registry.chmod(0o644)
    bindings[c["vm"]] = {"tenant": c["tenant"], "sub": c["sub"]}
    atomic(BINDINGS, json.dumps(bindings, indent=2) + "\n")
    # The host agent can read the binding map, but only root may replace it.
    BINDINGS.chmod(0o644)


# ---------------------------------------------------------------------------
# Offline resize
#
# provision.py resize <tenant> changes a stopped tenant's vCPU ceiling, guest
# RAM, cpuset, permit budget, idle suspend and disk quota. RAM and disk only
# grow. Every step is recorded first in the tenant's resize.json journal, so a
# crash at any point leaves either the old state (resize --abort restores it
# exactly) or a committed state that a same-argument re-run finishes. The host
# never mounts the tenant disk here: the file only grows sparsely, and the
# guest grows its filesystem and installs the new runner arguments itself on
# the next cold start, from runner-args.staged.
# ---------------------------------------------------------------------------

HELPER_CONFIG = Path("/etc/motor-vm-helper.json")
# motor-vm-helper runs in system.slice with DelegateSubgroup=helper and places
# each tenant VMM in <its service cgroup>/tenants/tenant-<tap_index>
# (crates/motor-vm-helper/src/linux/tenant.rs). With the helper stopped, one
# that still exists means a VM's processes or cleanup outlived it.
HELPER_CGROUPS = Path("/sys/fs/cgroup/system.slice/motor-vm-helper.service/tenants")
# The Windows build VM's threads. hosted-host/hosted_host.py has the same
# WINDOWS_CPUS; a test keeps the two equal.
WINDOWS_CPUS = "12-15,28-31"
RESIZE_SCHEMA = 1
# The record fields a resize may change; every other field stays as provisioned.
RESIZE_FIELDS = ("vcpu_ceiling", "guest_ram_mib", "cpuset", "max_weight", "idle_suspend_s", "disk_quota_bytes")
# Forward phases, in order. Before "committed" the published records are the
# old ones and resize --abort restores everything; from "committed" on the
# resize only rolls forward.
RESIZE_PHASES = ("started", "base_built", "staged", "set_aside", "committed", "published", "extended")
RESIZE_COMMITTED = RESIZE_PHASES.index("committed")
# Written durably before the first compensation; a forward resume then refuses.
RESIZE_ABORTING = "aborting"
RESIZE_JOURNAL_KEYS = {"schema", "tenant", "phase", "old", "new", "provision_old", "provision_new",
                       "registry_old", "registry_new", "helper_old", "helper_new", "runner_args",
                       "quota_grew", "base", "base_existed", "set_aside", "staged"}
JOURNAL_RECOVERY = ("the tenant stays start-refused and resize and abort refuse; "
                    "follow 'Malformed resize journal' in script/tenant-provision/README.md")


def cpu_numbers(cpuset):
    """The CPU numbers of a cpuset list such as 0-11,16-27."""
    numbers = set()
    for part in cpuset.split(","):
        bounds = [int(n) for n in part.split("-")]
        numbers.update(range(bounds[0], bounds[-1] + 1))
    return numbers


def registered_tenants():
    """(tenant, helper registry record) of every registered tenant."""
    return [(entry.stem, json.loads(entry.read_text())) for entry in sorted(REGISTRY.glob("*.json"))]


def durable_write(path, data, mode):
    """Replace path: write an fsynced temporary file, rename it over path and
    fsync the directory, so after a crash path holds the old or the new bytes."""
    _atomic_restore(path, data.encode() if isinstance(data, str) else data, mode)


def resize_journal_path(tenant):
    return ROOT / "tenants" / tenant / "resize.json"


def resize_staged_paths(tenant):
    """Files a resize stages before its commit point. The helper reads
    runner-args.staged on the tenant's next cold start; the copies show the
    operator what the commit will publish."""
    home = ROOT / "tenants" / tenant
    copies = home / "resize-staged"
    return {"runner_args": home / "runner-args.staged", "provision": copies / "provision.json",
            "registry": copies / "registry.json", "helper": copies / "motor-vm-helper.json"}


def staged_activation(arguments, quota_grew):
    """runner-args.staged bytes in the helper's format; the helper pins them in
    crates/motor-vm-helper/tests/vectors/runner-args-staged.json. quota_grew
    makes the helper check host free space after the guest grows its disk."""
    return json.dumps({"runner_args": arguments, "quota_grew": quota_grew}, separators=(",", ":")) + "\n"


def write_resize_journal(journal):
    durable_write(resize_journal_path(journal["tenant"]), json.dumps(journal, indent=2) + "\n", 0o600)


def read_resize_journal(tenant):
    """The tenant's resize journal, refusing anything unreadable, malformed,
    of another schema version or naming paths a resize would not create."""
    path = resize_journal_path(tenant)
    try:
        if not stat.S_ISREG(path.lstat().st_mode):
            raise ValueError("not a regular file")
        journal = json.loads(path.read_text())
        if type(journal) is not dict or set(journal) != RESIZE_JOURNAL_KEYS:
            raise ValueError("missing or unknown keys")
        if type(journal["schema"]) is not int or journal["schema"] != RESIZE_SCHEMA:
            raise ValueError(f"unknown schema version {journal['schema']!r}")
        if journal["tenant"] != tenant:
            raise ValueError("it names another tenant")
        if journal["phase"] not in RESIZE_PHASES + (RESIZE_ABORTING,):
            raise ValueError(f"unknown phase {journal['phase']!r}")
        for name in ("provision_old", "provision_new", "registry_old", "registry_new", "helper_old", "helper_new",
                     "runner_args", "base", "set_aside"):
            if type(journal[name]) is not str:
                raise ValueError(f"{name} is not a string")
        for name in ("quota_grew", "base_existed"):
            if type(journal[name]) is not bool:
                raise ValueError(f"{name} is not a boolean")
        old, new = checked(journal["old"]), checked(journal["new"])
        if json.loads(journal["provision_old"]) != journal["old"] or json.loads(journal["provision_new"]) != journal["new"]:
            raise ValueError("recorded provisioning bytes differ from the recorded records")
        # Abort deletes the base and the staged files and renames the set-aside
        # snapshots, so each path must be exactly one this resize would create.
        home = ROOT / "tenants" / tenant
        if journal["base"] != str(base_path(new, ROOT / "tenant-images" / new["image_version"])):
            raise ValueError("the size base path is not the new size's base")
        set_aside = Path(journal["set_aside"])
        if set_aside.parent != home or not set_aside.name.startswith("suspend.resize-"):
            raise ValueError("the snapshot set-aside path is outside the tenant home")
        if journal["staged"] != {name: str(value) for name, value in resize_staged_paths(tenant).items()}:
            raise ValueError("unexpected staged file paths")
        if journal["runner_args"] != runner_args(new) or \
                journal["quota_grew"] != (new["disk_quota_bytes"] > old["disk_quota_bytes"]):
            raise ValueError("recorded activation does not match the new record")
    except (OSError, ValueError, TypeError, KeyError, AttributeError) as error:
        raise ValueError(f"malformed resize journal {path}: {error}; {JOURNAL_RECOVERY}") from error
    return journal


def vmm_processes(proc=Path("/proc")):
    """Pids of every live Firecracker VMM or jailer on this host."""
    found = []
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            comm = (entry / "comm").read_text().strip()
        except OSError:
            continue  # exited, or a kernel thread
        if comm in ("firecracker", "jailer"):
            found.append(int(entry.name))
    return sorted(found)


def require_services_stopped(check_service, action):
    for unit in ("motor-host.service", "motor-vm-helper.service"):
        state = check_service(unit)
        if state != "inactive":
            raise ValueError(f"{unit} must be stopped before {action} (ActiveState={state}). "
                             f"Stop both with: {STOP_COMMAND} "
                             "(the helper stops each running tenant VM cleanly before it exits)")


def require_quiescent(check_service, vmms):
    """Resize is a host-wide outage: both services stopped, no VMM, and no
    tenant with evidence that its VM runs or did not stop cleanly."""
    require_services_stopped(check_service, "a resize")
    live = vmms()
    if live:
        raise ValueError(f"a Firecracker VMM or jailer is still running (pid {live[0]}); "
                         "a resize needs every VM on the host stopped")
    for name, record in registered_tenants():
        home = ROOT / "tenants" / name
        if (home / RUNNING_MARKER).exists():
            raise ValueError(f"tenant {name} has {home / RUNNING_MARKER}: its VM runs or its last stop was not clean; "
                             "start and cleanly stop it before resizing")
        if (home / "unclean-stop.json").exists():
            raise ValueError(f"tenant {name} has {home / 'unclean-stop.json'}: its last stop was not clean; "
                             "start and cleanly stop it before resizing")
        for group in (HELPER_CGROUPS / f"tenant-{record.get('tap_index')}", HELPER_CGROUPS / name):
            if group.exists():
                raise ValueError(f"tenant {name} has a leftover cgroup {group}; "
                                 "start and stop the helper so it reconciles before resizing")
        jail = ROOT / "jail" / "tenants" / name
        if jail.exists() or jail.is_symlink():
            raise ValueError(f"tenant {name} has a leftover jail {jail}; "
                             "start and stop the helper so it reconciles before resizing")


def resize_target(old_config, arguments):
    """The provisioning record a resize with these arguments publishes."""
    if set(arguments) - set(RESIZE_FIELDS):
        raise ValueError(f"a resize changes only {', '.join(RESIZE_FIELDS)}")
    return {**old_config, **arguments}


def render_helper_config(text, old_cpuset, new_cpuset):
    """/etc/motor-vm-helper.json with its slot CPUs moved to the new cpuset,
    in the layout hosted-host/install.sh writes (two-space JSON and a newline)."""
    if old_cpuset == new_cpuset:
        return text
    try:
        config = json.loads(text)
    except ValueError as error:
        raise ValueError(f"{HELPER_CONFIG} is not JSON: {error}") from error
    slots = config.get("slots") if isinstance(config, dict) else None
    if not isinstance(slots, list) or not slots or \
            any(not isinstance(slot, dict) or slot.get("cpus") != old_cpuset for slot in slots):
        raise ValueError(f"{HELPER_CONFIG} slots do not all use the tenant's cpuset {old_cpuset}; "
                         "reinstall the helper config before resizing")
    for slot in slots:
        slot["cpus"] = new_cpuset
    return json.dumps(config, indent=2) + "\n"


def plan_resize(tenant, arguments):
    """Check every precondition and return the new journal; writes nothing."""
    capacity = read_capacity()
    for name, _record in registered_tenants():
        staged = ROOT / "tenants" / name / "runner-args.staged"
        if staged.exists() or staged.is_symlink():
            raise ValueError(f"tenant {name} has a pending resize activation ({staged}); "
                             "start it once so its next start delivers the activation, then stop it again")
    home = ROOT / "tenants" / tenant
    provision_path = PROVISION / f"{tenant}.json"
    registry_path = REGISTRY / f"{tenant}.json"
    provision_old = provision_path.read_text()
    old_config = json.loads(provision_old)
    old = checked(old_config)
    if old["tenant"] != tenant:
        raise ValueError(f"provisioning record tenant does not match {tenant}")
    registry_old = registry_path.read_text()
    record = json.loads(registry_old)
    if not isinstance(record, dict):
        raise ValueError("helper registry entry is not an object")
    for name, value in record.items():
        if name in old and value != old[name]:
            raise ValueError(f"provisioning record and helper registry differ at {name}")
    image = ROOT / "tenant-images" / old["image_version"]
    if not image_capability(json.loads((image / "manifest.json").read_text()), "resize-v1"):
        raise ValueError(f"image {old['image_version']} does not list the resize-v1 capability; switch-image to "
                         "an image whose guest grows its disk and applies runner arguments before resizing")

    new_config = resize_target(old_config, arguments)
    new = checked(new_config)
    if new == old:
        raise ValueError("the resize changes nothing")
    if new["disk_quota_bytes"] < old["disk_quota_bytes"]:
        raise ValueError(f"disk shrink from {old['disk_quota_bytes']} to {new['disk_quota_bytes']} bytes is refused; "
                         "a tenant disk only grows")
    if new["guest_ram_mib"] < old["guest_ram_mib"]:
        raise ValueError(f"RAM decrease from {old['guest_ram_mib']} to {new['guest_ram_mib']} MiB is refused; "
                         "guest RAM only grows")
    disk = home / "disk.img"
    length = disk.stat().st_size
    if length != old["disk_quota_bytes"]:
        raise ValueError(f"disk length {length} differs from the recorded quota {old['disk_quota_bytes']}; "
                         "inspect the disk before resizing")
    suspend = home / "suspend"
    if suspend.is_symlink() or not suspend.is_dir():
        raise ValueError(f"suspend path is not a directory: {suspend}")
    if new["vcpu_ceiling"] > 32 or (new["vcpu_ceiling"] != 1 and new["vcpu_ceiling"] % 2):
        raise ValueError(f"vcpu_ceiling {new['vcpu_ceiling']} must be 1 or an even number of at most 32")
    cpus = cpu_numbers(new["cpuset"])
    if cpus & cpu_numbers(WINDOWS_CPUS):
        raise ValueError(f"cpuset {new['cpuset']} overlaps the Windows VM's CPUs {WINDOWS_CPUS}")
    for name, other in registered_tenants():
        if name != tenant and cpus & cpu_numbers(other["cpuset"]):
            raise ValueError(f"cpuset {new['cpuset']} overlaps tenant {name}'s cpuset {other['cpuset']}")

    base = base_path(new, image)
    existed = base.exists() or base.is_symlink()
    if existed and not base_present(new, image):
        raise ValueError("incomplete size base; refusing to reuse it")
    replacement = dict(new, tenant=tenant)
    needed = pre_operation_free_bytes(new, capacity, existed, threshold=host_storage_threshold(capacity, replacement))
    free = shutil.disk_usage(ROOT).free
    if free < needed:
        message = f"resize needs {needed} free bytes on {ROOT}; {free} are available"
        if set(arguments) <= {"max_weight", "idle_suspend_s"}:
            message += ("; a resize that changes only max_weight or idle_suspend_s needs the same space, "
                        "because it rebuilds nothing but must leave room for motor-host to keep admitting")
        raise ValueError(message)
    memory_fit(capacity, replacement=replacement)
    verify_jobs_slice(capacity)
    try:
        helper_old = HELPER_CONFIG.read_text()
    except OSError as error:
        raise ValueError(f"cannot read the helper config {HELPER_CONFIG}: {error}") from error
    helper_new = render_helper_config(helper_old, old["cpuset"], new["cpuset"])

    registry_new = dict(record)
    registry_new.update({name: new_config[name] for name in arguments})
    return {
        "schema": RESIZE_SCHEMA, "tenant": tenant, "phase": RESIZE_PHASES[0],
        "old": old_config, "new": new_config,
        "provision_old": provision_old, "provision_new": json.dumps(new_config, indent=2) + "\n",
        "registry_old": registry_old, "registry_new": json.dumps(registry_new, indent=2) + "\n",
        "helper_old": helper_old, "helper_new": helper_new,
        "runner_args": runner_args(new), "quota_grew": new["disk_quota_bytes"] > old["disk_quota_bytes"],
        "base": str(base), "base_existed": existed,
        "set_aside": str(home / f"suspend.resize-{time.time_ns()}"),
        "staged": {name: str(path) for name, path in resize_staged_paths(tenant).items()},
    }


def publish(path, text):
    """Replace a published file durably, keeping its mode."""
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode):
        raise ValueError(f"refusing to replace a non-regular file: {path}")
    durable_write(path, text, stat.S_IMODE(info.st_mode))


def resize_forward(journal, make_base, hook, output):
    """Run every step after the journal's phase. Each step is idempotent, so a
    re-run after a crash anywhere in a step repeats it safely."""
    tenant = journal["tenant"]
    home = ROOT / "tenants" / tenant
    old, new = checked(journal["old"]), checked(journal["new"])
    image = ROOT / "tenant-images" / new["image_version"]
    disk = home / "disk.img"
    suspend = home / "suspend"
    set_aside = Path(journal["set_aside"])
    staged = {name: Path(path) for name, path in journal["staged"].items()}
    position = RESIZE_PHASES.index(journal["phase"])

    def advance(phase):
        nonlocal position
        journal["phase"] = phase
        write_resize_journal(journal)
        position = RESIZE_PHASES.index(phase)
        hook(f"phase:{phase}")

    def pending(phase):
        return position < RESIZE_PHASES.index(phase)

    # Before the commit point the disk is untouched; after it the extension may
    # or may not have happened. Any other length means something else wrote it.
    lengths = [old["disk_quota_bytes"]] if pending("committed") else [old["disk_quota_bytes"], new["disk_quota_bytes"]]
    length = disk.stat().st_size
    if length not in lengths:
        raise ValueError(f"disk length {length} is not the expected {' or '.join(map(str, lengths))} bytes; "
                         "the disk changed outside the resize, inspect it")
    activation = staged_activation(journal["runner_args"], journal["quota_grew"])

    if pending("base_built"):
        if not base_present(new, image):
            make_base(new, image, Path(journal["base"]))
        hook("step:base")
        advance("base_built")
    if pending("staged"):
        durable_write(staged["runner_args"], activation, 0o600)
        hook("step:stage:runner_args")
        staged["provision"].parent.mkdir(mode=0o700, exist_ok=True)
        sync_dir(home)
        for name, key in (("provision", "provision_new"), ("registry", "registry_new"), ("helper", "helper_new")):
            durable_write(staged[name], journal[key], 0o600)
        hook("step:stage:copies")
        advance("staged")
    if pending("set_aside"):
        # Suspend keys omit the size, so an old-size snapshot left in place
        # would be selectable with the new sizing.
        if not (set_aside.exists() or set_aside.is_symlink()):
            os.rename(suspend, set_aside)
            sync_dir(home)
        if not suspend.exists():
            suspend.mkdir(mode=0o700)
            sync_dir(home)
        hook("step:set_aside")
        advance("set_aside")
    if pending("committed"):
        advance("committed")

    # After the commit point a re-run republishes all three files from the
    # journal, since a crash may fall between any two of them.
    for path, key, name in ((PROVISION / f"{tenant}.json", "provision_new", "provision"),
                            (REGISTRY / f"{tenant}.json", "registry_new", "registry"),
                            (HELPER_CONFIG, "helper_new", "helper")):
        publish(path, journal[key])
        hook(f"step:publish:{name}")
    if pending("published"):
        advance("published")
    if disk.stat().st_size != new["disk_quota_bytes"]:
        # Sparse: no blocks are allocated and nothing is mounted. The guest
        # grows its filesystem into the new length on its next cold start.
        os.truncate(disk, new["disk_quota_bytes"])
    with disk.open("rb") as file:
        os.fsync(file.fileno())
    hook("step:extend")
    if pending("extended"):
        advance("extended")
    current = staged["runner_args"]
    info = current.lstat() if current.exists() or current.is_symlink() else None
    if info is None or not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600 \
            or current.read_text() != activation:
        durable_write(current, activation, 0o600)
    hook("step:args")
    for name in ("provision", "registry", "helper"):
        staged[name].unlink(missing_ok=True)
    if staged["provision"].parent.exists():
        staged["provision"].parent.rmdir()
    sync_dir(home)
    resize_journal_path(tenant).unlink()
    sync_dir(home)
    output(f"{tenant}: resized; the old suspend snapshots are kept in {set_aside}")
    output(f"start the services: the tenant's next start cold-boots and applies {current}")


def resize(tenant, arguments, make_base=None, check_service=None, vmms=None, hook=None, output=print):
    """Resize a stopped tenant, or resume its journaled resize with the same arguments.

    hook(point) runs after every step and every journal phase write; tests use
    it to stop the resize at each point as a crash would.
    """
    tenant = component(tenant)
    make_base = build_base if make_base is None else make_base
    check_service = systemd_active_state if check_service is None else check_service
    vmms = vmm_processes if vmms is None else vmms
    hook = (lambda _point: None) if hook is None else hook
    with (ROOT / "provision.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        require_quiescent(check_service, vmms)
        own = resize_journal_path(tenant)
        for path in sorted((ROOT / "tenants").glob("*/resize.json")):
            if path != own:
                raise ValueError(f"another tenant's resize journal exists: {path}; resume or abort that resize first")
        if own.exists() or own.is_symlink():
            journal = read_resize_journal(tenant)
            if journal["phase"] == RESIZE_ABORTING:
                raise ValueError(f"the resize of {tenant} is being aborted; finish it with: resize --abort {tenant}")
            if resize_target(journal["old"], arguments) != journal["new"]:
                raise ValueError(f"these arguments differ from the pending resize in {own}; re-run it with the "
                                 "same arguments, or abort it if it is not committed")
            output(f"{tenant}: resuming the resize from phase {journal['phase']}")
        else:
            journal = plan_resize(tenant, arguments)
            write_resize_journal(journal)
            hook(f"phase:{journal['phase']}")
        resize_forward(journal, make_base, hook, output)


def base_referenced(base):
    """Whether any registered tenant boots from this size base."""
    return any(base_path(record, ROOT / "tenant-images" / record["image_version"]) == base
               for _name, record in registered_tenants())


def resize_abort(tenant, check_service=None, hook=None, output=print):
    """Undo a resize that has not reached its commit point.

    Each compensation is idempotent, so a crash during abort is finished by
    running abort again. The jobs-slice drop-in is left in place.
    """
    tenant = component(tenant)
    check_service = systemd_active_state if check_service is None else check_service
    hook = (lambda _point: None) if hook is None else hook
    with (ROOT / "provision.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        require_services_stopped(check_service, "aborting a resize")
        path = resize_journal_path(tenant)
        if not (path.exists() or path.is_symlink()):
            raise ValueError(f"no resize journal for tenant {tenant}: {path}")
        journal = read_resize_journal(tenant)
        if journal["phase"] != RESIZE_ABORTING and RESIZE_PHASES.index(journal["phase"]) >= RESIZE_COMMITTED:
            raise ValueError(f"the resize of {tenant} is committed and only rolls forward; "
                             "re-run it with the same arguments to finish it")
        if journal["phase"] != RESIZE_ABORTING:
            journal["phase"] = RESIZE_ABORTING
            write_resize_journal(journal)
            hook(f"phase:{RESIZE_ABORTING}")
        home = ROOT / "tenants" / tenant
        suspend = home / "suspend"
        set_aside = Path(journal["set_aside"])
        if set_aside.exists() or set_aside.is_symlink():
            if suspend.exists() or suspend.is_symlink():
                # Only the empty directory the set-aside step created; a
                # non-empty one fails here and keeps the journal for inspection.
                suspend.rmdir()
            os.rename(set_aside, suspend)
            sync_dir(home)
        hook("abort:snapshots")
        staged = {name: Path(value) for name, value in journal["staged"].items()}
        for value in staged.values():
            value.unlink(missing_ok=True)
        if staged["provision"].parent.exists():
            staged["provision"].parent.rmdir()
        sync_dir(home)
        hook("abort:staged")
        base = Path(journal["base"])
        if not journal["base_existed"] and base.exists() and not base_referenced(base):
            shutil.rmtree(base)
            sync_dir(base.parent)
        hook("abort:base")
        for published, key in ((PROVISION / f"{tenant}.json", "provision_old"),
                               (REGISTRY / f"{tenant}.json", "registry_old"), (HELPER_CONFIG, "helper_old")):
            if published.read_text() != journal[key]:
                raise ValueError(f"{published} differs from its state before the resize; inspect it before "
                                 f"removing {path}")
        quota = checked(journal["old"])["disk_quota_bytes"]
        length = (home / "disk.img").stat().st_size
        if length != quota:
            raise ValueError(f"disk length {length} differs from the old quota {quota}; inspect it before removing {path}")
        path.unlink()
        sync_dir(home)
        output(f"{tenant}: resize aborted; the old records, helper config, disk and snapshots are in place")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path)
    parser.add_argument("--remote-cache-dir", type=Path,
                        help="when provisioning a new tenant whose record sets remote_cache: a root-only directory "
                        "holding the runner's remote compile cache config.json and read-key, and optionally the "
                        "tenant's write-key; they are installed on the new tenant disk only")
    commands = parser.add_subparsers(dest="command")
    commands.add_parser("apply-jobs-slice", help="validate capacity and apply the direct jobs slice limits")
    switch = commands.add_parser("switch-image", help="change an existing tenant's immutable image; "
                                 f"stop both services first: {STOP_COMMAND}")
    switch.add_argument("tenant")
    switch.add_argument("image_version")
    repair = commands.add_parser(
        "repair-from-registry",
        help="set the provisioning record's image_version to the helper registry's, when nothing else differs")
    repair.add_argument("tenant")
    remove = commands.add_parser(
        "remove-tenant",
        help="remove a departed customer's tenant: quarantine its disk, snapshots and records for "
        f"{QUARANTINE_S // 86400} days, drop its network setup; stop both services first: {STOP_COMMAND}")
    remove.add_argument("tenant")
    remove.add_argument("--dry-run", action="store_true", help="list everything that would be removed; change nothing")
    remove.add_argument("--purge-now", action="store_true", help="delete at once instead of quarantining")
    resize_command = commands.add_parser(
        "resize", help="offline resize of a tenant: RAM and disk only grow; every tenant must be stopped "
        f"with: {STOP_COMMAND}. Re-run with the same options to resume after a failure")
    resize_command.add_argument("tenant")
    resize_command.add_argument("--abort", action="store_true",
                                help="undo a resize that has not reached its commit point")
    for field, kind in (("vcpu_ceiling", int), ("guest_ram_mib", int), ("cpuset", str), ("max_weight", int),
                        ("idle_suspend_s", int), ("disk_quota_bytes", int)):
        resize_command.add_argument("--" + field.replace("_", "-"), dest=field, type=kind)
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("provisioning requires root on the dedicated VM host")
    if args.command is not None and args.remote_cache_dir is not None:
        parser.error(f"{args.command} does not take --remote-cache-dir; cache files are installed only with a new tenant")
    if args.command == "apply-jobs-slice":
        if args.config is not None:
            parser.error("apply-jobs-slice does not take --config")
        trusted(ROOT)
        apply_jobs_slice()
        return
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
    if args.command == "remove-tenant":
        if args.config is not None:
            parser.error("remove-tenant takes a tenant, not --config")
        if args.dry_run and args.purge_now:
            parser.error("--dry-run and --purge-now are exclusive")
        tenant = component(args.tenant)
        for directory in (ROOT, REGISTRY, ROOT / "tenants"):
            trusted(directory)
        trusted(REGISTRY / f"{tenant}.json")
        for path in (PROVISION / f"{tenant}.json", ROOT / "tenants" / tenant, ROOT / "removed", BINDINGS):
            if path.exists() or path.is_symlink():
                trusted(path)
        remove_tenant(tenant, dry_run=args.dry_run, purge_now=args.purge_now)
        return
    if args.command == "resize":
        if args.config is not None:
            parser.error("resize takes a tenant and sizing options, not --config")
        tenant = component(args.tenant)
        arguments = {name: getattr(args, name) for name in RESIZE_FIELDS if getattr(args, name) is not None}
        if args.abort and arguments:
            parser.error("--abort takes no sizing options")
        if not args.abort and not arguments:
            parser.error("name at least one size to change")
        for directory in (ROOT, REGISTRY, PROVISION, ROOT / "tenants", ROOT / "tenants" / tenant):
            trusted(directory)
        for path in (PROVISION / f"{tenant}.json", REGISTRY / f"{tenant}.json", HELPER_CONFIG):
            trusted(path)
        if args.abort:
            resize_abort(tenant)
            return
        image = ROOT / "tenant-images" / checked(json.loads((PROVISION / f"{tenant}.json").read_text()))["image_version"]
        for name in ("manifest.json", "vmlinux", "rootfs"):
            trusted(image / name)
        for binary in (BIN / "firecracker", BIN / "jailer"):
            trusted(binary)
            if "1.17.0" not in run(binary, "--version").stdout:
                raise ValueError("Firecracker and jailer must both be 1.17.0")
        resize(tenant, arguments)
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
    remote_cache = None
    if args.remote_cache_dir is not None:
        directory = args.remote_cache_dir.resolve()
        trusted(directory)
        for entry in directory.iterdir():
            trusted(entry)
        remote_cache = read_remote_cache(directory, c["sub"])
    capacity = read_capacity()
    memory_fit(capacity, replacement=c)
    verify_jobs_slice(capacity)
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
        provision(c, remote_cache=remote_cache)


if __name__ == "__main__":
    main()
