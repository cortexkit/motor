#!/usr/bin/env python3
"""Entry points for the Boat units: measure idle runner cost before admitting
the sandbox's single job, wait for Boat's file restore, watch for the
broker's identity file, and check service readiness."""
import json
import fcntl
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time

MIB = 1 << 20
MARGIN = 512 * MIB
SOCKET = '/var/lib/motor-boat/state/boat/runner.sock'
# Startup deadlines for one service start. Boat restores files into the guest
# after boot, and first reads after that restore can still be cold, so these
# are generous; the unit's TimeoutStartSec covers both with room to spare.
BOOTSTRAP_DEADLINE_S = 120
READY_DEADLINE_S = 120
# Boat restores a snapshot's files lazily. While that runs, Boat's agent
# (ascii-agent) keeps this file and serves /etc, /usr and other system
# directories from a FUSE filesystem (ascii-lazyfs). When the restore finishes
# it removes the file and moves the real-disk directories into place, which
# strands anything started on the FUSE view: inotify watches stay on dead
# inodes and mounts stacked on those directories are moved aside.
RESTORE_ACTIVE = Path('/var/lib/ascii-lazy/active')
RESTORED_PATHS = ('/etc', '/usr', '/srv', '/var/lib/motor-boat', '/Users')
RESTORE_DEADLINE_S = 30 * 60
IDENTITY = Path('/etc/motor/tenant/id')
STATE = Path('/var/lib/motor-boat')
USERS = Path('/Users')
IMAGE = Path('/var/lib/motor-boat.xfs')
STORAGE_EVIDENCE = Path('/run/motor-boat-storage.json')


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


def mount_table(text):
    """(mount point, filesystem type, source) per /proc/self/mountinfo line, in
    table order; a later entry on the same mount point is mounted on top."""
    unescape = lambda value: re.sub(r'\\([0-7]{3})', lambda m: chr(int(m.group(1), 8)), value)
    rows = []
    for line in text.splitlines():
        left, _, right = line.partition(' - ')
        fields, tail = left.split(), right.split()
        if len(fields) >= 5 and len(tail) >= 2:
            rows.append((unescape(fields[4]), tail[0], unescape(tail[1])))
    return rows


def serving_mount(path, rows):
    """The mount that serves `path`: the deepest mount point containing it."""
    best = None
    for row in rows:
        point = row[0]
        if path == point or path.startswith(point.rstrip('/') + '/'):
            if best is None or len(point) >= len(best[0]):
                best = row
    return best


def restore_pending(mountinfo, restore_active):
    """A description of what still shows Boat's file restore in progress, or
    None once every checked path is on the real disk."""
    if restore_active:
        return f'Boat restore marker {RESTORE_ACTIVE} is present'
    rows = mount_table(mountinfo)
    for path in RESTORED_PATHS:
        row = serving_mount(path, rows)
        if row is not None and row[1].startswith('fuse'):
            return f'{path} is still served by {row[2]} ({row[1]}), not the real disk'
    return None


def wait_restored(clock=time.monotonic, pause=time.sleep):
    started = clock()
    last = None
    while True:
        reason = restore_pending(Path('/proc/self/mountinfo').read_text(), RESTORE_ACTIVE.exists())
        if reason is None:
            print(f'system files are on the real disk after {clock() - started:.1f} s', flush=True)
            return
        if reason != last:
            print('waiting for Boat to finish restoring files: ' + reason, flush=True)
            last = reason
        if clock() - started >= RESTORE_DEADLINE_S:
            raise RuntimeError('Boat file restore did not finish: ' + reason)
        pause(1)


def mounts():
    return mount_table(Path('/proc/self/mountinfo').read_text())


def state_mount(rows):
    row = serving_mount(str(STATE), rows)
    if row is None or row[:2] != (str(STATE), 'xfs'):
        raise RuntimeError(f'{STATE} must be an XFS mount of {IMAGE}, found {row}')
    unique_mount(STATE, rows)
    device = STATE.stat().st_dev
    backing = Path(f'/sys/dev/block/{os.major(device)}:{os.minor(device)}/loop/backing_file')
    if not backing.is_file() or not Path(backing.read_text().strip()).samefile(IMAGE):
        raise RuntimeError(f'{STATE} is not backed by {IMAGE}: {row}')
    return row


def users_mount(rows):
    row = serving_mount(str(USERS), rows)
    if row is None or row[:2] != (str(USERS), 'xfs') or not USERS.samefile(STATE / 'state/workspaces'):
        raise RuntimeError(f'{USERS} must bind {STATE}/state/workspaces, found {row}')
    unique_mount(USERS, rows)
    return row


def unique_mount(path, rows):
    count = sum(row[0] == str(path) for row in rows)
    if count != 1:
        raise RuntimeError(f'{path} must have exactly one mount, found {count}; stacked mount targets are unsafe')


def empty_mountpoint(path):
    if path.is_symlink() or not path.is_dir():
        raise RuntimeError(f'mount point is not a physical directory: {path}')
    if next(path.iterdir(), None) is not None:
        raise RuntimeError(f'refusing to mount over non-empty directory {path}; stop services and inspect the root-disk data')


