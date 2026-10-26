#!/usr/bin/env python3
"""Render the configuration files of a hosted-runners host.

install.sh calls this to produce, on stdout, the exact bytes it installs:

  host --host-id ID [--port N]      /etc/motor-host/host.json
  helper --uid UID --gid GID [--installed RECORD]
                                    /etc/motor-vm-helper.json, its slot CPUs
                                    taken from the installed tenant record
  tenant --sub S [--disk-quota-gib N] [--installed RECORD]
                                    the provision.py operator JSON for tenant 1:
                                    the installed record when there is one,
                                    the tenant-1.json template on a first install
  public-ip                         the one global IPv4 address of this machine

one check of the installed state, which install.sh --check and --start run:

  check-installed --uid UID --gid GID
                                    the installed tenant record, its helper
                                    registry entry, the helper config, the host
                                    capacity inputs and the jobs slice agree,
                                    and no resize is pending; it also names
                                    the remote cache files install put on the
                                    tenant disk (names and modes only)

and one operator query, which reads motor-host's admin socket as root:

  runner-log --tenant T --since TIME [--until TIME] [--job UUID]
                                    the tenant runner's journal for a window,
                                    redacted in the guest to the runner's own
                                    labels, counters and job ids

The machine-specific facts (CPU layout, account ranges, install paths) are
constants here, beside their reasons, so test_hosted_host.py can check them
against each other, against install.sh and against the Rust config structs
that read the rendered files.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import importlib.util
import ipaddress
import json
from pathlib import Path
import re
import socket
import struct
import subprocess
import sys

HERE = Path(__file__).resolve().parent
TENANT_FILE = HERE / "tenant-1.json"

# The host id and the tenant's broker subject are not here: this repository is
# public, so they are install.sh arguments and live only in the server's configs.
PORT = 8443
RESOLVERS = ["1.1.1.1", "9.9.9.9"]
FIRECRACKER = Path("/usr/local/lib/firecracker/1.17.0")
# docs/linux-microvm.md installs the helper under /usr/local/libexec, and the
# helper's fixture config names the static loader there too.
HELPER_BINARY = Path("/usr/local/libexec/motor-vm-helper")
LOADER = Path("/usr/local/libexec/motor-vm-loader")
HOST_BINARY = Path("/usr/local/bin/motor-host")
KEY_DIR = Path("/etc/motor-host/keys")
SIGNING_KEY = KEY_DIR / "host-signing.key"
TLS_CERT = KEY_DIR / "tls.crt"
TLS_KEY = KEY_DIR / "tls.key"
JOURNAL_DIR = Path("/var/lib/motor-host")
ADMIN_SOCKET = Path("/run/motor-host/admin.sock")
BINDINGS = Path("/etc/motor-host/tenants.json")
REGISTRY = Path("/etc/motor-vm-helper/tenants")
STORAGE = Path("/var/lib/motor-vm")
# The installed provisioning record of tenant 1 (provision.py's PROVISION
# directory) and the helper config install.sh writes.
TENANT_RECORD = Path("/etc/motor-host/provision/tenant-1.json")
HELPER_CONFIG = Path("/etc/motor-vm-helper.json")

# The build server has 16 cores / 32 threads; thread n and n+16 share a core.
# The Windows build VM owns cores 12-15 (threads 12-15 and 28-31). Nothing of
# the hosted host may run there.
THREADS = 32
WINDOWS_CPUS = "12-15,28-31"

# The helper's job slots are unused on a hosted host (motor-host only sends
# tenant operations), but its config requires at least one. These are the
# values qualification ran the helper with on this machine; the slot account
# uid 290000 and the one-uid job range keep clear of tenant uids 400000+.
SLOT_UID_BASE = 290000
JOB_ACCOUNT_RANGE = {"base": 300000, "count": 1}


def cpus(text: str) -> set[int]:
    result = set()
    for part in text.split(","):
        bounds = [int(n) for n in part.split("-")]
        result.update(range(bounds[0], bounds[-1] + 1))
    return result


def host_config(host_id: str, port: int = PORT) -> dict:
    """/etc/motor-host/host.json. motor-host refuses unknown keys, so this is every
    key it reads, with none of the qualification-only fake_broker_url."""
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,64}", host_id):
        raise ValueError("host_id must match [A-Za-z0-9._-]{1,64}")
    if not 1024 < port <= 65535:
        raise ValueError("host agent port must be 1025-65535: the agent has no bind capability")
    return {
        "host_id": host_id,
        # IPv4 only: the broker endpoint and the firewall exception are IPv4.
        "listen": f"0.0.0.0:{port}",
        "certificate": str(TLS_CERT),
        "private_key": str(TLS_KEY),
        "host_signing_key": str(SIGNING_KEY),
        "journal_dir": str(JOURNAL_DIR),
        "helper_socket": "/run/motor-vm-helper.sock",
        "registry_dir": str(REGISTRY),
        "bindings": str(BINDINGS),
        "resolvers": RESOLVERS,
        "admin_socket": str(ADMIN_SOCKET),
        # The bind mount whose every ancestor is root-owned, which the helper also
        # uses; /srv/build/vm, its source, sits under an unprivileged-owned parent.
        "storage": str(STORAGE),
    }


def render_json(value) -> str:
    """The bytes install.sh installs for a rendered config: two-space JSON and a
    newline, the layout provision.py also uses when it rewrites these files."""
    return json.dumps(value, indent=2) + "\n"


def read_record(path: Path) -> dict:
    """An installed tenant provisioning record."""
    try:
        record = json.loads(Path(path).read_text())
    except (OSError, ValueError) as error:
        raise ValueError(f"cannot read the installed tenant record {path}: {error}") from error
    if not isinstance(record, dict):
        raise ValueError(f"the installed tenant record {path} is not a JSON object")
    return record


def helper_config(uid: int, gid: int, tenant: dict | None = None) -> dict:
    """/etc/motor-vm-helper.json admitting only the motor-host account.

    Its slot CPUs are the tenant's cpuset: the installed record's when one is
    given, so a resize that moved the tenant (and rewrote this file) is kept,
    and the template's on a first install."""
    if tenant is None:
        tenant = json.loads(TENANT_FILE.read_text())
    if uid <= 0 or gid <= 0:
        raise ValueError("the host agent account must not be root")
    if not isinstance(tenant.get("cpuset"), str):
        raise ValueError("the tenant record has no cpuset")
    if cpus(tenant["cpuset"]) & cpus(WINDOWS_CPUS):
        raise ValueError(f"tenant cpuset {tenant['cpuset']} overlaps the Windows VM's CPUs {WINDOWS_CPUS}")
    images = STORAGE / "images"
    # Absent files the helper never opens: they are read only when a job slot
    # starts, and no client of this helper asks for one.
    unused = {"vmstate": str(images / "job.vmstate"), "rootfs": str(images / "rootfs"),
              "memory": str(images / "job.memory"), "record": str(images / "job.record.json")}
    return {
        "allowed_uids": [uid],
        "socket_gid": gid,
        "uid_base": SLOT_UID_BASE,
        "job_account_range": JOB_ACCOUNT_RANGE,
        "slots": [{"cpus": tenant["cpuset"], "mems": "0", "cpu_max": "100000 100000",
                   "memory_max": 1024**3, "io_weight": 10, "io_max": None}],
        "jailer": str(FIRECRACKER / "jailer"),
        "firecracker": str(FIRECRACKER / "firecracker"),
        "loader": str(LOADER),
        "job_snapshot": unused,
        "scan_snapshot": unused,
        "hugepages_per_slot": 1,
        "agent_port": 52,
        "proxy_port": 53,
    }


