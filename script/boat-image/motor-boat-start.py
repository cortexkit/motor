#!/usr/bin/env python3
"""Measure idle runner cost before admitting the sandbox's single job."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

MIB = 1 << 20
MARGIN = 512 * MIB
SOCKET = '/var/lib/motor-boat/state/boat/runner.sock'


def budget(available, overhead, margin=MARGIN):
    result = (available - overhead - margin) // MIB * MIB
    if result < (1 << 30):
        raise ValueError('less than 1 GiB remains after idle cost and headroom')
    return result


def clear_provider_environment():
    for name in list(os.environ):
        if name == 'ASCII_TOKEN' or name.startswith('BOAT_'):
            del os.environ[name]


def owns_listener(pid, path=SOCKET):
    inodes = set()
    for line in Path('/proc/net/unix').read_text().splitlines()[1:]:
        fields = line.split(maxsplit=7)
        if len(fields) == 8 and fields[7] == path and int(fields[3], 16) & 0x10000:
            inodes.add('socket:[' + fields[6] + ']')
    for fd in Path(f'/proc/{pid}/fd').iterdir():
        try:
            if os.readlink(fd) in inodes:
                return True
        except FileNotFoundError:
            pass
    return False


def owns_endpoint(pid):
    inodes = set()
    for path in ('/proc/net/tcp', '/proc/net/tcp6'):
        for line in Path(path).read_text().splitlines()[1:]:
            fields = line.split()
            if fields[1].split(':')[1] == '24E3' and fields[3] == '0A':
                if fields[1] != '0100007F:24E3':
                    raise RuntimeError('endpoint is not exclusively IPv4 loopback')
                inodes.add('socket:[' + fields[9] + ']')
    return any(os.readlink(fd) in inodes for fd in Path(f'/proc/{pid}/fd').iterdir())


def main():
    clear_provider_environment()
    if sys.argv[1:2] == ['--host-exec']:
        os.execv('/usr/local/bin/motor-host', ['/usr/local/bin/motor-host', *sys.argv[2:]])
    if sys.argv[1:] in (['--wait-ready'], ['--wait-host-ready']):
        host = sys.argv[1] == '--wait-host-ready'
        until = time.monotonic() + 45
        while time.monotonic() < until:
            unit = 'motor-boat-host' if host else 'motor-boat'
            pid = int(subprocess.check_output(['systemctl', 'show', unit, '--property=MainPID', '--value'], text=True))
            try:
                ready = (Path(f'/proc/{pid}/exe').resolve().name == 'motor-host' and owns_endpoint(pid)) if host else (
                    Path(f'/proc/{pid}/exe').resolve().name == 'motor-runner' and
                    all(owns_listener(pid, path) for path in (SOCKET, '/run/motor-tenant/jobs.sock', '/run/motor-tenant/events.sock', '/run/motor-tenant/admin.sock')))
                if ready:
                    return
            except (OSError, ValueError):
                pass
            time.sleep(.1)
        raise RuntimeError('service never owned its required listening sockets')
    baseline = json.loads(Path('/run/motor-boat-memory.json').read_text())
    available = baseline['mem_available_bytes']
    command = ['/usr/local/bin/motor-runner', *sys.argv[1:]]
    child = subprocess.Popen([*command, '--boat-memory-max-bytes', str(budget(available, 0))])
    try:
        until = time.monotonic() + 30
        while not owns_listener(child.pid):
            if child.poll() is not None or time.monotonic() >= until:
                raise RuntimeError('bootstrap runner never owned a listening socket')
            time.sleep(.1)
        membership = Path('/proc/self/cgroup').read_text().strip().split('0::', 1)[1].lstrip('/')
        cgroup = Path('/sys/fs/cgroup') / membership
        samples = []
        for _ in range(10):
            samples.append(int((cgroup / 'memory.current').read_text()))
            time.sleep(.1)
        overhead = max(samples)
    finally:
        child.send_signal(signal.SIGTERM)
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)
    limit = budget(available, overhead)
    evidence = dict(baseline, runner_idle_cgroup=str(cgroup), runner_idle_samples_bytes=samples,
                    runner_idle_bytes=overhead, margin_bytes=MARGIN, memory_max_bytes=limit)
    Path('/var/lib/motor-boat/budget.json').write_text(json.dumps(evidence, indent=2) + '\n')
    os.execv(command[0], [*command, '--boat-memory-max-bytes', str(limit)])


if __name__ == '__main__':
    main()