def unmount_root_view(path):
    empty_mountpoint(path)
    rows = mounts()
    exact = [row for row in rows if row[0] == str(path)]
    if not exact:
        return
    if len(exact) != 1 or exact[0][1] != 'ext4' or path.stat().st_dev != Path('/').stat().st_dev:
        raise RuntimeError(f'refusing to unmount an unexpected storage view at {path}: {exact}')
    # Only the public, empty root-disk bind is retired. Boat's references under
    # /var/lib/ascii-lazy/sysbind remain mounted and owned by its restore agent.
    subprocess.run(['/usr/bin/umount', '--', str(path)], check=True)
    empty_mountpoint(path)


def check_storage():
    rows = mounts()
    state, users = state_mount(rows), users_mount(rows)
    geometry = subprocess.check_output(['/usr/sbin/xfs_info', str(STATE)], text=True)
    if not re.search(r'\breflink=1\b', geometry):
        raise RuntimeError(f'{STATE} does not have XFS reflink enabled')
    with tempfile.TemporaryFile(dir=STATE) as source, tempfile.TemporaryFile(dir=USERS) as target:
        source.write(os.urandom(4096))
        source.flush()
        fcntl.ioctl(target.fileno(), 0x40049409, source.fileno())
        target.seek(0)
        source.seek(0)
        if target.read() != source.read():
            raise RuntimeError('storage FICLONE content mismatch')
    return {'state': state, 'users': users, 'reflink': True}


def prepare_storage():
    """Replace Boat's empty root-disk sysbind views only after file restoration.
    A .mount unit can be active for that foreign bind, so systemctl start alone
    cannot establish the disk identity. Retire only the empty public root-disk
    binds, so neither path has duplicate mount targets after preparation."""
    reason = restore_pending(Path('/proc/self/mountinfo').read_text(), RESTORE_ACTIVE.exists())
    if reason:
        raise RuntimeError('cannot prepare storage before Boat restore: ' + reason)
    rows = mounts()
    ready = []
    for path, validate in ((STATE, state_mount), (USERS, users_mount)):
        path.mkdir(exist_ok=True)
        try:
            validate(rows)
            ready.append(True)
        except (OSError, RuntimeError):
            empty_mountpoint(path)
            ready.append(False)
    # Check both views before mounting either: a dirty root-disk workspace must
    # not be hidden even when the state mount point is empty.
    for path, mounted in ((USERS, ready[1]), (STATE, ready[0])):
        if not mounted:
            unmount_root_view(path)
    if not ready[0]:
        subprocess.run(['/usr/bin/mount', '-t', 'xfs', '-o', 'loop,noatime', str(IMAGE), str(STATE)], check=True)
    state_mount(mounts())
    for directory in ('state', 'state/workspaces', 'home', 'tmp'):
        (STATE / directory).mkdir(parents=True, exist_ok=True)
    if not ready[1]:
        subprocess.run(['/usr/bin/mount', '--bind', str(STATE / 'state/workspaces'), str(USERS)], check=True)
    evidence = check_storage()
    evidence.update(boot_id=Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                    verified_monotonic_ns=time.monotonic_ns())
    STORAGE_EVIDENCE.write_text(json.dumps(evidence) + '\n')
    print('Boat storage ready: ' + json.dumps(evidence), flush=True)


def identity_present(path=IDENTITY):
    try:
        return stat.S_ISREG(os.lstat(path).st_mode)
    except FileNotFoundError:
        return False


def watch_identity(start_host, present=identity_present, pause=time.sleep):
    """Start the endpoint once the broker's identity file exists, however it
    was written (in place or by rename) and whether it predates this watcher.
    Polling by path, not inotify: a path lookup always goes through the
    current mount table. No deadline, because the broker installs the file
    whenever it books the sandbox and installs it again after a resume."""
    while not present():
        pause(1)
    print('sandbox identity present; starting the endpoint', flush=True)
    # Starting an active unit is a no-op, so a repeat never adds an endpoint.
    start_host()


def main():
    clear_provider_environment()
    if sys.argv[1:] == ['--prepare-storage']:
        return prepare_storage()
    if sys.argv[1:] == ['--check-mountpoints']:
        for path in (STATE, USERS):
            empty_mountpoint(path)
        return
    if sys.argv[1:2] == ['--host-exec']:
        check_storage()
        os.execv('/usr/local/bin/motor-host', ['/usr/local/bin/motor-host', *sys.argv[2:]])
    if sys.argv[1:] == ['--wait-restored']:
        return wait_restored()
    if sys.argv[1:] == ['--watch-identity']:
        return watch_identity(lambda: subprocess.run(
            ['/usr/bin/systemctl', 'start', '--no-block', 'motor-boat-host.service'], check=True))
    if sys.argv[1:] in (['--wait-ready'], ['--wait-host-ready']):
        host = sys.argv[1] == '--wait-host-ready'
        until = time.monotonic() + READY_DEADLINE_S
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
    check_storage()
    baseline = json.loads(Path('/run/motor-boat-memory.json').read_text())
    available = baseline['mem_available_bytes']
    command = ['/usr/local/bin/motor-runner', *sys.argv[1:]]
    child = subprocess.Popen([*command, '--boat-memory-max-bytes', str(budget(available, 0))])
    try:
        until = time.monotonic() + BOOTSTRAP_DEADLINE_S
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
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f'Boat startup refused: {error}', file=sys.stderr, flush=True)
        sys.exit(1)
