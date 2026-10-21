#!/usr/bin/env python3
"""Measure fresh large/xlarge usable resources, sequentially, without installs.

<key reader> | python3.14 script/boat-probe/resources.py --key-stdin --output DIR
"""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import shlex
import signal
import time
import uuid

from probe import Client, Probe, READY, cleanup, interrupt, read_key, sandbox, save, scrub

GUEST = r'''
import json, os, pathlib, subprocess
root = pathlib.Path('/sys/fs/cgroup')
relative = pathlib.Path('/proc/self/cgroup').read_text().strip().split('0::',1)[1].lstrip('/')
path = root / relative
ancestors = []
while True:
    ancestors.append({'path': str(path), **{name: (path/name).read_text().strip() if (path/name).exists() else None for name in (
        'cgroup.controllers', 'cgroup.subtree_control', 'cpuset.cpus.effective', 'cpu.max', 'memory.max', 'memory.current')}})
    if path == root:
        break
    path = path.parent
print(json.dumps({'nproc': subprocess.check_output(['nproc'], text=True).strip(),
    'nproc_all': subprocess.check_output(['nproc','--all'], text=True).strip(),
    'affinity': sorted(os.sched_getaffinity(0)), 'cgroup_membership': pathlib.Path('/proc/self/cgroup').read_text(),
    'cgroup_ancestors': ancestors, 'meminfo': pathlib.Path('/proc/meminfo').read_text(),
    'uptime': pathlib.Path('/proc/uptime').read_text(),
    'runner_idle': {'installed': False, 'bytes': None, 'reason': 'fresh-resource mode does not install the runner or services'}}))
'''


def parse(status):
    if status.get('exitCode') != 0 or status.get('stdoutTruncated') or status.get('stderrTruncated'):
        raise ValueError('resource command failed or was truncated: ' + json.dumps(status))
    data = json.loads(status['stdout'])
    if not data.get('cgroup_ancestors') or int(data['nproc']) < 1 or not data.get('affinity'):
        raise ValueError('missing CPU evidence')
    mem = dict((line.split(':', 1)[0], line.split(':', 1)[1].strip()) for line in data['meminfo'].splitlines() if ':' in line)
    data['memory_bytes'] = {}
    for key in ('MemTotal', 'MemAvailable'):
        amount, unit = mem[key].split()
        if unit != 'kB' or int(amount) <= 0:
            raise ValueError('unexpected memory unit/value')
        data['memory_bytes'][key] = int(amount) * 1024
    return data


def create(probe, size):
    body = {'type': size, 'ttlSeconds': 300, 'noEnv': True}
    pending = {'idempotency': 'boat-resources-' + uuid.uuid4().hex, 'body': body}
    probe.data['pending_creates'].append(pending)
    probe.checkpoint()
    start = time.monotonic()
    item = sandbox(probe.request('POST', '/sandboxes', body, idempotency=pending['idempotency']))
    resource = {'kind': 'sandbox', 'id': item['id']}
    probe.data['resources'].append(resource)
    probe.data['pending_creates'].remove(pending)
    probe.checkpoint()
    ready = probe.wait_sandbox(item['id'], READY)
    return resource, {'seconds_to_ready': time.monotonic()-start, 'sandbox': ready}


def run(probe):
    measurements = probe.data['resource_measurements'] = []
    for size in ('large', 'xlarge'):
        resource, creation = create(probe, size)
        record = {'size': size, 'create': creation}
        measurements.append(record)
        probe.checkpoint()
        record['guest'] = parse(probe.execute(resource['id'], 'sudo -n /usr/bin/python3 -c ' + shlex.quote(GUEST), poll_seconds=60))
        probe.checkpoint()
        subset = {'resources': [resource], 'pending_creates': [], 'cleanup': probe.data['cleanup']}
        cleanup(probe.client, subset, probe.checkpoint)
        state = resource.get('deletion_state', {})
        if not resource.get('deleted') and state.get('stage') != 'waiting_for_uploads':
            raise ValueError('previous guest not confirmed stopped/deletion-accepted; refusing concurrent sandbox')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--key-stdin', action='store_true')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    key = read_key(args)
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    path = args.output / 'results.json'
    data = {'schema': 1, 'run_id': uuid.uuid4().hex, 'started_at': datetime.now(timezone.utc).isoformat(),
            'api': [], 'resources': [], 'pending_creates': [], 'cleanup': []}
    checkpoint = lambda: save(path, data, key)
    client = Client(key, data, checkpoint)
    signal.signal(signal.SIGTERM, interrupt)
    try:
        run(Probe(client, data, checkpoint, minutes=10))
        data['measurements_complete'] = True
    except (Exception, KeyboardInterrupt) as error:
        data['error'] = scrub(str(error), key)
    finally:
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        cleanup(client, data, checkpoint)
        data['finished_at'] = datetime.now(timezone.utc).isoformat()
        checkpoint()
    print(json.dumps(scrub(data, key), indent=2))
    return 0 if data.get('measurements_complete') and data.get('cleanup_complete') and not data.get('error') else 1


if __name__ == '__main__':
    raise SystemExit(main())
