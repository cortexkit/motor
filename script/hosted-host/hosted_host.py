#!/usr/bin/env python3
"""Render the configuration files of a hosted-runners host.

install.sh calls this to produce, on stdout, the exact bytes it installs:

  host --host-id ID [--port N]      /etc/motor-host/host.json
  helper --uid UID --gid GID        /etc/motor-vm-helper.json
  tenant --sub S [--disk-quota-gib N]
                                    the provision.py operator JSON for tenant 1
  public-ip                         the one global IPv4 address of this machine

The machine-specific facts (CPU layout, account ranges, install paths) are
constants here, beside their reasons, so test_hosted_host.py can check them
against each other, against install.sh and against the Rust config structs
that read the rendered files.
"""
from __future__ import annotations

import argparse
import importlib.util
import ipaddress
import json
from pathlib import Path
import re
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


def helper_config(uid: int, gid: int) -> dict:
    """/etc/motor-vm-helper.json admitting only the motor-host account."""
    tenant = json.loads(TENANT_FILE.read_text())
    if uid <= 0 or gid <= 0:
        raise ValueError("the host agent account must not be root")
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


def tenant_config(sub: str, disk_quota_gib: int | None = None, image_version: str | None = None) -> dict:
    """The provision.py operator JSON for tenant 1, validated by provision.py."""
    config = json.loads(TENANT_FILE.read_text())
    config["sub"] = sub
    if image_version is not None:
        config["image_version"] = image_version
    if disk_quota_gib is not None:
        config["disk_quota_bytes"] = disk_quota_gib * 1024**3
    provision = load_provision()
    provision.checked(config)
    if cpus(config["cpuset"]) & cpus(WINDOWS_CPUS) or max(cpus(config["cpuset"])) >= THREADS:
        raise ValueError("tenant cpuset must avoid the Windows VM's CPUs")
    if config["resolvers"] != RESOLVERS:
        raise ValueError("tenant resolvers must match the host agent's")
    return config


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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    host = commands.add_parser("host")
    host.add_argument("--host-id", required=True)
    host.add_argument("--port", type=int, default=PORT)
    helper = commands.add_parser("helper")
    helper.add_argument("--uid", type=int, required=True)
    helper.add_argument("--gid", type=int, required=True)
    tenant = commands.add_parser("tenant")
    tenant.add_argument("--sub", required=True)
    tenant.add_argument("--disk-quota-gib", type=int)
    tenant.add_argument("--image-version")
    commands.add_parser("public-ip")
    args = parser.parse_args()
    try:
        if args.command == "public-ip":
            output = subprocess.run(["ip", "-4", "-o", "addr", "show", "scope", "global"],
                                    check=True, capture_output=True, text=True).stdout
            print(public_ipv4(output))
            return 0
        if args.command == "host":
            value = host_config(args.host_id, args.port)
        elif args.command == "helper":
            value = helper_config(args.uid, args.gid)
        else:
            value = tenant_config(args.sub, args.disk_quota_gib, args.image_version)
    except ValueError as error:
        print(f"hosted-host: {error}", file=sys.stderr)
        return 2
    print(json.dumps(value, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
