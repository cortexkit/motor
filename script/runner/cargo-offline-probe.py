#!/usr/bin/env python3
"""Replay the two registry misses inside an offline Boat job, without network."""

import json
import os
from pathlib import Path
import subprocess
import tempfile


def run_probe():
    env = dict(os.environ, CARGO_NET_OFFLINE='true', RUSTC_WRAPPER='',
               CARGO_BUILD_RUSTC_WRAPPER='')
    for name in ('GIT_CONFIG_COUNT', 'GIT_CONFIG_KEY_0', 'GIT_CONFIG_VALUE_0'):
        env.pop(name, None)
    results = []
    with tempfile.TemporaryDirectory(prefix='cargo-offline-probe-') as scratch:
        root = Path(scratch)
        for label, crate, version in (('nested-helper-lockfile', 'cortexkit-cow', '0.1.0'),
                                      ('no-lockfile-entry', 'cortexkit-bus-testkit', '0.1.3')):
            path = root / label / 'probes/helper'
            (path / 'src').mkdir(parents=True)
            (path / 'src/lib.rs').write_text('pub fn offline_probe() {}\n')
            (path / 'Cargo.toml').write_text(
                f'[package]\nname="offline-probe"\nversion="0.1.0"\nedition="2021"\n'
                f'[workspace]\n[dependencies]\n{crate}="={version}"\n')
            locked = label == 'nested-helper-lockfile'
            if locked:
                subprocess.run(['cargo', 'generate-lockfile', '--offline'], cwd=path, env=env, check=True)
            assert (path / 'Cargo.lock').exists() == locked
            command = ['cargo', 'check', '--offline'] + (['--locked'] if locked else [])
            subprocess.run(command, cwd=path, env=env, check=True)
            results.append({'case': label, 'crate': crate, 'version': version, 'passed': True})
    print(json.dumps({'passed': True, 'checks': results}))


if __name__ == '__main__':
    run_probe()
