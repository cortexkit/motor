"""Destructive-to-job, not destructive-to-sandbox, disk exhaustion qualification."""
import errno
import json
import math
import uuid
from quota_inheritance import SOURCE as INHERITANCE, POSITIVE_CONTROL
from quota_adversarial import BASELINE_JOB, CAPACITY, DEEP_JOB, reflink_seed

RESERVE_BYTES = 24 * 1024**3  # Provisional until stop/resume headroom is measured.
LANES = ('tmp', 'home', 'var_tmp', 'output_stdout', 'workspace', 'target', 'reused_target', 'reflink_target')
RETAINED_LANE = 'retained_target'
# Filling a lane to the real cap writes about 12 GiB and takes a minute or more,
# so only the lanes whose proof needs the limit fill to it: the retained target
# (EDQUOT/ENOSPC at the job's own hard limit, the reserve at that limit, and a
# quota-sized target kept across stop/resume) and the reflink target (unique
# data written after released reflink copies stays inside the physical
# allowance). Every other lane writes CHARGE_BYTES and proves the kernel
# charged those bytes to the job's project, which is what makes that path
# subject to the limit the full lanes prove.
FULL_LANES = ('reflink_target', RETAINED_LANE)
CHARGE_BYTES = 256 * 1024**2  # A multiple of the 8 MiB write and 1 MiB fallocate steps.


def fill_bytes(lane):
    """Bytes the lane's job writes; 0 means until the kernel refuses the write."""
    return 0 if lane in FULL_LANES else CHARGE_BYTES


def job_seconds(lane, remaining):
    return min(1800, max(1, math.ceil(remaining))) if lane == RETAINED_LANE else 900

