#!/usr/bin/env python3
"""Entry points for the Boat units: measure idle runner cost before admitting
the sandbox's single job, wait for Boat's file restore, watch for the
broker's identity file, and check service readiness."""
import json
from contextlib import contextmanager
import fcntl
import os
from pathlib import Path
import re
import signal
import socket
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
RESTORE_STABLE_S = 5
IDENTITY = Path('/etc/motor/tenant/id')
STATE = Path('/var/lib/motor-boat')
USERS = Path('/Users')
IMAGE = Path('/var/lib/motor-boat.xfs')
STORAGE_EVIDENCE = Path('/run/motor-boat-storage.json')
ENDPOINT_READY = Path('/run/motor/endpoint-ready')
STORAGE_HEALTH = Path('/run/motor-boat-storage/health.json')
STORAGE_LOCK = Path('/run/motor-boat-storage/lock')
STORAGE_HEALTH_TTL_NS = 5_000_000_000
# The endpoint's configuration with vcpu_ceiling set to this sandbox's CPU count, rewritten
# on every endpoint start. RuntimeDirectory=motor in motor-boat-host.service owns /run/motor.
SIZED_SANDBOX = Path('/run/motor/sandbox.json')


def sandbox_cpus():
    """The CPUs this sandbox really has, measured on every start (what nproc prints).

    The image is built once, on one Boat size, and sandboxes of other sizes are created
    from it, so a CPU count written at build time would be wrong on every other size."""
    return len(os.sched_getaffinity(0))


def sized_runner_args(args, cpus):
    """The runner's arguments with its job capacity and rustc slots set to `cpus`.
    Any count already present (an older unit file) is replaced, never trusted."""
    kept = []
    words = iter(args)
    for word in words:
        if word in ('--max-weight', '--rustc-slots'):
            next(words, None)
            continue
        kept.append(word)
    return [*kept, '--max-weight', str(cpus), '--rustc-slots', str(cpus)]


def sized_host_args(args, cpus, output=None):
    """The endpoint's arguments, with `--sandbox PATH` replaced by a copy of that
    configuration whose vcpu_ceiling (the most vCPUs it admits at once) is `cpus`."""
    args = list(args)
    if '--sandbox' not in args or args.index('--sandbox') + 1 >= len(args):
        return args
    index = args.index('--sandbox') + 1
    config = json.loads(Path(args[index]).read_text())
    config['vcpu_ceiling'] = cpus
    output = SIZED_SANDBOX if output is None else output
    temporary = output.with_suffix('.tmp')
    with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600), 'w') as stream:
        json.dump(config, stream, indent=2)
        stream.write('\n')
    temporary.replace(output)
    args[index] = str(output)
    return args


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
    # The active marker disappears before Boat's deferred public-bind cleanup.
    # Do not replace those binds: that cleanup would unmount our replacement.
    for path in (str(STATE), str(USERS)):
        if any(row[0] == path and row[1] == 'ext4' for row in rows):
            return f'Boat public restore bind at {path} has not been retired'
    return None


def wait_restored(clock=time.monotonic, pause=time.sleep):
    started = clock()
    last = None
    stable_since = None
    while True:
        reason = restore_pending(Path('/proc/self/mountinfo').read_text(), RESTORE_ACTIVE.exists())
        if reason is None:
            if stable_since is None:
                stable_since = clock()
            if clock() - stable_since >= RESTORE_STABLE_S:
                print(f'system files and public mount cleanup settled after {clock() - started:.1f} s', flush=True)
                return
            reason = 'waiting for a stable real-disk mount view'
        else:
            stable_since = None
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


