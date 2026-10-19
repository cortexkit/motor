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


def owns_listener(pid):
    inodes = set()
    for line in Path('/proc/net/unix').read_text().splitlines()[1:]:
        fields = line.split(maxsplit=7)
        if len(fields) == 8 and fields[7] == SOCKET and int(fields[3], 16) & 0x10000:
            inodes.add('socket:[' + fields[6] + ']')
    for fd in Path(f'/proc/{pid}/fd').iterdir():
        try:
            if os.readlink(fd) in inodes:
                return True
        except FileNotFoundError:
            pass
    return False


def main():
    if sys.argv[1:] == ['--wait-ready']:
        until = time.monotonic() + 45
        while time.monotonic() < until:
            pid = int(subprocess.check_output(['systemctl', 'show', 'motor-boat', '--property=MainPID', '--value'], text=True))
            try:
                if Path(f'/proc/{pid}/exe').resolve().name == 'motor-runner' and owns_listener(pid):
                    return
            except (OSError, ValueError):
                pass
            time.sleep(.1)
        raise RuntimeError('final runner never owned its listening socket')
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
