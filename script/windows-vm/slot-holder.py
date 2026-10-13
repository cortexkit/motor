#!/usr/bin/env python3
"""Identify the shared flock holder without requiring access to its open files."""
import os
from pathlib import Path
import sys


def describe_holder(lock, proc=Path("/proc")):
    info = lock.stat()
    device = (os.major(info.st_dev), os.minor(info.st_dev), info.st_ino)
    # Match the kernel lock's device/inode, not a writable sidecar or stale PID file.
    for line in (proc / "locks").read_text().splitlines():
        fields = line.split()
        if len(fields) < 6 or fields[1] != "FLOCK":
            continue
        major, minor, inode = fields[5].split(":")
        if (int(major, 16), int(minor, 16), int(inode)) != device:
            continue
        pid = fields[4]
        description = f"PID {pid}"
        try:
            description += f" ({(proc / pid / 'comm').read_text().strip()})"
            units = [part for part in (proc / pid / "cgroup").read_text().split("/")
                     if part.strip().endswith(".service")]
            if units:
                description += f" unit {units[-1].strip()}"
        except OSError:
            # If /proc/PID details are hidden, the kernel lock entry still identifies the holder PID.
            pass
        return description
    return "a holder that released the lock before inspection"


if __name__ == "__main__":
    try:
        print(describe_holder(Path(sys.argv[1])))
    except OSError as error:
        print(f"an uninspectable holder ({error})")