def validate_tenant(config: dict, provision=None) -> None:
    """provision.py's own checks, plus this machine's: the cpuset avoids the
    Windows VM's CPUs and exists on this host, and the resolvers are the host
    agent's."""
    provision = load_provision() if provision is None else provision
    try:
        provision.checked(config)
    except (KeyError, TypeError) as error:
        raise ValueError(f"invalid tenant record: {error!r}") from error
    cpuset = cpus(config["cpuset"])
    if cpuset & cpus(WINDOWS_CPUS):
        raise ValueError(f"tenant cpuset {config['cpuset']} overlaps the Windows VM's CPUs {WINDOWS_CPUS}")
    if max(cpuset) >= THREADS:
        raise ValueError(f"tenant cpuset {config['cpuset']} names a CPU beyond this host's {THREADS} threads")
    if config["resolvers"] != RESOLVERS:
        raise ValueError("tenant resolvers must match the host agent's")


def tenant_config(sub: str, disk_quota_gib: int | None = None, image_version: str | None = None,
                  installed: Path | None = None) -> dict:
    """The provision.py operator JSON for tenant 1, validated by provision.py.

    With an installed record, that record is the result: its sizing may have
    been changed by provision.py resize and its image by switch-image, and
    reinstalling must not put the template's values back. The template
    applies only to a first install."""
    if installed is not None:
        if disk_quota_gib is not None or image_version is not None:
            raise ValueError("the installed tenant record keeps its sizing and image; change them with "
                             "provision.py resize or switch-image, not the installer")
        config = read_record(installed)
        if config.get("sub") != sub:
            raise ValueError(f"--tenant-sub differs from the subject in the installed record {installed}")
    else:
        config = json.loads(TENANT_FILE.read_text())
        config["sub"] = sub
        if image_version is not None:
            config["image_version"] = image_version
        if disk_quota_gib is not None:
            config["disk_quota_bytes"] = disk_quota_gib * 1024**3
    validate_tenant(config)
    return config