# Keep the fill alive until the host measures both filesystems. The barrier is
# allocated before filling: publishing the errno must not need another block.
JOB_SOURCE = r'''
import ctypes, errno, fcntl, json, os, pathlib, struct, sys, time
directory = pathlib.Path(os.environ['TMPDIR'])
marker = directory / 'boat-quota-at-limit.json'
target = pathlib.Path(os.environ['CARGO_TARGET_DIR'])
base = {'tmp': directory, 'home': pathlib.Path(os.environ['HOME']),
        'var_tmp': pathlib.Path('/var/tmp'), 'output_stdout': directory,
        'workspace': pathlib.Path.cwd(), 'target': target, 'reused_target': target,
        'reflink_target': target, 'retained_target': target}[QUOTA_LANE]
fill_directory, inheritance = verify_quota_inheritance(base)
fill = fill_directory / 'boat-quota-fill'
paths = {'workspace': pathlib.Path.cwd(), 'target': pathlib.Path(os.environ['CARGO_TARGET_DIR']),
         'home': pathlib.Path('/motor-home'), 'tmp': pathlib.Path('/tmp'), 'var_tmp': pathlib.Path('/var/tmp'),
         **{name: pathlib.Path('/motor-output') / name for name in ('stdout', 'stderr', 'exit', 'cache-stats')}}
projects = {}
def project(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        value = bytearray(28)
        fcntl.ioctl(fd, 0x801c581f, value, True)
        return struct.unpack_from('=I', value, 12)[0]
    finally:
        os.close(fd)
for name, path in paths.items():
    projects[name] = project(path)
assert set(projects.values()) == {inheritance['project']}, projects
baseline = target / 'boat-existing-baseline'
shared = target / 'boat-reflink-baseline'
baseline_bytes = 0
released_logical_bytes = 0
baseline_project_ids = []
copies = []
if QUOTA_LANE == 'reused_target':
    assert baseline.is_file(), 'reused baseline was unexpectedly evicted'
    baseline_bytes = baseline.stat().st_blocks * 512
    baseline_project_ids = [project(baseline)]
if QUOTA_LANE == 'reflink_target':
    assert shared.is_dir(), 'reflink baseline was unexpectedly evicted'
    baseline_project_ids = sorted({project(path) for path in shared.iterdir()})
    copies = [path for path in shared.iterdir() if path.name != 'source']
    released_logical_bytes = sum(path.stat().st_blocks * 512 for path in copies)
    baseline_bytes = released_logical_bytes + (shared / 'source').stat().st_blocks * 512
assert not baseline_project_ids or set(baseline_project_ids) == {inheritance['project']}, baseline_project_ids
libc = ctypes.CDLL(None, use_errno=True)
libc.fallocate.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_longlong, ctypes.c_longlong]
libc.fallocate.restype = ctypes.c_int
output_fd = None
with marker.open('w+b', buffering=0) as barrier:
    barrier.write(b' ' * 4096)
    os.fsync(barrier.fileno())
    def publish(report):
        encoded = json.dumps(report).encode()
        assert len(encoded) <= 4096
        barrier.seek(0)
        barrier.write(encoded.ljust(4096, b' '))
        os.fsync(barrier.fileno())
    def acknowledged(seconds=90):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            barrier.seek(0)
            if barrier.read(1) == b'!': return True
            time.sleep(.2)
        return False
    publish({'phase': 'before-fill', 'lane': QUOTA_LANE, 'probe_token': QUOTA_PROBE_TOKEN})
    assert acknowledged(), 'root observer never captured pre-fill headroom'
    barrier.seek(0); barrier.write(b' ' * 4096); os.fsync(barrier.fileno())
    for path in copies: path.unlink()
    observed = None
    written = 0
    def more():
        # QUOTA_FILL_BYTES 0 fills until the quota refuses a write.
        return written < QUOTA_FILL_BYTES if QUOTA_FILL_BYTES else written <= 32 * 1024**3
    try:
        if QUOTA_LANE == 'output_stdout':
            output_fd = os.open('/motor-output/stdout', os.O_RDWR)
            while more():
                # KEEP_SIZE allocates this existing inode without streaming tens
                # of GiB through the client. It exercises the same block quota.
                if libc.fallocate(output_fd, 1, written, 1024 * 1024) != 0:
                    raise OSError(ctypes.get_errno(), 'stdout fallocate KEEP_SIZE')
                written += 1024 * 1024
                os.fsync(output_fd)
        else:
            with fill.open('wb', buffering=0) as output:
                while more():
                    count = output.write(os.urandom(8 * 1024 * 1024))
                    if not count: raise OSError(errno.EIO, 'zero-length quota probe write')
                    written += count
                    os.fsync(output.fileno())
    except OSError as error:
        observed = error.errno
    # 'at-limit' marks the end of the fill: at the quota, or at QUOTA_FILL_BYTES.
    report = {'phase': 'at-limit', 'lane': QUOTA_LANE, 'errno': observed, 'bytes_written': written,
              'fill_target_bytes': QUOTA_FILL_BYTES, 'inheritance': inheritance, 'projects': projects,
              'baseline_bytes': baseline_bytes, 'baseline_project_ids': baseline_project_ids, 'released_logical_bytes': released_logical_bytes,
              'fill_relative': str(fill.relative_to(base)), 'probe_token': QUOTA_PROBE_TOKEN}
    if output_fd is not None:
        metadata = os.fstat(output_fd)
        report.update(output_allocated_bytes=metadata.st_blocks * 512, output_size_bytes=metadata.st_size)
    publish(report)
    if not acknowledged(QUOTA_HOLD_SECONDS if QUOTA_LANE == 'retained_target' else 90):
        report['observer_timeout'] = True
    if output_fd is not None:
        # Punch only the probe's unwritten extents; stdout still has size zero.
        if libc.fallocate(output_fd, 3, 0, written + 1024 * 1024) != 0:
            raise OSError(ctypes.get_errno(), 'stdout cleanup hole punch')
        os.close(output_fd)
    if QUOTA_LANE != 'retained_target': fill.unlink(missing_ok=True)
    if QUOTA_LANE == 'reused_target': baseline.unlink()
    if QUOTA_LANE == 'reflink_target': (shared / 'source').unlink(); shared.rmdir()
    marker.unlink()
print(json.dumps(report), flush=True)
if QUOTA_FILL_BYTES:
    sys.exit(0 if observed is None and written == QUOTA_FILL_BYTES else 74)
sys.exit(73 if observed in (errno.EDQUOT, errno.ENOSPC) else 74)
'''


