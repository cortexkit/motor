"""Job programs for retained baseline and hostile-tree quota qualification."""

DEEP_JOB = r'''
import json, os
os.chdir(os.environ['TMPDIR'])
for _ in range(2200):
    os.mkdir('d')
    os.chdir('d')
with open('leaf', 'wb') as output:
    output.write(b'deep-tree')
print(json.dumps(dict(depth=2200)), flush=True)
'''

BASELINE_JOB = r'''
import os, pathlib
path = pathlib.Path(os.environ['CARGO_TARGET_DIR']) / 'boat-existing-baseline'
with path.open('wb', buffering=0) as output:
    data = os.urandom(1024 * 1024)
    for _ in range(64):
        output.write(data)
    os.fsync(output.fileno())
print('baseline-seeded', flush=True)
'''

CAPACITY = r'''
import json, os
def free(path):
    value = os.statvfs(path)
    return value.f_bavail * value.f_frsize
cap = min(32 * 1024**3, min(free('/'), free('/var/lib/motor-boat')) - 24 * 1024**3 - 64 * 1024**2)
print(json.dumps(dict(cap_bytes=cap, baseline_bytes=max(0, cap - 8 * 1024**3 - 256 * 1024**2))))
'''

REFLINK_JOB = r'''
import fcntl, json, os, pathlib
root = pathlib.Path(os.environ['CARGO_TARGET_DIR']) / 'boat-reflink-baseline'
root.mkdir()
source = root / 'source'
with source.open('w+b', buffering=0) as input:
    input.write(os.urandom(8 * 1024**2))
    os.fsync(input.fileno())
    count = max(1, BASELINE_BYTES // (8 * 1024**2))
    for index in range(count - 1):
        with (root / ('copy-' + str(index))).open('w+b', buffering=0) as output:
            fcntl.ioctl(output.fileno(), 0x40049409, input.fileno())
            os.fsync(output.fileno())
logical = sum(path.stat().st_blocks * 512 for path in root.iterdir())
print(json.dumps(dict(copies=count, logical_bytes=logical, source_bytes=source.stat().st_blocks * 512)), flush=True)
'''


def reflink_seed(baseline_bytes):
    if type(baseline_bytes) is not int or not (64 * 1024**2 <= baseline_bytes <= 24 * 1024**3):
        raise ValueError('insufficient headroom for a meaningful retained-reflink baseline')
    return 'BASELINE_BYTES=' + str(baseline_bytes) + '\n' + REFLINK_JOB


def abort_source(token):
    return 'TOKEN=' + repr(token) + '\n' + r'''
import ctypes, json, os, pathlib, subprocess, uuid
state = pathlib.Path('/var/lib/motor-boat/state/boat')
# This disposable sandbox is being abandoned after a failed qualification.
# Prevent a pending setup from starting another writer during resource cleanup.
subprocess.run(['systemctl', 'stop', '--no-block', 'motor-boat.service'], check=False)
subprocess.run(['systemctl', 'kill', '--signal=KILL', 'motor-boat.service', 'motor-job-*.service'], check=False)
removed = []
for record_path in (state / 'jobs').glob('*/record.json'):
    try:
        record = json.loads(record_path.read_text())
    except (ValueError, FileNotFoundError):
        continue
    request = record.get('request') or {}
    if TOKEN not in request.get('command', ''):
        continue
    runtime = state / 'sandboxes' / str(uuid.UUID(record_path.parent.name))
    workspace = pathlib.Path(request.get('workspace_key', '/'))
    roots = [runtime / 'home', runtime / 'home/tmp', runtime / 'tmp']
    if str(workspace).startswith('/Users/'):
        roots.extend((workspace, workspace / 'target'))
    for root in roots:
        try:
            owner = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        except (FileNotFoundError, NotADirectoryError):
            continue
        try:
            for name in os.listdir(owner):
                if not name.startswith('quota-inheritance-child-'):
                    continue
                try:
                    fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=owner)
                except (FileNotFoundError, NotADirectoryError):
                    continue
                try:
                    os.unlink('boat-quota-fill', dir_fd=fd)
                    removed.append(str(root / name / 'boat-quota-fill'))
                except FileNotFoundError:
                    pass
                finally:
                    os.close(fd)
        finally:
            os.close(owner)
    try:
        output = os.open(runtime / 'output/stdout', os.O_RDWR | os.O_NOFOLLOW)
    except FileNotFoundError:
        continue
    try:
        libc = ctypes.CDLL(None, use_errno=True)
        libc.fallocate.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_longlong, ctypes.c_longlong]
        libc.fallocate(output, 3, 0, 64 * 1024**3)
    finally:
        os.close(output)
os.sync()
print(json.dumps(dict(removed_probe_fills=removed)))
'''


def retained_guard(observation):
    return 'OBSERVATION=' + repr(observation) + '\n' + r'''
import json, os, pathlib, stat, subprocess, uuid
job_id = str(uuid.UUID(OBSERVATION['job_id']))
state = pathlib.Path('/var/lib/motor-boat/state/boat')
job = json.loads((state/'jobs'/job_id/'record.json').read_text())
assert job.get('terminal') is None and job.get('started_ms') is not None, 'retained job already ended'
unit = str(uuid.UUID(job['process']['unit']))
assert subprocess.run(['systemctl','is-active','--quiet','motor-job-' + unit + '.service']).returncode == 0, 'retained unit is not active'
path = pathlib.Path(OBSERVATION['host_fill_path'])
metadata = path.lstat()
assert stat.S_ISREG(metadata.st_mode), 'retained fill was replaced'
assert metadata.st_blocks * 512 >= max(1024**2, OBSERVATION['lease']['growth_bytes'] - 16 * 1024**2)
def free(path):
    value = os.statvfs(path)
    return value.f_bavail * value.f_frsize
assert min(free('/'), free('/var/lib/motor-boat')) >= 24 * 1024**3
print(json.dumps(dict(job_running=True, allocated_bytes=metadata.st_blocks*512, root_free_bytes=free('/'), xfs_free_bytes=free('/var/lib/motor-boat'))))
'''


def resume_eviction_source(workspace, cursor):
    return 'WORKSPACE=' + repr(workspace) + '\nCURSOR=' + repr(cursor) + '\n' + r'''
import json, os, re, subprocess
result = subprocess.run(['journalctl', '--unit=motor-boat.service', '--after-cursor=' + CURSOR,
                         '--no-pager', '--output=json'], check=True, capture_output=True, text=True)
events = []
path_field = re.compile(r'\bpath=(?:"' + re.escape(WORKSPACE) + r'"|' + re.escape(WORKSPACE) + r')(?:\s|$)')
for line in result.stdout.splitlines():
    message = re.sub(r'\x1b\[[0-9;]*m', '', json.loads(line).get('MESSAGE', ''))
    if 'runner disk eviction' in message and path_field.search(message):
        events.append(message)
def free(path):
    value = os.statvfs(path)
    return value.f_bavail * value.f_frsize
print(json.dumps(dict(workspace=WORKSPACE, workspace_absent=not os.path.lexists(WORKSPACE),
                     eviction_logged=any('runner disk eviction failed' not in event for event in events),
                     events=events, root_free_bytes=free('/'), xfs_free_bytes=free('/var/lib/motor-boat'))))
'''
