#!/usr/bin/env python3
"""Measure Linux x86-64 syscalls directly, without libc's ENOSYS fallbacks.

All writable operands are private scratch files. Privileged/process-creating
operations receive invalid arguments so they cannot change mounts or spawn a
child. Job-mode clone3 intentionally returns ENOSYS under PID-init's seccomp
policy; outside jobs the invalid call must reach the kernel and return EINVAL.
"""

import argparse
import ctypes
import errno
import json
import os
from pathlib import Path
import platform
import tempfile

# Linux's O_PATH is absent from Python's stubs on non-Linux developer hosts.
O_PATH = 0x200000


class OpenHow(ctypes.Structure):
    _fields_ = [('flags', ctypes.c_uint64), ('mode', ctypes.c_uint64),
                ('resolve', ctypes.c_uint64)]


class Flock(ctypes.Structure):
    _fields_ = [('type', ctypes.c_short), ('whence', ctypes.c_short),
                ('start', ctypes.c_int64), ('length', ctypes.c_int64),
                ('pid', ctypes.c_int)]


def capture():
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        raise RuntimeError('syscall surface probe requires Linux x86_64')
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    results = {}

    def call(name, number, *args):
        ctypes.set_errno(0)
        result = libc.syscall(ctypes.c_long(number), *args)
        error = ctypes.get_errno() if result == -1 else 0
        results[name] = {'errno': error, 'result': errno.errorcode.get(error, 'OK')}
        return result

    # Querying the ABI changes no sandbox state and detects both missing and
    # boot-disabled Landlock, which job shell sandboxes require.
    abi = call('landlock_create_ruleset.VERSION', 444, ctypes.c_void_p(),
               ctypes.c_size_t(0), ctypes.c_uint(1))
    results['landlock_create_ruleset.VERSION']['abi'] = abi

    with tempfile.TemporaryDirectory(prefix='motor-syscalls-') as tmp:
        root = Path(tmp)
        (root / 'source').write_bytes(b'motor syscall probe\n')
        (root / 'dest').write_bytes(b'destination\n')
        (root / 'link').symlink_to('source')
        directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
        source = os.open(root / 'source', os.O_RDWR)
        dest = os.open(root / 'dest', os.O_RDWR)
        try:
            for name, resolve, path in (
                    ('openat2', 0, b'source'),
                    ('openat2.BENEATH_NO_MAGICLINKS', 0x08 | 0x02, b'source'),
                    ('openat2.BENEATH_NO_SYMLINKS', 0x08 | 0x04, b'source'),
                    ('openat2.IN_ROOT', 0x10, b'/source'),
                    ('openat2.reject_symlink', 0x08 | 0x04, b'link')):
                how = OpenHow(os.O_RDONLY | os.O_CLOEXEC, 0, resolve)
                fd = call(name, 437, directory, path, ctypes.byref(how), ctypes.sizeof(how))
                if fd >= 0:
                    os.close(fd)
            capability = os.open(root, O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
            try:
                call('fchmodat2.EMPTY_PATH', 452, capability, b'', 0o700, 0x1000)
            finally:
                os.close(capability)
            call('renameat2.NOREPLACE', 316, directory, b'source', directory, b'new', 1)
            call('renameat2.EXCHANGE', 316, directory, b'new', directory, b'dest', 2)
            stat = ctypes.create_string_buffer(256)
            call('statx.BASIC_STATS', 332, directory, b'.', 0x100, 0x7ff, ctypes.byref(stat))
            call('flock.EX_NB', 73, source, 2 | 4)
            call('flock.UN', 73, source, 8)
            lock = Flock(1, os.SEEK_SET, 0, 0, 0)
            call('fcntl.SETLK', 72, source, 6, ctypes.byref(lock))
            lock.type = 2
            call('fcntl.UNLCK', 72, source, 6, ctypes.byref(lock))
            lock.type = 1
            call('fcntl.OFD_SETLK', 72, source, 37, ctypes.byref(lock))
            lock.type = 2
            call('fcntl.OFD_UNLCK', 72, source, 37, ctypes.byref(lock))
            # A range above every ordinary fd leaves the Python process intact.
            call('close_range', 436, 0x7ffffffe, 0x7fffffff, 0)
            fd = call('pidfd_open', 434, os.getpid(), 0)
            if fd >= 0:
                os.close(fd)
            call('clone3.invalid', 435, 0, 0)
            call('faccessat2.EACCESS', 439, directory, b'.', os.R_OK, 0x200)
            call('mount_setattr.invalid', 442, -1, b'', 0x1000, 0, 0)
            call('open_tree.invalid', 428, -1, b'', 0x1000)
            call('move_mount.invalid', 429, -1, b'', -1, b'', 0x44)
            flags = ctypes.c_int()
            call('ioctl.FS_IOC_GETFLAGS', 16, source, ctypes.c_ulong(0x80086601), ctypes.byref(flags))
            call('ioctl.FICLONE', 16, dest, ctypes.c_ulong(0x40049409), source)
            call('copy_file_range', 326, source, 0, dest, 0, 1, 0)
            call('fallocate.PUNCH_HOLE_KEEP_SIZE', 285, dest, 3, ctypes.c_long(0), ctypes.c_long(4096))
            call('fsync', 74, source)
            call('syncfs', 306, directory)
            call('getrandom', 318, ctypes.byref(stat), 1, 0)
            fd = call('memfd_create', 319, b'motor-probe', 1)
            if fd >= 0:
                os.close(fd)
        finally:
            os.close(dest)
            os.close(source)
            os.close(directory)
    return results


# This contract is shared by every runner role, not learned from the machine
# being qualified. In particular, ENOSYS must not silently become the baseline.
SUCCESS = (
    'openat2', 'openat2.BENEATH_NO_MAGICLINKS', 'openat2.BENEATH_NO_SYMLINKS',
    'openat2.IN_ROOT', 'fchmodat2.EMPTY_PATH', 'renameat2.NOREPLACE',
    'renameat2.EXCHANGE', 'statx.BASIC_STATS', 'flock.EX_NB', 'flock.UN',
    'fcntl.SETLK', 'fcntl.UNLCK', 'fcntl.OFD_SETLK', 'fcntl.OFD_UNLCK',
    'close_range', 'pidfd_open', 'faccessat2.EACCESS', 'ioctl.FS_IOC_GETFLAGS',
    'ioctl.FICLONE', 'copy_file_range', 'fallocate.PUNCH_HOLE_KEEP_SIZE',
    'fsync', 'syncfs', 'getrandom', 'memfd_create', 'landlock_create_ruleset.VERSION',
)


def expectations(job):
    expected = {name: {0} for name in SUCCESS}
    expected.update({
        'openat2.reject_symlink': {errno.ELOOP},
        'mount_setattr.invalid': {errno.EINVAL},
        'open_tree.invalid': {errno.EBADF, errno.EPERM},
        'move_mount.invalid': {errno.EBADF, errno.EPERM},
        # In jobs, PID-init denies clone3 so libc uses the supervised clone path.
        # Outside jobs, invalid arguments must reach the kernel and yield EINVAL.
        'clone3.invalid': {errno.ENOSYS} if job else {errno.EINVAL},
    })
    if not job:
        # Image installation also checks in a chroot on a non-reflink rootfs.
        # Real jobs must succeed on their XFS scratch, not merely have ioctl.
        for name in ('ioctl.FICLONE', 'ioctl.FS_IOC_GETFLAGS'):
            expected[name] |= {errno.EOPNOTSUPP, errno.ENOTTY}
    return expected


def differences(actual, job=False):
    expected = expectations(job)
    errors = []
    if set(actual) != set(expected):
        errors.append('syscall probe names: expected ' + repr(sorted(expected))
                      + ', observed ' + repr(sorted(actual)))
    for name, allowed in expected.items():
        observed = actual.get(name, {}).get('errno')
        if observed not in allowed:
            labels = [errno.errorcode.get(value, 'OK') for value in sorted(allowed)]
            errors.append(f'syscall {name}: expected {labels}, observed errno {observed!r}')
    landlock = actual.get('landlock_create_ruleset.VERSION', {})
    if landlock.get('errno') == 0:
        abi = landlock.get('abi')
        if type(abi) is not int or abi < 1:
            errors.append(f'syscall landlock_create_ruleset.VERSION: expected ABI >= 1, observed ABI {abi!r}')
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='fail if the shared syscall contract is not met')
    parser.add_argument('--job', action='store_true', help='expect the common PID-init isolation policy and XFS scratch')
    args = parser.parse_args()
    status = Path('/proc/self/status').read_text().splitlines()
    actual = capture()
    errors = differences(actual, args.job) if args.check else []
    print(json.dumps({'kernel': platform.release(),
                      'seccomp': [line for line in status if line.startswith('Seccomp')],
                      'syscalls': actual, 'errors': errors, 'passed': not errors}, indent=2, sort_keys=True))
    return int(bool(errors))


if __name__ == '__main__':
    raise SystemExit(main())