# How much less than the retained target's allocation the backing file may give
# back and still count as having returned it: XFS metadata, the log and the
# first resumed job's own writes land in the same file.
RECLAIM_SLACK_BYTES = 1024**3


def backing_allocation_dropped(before, after, retained_bytes):
    """Whether the sparse backing file returned the evicted retained target's
    blocks to the root disk. Any smaller allocation is not enough: in the run
    recorded in fixtures/boat-verify-c7c6127f.json it shrank by 2 MiB while the
    12.9 GB target was still in place, which is no evidence of eviction or trim."""
    # Boat can recreate the backing inode on restore; compare the verified
    # backing path and allocated sectors, not inode numbers across snapshots.
    if not (before.get('expected_backing') is True and after.get('expected_backing') is True
            and before.get('backing_file') == after.get('backing_file') == '/var/lib/motor-boat.xfs'
            and type(before.get('backing_allocated_blocks')) is int
            and type(after.get('backing_allocated_blocks')) is int
            and type(retained_bytes) is int and retained_bytes > RECLAIM_SLACK_BYTES
            and after['backing_allocated_blocks'] >= 0):
        return False
    dropped = (before['backing_allocated_blocks'] - after['backing_allocated_blocks']) * 512
    return dropped >= retained_bytes - RECLAIM_SLACK_BYTES


def retained_release_passed(report):
    """The retained job read its release and ended, so its workspace is idle."""
    return (isinstance(report, dict) and report.get('released') is True
            and report.get('job_terminal') is True and report.get('unit_active') is False)


def stop_progress(samples, operation=None):
    """Summarize what Boat did with a stop that has not completed, from the
    sandbox states sampled while waiting, so a timeout names Boat's own
    progress instead of only its last state.

    `classification` is 'boat_stop_never_attempted' when every sample shows
    the stop pending with no attempt (stop.lastAttemptAt null) and the sandbox
    archiving with health ok, 'boat_stop_attempted_not_completed' when Boat did
    attempt it, 'boat_stop_pending_unclassified' otherwise, and 'unobserved'
    without samples. Snapshot times are Boat's
    own: `snapshots_completed_during_wait` lists snapshotCompletedAt values
    that first appeared after the first sample, and
    `snapshot_attempts_after_request` the lastSnapshotAttemptAt values later
    than the stop request."""
    states = [sample.get('sandbox') or {} for sample in samples]
    if not states:
        return {'classification': 'unobserved', 'samples': 0}
    stops = [state.get('stop') or {} for state in states]
    requested = (operation or {}).get('requestedAt') or next(
        (stop.get('requestedAt') for stop in stops if stop.get('requestedAt')), None)
    attempted = [stop.get('lastAttemptAt') for stop in stops if stop.get('lastAttemptAt')]
    completed, attempts = [], []
    for state in states:
        done, began = state.get('snapshotCompletedAt'), state.get('lastSnapshotAttemptAt')
        if done and done != states[0].get('snapshotCompletedAt') and done not in completed:
            completed.append(done)
        # ISO-8601 UTC strings in one format compare in time order.
        if began and requested and began > requested and began not in attempts:
            attempts.append(began)
    unhealthy = sorted({state.get('health') for state in states if state.get('health') != 'ok'}, key=str)
    last = states[-1]
    if attempted:
        classification = 'boat_stop_attempted_not_completed'
    elif (all(state.get('state') == 'archiving' for state in states)
          and all(stop.get('status') == 'pending' for stop in stops) and not unhealthy):
        classification = 'boat_stop_never_attempted'
    else:
        classification = 'boat_stop_pending_unclassified'
    return {'classification': classification, 'samples': len(states), 'stop_id': (operation or stops[0]).get('id'),
            'requested_at': requested, 'stop_attempts': attempted, 'states': sorted({str(s.get('state')) for s in states}),
            'unhealthy': unhealthy, 'snapshots_completed_during_wait': completed,
            'snapshot_attempts_after_request': attempts, 'last_snapshot_attempt_at': last.get('lastSnapshotAttemptAt'),
            'last_snapshot_status': last.get('lastSnapshotStatus')}


