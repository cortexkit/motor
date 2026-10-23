#!/usr/bin/env python3
"""Restartable operator comparison over SSH and Boat sandboxes, never Motor runner jobs."""
from __future__ import annotations

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

import bench

HERE = Path(__file__).resolve().parent
PROBE = HERE.parent / 'boat-probe/probe.py'
HOURS = 72
INTERVAL = 3600
TTL = 1800
MARGIN = 180
# Hydration/uploads, two routes' network probes per repository/class, and cleanup
# are outside Cargo wall time. This is an admission allowance, not a runtime guarantee.
NON_BUILD_ALLOWANCE = 360
# Read-only samples per class, most preferred first; a repository uses the largest that fits.
SAMPLE_CHOICES = (3, 2)

REMOTE = '''import sys,json,argparse
payload=json.load(sys.stdin)
ns={'__name__':'cache_bench_remote','__file__':'cache-bench.py'}
exec(compile(payload['driver'],'cache-bench.py','exec'),ns)
report={'schema':1,'placement':'A','backends':[],'complete':False}
def checkpoint(): print(json.dumps(report,allow_nan=False),flush=True)
for repo in payload['repositories']:
 for item in payload['backends']:
  config=item['config']
  entry=dict(name=config['name'],storage_class=config.get('storage_class'),placement='A',repository=repo['name'],status='running')
  report['backends'].append(entry)
  try:
   s3=ns['S3'](*item['credential'],config)
   def progress(partial):
    entry['result']=partial
    checkpoint()
   entry['result']=ns['network_probes'](s3,repo['prefix']+config['name']+'/',gateway=False,checkpoint=progress)
   entry['status']=ns['result_status'](entry['result'])
  except Exception as error:
   entry.update(status='failed',error=type(error).__name__)
  checkpoint()
report['complete']=all(entry['status']=='passed' for entry in report['backends'])
checkpoint()
'''

REMOTE_FILL = '''import sys,json,argparse,tempfile,pathlib,shutil,subprocess,contextlib,io,os
payload=json.load(sys.stdin)
layout=payload['toolchain_layout']
os.environ['PATH']=layout['cargo_home']+'/bin:'+os.environ.get('PATH','/usr/bin:/bin')
os.environ['RUSTUP_HOME']=layout['rustup_home']
with tempfile.TemporaryDirectory(prefix='cache-bench-driver-',dir='/dev/shm') as temporary:
 driver=pathlib.Path(temporary)/'bench.py'
 driver.write_text(payload['driver'])
 ns={'__name__':'cache_bench_remote','__file__':str(driver)}
 exec(compile(payload['driver'],str(driver),'exec'),ns)
 configs=[item['config'] for item in payload['backends']]
 clients={item['config']['name']:ns['S3'](*item['credential'],item['config']) for item in payload['backends']}
 # Preflight every repository and class before any write or expensive build.
 for repo in payload['repositories']:
  bundle=pathlib.Path(payload['server_assets'])/(repo['name']+'.tar.gz')
  if ns['sha'](bundle)!=repo['bundle_sha256']:
   raise RuntimeError('server bundle digest differs')
  if (pathlib.Path(payload['server_assets'])/'fills'/repo['name']).exists():
   raise RuntimeError('fill evidence directory already exists')
  for config in configs:
   if list(clients[config['name']].objects(repo['prefix']+config['name']+'/')):
    raise RuntimeError('fill prefix nonempty; no fills started')
 root=ns['ROOT']
 if root.is_symlink() or root.resolve()!=root or not root.is_dir():
  raise RuntimeError('reserved benchmark root must be an operator-owned directory')
 report={'fills':{},'complete':True}
 for repo in payload['repositories']:
  shutil.rmtree(root)
  root.mkdir(mode=0o700)
  bundle=pathlib.Path(payload['server_assets'])/(repo['name']+'.tar.gz')
  subprocess.run(['tar','-xzf',str(bundle),'-C',str(root)],check=True)
  output=pathlib.Path(payload['server_assets'])/'fills'/repo['name']
  args=argparse.Namespace(placement='A',profile='fill',baseline=False,prefix=repo['prefix'],output=output)
  with contextlib.redirect_stdout(io.StringIO()):
   complete=ns['matrix'](args,configs,clients=clients)
  report['fills'][repo['name']]=json.loads((output/'results.json').read_text())
  report['complete'] &= complete
 print(json.dumps(report,allow_nan=False))
'''