def registry_disagreements(record: dict, registry: dict, provision) -> list[str]:
    """Fields where the helper registry entry is not what provision.py writes
    for this record: every field the two share, the registry's disk paths, and
    a field (such as max_weight) present in only one of them."""
    current = provision.checked(record)
    expected = {name: value for name, value in current.items() if name not in provision.PROVISION_ONLY}
    home = provision.ROOT / "tenants" / current["tenant"]
    expected.update({name: str(home / leaf) for name, leaf in provision.REGISTRY_PATHS.items()})
    missing = object()
    return sorted(name for name in set(registry) | set(expected)
                  if registry.get(name, missing) != expected.get(name, missing))


def remote_cache_report(record: dict, provision) -> str:
    """Which of the tenant runner's remote compile cache files provision.py
    installed, with their modes; never their contents, which hold the keys.

    The files live on the tenant disk, which the host never mounts after
    install, so this reads the manifest install wrote beside the disk. Image
    switches and resizes keep the disk, and so the files. Raises ValueError
    when the record turns the cache on but the manifest is missing or names
    other files or modes than the runner requires."""
    if not record.get("remote_cache"):
        return "remote cache: off"
    path = provision.ROOT / "tenants" / provision.component(record["tenant"]) / provision.REMOTE_CACHE_MANIFEST
    try:
        manifest = json.loads(path.read_text())
    except (OSError, ValueError) as error:
        raise ValueError(f"the record turns the remote cache on, but {path} cannot be read: {error}") from error
    expected = {name: f"{mode:04o}" for name, mode in provision.REMOTE_CACHE_FILES.items()}
    if (not isinstance(manifest, dict) or set(manifest) - set(expected)
            or not provision.REMOTE_CACHE_REQUIRED <= set(manifest)
            or any(manifest[name] != expected[name] for name in manifest)):
        raise ValueError(f"{path} does not list config.json and read-key (and at most write-key) "
                         "with the runner's modes")
    write = (f"write-key {manifest['write-key']}, so the tenant fills its own cache" if "write-key" in manifest
             else "no write-key, so the tenant reads the cache and never fills it")
    return (f"remote cache: config.json {manifest['config.json']}, read-key {manifest['read-key']}, {write}; "
            "installed on the tenant disk at provisioning and kept by switch-image and resize")