def stop_progress_reason(progress, wait):
    """One readable sentence naming Boat's stop progress, for a failure reason."""
    seconds = wait.get('seconds')
    limit = ('the phase budget ended the wait' if wait.get('budget_limited')
             else 'the requested wait ended') + (f" after {seconds:.0f} s" if isinstance(seconds, (int, float)) else '')
    if progress['classification'] == 'boat_stop_never_attempted':
        head = (f"boat_stop_never_attempted: Boat accepted stop {progress['stop_id']} at {progress['requested_at']} "
                f"and never attempted it (stop.lastAttemptAt null and state archiving, health ok, in all "
                f"{progress['samples']} samples)")
    elif progress['classification'] == 'boat_stop_attempted_not_completed':
        head = (f"boat_stop_attempted_not_completed: Boat attempted stop {progress['stop_id']} at "
                f"{progress['stop_attempts']} without completing it")
    else:
        head = f"{progress['classification']}: stop {progress.get('stop_id')} stayed pending (states {progress.get('states')})"
    return (f"{head}; meanwhile Boat completed snapshots at {progress.get('snapshots_completed_during_wait')} and began "
            f"snapshot attempts at {progress.get('snapshot_attempts_after_request')}, the last "
            f"({progress.get('last_snapshot_attempt_at')}) {progress.get('last_snapshot_status')}; {limit}")


def resume_eviction_checks(before, after, ready):
    roots = before.get('lease', {}).get('roots', [])
    floor = RESERVE_BYTES + (64 << 20) + (8 << 30)
    return {
        'retained_workspace_was_present': before.get('retained_allocated_bytes', 0) > 0 and bool(roots),
        'idle_workspace_evicted_before_jobs': bool(roots) and after.get('workspace') == roots[0]
            and after.get('workspace_absent') is True and after.get('eviction_logged') is True,
        'cold_capacity_and_readiness_recovered': ready is True
            and min(after.get('root_free_bytes', 0), after.get('xfs_free_bytes', 0)) >= floor,
    }


def job(lane, token=None, hold_seconds=90):
    if lane not in LANES + (RETAINED_LANE,):
        raise ValueError('unknown quota probe lane')
    token = token or uuid.uuid4().hex
    return ('QUOTA_LANE=' + repr(lane) + '\nQUOTA_PROBE_TOKEN=' + repr(token) + '\nQUOTA_FILL_BYTES='
            + repr(fill_bytes(lane)) + '\nQUOTA_HOLD_SECONDS=' + repr(hold_seconds) + '\n' + INHERITANCE + JOB_SOURCE)