def atomic(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2, allow_nan=False) + '\n')
    temporary.replace(path)


def read_key(command):
    result = subprocess.run(shlex.split(command), capture_output=True, timeout=30,
                            env={k: v for k, v in os.environ.items() if k != 'BOAT_API_KEY'})
    if result.returncode or not result.stdout.strip() or len(result.stdout) > 16384:
        raise bench.BenchError('Boat key reader failed or returned an invalid key')
    return result.stdout.strip() + b'\n'


def transport_env():
    return {k: v for k, v in os.environ.items() if k in ('PATH', 'HOME', 'USER', 'LOGNAME', 'SSH_AUTH_SOCK')}


def backend_pair(path):
    configs = bench.load_backends(argparse.Namespace(backends=path))
    if len(configs) != 2 or {c.get('storage_class') for c in configs} != {'STANDARD', 'EXPRESS_ONEZONE'}:
        raise bench.BenchError('schedule requires exactly STANDARD and EXPRESS_ONEZONE')
    for config in configs:
        endpoint, _, _ = bench.credentials(Path(config['credentials']), config)
        if endpoint != bench.ENDPOINT or config['region'] != 'de':
            raise bench.BenchError('schedule requires OVH Frankfurt .io endpoints and region de')
    configs.sort(key=lambda c: c['storage_class'] != 'STANDARD')
    return configs


def boat(args, directory, repositories, configs, key, baseline=False, profile='hourly', samples=None):
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    repos_path = directory / 'repositories.json'
    backends_path = directory / 'backends.json'
    atomic(repos_path, repositories)
    atomic(backends_path, configs)
    argv = [sys.executable, str(PROBE), '--key-stdin', '--cache-bench', '--cache-image', args.image,
            '--size', 'large', '--max-minutes', '30', '--cache-repositories', str(repos_path),
            '--cache-backends', str(backends_path), '--cache-profile', profile, '--output', str(directory / 'probe')]
    if baseline:
        argv.append('--cache-baseline')
    if samples:
        argv += ['--cache-samples', str(int(samples))]
    try:
        result = subprocess.run(argv, input=key, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                timeout=TTL + 180, env=transport_env())
        code = result.returncode
    except subprocess.TimeoutExpired:
        code = 124
    report = directory / 'probe/cache-results/results.json'
    lifecycle = directory / 'probe/results.json'
    if code == 3 or code == 124:
        cleanup(args, lifecycle, key)
    return {'exit_code': code, 'report': str(report.relative_to(args.output)) if report.exists() else None,
            'complete': code == 0 and report.exists() and json.loads(report.read_text()).get('complete', False)}


def cleanup(args, lifecycle, key):
    if not lifecycle.exists():
        return False
    result = subprocess.run([sys.executable, str(PROBE), '--key-stdin', '--cleanup', str(lifecycle)],
                            input=key, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=180, env=transport_env())
    return result.returncode == 0


def server(args, directory, repositories, configs):
    # Secrets exist only in this process and pinned operator SSH stdin, never argv/evidence.
    payload = {'driver': (HERE / 'bench.py').read_text(), 'repositories': repositories,
               'backends': [{'config': config, 'credential': bench.credentials(Path(config['credentials']), config)}
                            for config in configs]}
    try:
        result = subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes', args.server,
                                 'python3 -c ' + shlex.quote(REMOTE)], input=json.dumps(payload).encode(),
                                capture_output=True, timeout=900, env=transport_env())
        code, output = result.returncode, result.stdout
    except subprocess.TimeoutExpired as error:
        code, output = 124, error.stdout or b''
    # The final complete JSON line survives truncation of a later checkpoint.
    report = None
    for line in output.splitlines():
        try:
            report = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            continue
    if report is None:
        return {'complete': False, 'exit_code': code, 'report': None}
    if code:
        report['complete'] = False
    path = directory / 'server.json'
    atomic(path, report)
    return {'complete': not code and bool(report['complete']), 'exit_code': code, 'report': str(path.relative_to(args.output))}