def check_storage():
    rows = mounts()
    state, users = state_mount(rows), users_mount(rows)
    options = subprocess.check_output(['/usr/bin/findmnt', '-n', '-o', 'OPTIONS', '--target', str(STATE)], text=True)
    if not ({'prjquota', 'pquota'} & set(options.strip().split(','))):
        raise RuntimeError('Boat state disk requires enforced XFS project quotas')
    if 'discard' not in options.strip().split(','):
        raise RuntimeError('Boat state disk requires discard to reclaim sparse backing space')
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
    """Prepare storage with systemd after Boat removes its public bind mounts."""
    reason = restore_pending(Path('/proc/self/mountinfo').read_text(), RESTORE_ACTIVE.exists())
    if reason:
        raise RuntimeError('cannot prepare storage before Boat restore: ' + reason)
    for unit in ('motor-boat.service', 'motor-boat-host.service'):
        pid = subprocess.check_output(['/usr/bin/systemctl', 'show', unit, '-p', 'MainPID', '--value'], text=True).strip()
        if pid != '0':
            raise RuntimeError(f'refusing to change storage while {unit} has MainPID={pid}')
    rows = mounts()
    for path, validate in ((STATE, state_mount), (USERS, users_mount)):
        path.mkdir(exist_ok=True)
        try:
            validate(rows)
        except (OSError, RuntimeError):
            empty_mountpoint(path)
    # The evidence file blocks fresh mount-unit starts after verification. It may be
    # removed only with both consumers stopped and both target views checked.
    STORAGE_EVIDENCE.unlink(missing_ok=True)
    subprocess.run(['/usr/bin/systemctl', 'daemon-reload'], check=True)
    subprocess.run(['/usr/bin/systemctl', 'start', r'var-lib-motor\x2dboat.mount'], check=True)
    state_mount(mounts())
    for directory in ('state', 'state/workspaces', 'home', 'tmp'):
        (STATE / directory).mkdir(parents=True, exist_ok=True)
    subprocess.run(['/usr/bin/systemctl', 'start', 'Users.mount'], check=True)
    subprocess.run(['/usr/bin/systemctl', 'is-active', '--quiet', r'var-lib-motor\x2dboat.mount', 'Users.mount'], check=True)
    evidence = check_storage()
    evidence.update(identity=storage_identity(), boot_id=Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                    verified_monotonic_ns=time.monotonic_ns())
    STORAGE_EVIDENCE.write_text(json.dumps(evidence) + '\n')
    print('Boat storage ready: ' + json.dumps(evidence), flush=True)


def storage_identity():
    state, users = STATE.stat(), USERS.stat()
    if state.st_dev == Path('/').stat().st_dev or users.st_dev != state.st_dev or not USERS.samefile(STATE / 'state/workspaces'):
        raise RuntimeError('Boat storage view changed: /Users must bind the runner state disk workspaces')
    return state.st_dev, state.st_ino, users.st_ino


@contextmanager
def storage_lock():
    # This inode must survive both services' restarts so all writers serialize
    # on the same lock, rather than locking different files at the same path.
    with os.fdopen(os.open(STORAGE_LOCK, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600), 'r+') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def write_storage_health(status, identity=None, reason=None):
    temporary = STORAGE_HEALTH.with_suffix('.tmp')
    with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600), 'w') as output:
        os.fchmod(output.fileno(), 0o600)
        expires = time.monotonic_ns() + STORAGE_HEALTH_TTL_NS if status == 'healthy' else 0
        json.dump({'status': status, 'identity': identity, 'expires_monotonic_ns': expires, 'reason': reason}, output)
    temporary.replace(STORAGE_HEALTH)


