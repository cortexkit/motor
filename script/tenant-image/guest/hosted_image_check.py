#!/usr/bin/python3
"""Operator-only checks inside a booted guest; --tenant also probes the attached XFS disk."""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pwd
import subprocess
import tempfile


def mounts(text):
    result = {}
    for line in text.splitlines():
        before, after = line.split(" - ", 1)
        fields, filesystem = before.split(), after.split()
        result[fields[4]] = (filesystem[0], set(fields[5].split(",")))
    return result


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def check():
    table = mounts(Path("/proc/self/mountinfo").read_text())
    if table["/"][0] != "overlay" or "rw" not in table["/"][1]:
        raise ValueError("guest root must be a writable overlay")
    if table["/.lower"][0] != "ext4" or "ro" not in table["/.lower"][1]:
        raise ValueError("image lower must be read-only ext4")
    if table["/overlay"][0] != "tmpfs":
        raise ValueError("overlay upper must reside in tmpfs")
    size = os.statvfs("/overlay")
    if size.f_blocks * size.f_frsize != 1024**3:
        raise ValueError("overlay upper must be limited to 1 GiB")
    if Path("/proc/1/comm").read_text().strip() != "systemd":
        raise ValueError("systemd must be PID 1")
    for name in ("motor-job", "motor-cache"):
        user = pwd.getpwnam(name)
        if user.pw_uid == 0 or user.pw_shell != "/usr/sbin/nologin":
            raise ValueError(f"unsafe job/cache account: {name}")
    for service in ("motor-net-rules", "motor-tenant-agent"):
        if run("systemctl", "is-active", service) != "active":
            raise ValueError(f"inactive service: {service}")
    rules = json.loads(run("nft", "-j", "list", "table", "inet", "motor_net"))
    if not any("rule" in item for item in rules["nftables"]):
        raise ValueError("motor_net has no rules")
    with tempfile.NamedTemporaryFile(dir="/etc", prefix=".motor-image-probe-") as probe:
        probe.write(b"private RAM overlay\n")
        probe.flush()
        lower = Path("/.lower", probe.name.lstrip("/"))
        if lower.exists():
            raise ValueError("overlay write leaked into the image lower")
    try:
        with tempfile.NamedTemporaryFile(dir="/.lower", prefix=".motor-image-probe-"):
            pass
    except OSError as error:
        if error.errno != errno.EROFS:
            raise
    else:
        raise ValueError("image lower accepted a write")
    print("hosted image live inventory: 10 checks passed (overlay, read-only lower, tmpfs, size, "
          "PID 1, accounts, services, network, private writes, denied lower writes)")


def check_tenant():
    table = mounts(Path("/proc/self/mountinfo").read_text())
    if table["/tenant"][0] != "xfs":
        raise ValueError("tenant disk must be XFS")
    for root in ("/Users", "/home"):
        with tempfile.NamedTemporaryFile(dir="/tenant/state") as source:
            source.write(b"nonempty reflink probe\n")
            source.flush()
            with tempfile.NamedTemporaryFile(dir=root) as target:
                fcntl.ioctl(target.fileno(), 0x40049409, source.fileno())
                target.seek(0)
                if target.read() != b"nonempty reflink probe\n":
                    raise ValueError(f"FICLONE content differs: {root}")
    print("hosted image tenant inventory: 3 checks passed (XFS, FICLONE to /Users and /home)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tenant", action="store_true")
    args = parser.parse_args()
    check()
    if args.tenant:
        check_tenant()