def fill(args, repositories, configs):
    if not args.server_assets or not args.server_assets.startswith('/'):
        raise bench.BenchError('--fill requires --server-assets ABSOLUTE_PRIVATE_DIRECTORY')
    if any((args.output / r['name']).exists() for r in repositories):
        raise bench.BenchError('local fill evidence exists; use a fresh experiment/output')
    payload = {'driver': (HERE / 'bench.py').read_text(), 'repositories': repositories,
               'server_assets': args.server_assets,
               'toolchain_layout': json.loads((HERE.parent / 'runner/toolchain.json').read_text()),
               'backends': [{'config': c, 'credential': bench.credentials(Path(c['credentials']), c)} for c in configs]}
    result = subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes', args.server,
                             'python3 -c ' + shlex.quote(REMOTE_FILL)], input=json.dumps(payload).encode(),
                            capture_output=True, timeout=4 * TTL, env=transport_env())
    if result.returncode:
        raise bench.BenchError('server fill failed; inspect private server evidence, never retry into a nonempty prefix')
    report = json.loads(result.stdout)
    for name, data in report['fills'].items():
        output = args.output / name
        output.mkdir(mode=0o700, exist_ok=False)
        bench.save(output, data)
    return report['complete']


def sandbox_budget(cold_seconds, builds):
    """Conservative admission estimate for one sandbox running `builds` cold-length builds."""
    return builds * cold_seconds + NON_BUILD_ALLOWANCE + MARGIN


def sizing(cold_seconds):
    """Choose one repository's read-only samples per class, or None when it cannot fit.

    Each repository has its own sandbox every hour, holding both classes. R is the
    largest of SAMPLE_CHOICES whose busiest hour (2R samples plus the sixth-hour
    uncached baseline) fits the TTL. When even R=2 cannot also hold the baseline,
    the baseline moves to a separate baseline-only sandbox in the same slot; it is
    never skipped. R is the same in every hour so samples stay comparable.
    """
    for samples in SAMPLE_CHOICES:
        budget = sandbox_budget(cold_seconds, 2 * samples + 1)
        if budget <= TTL:
            return dict(samples=samples, baseline_sandbox=False,
                        sandbox_budget_seconds=sandbox_budget(cold_seconds, 2 * samples),
                        baseline_hour_budget_seconds=budget, baseline_sandbox_budget_seconds=None)
    samples = SAMPLE_CHOICES[-1]
    budget = sandbox_budget(cold_seconds, 2 * samples)
    alone = sandbox_budget(cold_seconds, 1)
    if budget <= TTL and alone <= TTL:
        return dict(samples=samples, baseline_sandbox=True, sandbox_budget_seconds=budget,
                    baseline_hour_budget_seconds=budget, baseline_sandbox_budget_seconds=alone)
    return None


def selection(records):
    """Pick the heavier-dependency candidate that fits its own sandbox, preferring more dependency work.

    Only candidates with more measured dependency unit-seconds than motor qualify:
    the point of the comparison is a repository whose dependency builds dominate.
    """
    motor = records['motor']
    if sizing(motor['cold_seconds']) is None:
        return None
    candidates = sorted((name for name in ('prefrontal', 'broca')
                         if records[name]['dependency_unit_seconds'] > motor['dependency_unit_seconds']),
                        key=lambda name: records[name]['dependency_unit_seconds'], reverse=True)
    for name in candidates:
        if sizing(records[name]['cold_seconds']) is not None:
            return name
    return None


def scheduled_sizing(records, selected):
    """Per-repository sizing for motor and the selected repository, motor first."""
    return {name: sizing(records[name]['cold_seconds']) for name in ('motor', selected)}