OBSERVER = r'''
import ctypes, json, os, pathlib, struct, subprocess, time
state = pathlib.Path('/var/lib/motor-boat/state/boat')
deadline = time.monotonic() + 900
started = time.monotonic()
before = {}
seen_request = False
def free(path):
    value = os.statvfs(path)
    return value.f_bavail * value.f_frsize
def kernel_usage(lease):
    # Boat's Ubuntu 24.04 ships glibc 2.39, which has no quotactl_fd wrapper,
    # so call the system call by number (443 on x86-64), as the runner does.
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    data = ctypes.create_string_buffer(112)
    fd = os.open('/var/lib/motor-boat', os.O_RDONLY | os.O_DIRECTORY)
    try:
        if libc.syscall(ctypes.c_long(443), ctypes.c_int(fd), ctypes.c_uint((0x5803 << 8) | 2),
                        ctypes.c_uint(lease['project']), ctypes.byref(data)) != 0:
            raise OSError(ctypes.get_errno(), 'Q_XGETQUOTA')
        return dict(hard_bytes=struct.unpack_from('=Q', data, 8)[0] * 512,
                    used_bytes=struct.unpack_from('=Q', data, 40)[0] * 512)
    finally:
        os.close(fd)
while time.monotonic() < deadline:
    if min(free('/'), free('/var/lib/motor-boat')) < 24 * 1024**3 - 64 * 1024**2:
        subprocess.run(['systemctl', 'kill', '--signal=KILL', 'motor-job-*.service'], check=True)
        print(json.dumps(dict(safety_stop=True, root_free_bytes=free('/'), xfs_free_bytes=free('/var/lib/motor-boat'))))
        raise SystemExit(0)
    for marker in state.glob('sandboxes/*/home/tmp/boat-quota-at-limit.json'):
        try:
            report = json.loads(marker.read_text())
        except (ValueError, FileNotFoundError):
            continue
        if report.get('probe_token') != EXPECTED_TOKEN:
            continue
        if report.get('phase') == 'before-fill':
            before = dict(root_free_bytes=free('/'), xfs_free_bytes=free('/var/lib/motor-boat'))
            before['physical_allowance_bytes'] = max(0, min(32 * 1024**3, min(before.values()) - 24 * 1024**3 - 64 * 1024**2))
            # The project's charge before any fill byte, so a charge-only lane
            # can show its own writes were added to the job's quota.
            try:
                before['kernel_usage'] = kernel_usage(json.loads((state / 'boat-quotas' / (marker.parts[-4] + '.json')).read_text()))
            except (OSError, ValueError, KeyError) as error:
                before['kernel_usage_error'] = str(error)
            with marker.open('r+b', buffering=0) as output:
                output.write(b'!'); os.fsync(output.fileno())
            continue
        if report.get('phase') != 'at-limit':
            continue
        lease = json.loads((state / 'boat-quotas' / (marker.parts[-4] + '.json')).read_text())
        report.update(root_free_bytes=free('/'), xfs_free_bytes=free('/var/lib/motor-boat'), lease=lease,
                      before=before, job_id=marker.parts[-4], kernel_usage=kernel_usage(lease))
        if report.get('lane') in ('workspace', 'target', 'reused_target', 'reflink_target', 'retained_target'):
            relative = pathlib.PurePosixPath(report['fill_relative'])
            assert not relative.is_absolute() and '..' not in relative.parts
            root = pathlib.Path(lease['roots'][0 if report['lane'] == 'workspace' else 1])
            path = root.joinpath(*relative.parts)
            metadata = path.lstat()
            report.update(host_fill_path=str(path), retained_allocated_bytes=metadata.st_blocks * 512)
        hit = state / 'boat-quotas' / (marker.parts[-4] + '.hit')
        # A charge-only fill stops below the limit, so no warning can arrive.
        warning_deadline = time.monotonic() + (0 if report.get('fill_target_bytes') else 5)
        while not hit.exists() and time.monotonic() < warning_deadline:
            time.sleep(.1)
        report['kernel_hard_limit_warning'] = hit.exists()
        if not HOLD_AT_LIMIT:
            with marker.open('r+b', buffering=0) as output:
                output.write(b'!')
                os.fsync(output.fileno())
        print(json.dumps(report))
        raise SystemExit(0)
    for path in (state / 'jobs').glob('*/record.json'):
        try:
            record = json.loads(path.read_text())
        except (ValueError, FileNotFoundError):
            continue
        if EXPECTED_TOKEN not in (record.get('request') or {}).get('command', ''):
            continue
        seen_request = True
        if record.get('terminal') is not None:
            print(json.dumps(dict(job_ended_before_limit=True, terminal=record['terminal'])))
            raise SystemExit(0)
    if not seen_request and time.monotonic() - started > 30:
        print(json.dumps(dict(job_not_accepted=True)))
        raise SystemExit(0)
    time.sleep(.2)
raise SystemExit('quota job never published its write failure')
'''


def observer(token, hold=False):
    return 'EXPECTED_TOKEN=' + repr(token) + '\nHOLD_AT_LIMIT=' + repr(hold) + '\n' + OBSERVER

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


