#!/usr/bin/env python3
"""Check the build server's inbound firewall rules and listening sockets.

The builder accepts inbound SSH and nothing else. One named exception exists
for a hosted-runners host: the motor-host TLS listener on one TCP port, IPv4
only. It is allowed only when the caller names that port with
--host-agent-port, and only as the exact rule script/hosted-host/install.sh
creates:

    ufw allow in proto tcp from any to 0.0.0.0/0 port <port> comment motor-host-agent

That rule lists as `<port>/tcp  ALLOW IN  Anywhere  # motor-host-agent` in
`ufw status verbose`, with no `(v6)` twin. Any other inbound allow, an IPv6 rule
for the port, or the port without the comment is refused. With the port named,
a TCP listener on 0.0.0.0:<port> is allowed; an IPv6 or wildcard-dual-stack
listener on it is not.

setup.sh sends this file to the server over SSH and runs it there, so it must
depend on nothing but the Python standard library.
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys

HOST_AGENT_COMMENT = "motor-host-agent"

# ufw prints `%-26s %-12s%s` (To, Action with direction, From) and an optional
# `# comment`. Only `ufw status verbose` prints the IN direction.
RULE = re.compile(
    r"^(?P<to>\S(?:.*?\S)?)\s+(?P<action>ALLOW|DENY|REJECT|LIMIT)(?: (?P<direction>IN|OUT|FWD))?"
    r"\s+(?P<source>\S(?:.*?\S)?)(?:\s+# (?P<comment>.*?))?\s*$")


def firewall_violations(status: str, host_agent_port: int | None) -> list[str]:
    """Problems in `ufw status verbose` output; empty means the policy holds."""
    lines = status.splitlines()
    problems = []
    if "Status: active" not in lines:
        problems.append("ufw is not active")
    if not any(line.startswith("Default: deny (incoming)") for line in lines):
        problems.append("ufw default incoming policy is not deny")
    try:
        start = next(i for i, line in enumerate(lines) if re.match(r"^--\s+------\s+----\s*$", line)) + 1
    except StopIteration:
        start = len(lines)
    allowed = {"22/tcp": None, "22/tcp (v6)": None}
    if host_agent_port is not None:
        allowed[f"{host_agent_port}/tcp"] = HOST_AGENT_COMMENT
    for line in lines[start:]:
        if not line.strip():
            continue
        rule = RULE.match(line)
        if rule is None:
            problems.append(f"Unparsed firewall rule: {line}")
            continue
        if rule["action"] != "ALLOW" or rule["direction"] != "IN":
            continue
        to = rule["to"]
        if to not in allowed:
            problems.append(f"Unexpected inbound allow: {line}")
        elif allowed[to] is not None and rule["comment"] != allowed[to]:
            problems.append(f"Host agent rule must carry the comment {allowed[to]!r}: {line}")
        elif allowed[to] is not None and rule["source"] != "Anywhere":
            problems.append(f"Host agent rule must be the IPv4 allow from Anywhere that install.sh writes: {line}")
    return problems


def listener_violations(sockets: str, host_agent_port: int | None) -> list[str]:
    """Problems in `ss -H -lntup` output: non-loopback listeners other than SSH,
    the DHCP client and, when named, the host agent on IPv4."""
    problems = []
    for line in sockets.splitlines():
        fields = line.split()
        if len(fields) < 5:
            continue
        netid, address = fields[0], fields[4]
        if address.startswith("127.") or address.startswith("[::1]:"):
            continue
        if netid == "tcp" and address.endswith(":22"):
            continue
        if netid == "udp" and address.endswith(":68"):
            continue
        if host_agent_port is not None and netid == "tcp" and address == f"0.0.0.0:{host_agent_port}":
            continue
        problems.append(f"Unexpected listener: {line}")
    return problems


def port(text: str) -> int:
    value = int(text)
    if not 1 <= value <= 65535 or value == 22 or str(value) != text:
        raise argparse.ArgumentTypeError("host agent port must be 1-65535 and not 22")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host-agent-port", type=port,
                        help="allow the motor-host TLS port (IPv4 TCP, ufw comment motor-host-agent)")
    args = parser.parse_args()
    status = subprocess.run(["sudo", "-n", "ufw", "status", "verbose"], check=True, capture_output=True, text=True).stdout
    sockets = subprocess.run(["sudo", "-n", "ss", "-H", "-lntup"], check=True, capture_output=True, text=True).stdout
    print(status, end="")
    problems = firewall_violations(status, args.host_agent_port) + listener_violations(sockets, args.host_agent_port)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        return 1
    exception = f"; host agent exception {args.host_agent_port}/tcp IPv4" if args.host_agent_port else ""
    print(f"inbound policy: SSH only{exception}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