def calibrate(args, repositories, configs):
    if {r['name'] for r in repositories} != {'motor', 'prefrontal', 'broca'}:
        raise bench.BenchError('calibration requires motor, prefrontal and broca pinned bundles')
    evidence = {'schema': 2, 'image': args.image, 'size': 'large', 'repositories': repositories,
                'ttl_seconds': TTL, 'margin_seconds': MARGIN, 'non_build_allowance_seconds': NON_BUILD_ALLOWANCE,
                'sample_choices': list(SAMPLE_CHOICES), 'records': {}}
    for repo in repositories:
        directory = args.output / ('calibration-' + repo['name'])
        result = boat(args, directory, [repo], configs[:1], read_key(args.key_reader), profile='baseline')
        if result['complete']:
            report = json.loads((args.output / result['report']).read_text())
            runs = report['backends'][0]['result']['runs']
            sample = next(run for run in runs if run['name'] == 'no-cache')
            dependency = [s.get('dependency_unit_seconds') for s in sample['steps']]
            if any(s is None for s in dependency):
                raise bench.BenchError('calibration requires Cargo dependency timing evidence')
            evidence['records'][repo['name']] = dict(cold_seconds=sample['wall_seconds'],
                                                      dependency_unit_seconds=sum(dependency), evidence=result)
        else:
            evidence['records'][repo['name']] = dict(cold_seconds=TTL, dependency_unit_seconds=0, evidence=result)
        evidence['records'][repo['name']]['sizing'] = sizing(evidence['records'][repo['name']]['cold_seconds'])
        atomic(args.output / 'calibration.json', evidence)
    selected = selection(evidence['records'])
    evidence.update(selected=selected, complete=bool(selected),
                    sizing=scheduled_sizing(evidence['records'], selected) if selected else None,
                    reason='Candidate with the most measured dependency unit-seconds above motor whose own '
                           'per-repository sandbox fits the TTL with margin at R=3 or R=2.'
                    if selected else 'No candidate has measured heavier dependencies than motor and fits its own '
                                     'sandbox at R=2, even with a separate baseline sandbox.')
    atomic(args.output / 'calibration.json', evidence)
    print(json.dumps({'selected': selected, 'sizing': evidence['sizing'],
                      'calibration': str(args.output / 'calibration.json')}))
    return bool(selected)


def admitted(args, repositories):
    evidence = json.loads(args.calibration.read_text())
    if (evidence.get('schema') != 2 or not evidence.get('complete') or
            evidence.get('selected') not in ('prefrontal', 'broca') or
            evidence.get('image') != args.image or evidence.get('size') != 'large' or
            evidence.get('ttl_seconds') != TTL or evidence.get('margin_seconds') != MARGIN or
            evidence.get('non_build_allowance_seconds') != NON_BUILD_ALLOWANCE or
            selection(evidence['records']) != evidence['selected'] or
            evidence.get('sizing') != scheduled_sizing(evidence['records'], evidence['selected'])):
        raise bench.BenchError('a successful matching Boat-large calibration is required')
    order = ('motor', evidence['selected'])
    chosen = sorted((r for r in repositories if r['name'] in order), key=lambda r: order.index(r['name']))
    if len(chosen) != 2 or any(r not in evidence['repositories'] for r in chosen):
        raise bench.BenchError('source bundle/prefix changed since calibration; recalibrate')
    for repo in chosen:
        record = evidence['records'][repo['name']]
        report = args.calibration.parent / record['evidence']['report']
        if not record['evidence']['complete'] or not json.loads(report.read_text()).get('complete'):
            raise bench.BenchError('calibration raw evidence is missing or incomplete')
    return chosen, evidence['sizing']


def verify_fills(args, repositories, configs):
    for repo in repositories:
        path = args.fills / repo['name'] / 'results.json'
        report = json.loads(path.read_text())
        entries = report['backends']
        if not report.get('complete') or {e['name'] for e in entries} != {c['name'] for c in configs}:
            raise bench.BenchError('both successful server fills are required for each repository')
        for entry in entries:
            config = next(c for c in configs if c['name'] == entry['name'])
            result = entry['result']
            manifest = bench.bundle_manifest(repo['bundle'])
            if (entry['status'] != 'passed' or entry.get('storage_class') != config['storage_class'] or
                    result['prefix'] != repo['prefix'] + entry['name'] + '/' or result['manifest'] != manifest or
                    not result['class_audit']['matches_requested'] or
                    [r['name'] for r in result['runs']] != ['remote-fill']):
                raise bench.BenchError('fill evidence does not match the pinned repository/classes/prefixes')