def withdraw_storage(reason='storage_watch_starting'):
    with storage_lock():
        # Withdraw before disk writes or logging; the lock prevents the endpoint
        # from publishing against the old health state during this transition.
        try:
            ENDPOINT_READY.unlink(missing_ok=True)
        finally:
            # Removing the old lease needs no new file allocation. A failed
            # unhealthy-state write must not leave a valid healthy lease behind.
            STORAGE_HEALTH.unlink(missing_ok=True)
            write_storage_health('unhealthy', reason=reason)
        volumes = {}
        for name, path in [('runner_state', STATE), ('workspace', USERS)]:
            try:
                stats = os.statvfs(path)
                volumes[name] = {'volume': str(path), 'free_bytes': stats.f_bavail * stats.f_frsize,
                                 'free_inodes': stats.f_favail}
            except OSError as error:
                volumes[name] = {'volume': str(path), 'probe_error': str(error)}
        record = {'at_ms': time.time_ns() // 1_000_000, 'rule': reason, 'volumes': volumes}
        path = ENDPOINT_READY.with_name('endpoint-withdrawal.json')
        temporary = path.with_suffix('.tmp')
        with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600), 'w') as output:
            output.write(json.dumps(record) + '\n')
        temporary.replace(path)
    print('endpoint_readiness_withdrawn ' + json.dumps(record), file=sys.stderr, flush=True)


def notify_systemd(message):
    address = os.environ.get('NOTIFY_SOCKET')
    if address:
        if address.startswith('@'):
            address = '\0' + address[1:]
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as connection:
            connection.settimeout(1)
            connection.connect(address)
            connection.sendall(message.encode())


QUOTA_READY_FREE_BYTES = (24 << 30) + (64 << 20) + (8 << 30)


def quota_capacity_ready():
    def free(path):
        value = os.statvfs(path)
        return value.f_bavail * value.f_frsize
    return min(free(STATE), free('/')) >= QUOTA_READY_FREE_BYTES


def watch_storage(pause=time.sleep, notify=None):
    notify = notify or notify_systemd
    try:
        withdraw_storage()
        # READY starts systemd's watchdog even if the first evidence read or
        # identity check hangs. It describes monitor liveness, not storage health.
        notify('READY=1')
        expected = tuple(json.loads(STORAGE_EVIDENCE.read_text())['identity'])
        if len(expected) != 3 or any(type(value) is not int or value < 0 for value in expected):
            raise ValueError('invalid Boat storage identity evidence')
        capacity_withdrawn = False
        while True:
            if storage_identity() != expected:
                raise RuntimeError('Boat runner state disk identity changed')
            if not quota_capacity_ready():
                if not capacity_withdrawn:
                    withdraw_storage('cold_quota_capacity_below_floor')
                else:
                    with storage_lock():
                        ENDPOINT_READY.unlink(missing_ok=True)
                        write_storage_health('unhealthy', reason='cold_quota_capacity_below_floor')
                capacity_withdrawn = True
            else:
                with storage_lock():
                    write_storage_health('healthy', expected)
                capacity_withdrawn = False
            pause(1)
            notify('WATCHDOG=1')
    except BaseException as error:
        # Attempt withdrawal before logging even for malformed evidence, failed
        # notification, or an interrupted sleep. ExecStopPost repeats cleanup for
        # uncatchable exits; the health lease expires if cleanup cannot complete.
        try:
            withdraw_storage(f'storage_watch_failed: {error}')
        except BaseException:
            try:
                ENDPOINT_READY.unlink(missing_ok=True)
            except OSError:
                pass
        try:
            print(f'Boat storage unhealthy; withdrawing endpoint readiness: {error}', file=sys.stderr, flush=True)
        except SystemExit as log_error:
            raise RuntimeError('Boat storage watcher logging exited') from log_error
        if isinstance(error, SystemExit):
            raise RuntimeError('Boat storage watcher exited during a poll') from error
        raise


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
    if sys.argv[1:] == ['--watch-storage']:
        return watch_storage()
    if sys.argv[1:] == ['--withdraw-storage']:
        return withdraw_storage()
    if sys.argv[1:] == ['--check-mountpoints']:
        for path in (STATE, USERS):
            empty_mountpoint(path)
        return
    if sys.argv[1:2] == ['--host-exec']:
        check_storage()
        args = sized_host_args(sys.argv[2:], sandbox_cpus())
        os.execv('/usr/local/bin/motor-host', ['/usr/local/bin/motor-host', *args])
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
    command = ['/usr/local/bin/motor-runner', *sized_runner_args(sys.argv[1:], sandbox_cpus())]
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
