#!/usr/bin/env python3
"""Operator-only measurements; provision motor_net first. Remote runon jobs lack
host privileges, so run this directly on trusted build-server infrastructure.

--results must be outside regenerable build trees so cleanup cannot erase it.
Ten real jobs time systemctl is-active success through completion of namespace
attachment. A final job measures public Bun install throughput using Elysia's
pinned 413-package lock inventory and an empty private cache.
"""
import argparse
import json
import os
import pathlib
import re
import statistics
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--results', required=True, type=pathlib.Path)
    args = parser.parse_args()
    result = args.results.resolve()
    assert not any(part in {'target', 'dist', 'node_modules', '.cache'} for part in result.parts), 'results must survive build cleanup'
    result.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, MOTOR_NET_RESULTS=str(result), MOTOR_NET_MEASURE_ATTACH='1')
    for key in ['GIT_CONFIG_COUNT', 'GIT_CONFIG_KEY_0', 'GIT_CONFIG_VALUE_0']:
        env.pop(key, None)
    versions = []
    for command in [['rustc', '--version'], ['cargo', '--version'], ['cargo', 'nextest', '--version']]:
        versions.append(subprocess.check_output(command, env=env, text=True))
    versions.append(subprocess.check_output(['git', 'rev-parse', 'HEAD'], env=env, text=True))
    (result / 'tool-versions.txt').write_text(''.join(versions))
    command = ['cargo', 'nextest', 'run', '-p', 'motor-runner', '--lib', '--locked',
               '--run-ignored', 'only', '-E', 'test(=isolation::tests::isolated_net_measurement)', '--no-capture']
    completed = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (result / 'measurement.log').write_text(completed.stdout)
    print(completed.stdout)
    if completed.returncode:
        raise SystemExit(completed.returncode)
    samples = [int(value) for value in re.findall(r'MOTOR_NET_ATTACH_US (\d+)', completed.stdout)]
    assert len(samples) == 11, f'expected ten setup jobs plus the Bun fixture, got {len(samples)}'
    install = next(json.loads(line) for line in (result / 'bun-install-output.txt').read_text().splitlines()
                   if line.startswith('{"lockfile_path"'))
    install['lockfile_path'] = str(result / 'bun.lock')
    report = {'veth_active_to_attach_us': samples[:10],
              'veth_median_us': statistics.median(samples[:10]), 'bun_install': install}
    (result / 'results.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
    print('Retain results.json and the logs as measured evidence; do not estimate missing numbers.')


if __name__ == '__main__':
    main()