def slot_sandboxes(repositories, sizes, baseline):
    """The Boat sandboxes of one slot, run one after another in this order.

    Each repository gets its own fresh sandbox holding only its samples for both
    classes. In a sixth-hour slot, a repository whose baseline does not fit beside
    its samples gets a baseline-only sandbox first, so the baseline still precedes
    that repository's first class as it does when it fits inline.
    """
    sandboxes = []
    for repo in repositories:
        size = sizes[repo['name']]
        if baseline and size['baseline_sandbox']:
            sandboxes.append(dict(name=repo['name'] + '-baseline', repository=repo['name'], profile='baseline',
                                  samples=None, baseline=True))
        sandboxes.append(dict(name=repo['name'], repository=repo['name'], profile='hourly', samples=size['samples'],
                              baseline=baseline and not size['baseline_sandbox']))
    return sandboxes


def plan(output, fingerprint, now, repositories=(), sizes=None):
    path = output / 'state.json'
    if path.exists():
        state = json.loads(path.read_text())
        if state['fingerprint'] != fingerprint:
            raise bench.BenchError('schedule inputs changed; resume with the original inputs')
        for item in state['iterations']:
            if item['status'] == 'running':
                item.update(status='failed', error='scheduler interrupted; sample not repeated', finished_at=now)
    else:
        sizes = sizes or {}
        samples = {repo['name']: sizes[repo['name']]['samples'] for repo in repositories}
        state = {'schema': 2, 'fingerprint': fingerprint, 'started_at': now, 'sizing': sizes,
                 'iterations': [{'hour': i + 1, 'due_at': now + i * INTERVAL, 'status': 'pending',
                                 'baseline': (i + 1) % 6 == 0, 'reverse': bool(i % 2), 'samples': dict(samples),
                                 'sandboxes': slot_sandboxes(repositories, sizes, (i + 1) % 6 == 0)}
                                for i in range(HOURS)]}
    atomic(path, state)
    return state


def resolve_cleanups(args, key):
    """Retry unresolved cleanup of any earlier sandbox before creating another one."""
    for lifecycle in sorted(args.output.glob('hour-*/boat-*/probe/results.json')):
        data = json.loads(lifecycle.read_text())
        if not data.get('cleanup_complete', False):
            if not cleanup(args, lifecycle, key):
                raise bench.BenchError('previous sandbox cleanup unresolved')


