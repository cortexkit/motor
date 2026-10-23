"""Destructive-to-job, not destructive-to-sandbox, disk exhaustion qualification."""
import errno
import json
from quota_inheritance import SOURCE as INHERITANCE

RESERVE_BYTES = 24 * 1024**3  # Provisional until stop/resume headroom is measured.

# Keep the fill alive until the host measures both filesystems. The barrier is
# allocated before filling: publishing the errno must not need another block.
JOB = INHERITANCE + r'''
import errno, json, os, pathlib, sys, time
directory = pathlib.Path(os.environ['TMPDIR'])
marker = directory / 'boat-quota-at-limit.json'
fill = directory / 'boat-quota-fill'
fill_directory, inheritance = verify_quota_inheritance(directory)
fill = fill_directory / 'boat-quota-fill'
with marker.open('w+b', buffering=0) as barrier:
    barrier.write(b' ' * 4096)
    os.fsync(barrier.fileno())
    observed = None
    written = 0
    try:
        with fill.open('wb', buffering=0) as output:
            while written <= 32 * 1024**3:
                output.write(os.urandom(1024 * 1024))
                written += 1024 * 1024
                os.fsync(output.fileno())
    except OSError as error:
        observed = error.errno
    report = {'errno': observed, 'bytes_written': written, 'inheritance': inheritance}
    encoded = json.dumps(report).encode()
    barrier.seek(0)
    barrier.write(encoded.ljust(4096, b' '))
    os.fsync(barrier.fileno())
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        barrier.seek(0)
        if barrier.read(1) == b'!':
            break
        time.sleep(.2)
    else:
        report['observer_timeout'] = True
    fill.unlink(missing_ok=True)
    marker.unlink()
print(json.dumps(report), flush=True)
sys.exit(73 if observed in (errno.EDQUOT, errno.ENOSPC) else 74)
'''

OBSERVER = r'''
import json, os, pathlib, subprocess, time
state = pathlib.Path('/var/lib/motor-boat/state/boat')
deadline = time.monotonic() + 900
def free(path):
    value = os.statvfs(path)
    return value.f_bavail * value.f_frsize
while time.monotonic() < deadline:
    if free('/') < 24 * 1024**3 - 64 * 1024**2:
        subprocess.run(['systemctl', 'kill', '--signal=KILL', 'motor-job-*.service'], check=True)
        print(json.dumps(dict(safety_stop=True, root_free_bytes=free('/'), xfs_free_bytes=free('/var/lib/motor-boat'))))
        raise SystemExit(0)
    for marker in state.glob('sandboxes/*/home/tmp/boat-quota-at-limit.json'):
        try:
            report = json.loads(marker.read_text())
        except (ValueError, FileNotFoundError):
            continue
        lease = json.loads((state / 'boat-quotas' / (marker.parts[-4] + '.json')).read_text())
        report.update(root_free_bytes=free('/'), xfs_free_bytes=free('/var/lib/motor-boat'), lease=lease)
        hit = state / 'boat-quotas' / (marker.parts[-4] + '.hit')
        warning_deadline = time.monotonic() + 5
        while not hit.exists() and time.monotonic() < warning_deadline:
            time.sleep(.1)
        report['kernel_hard_limit_warning'] = hit.exists()
        with marker.open('r+b', buffering=0) as output:
            output.write(b'!')
            os.fsync(output.fileno())
        print(json.dumps(report))
        raise SystemExit(0)
    time.sleep(.2)
raise SystemExit('quota job never published its write failure')
'''

CLEANUP = r'''
import json, os, pathlib, time
state = pathlib.Path('/var/lib/motor-boat/state/boat')
deadline = time.monotonic() + 90
while time.monotonic() < deadline:
    leases = list((state/'boat-quotas').glob('*.json'))
    ready = pathlib.Path('/run/motor/endpoint-ready').exists()
    if not leases and ready:
        def free(path):
            value = os.statvfs(path)
            return value.f_bavail * value.f_frsize
        print(json.dumps(dict(ready=ready, leases=[], root_free_bytes=free('/'), xfs_free_bytes=free('/var/lib/motor-boat'))))
        break
    time.sleep(.2)
else:
    raise SystemExit('quota cleanup/readiness did not settle')
'''


def checks(result, observation, cleanup):
    try:
        report = json.loads(result.get('stdout', ''))
    except (ValueError, TypeError):
        report = {}
    lease = observation.get('lease', {})
    inheritance = report.get('inheritance', {})
    return {
        'job_write_failed': result.get('code') == 73 and report.get('errno') in (errno.EDQUOT, errno.ENOSPC)
                            and report.get('errno') == observation.get('errno') and not report.get('observer_timeout'),
        # A tiny unrelated failure is not evidence that the quota was reached.
        'filled_real_quota': lease.get('growth_bytes', 0) > 0
                             and observation.get('bytes_written', 0) >= max(1024**2, lease.get('growth_bytes', 0) - 16 * 1024**2)
                             and observation.get('kernel_hard_limit_warning') is True,
        'inheritance_escape_denied': inheritance.get('project', 0) == lease.get('project', -1)
                                     and inheritance.get('child_project') == lease.get('project', -1)
                                     and inheritance.get('setters_refused') == [0x401c5820, 0x40086602, 0x40046602],
        'reserve_at_limit': all(observation.get(key, -1) >= RESERVE_BYTES for key in ('root_free_bytes', 'xfs_free_bytes')),
        'cleanup_ready': cleanup.get('ready') is True and cleanup.get('leases') == [],
        'reserve_after_cleanup': all(cleanup.get(key, -1) >= RESERVE_BYTES for key in ('root_free_bytes', 'xfs_free_bytes')),
    }