def check_installed(uid: int, gid: int, record_path: Path = TENANT_RECORD, helper_path: Path = HELPER_CONFIG,
                    provision=None) -> list[str]:
    """Every problem with the installed tenant state; empty when it is sound.

    The installed record, not the template, is the reference: a tenant
    resized by provision.py passes as long as its record, its helper registry
    entry and the helper config agree with each other and with this host.
    """
    provision = load_provision() if provision is None else provision
    problems = []

    def check(label, action):
        try:
            action()
        except (OSError, ValueError, KeyError, TypeError) as error:
            problems.append(f"{label}: {error}")
            return False
        return True

    record = None

    def load():
        nonlocal record
        record = read_record(record_path)

    if check(str(record_path), load):
        check(str(record_path), lambda: validate_tenant(record, provision))

        def registry():
            path = provision.REGISTRY / f"{provision.component(record['tenant'])}.json"
            try:
                entry = json.loads(path.read_text())
            except (OSError, ValueError) as error:
                raise ValueError(f"cannot read the helper registry entry {path}: {error}") from error
            if not isinstance(entry, dict):
                raise ValueError(f"the helper registry entry {path} is not a JSON object")
            differ = registry_disagreements(record, entry, provision)
            if differ:
                raise ValueError(f"the installed record and {path} differ at {', '.join(differ)}")

        check("helper registry", registry)
        check("remote cache", lambda: remote_cache_report(record, provision))

        def helper():
            expected = render_json(helper_config(uid, gid, record))
            if helper_path.read_text() != expected:
                raise ValueError("differs from the render of the installed tenant record")

        check(str(helper_path), helper)

    capacity = None

    def read_capacity():
        nonlocal capacity
        capacity = provision.read_capacity()

    if check("capacity.json", read_capacity):
        check("memory fit", lambda: provision.memory_fit(capacity))
        check("jobs slice", lambda: provision.verify_jobs_slice(capacity))
    check("resize", provision.refuse_resize_journal)
    return problems


def load_provision():
    spec = importlib.util.spec_from_file_location("hosted_host_provision", HERE.parent / "tenant-provision" / "provision.py")
    if spec is None or spec.loader is None:
        raise ImportError("cannot load provision.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def public_ipv4(ip_output: str) -> str:
    """The single global IPv4 address in `ip -4 -o addr show scope global`."""
    found = []
    for line in ip_output.splitlines():
        match = re.search(r"\binet (\d+\.\d+\.\d+\.\d+)/\d+", line)
        if match and ipaddress.IPv4Address(match[1]).is_global:
            found.append(match[1])
    if len(set(found)) != 1:
        raise ValueError(f"expected exactly one public IPv4 address, found {sorted(set(found))}; pass --public-ip")
    return found[0]


def unix_seconds(text: str) -> int:
    """A Unix time in seconds, or an ISO 8601 time; one without a zone is UTC."""
    if re.fullmatch(r"\d+", text):
        return int(text)
    moment = datetime.fromisoformat(text.replace("Z", "+00:00"))
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=timezone.utc)
    return int(moment.timestamp())


def runner_log_request(tenant: str, since: int, until: int | None, job: str | None) -> dict:
    """The admin-socket request for the tenant runner's journal in a window.
    motor-host forwards it to the guest as the read-only diagnostics op."""
    if until is not None and until < since:
        raise ValueError("--until is before --since")
    request = {"tenant": tenant, "op": "diagnostics", "since_unix_s": since}
    if until is not None:
        request["until_unix_s"] = until
    if job is not None:
        request["jti"] = job
    return request