def run_schedule(args, repositories, configs, sizes, clock=time.time, wait=time.sleep, boat_leg=boat, server_leg=server):
    identity = dict(repositories=repositories, backends=configs, image=args.image, server=args.server, sizing=sizes,
                    calibration_sha256=bench.sha(args.calibration), driver_sha256=bench.sha(HERE / 'bench.py'),
                    scheduler_sha256=bench.sha(Path(__file__)), probe_sha256=bench.sha(PROBE),
                    boat_driver_sha256=bench.sha(PROBE.with_name('cache_bench.py')),
                    boat_helpers={p.name: bench.sha(p) for p in sorted(PROBE.parent.glob('*.py')) if not p.name.startswith('test_')},
                    key_reader_sha256=hashlib.sha256(args.key_reader.encode()).hexdigest())
    fingerprint = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    state = plan(args.output, fingerprint, clock(), repositories, sizes)
    by_name = {repo['name']: repo for repo in repositories}
    for item in state['iterations']:
        if item['status'] != 'pending':
            continue
        if clock() >= item['due_at'] + INTERVAL:
            item.update(status='missed', finished_at=clock(), error='hour elapsed while scheduler was offline')
            atomic(args.output / 'state.json', state)
            continue
        wait(max(0, item['due_at'] - clock()))
        item.update(status='running', started_at=clock(), boat={})
        atomic(args.output / 'state.json', state)
        directory = args.output / f"hour-{item['hour']:03d}"
        directory.mkdir(mode=0o700, exist_ok=True)
        ordered = list(reversed(configs)) if item['reverse'] else list(configs)
        item['order'] = [c['name'] for c in ordered]
        # Each sandbox and the server leg are independent: one failure must not
        # suppress the other repository's sandbox or the server evidence.
        for sandbox in item['sandboxes']:
            repo = by_name[sandbox['repository']]
            try:
                # A fresh key read per sandbox lets a rotated key take effect mid-hour.
                key = read_key(args.key_reader)
                resolve_cleanups(args, key)
                target = directory / ('boat-' + sandbox['name'])
                if sandbox['profile'] == 'baseline':
                    # Uncached builds touch no cache, so one class's credentials suffice, as in calibration.
                    result = boat_leg(args, target, [repo], configs[:1], key, profile='baseline')
                else:
                    result = boat_leg(args, target, [repo], ordered, key, baseline=sandbox['baseline'],
                                      samples=sandbox['samples'])
            except Exception as error:
                result = dict(complete=False, error=type(error).__name__)
            item['boat'][sandbox['name']] = dict(result, repository=sandbox['repository'], profile=sandbox['profile'],
                                                 samples=sandbox['samples'])
            atomic(args.output / 'state.json', state)
        try:
            item['server'] = server_leg(args, directory, repositories, ordered)
        except Exception as error:
            item['server'] = dict(complete=False, error=type(error).__name__)
        atomic(args.output / 'state.json', state)
        complete = all(leg['complete'] for leg in item['boat'].values()) and item['server']['complete']
        item.update(status='passed' if complete else 'failed', finished_at=clock())
        atomic(args.output / 'state.json', state)
        print(f"hour {item['hour']:02d}/{HOURS}: {item['status']}", flush=True)
    return all(item['status'] == 'passed' for item in state['iterations'])


def main(argv=None):
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--calibrate', action='store_true')
    parser.add_argument('--fill', action='store_true', help='fill calibrated prefixes once, over operator SSH stdin')
    parser.add_argument('--repositories', required=True, type=Path)
    parser.add_argument('--backends', required=True, type=Path)
    parser.add_argument('--image', required=True)
    parser.add_argument('--key-reader', required=True, help='trusted executable and arguments, parsed by shlex, not a shell pipeline')
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--calibration', type=Path)
    parser.add_argument('--fills', type=Path)
    parser.add_argument('--server', help='operator SSH alias with a pinned known_hosts entry')
    parser.add_argument('--server-assets', help='absolute private server directory holding <repository>.tar.gz bundles')
    args = parser.parse_args(argv)
    if args.server and (args.server.startswith('-') or any(c.isspace() for c in args.server)):
        parser.error('--server must be an SSH host/alias, not options')
    args.output = args.output.expanduser().resolve()
    args.output.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (args.output / 'scheduler.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise bench.BenchError('another scheduler owns this output directory') from None
        repositories = bench.load_repositories(args.repositories)
        configs = backend_pair(args.backends)
        if args.calibrate and args.fill:
            parser.error('--calibrate and --fill are mutually exclusive')
        if args.calibrate:
            if (args.output / 'calibration.json').exists():
                raise bench.BenchError('calibration output already exists; use a fresh directory')
            return calibrate(args, repositories, configs)
        if not (args.calibration and args.server and (args.fill or args.fills)):
            parser.error('schedule requires --calibration, --fills and --server')
        repositories, sizes = admitted(args, repositories)
        if args.fill:
            return fill(args, repositories, configs)
        verify_fills(args, repositories, configs)
        return run_schedule(args, repositories, configs, sizes)


if __name__ == '__main__':
    try:
        sys.exit(0 if main() else 1)
    except (Exception, KeyboardInterrupt) as error:
        # Do not echo command errors: a failed key reader or SSH peer can contain secrets.
        print('scheduler stopped: ' + (str(error) if isinstance(error, bench.BenchError) else type(error).__name__), file=sys.stderr)
        sys.exit(1)
