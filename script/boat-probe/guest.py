#!/usr/bin/env python3
"""Disposable guest-side measurements; never receives the Boat API credential."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import time


def command(argv, timeout=30, env=None):
    start = time.monotonic()
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, env=env)
        return {"argv": argv, "exit_code": result.returncode, "stdout": result.stdout,
                "stderr": result.stderr, "seconds": time.monotonic() - start}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"argv": argv, "error": str(error), "seconds": time.monotonic() - start}


def root_prefix():
    if os.geteuid() == 0:
        return []
    if command(["sudo", "-n", "true"]).get("exit_code") == 0:
        return ["sudo", "-n"]
    return []


CGROUP = r'''
import json, pathlib
root = pathlib.Path('/sys/fs/cgroup')
relative = next(line.split(':', 2)[2] for line in pathlib.Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
parent = root / relative.lstrip('/')
# A cgroup namespace may hide the host-side path from the mounted hierarchy.
if not parent.exists():
    parent = root
out = {'parent': str(parent)}
for name in ('cgroup.controllers', 'cgroup.subtree_control', 'cgroup.type'):
    try:
        out[name] = (parent / name).read_text()
    except OSError as e:
        out[name] = str(e)
child = parent / ('boat-probe-' + str(__import__('os').getpid()))
try:
    child.mkdir()
    out['created'] = True
    for name, value in (('memory.max', '67108864'), ('cpu.max', '50000 100000'), ('pids.max', '32')):
        try:
            (child / name).write_text(value)
            out[name] = {'requested': value, 'read_back': (child / name).read_text()}
        except OSError as e:
            out[name] = {'error': str(e)}
except OSError as e:
    out['error'] = str(e)
finally:
    if child.exists():
        try:
            child.rmdir()
            out['removed'] = True
        except OSError as e:
            out['remove_error'] = str(e)
print(json.dumps(out))
'''


def inspect(base):
    privileged = root_prefix()
    systemd_user = [] if privileged or os.geteuid() == 0 else ["--user"]
    checks = {
        "identity": ["sh", "-c", "whoami; id; command -v sudo; sudo -n -l"],
        "capabilities_user": ["sh", "-c", "command -v capsh && capsh --print; "
                              "command -v getpcaps && getpcaps $$; "
                              "cat /proc/self/status"],
        "capabilities_privileged": privileged + ["sh", "-c", "id; cat /proc/self/status"],
        "kernel_init": ["sh", "-c", "uname -a; cat /proc/version; "
                        "ps -p 1 -o pid,user,comm,args; readlink /proc/1/exe; "
                        "cat /proc/1/cgroup; findmnt -t cgroup2"],
        "systemctl": privileged + ["systemctl", *systemd_user, "is-system-running", "--no-pager"],
        "systemd_run": privileged + ["systemd-run", *systemd_user, "--wait", "--pipe", "--collect",
                                    "--unit=boat-probe-" + str(os.getpid()),
                                    "-p", "MemoryMax=64M", "-p", "CPUQuota=50%", "/bin/true"],
        "cgroup": privileged + ["python3", "-c", CGROUP],
        "namespace_user": privileged + ["unshare", "--user", "--map-root-user", "id"],
        "namespace_mount": privileged + ["unshare", "--mount", "sh", "-c", "readlink /proc/self/ns/mnt"],
        "namespace_pid": privileged + ["unshare", "--pid", "--fork", "--mount-proc", "sh", "-c", "echo $$"],
        "namespace_network": privileged + ["unshare", "--net", "sh", "-c", "readlink /proc/self/ns/net; ip link"],
        "kvm": ["sh", "-c", "ls -l /dev/kvm; test -r /dev/kvm && test -w /dev/kvm"],
        "disk": ["sh", "-c", "df -B1 -T " + shlex.quote(str(base)) +
                 "; findmnt -T " + shlex.quote(str(base)) + "; lsblk -b -f"],
        "egress": ["sh", "-c", "getent ahostsv4 example.com; "
                   "curl --noproxy '*' --connect-timeout 5 --max-time 15 -sS -o /dev/null "
                   "-w 'https_code=%{http_code} remote_ip=%{remote_ip} connect=%{time_connect} tls=%{time_appconnect} total=%{time_total}\\n' "
                   "https://example.com/"],
    }
    out = {"uid": os.geteuid(), "privilege_prefix": privileged,
           "checks": {name: command(argv) for name, argv in checks.items()}}
    import fcntl
    source, destination = base / "reflink-source", base / "reflink-destination"
    source.write_bytes(secrets.token_bytes(65536))
    try:
        with source.open("rb") as src, destination.open("wb") as dst:
            fcntl.ioctl(dst.fileno(), 0x40049409, src.fileno())  # Linux FICLONE, not a copying fallback.
        out["reflink"] = {"ficlone": True, "equal": source.read_bytes() == destination.read_bytes()}
    except OSError as error:
        out["reflink"] = {"ficlone": False, "errno": error.errno, "error": str(error)}
    finally:
        source.unlink(missing_ok=True)
        destination.unlink(missing_ok=True)
    marker = base / "persistence"
    with marker.open("wb") as stream:
        stream.write(secrets.token_bytes(4096))
        stream.flush()
        os.fsync(stream.fileno())
    out["persistence_sha256"] = hashlib.sha256(marker.read_bytes()).hexdigest()
    return out


SERVER = r'''
import pathlib, socket, ssl, sys
base, port = pathlib.Path(sys.argv[1]), int(sys.argv[2])
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(base / 'cert.pem', base / 'key.pem')
dual = socket.has_dualstack_ipv6()
with socket.create_server(('::' if dual else '0.0.0.0', port),
                          family=socket.AF_INET6 if dual else socket.AF_INET,
                          dualstack_ipv6=dual, backlog=16) as listener:
    while True:
        raw, _ = listener.accept()
        raw.settimeout(3)
        try:
            with context.wrap_socket(raw, server_side=True) as stream:
                data = b''
                while not data.endswith(b'\n') and len(data) < 256:
                    chunk = stream.recv(256)
                    if not chunk:
                        break
                    data += chunk
                stream.sendall(b'boat-probe:' + data)
        except (OSError, ssl.SSLError):
            raw.close()
'''


def listener(base, port):
    import socket
    import ssl
    if not (base / "cert.pem").exists():
        cert = command(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                        "-keyout", str(base / "key.pem"), "-out", str(base / "cert.pem"),
                        "-days", "2", "-subj", "/CN=boat-probe"])
        if cert.get("exit_code") != 0:
            return {"certificate_error": cert}
        (base / "key.pem").chmod(0o600)
    (base / "server.py").write_text(SERVER)
    context = ssl.create_default_context(cafile=str(base / "cert.pem"))
    context.check_hostname = False

    def healthy():
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=2) as raw:
                with context.wrap_socket(raw, server_hostname="boat-probe") as stream:
                    stream.sendall(b"local-readiness\n")
                    return stream.recv(256) == b"boat-probe:local-readiness\n"
        except OSError:
            return False

    if not healthy():
        with (base / "server.log").open("ab") as log:
            process = subprocess.Popen([sys.executable, str(base / "server.py"), str(base), str(port)],
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                       start_new_session=True)
        (base / "server.pid").write_text(str(process.pid))
        until = time.monotonic() + 10
        while time.monotonic() < until and not healthy():
            time.sleep(0.1)
    return {"ready": healthy(), "pid": (base / "server.pid").read_text(),
            "certificate_pem": (base / "cert.pem").read_text(),
            "server_log": (base / "server.log").read_text()}


def persistence(base):
    marker = base / "persistence"
    return {"exists": marker.exists(),
            "sha256": hashlib.sha256(marker.read_bytes()).hexdigest() if marker.exists() else None}


def firewall(base, port, apply):
    prefix = root_prefix()
    out = {"apply": apply, "rules": []}
    if apply:
        out["allow"] = command(prefix + ["ufw", "allow", str(port) + "/tcp"])
    for binary in ("iptables", "ip6tables"):
        out["rules"].append(command(prefix + [binary, "-S", "INPUT"]))
    out["ufw"] = command(prefix + ["ufw", "status", "verbose"])
    return out


def toolchain_install(base, version):
    env = dict(os.environ, CARGO_HOME=str(base / "cargo"), RUSTUP_HOME=str(base / "rustup"))
    steps = []
    rustup = str(base / "cargo/bin/rustup")
    if not Path(rustup).exists():
        steps.append(command(["curl", "--fail", "--location", "--max-time", "120",
                              "https://sh.rustup.rs", "-o", str(base / "rustup-init.sh")], timeout=130))
        if steps[-1].get("exit_code") == 0:
            steps.append(command(["sh", str(base / "rustup-init.sh"), "-y", "--no-modify-path",
                                  "--profile", "default", "--default-toolchain", version], timeout=440, env=env))
    else:
        steps.append(command([rustup, "toolchain", "install", version, "--profile", "default"], timeout=540, env=env))
    return {"version": version, "steps": steps}


def toolchain(base):
    env = dict(os.environ, CARGO_HOME=str(base / "cargo"), RUSTUP_HOME=str(base / "rustup"))
    sizes = command(["du", "-s", "-B1", str(base / "rustup"), str(base / "cargo")])
    versions = command([str(base / "cargo/bin/rustup"), "show"], env=env)
    try:
        installed_bytes = sum(int(line.split()[0]) for line in sizes["stdout"].splitlines())
    except (KeyError, ValueError):
        installed_bytes = 0
    return {"sizes": sizes, "versions": versions, "installed_bytes": installed_bytes,
            "few_gb_installed": installed_bytes >= 2 * 1024 ** 3}


def toolchain_check(base):
    env = dict(os.environ, CARGO_HOME=str(base / "cargo"), RUSTUP_HOME=str(base / "rustup"))
    return {"versions": [command([str(base / "cargo/bin/rustup"), "run", version, "rustc", "--version"], env=env)
                         for version in ("1.85.1", "1.86.0", "1.87.0")],
            "sizes": command(["du", "-s", "-B1", str(base / "rustup"), str(base / "cargo")])}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=("inspect", "listener", "persistence", "toolchain-install", "toolchain", "toolchain-check", "firewall-read", "firewall-open"))
    parser.add_argument("base", type=Path)
    parser.add_argument("--port", type=int, default=9443)
    parser.add_argument("--toolchain-version", choices=("1.85.1", "1.86.0", "1.87.0"), default="1.85.1")
    args = parser.parse_args()
    args.base = args.base.resolve()
    args.base.mkdir(mode=0o700, parents=True, exist_ok=True)
    if args.phase == "listener":
        out = listener(args.base, args.port)
    elif args.phase == "toolchain-install":
        out = toolchain_install(args.base, args.toolchain_version)
    elif args.phase.startswith("firewall-"):
        out = firewall(args.base, args.port, args.phase == "firewall-open")
    else:
        out = globals()[args.phase.replace("-", "_")](args.base)
    print(json.dumps(out))


if __name__ == "__main__":
    main()