def admin_exchange(request: dict, path: Path = ADMIN_SOCKET) -> dict:
    """One length-prefixed JSON request and reply on motor-host's admin socket."""
    body = json.dumps(request).encode()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(60)
        connection.connect(str(path))
        connection.sendall(struct.pack(">I", len(body)) + body)
        header = connection.recv(4, socket.MSG_WAITALL)
        if len(header) != 4:
            raise ValueError("motor-host closed the admin socket without a reply")
        (length,) = struct.unpack(">I", header)
        reply = b""
        while len(reply) < length:
            chunk = connection.recv(length - len(reply))
            if not chunk:
                raise ValueError("motor-host closed the admin socket mid-reply")
            reply += chunk
    return json.loads(reply)


def runner_log_lines(reply: dict) -> list[str]:
    """One `<UTC time> <message>` line per journal entry, then a note when the
    guest cut the window short."""
    if reply.get("ok") is not True:
        raise ValueError(f"diagnostics refused: {reply.get('error', 'no reason given')}")
    journal = reply.get("journal", {})
    if journal.get("unavailable"):
        raise ValueError("the guest could not read the tenant runner's journal")
    lines = []
    for entry in journal.get("entries", []):
        moment = datetime.fromtimestamp(entry["unix_us"] / 1_000_000, tz=timezone.utc)
        lines.append(f"{moment.isoformat(timespec='microseconds')} {entry['message']}")
    if journal.get("truncated"):
        lines.append("# truncated: narrow --since/--until or add --job")
    return lines


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    host = commands.add_parser("host")
    host.add_argument("--host-id", required=True)
    host.add_argument("--port", type=int, default=PORT)
    helper = commands.add_parser("helper")
    helper.add_argument("--uid", type=int, required=True)
    helper.add_argument("--gid", type=int, required=True)
    helper.add_argument("--installed", type=Path, help="the installed tenant record; absent on a first install")
    tenant = commands.add_parser("tenant")
    tenant.add_argument("--sub", required=True)
    tenant.add_argument("--disk-quota-gib", type=int)
    tenant.add_argument("--image-version")
    tenant.add_argument("--installed", type=Path, help="the installed tenant record; absent on a first install")
    check = commands.add_parser("check-installed")
    check.add_argument("--uid", type=int, required=True)
    check.add_argument("--gid", type=int, required=True)
    commands.add_parser("public-ip")
    runner_log = commands.add_parser("runner-log")
    runner_log.add_argument("--tenant", required=True)
    runner_log.add_argument("--since", required=True, help="Unix seconds or ISO 8601 (UTC if no zone)")
    runner_log.add_argument("--until", help="Unix seconds or ISO 8601 (UTC if no zone)")
    runner_log.add_argument("--job", help="keep only entries naming this job id (jti)")
    args = parser.parse_args()
    try:
        if args.command == "runner-log":
            request = runner_log_request(args.tenant, unix_seconds(args.since),
                                         None if args.until is None else unix_seconds(args.until), args.job)
            for line in runner_log_lines(admin_exchange(request)):
                print(line)
            return 0
        if args.command == "public-ip":
            output = subprocess.run(["ip", "-4", "-o", "addr", "show", "scope", "global"],
                                    check=True, capture_output=True, text=True).stdout
            print(public_ipv4(output))
            return 0
        if args.command == "check-installed":
            problems = check_installed(args.uid, args.gid)
            for problem in problems:
                print(f"hosted-host: {problem}", file=sys.stderr)
            if problems:
                return 1
            print(f"installed tenant state agrees: {TENANT_RECORD}, its helper registry entry, {HELPER_CONFIG}, "
                  "capacity.json, the memory fit and the jobs slice; no resize is pending")
            print(remote_cache_report(read_record(TENANT_RECORD), load_provision()))
            return 0
        if args.command == "host":
            value = host_config(args.host_id, args.port)
        elif args.command == "helper":
            value = helper_config(args.uid, args.gid, None if args.installed is None else read_record(args.installed))
        else:
            value = tenant_config(args.sub, args.disk_quota_gib, args.image_version, args.installed)
    except ValueError as error:
        print(f"hosted-host: {error}", file=sys.stderr)
        return 2
    sys.stdout.write(render_json(value))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