def checks(result, observation, cleanup, charge_bytes=0):
    """Checks for one ordinary lane. charge_bytes 0 is a lane filled until the
    quota refused a write; otherwise the lane wrote exactly charge_bytes and must
    show them charged to the job's project (see FULL_LANES)."""
    try:
        report = json.loads(result.get('stdout', ''))
    except (ValueError, TypeError):
        report = {}
    lease = observation.get('lease', {})
    inheritance = report.get('inheritance', {})
    projects = report.get('projects', {})
    hard_bytes = lease.get('blocks', 0) * 512
    initial_bytes = lease.get('initial_blocks', 0) * 512
    kernel = observation.get('kernel_usage', {})
    physical_allowance = observation.get('before', {}).get('physical_allowance_bytes', -1)
    common = {
        'total_cap_bound': hard_bytes > 0 and physical_allowance >= 0 and hard_bytes <= physical_allowance + 16 * 1024**2,
        'inheritance_escape_denied': inheritance.get('project', 0) == lease.get('project', -1)
                                     and inheritance.get('child_project') == lease.get('project', -1)
                                     and inheritance.get('setters_refused') == [0x401c5820, 0x40086602, 0x40046602],
        'all_writable_paths_charged': set(projects) == {'workspace', 'target', 'home', 'tmp', 'var_tmp', 'stdout', 'stderr', 'exit', 'cache-stats'}
                                      and set(projects.values()) == {lease.get('project', -1)},
        'output_inode_was_charged': report.get('lane') != 'output_stdout' or (
            report.get('output_allocated_bytes', 0) >= (charge_bytes or max(1024**2, lease.get('growth_bytes', 0) - 16 * 1024**2))
            and report.get('output_size_bytes') == 0),
        'baseline_consumes_cap': report.get('lane') not in ('reused_target', 'reflink_target') or (
            report.get('baseline_bytes', 0) >= 64 * 1024**2
            and initial_bytes >= report.get('baseline_bytes', 0)
            and initial_bytes + lease.get('growth_bytes', 0) == hard_bytes
            and lease.get('growth_bytes', 0) < physical_allowance
            and report.get('baseline_project_ids') == [lease.get('project')]),
        'cleanup_ready': cleanup.get('ready') is True and cleanup.get('leases') == [],
        'reserve_after_cleanup': all(cleanup.get(key, -1) >= RESERVE_BYTES for key in ('root_free_bytes', 'xfs_free_bytes')),
    }
    if charge_bytes:
        before = observation.get('before', {}).get('kernel_usage', {})
        charged = kernel.get('used_bytes', 0) - before.get('used_bytes', kernel.get('used_bytes', 0))
        return {
            'charge_fill_completed': result.get('code') == 0 and report.get('errno') is None
                                     and report.get('fill_target_bytes') == charge_bytes
                                     and report.get('bytes_written') == charge_bytes
                                     and observation.get('bytes_written') == charge_bytes
                                     and not report.get('observer_timeout'),
            # The kernel, not the job, reports that this path's bytes count
            # against the job's own hard limit.
            'fill_charged_to_job_project': hard_bytes > 0 and kernel.get('hard_bytes') == hard_bytes
                                           and 'used_bytes' in before
                                           and charged >= charge_bytes - 16 * 1024**2,
            **common,
        }
    return {
        'job_write_failed': result.get('code') == 73 and report.get('errno') in (errno.EDQUOT, errno.ENOSPC)
                            and report.get('errno') == observation.get('errno') and not report.get('observer_timeout'),
        # A tiny unrelated failure is not evidence that the quota was reached.
        'filled_real_quota': lease.get('growth_bytes', 0) > 0
                             and observation.get('bytes_written', 0) >= max(1024**2, lease.get('growth_bytes', 0) - 16 * 1024**2)
                             and kernel.get('hard_bytes') == hard_bytes
                             and kernel.get('used_bytes', 0) >= hard_bytes - 16 * 1024**2,
        **common,
        'reflink_physical_bound': report.get('lane') != 'reflink_target' or reflink_bound(observation),
        'reserve_at_limit': all(observation.get(key, -1) >= RESERVE_BYTES for key in ('root_free_bytes', 'xfs_free_bytes')),
    }


