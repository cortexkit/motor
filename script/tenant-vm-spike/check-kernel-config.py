#!/usr/bin/env python3
"""Check resolved kernel prerequisites, not the builder's requested options."""

from pathlib import Path
import sys

REQUIRED = ("NAMESPACES", "UTS_NS", "IPC_NS", "USER_NS", "PID_NS", "NET_NS", "CGROUPS",
            "MEMCG", "CGROUP_PIDS", "SECCOMP", "SECCOMP_FILTER", "BPF_SYSCALL", "CGROUP_BPF",
            "BPF_JIT", "XFS_FS", "VIRTIO_BLK", "VIRTIO_NET", "VIRTIO_VSOCKETS")
FALLBACK_TUNNELS = ("IPV6_SIT", "NET_IPGRE", "NET_IPIP", "IPV6_GRE", "IPV6_TUNNEL")


def check(text):
    lines = set(text.splitlines())
    errors = [f"CONFIG_{name} must be built in" for name in REQUIRED if f"CONFIG_{name}=y" not in lines]
    for name in FALLBACK_TUNNELS:
        if any(f"CONFIG_{name}={state}" in lines for state in ("y", "m")):
            errors.append(f"CONFIG_{name} must be unset: namespace fallback interfaces are not allowed")
    return errors


if __name__ == "__main__":
    errors = check(Path(sys.argv[1]).read_text())
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        sys.exit(1)
    print(f"kernel config: {len(REQUIRED) + len(FALLBACK_TUNNELS)} checks passed "
          "(namespaces, cgroup BPF, seccomp, XFS/virtio; no fallback tunnels)")