def reflink_bound(observation):
    lease = observation.get('lease', {})
    before = observation.get('before', {})
    allowance = before.get('physical_allowance_bytes', -1)
    remaining_baseline = max(0, lease.get('initial_blocks', 0) * 512 - observation.get('released_logical_bytes', 0))
    return (observation.get('released_logical_bytes', 0) >= 64 * 1024**2
            and observation.get('bytes_written', 0) >= max(1024**2, lease.get('blocks', 0) * 512 - remaining_baseline - 16 * 1024**2)
            and 0 < observation.get('retained_allocated_bytes', 0) <= allowance
            and all(before.get(key, -1) - observation.get(key, -1) <= allowance + 64 * 1024**2
                    for key in ('root_free_bytes', 'xfs_free_bytes')))


def retained_checks(observation):
    lease = observation.get('lease', {})
    kernel = observation.get('kernel_usage', {})
    hard = lease.get('blocks', 0) * 512
    return (observation.get('errno') in (errno.EDQUOT, errno.ENOSPC)
            and hard > 0 and kernel.get('hard_bytes') == hard
            and hard <= observation.get('before', {}).get('physical_allowance_bytes', -1) + 16 * 1024**2
            and kernel.get('used_bytes', 0) >= hard - 16 * 1024**2
            and observation.get('retained_allocated_bytes', 0) >= max(1024**2, lease.get('growth_bytes', 0) - 16 * 1024**2)
            and all(observation.get(key, -1) >= RESERVE_BYTES for key in ('root_free_bytes', 'xfs_free_bytes')))


def positive_control_passed(report):
    return (report.get('request') == 0x401c5820 and report.get('clear_succeeded') is True
            and bool(report.get('before_flags', 0) & 0x200) and not (report.get('after_flags', 0) & 0x200)
            and isinstance(report.get('before_project'), int)
            and report.get('before_project') == report.get('after_project'))


def retained_hold_status(samples, pre_stop=None):
    """Missing live observations cannot qualify a stop as retaining a live, quota-sized target."""
    observed = (isinstance(pre_stop, dict) and pre_stop.get('job_running') is True
                and type(pre_stop.get('allocated_bytes')) is int and pre_stop['allocated_bytes'] > 0)
    gap = False
    for sample in samples:
        state = sample.get('sandbox') or {}
        if state.get('state') == 'archived' and (state.get('stop') or {}).get('status') == 'completed':
            continue
        guest = sample.get('retained_job') or {}
        if guest.get('status') != 'observed':
            gap = True
            continue
        observed = True
        if guest.get('job_running') is False or guest.get('retained_target_at_limit') is False:
            return 'FAIL'
        if guest.get('job_running') is not True or guest.get('retained_target_at_limit') is not True:
            gap = True
    return 'INCONCLUSIVE' if gap or not observed else 'PASS'


def file_setattr_status(result, control):
    """PASS, INCONCLUSIVE or FAIL for the job's file_setattr refusal.

    Judged only from the root control and the job's inheritance report. The
    fill that follows the inheritance probe is checked elsewhere: a lane that
    writes a partial fill exits 0 with errno null, a full lane exits 73 with
    EDQUOT/ENOSPC, and neither outcome says anything about this syscall."""
    try:
        report = json.loads(result.get('stdout', ''))
    except (ValueError, TypeError):
        return 'FAIL'
    inheritance = report.get('inheritance') if isinstance(report, dict) else None
    if not isinstance(inheritance, dict):
        return 'FAIL'
    # type() rejects True, which equals 1 and would otherwise pass as an errno.
    refused = (type(inheritance.get('file_setattr_errno')) is int
               and inheritance['file_setattr_errno'] == errno.ENOSYS
               and inheritance.get('setters_refused') == [0x401c5820, 0x40086602, 0x40046602])
    if not refused or not positive_control_passed(control):
        return 'FAIL'
    if control.get('file_setattr_supported') is True and control.get('file_setattr_errno') == 0:
        return 'PASS'
    if control.get('file_setattr_supported') is False and control.get('file_setattr_errno') == errno.ENOSYS:
        return 'INCONCLUSIVE'
    return 'FAIL'
