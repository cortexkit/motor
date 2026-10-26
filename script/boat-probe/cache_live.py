"""Live check that a prepared Boat image really reads the fleet compile cache.

CLOUD's broker installs `/run/motor/cache/` into every fleet sandbox on create
and resume, and the sandbox runner starts with its remote-cache switch on, so a
real Rust job must be answered by the shared store. The shapes here come from
the pinned contract in
`crates/motor-lease/contracts/cloud/runners-contract.md`: a root-owned 0700 tmpfs
directory holding a root-owned mode-0400 `read-key` and a root-owned mode-0444
`config.json`, both carrying one `generation`, with the key confined to the cache
tenant's read-only prefix.

Verify sandboxes are not broker-created, so this module installs that same
provisioning from files the operator supplies. The key is a secret: it never
enters the probe's evidence, a fetched result or the image snapshot, and every
installed or staged copy is removed before the check ends.
"""
from __future__ import annotations

import json
import shlex

# Boat's own /tmp is never saved in a snapshot, which is why the contract's
# staging area lives there and not under /etc.
STAGING = '/tmp/ck-motor-cache'
DIRECTORY = '/run/motor/cache'
READ_KEY = DIRECTORY + '/read-key'
CONFIG = DIRECTORY + '/config.json'
# What the contract installs, checked after the install and never assumed.
MODES = {'directory': '700', 'read-key': '400', 'config.json': '444'}
# The members of each file, exactly as the contract lists them. Unknown members
# are refused: one would leave the runner's parse failing and the cache off.
KEY_MEMBERS = ('access_key_id', 'generation', 'secret_access_key')
CONFIG_MEMBERS = ('bucket', 'enabled', 'endpoint', 'generation', 'prefix', 'region',
                  'storage_class', 'tenant')
# Least a real Rust job can fetch: one compile unit or more from the store.
MINIMUM_REMOTE_HITS = 1
# The runner polls provisioning metadata every second and fully rereads it at
# least every ten seconds. Observe admission without restarting the runner.
ADMISSION_SECONDS = 15

# A Cargo package with real compile units. Every dependency is resolved offline
# from the prepared image's seed, so the job neither reaches the network nor writes the
# store; the job's cache mode is read-only in any case.
CRATE_TOML = '''[package]
name = "boat-cache-live"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
'''
CRATE_MAIN = '''use serde::Serialize;

#[derive(Serialize)]
struct Built {
    cached: bool,
}

fn main() {
    let value = Built { cached: true };
    println!("boat-cache-live cached={}", value.cached);
}
'''


def read_inputs(config_path, key_path):
    """Both provisioning files, validated. Nothing here is recorded."""
    try:
        config = json.loads(config_path.read_text())
        key = json.loads(key_path.read_text())
    except (OSError, ValueError) as error:
        raise ValueError(f'cache provisioning file refused: {type(error).__name__}') from None
    if not isinstance(config, dict) or tuple(sorted(config)) != CONFIG_MEMBERS:
        raise ValueError('cache config must be one JSON object with exactly the contract members: '
                         + ', '.join(CONFIG_MEMBERS))
    if not isinstance(key, dict) or tuple(sorted(key)) != KEY_MEMBERS:
        raise ValueError('cache read-key must be one JSON object with exactly the contract members: '
                         + ', '.join(KEY_MEMBERS))
    if not isinstance(config['enabled'], bool):
        raise ValueError('cache config enabled must be a boolean')
    if config['generation'] != key['generation']:
        raise ValueError('cache config and read-key do not share one generation')
    for name, value in config.items():
        if name != 'enabled' and (not isinstance(value, str) or not value):
            raise ValueError(f'cache config {name} must be a non-empty string')
    for name in KEY_MEMBERS:
        if name != 'generation' and (not isinstance(key[name], str) or not key[name]):
            raise ValueError(f'cache read-key {name} must be a non-empty string')
    return config, key


def describe(config_path):
    """The config members a report may keep: none of them is a secret. The key
    file is not opened."""
    try:
        config = json.loads(config_path.read_text())
    except (OSError, ValueError) as error:
        return {'unreadable': type(error).__name__}
    if not isinstance(config, dict):
        return {'unreadable': 'not an object'}
    return {name: config.get(name) for name in CONFIG_MEMBERS if name != 'generation'}


def install_command():
    """Install both files where the contract puts them, at its modes."""
    return ' && '.join([
        f'sudo -n install -d -m 0700 -o root -g root {DIRECTORY}',
        f'sudo -n install -m 0444 -o root -g root {STAGING}/config.json {CONFIG}',
        f'sudo -n install -m 0400 -o root -g root {STAGING}/read-key {READ_KEY}',
    ])


def remove_command():
    """Remove the installed and staged copies, whatever the verdict is."""
    return f'sudo -n rm -f -- {READ_KEY} {CONFIG} {STAGING}/read-key {STAGING}/config.json'


def absent_command():
    return ' && '.join(f'test ! -e {path}' for path in (READ_KEY, CONFIG, f'{STAGING}/read-key',
                                                        f'{STAGING}/config.json'))


def admission_command():
    """A cheap real job observes the cache environment the watcher admitted."""
    source = '''
import json, os
from pathlib import Path
mounts = [line.split()[4] for line in Path('/proc/self/mountinfo').read_text().splitlines()]
print(json.dumps(dict(wrapper=os.environ.get('RUSTC_WRAPPER'), rw_mode=os.environ.get('SCCACHE_S3_RW_MODE'),
                     recache='SCCACHE_RECACHE' in os.environ,
                     tools_mounted='/motor-tools' in mounts, gateway_mounted='/motor-gateway' in mounts)))
'''
    return 'python3 -c ' + shlex.quote(source)


def admitted(report):
    return (isinstance(report, dict) and report.get('wrapper') == '/motor-tools/sccache'
            and report.get('rw_mode') == 'READ_ONLY' and report.get('recache') is False
            and report.get('tools_mounted') is True and report.get('gateway_mounted') is True)


def job_command():
    """Build in the Motor workspace with the image's canonical, seeded Cargo home.

    A different Cargo home changes cache keys. Ask the runner's own wrapper for
    statistics, not a second sccache server that never saw the build. Cargo's
    chatter goes to stderr so stdout contains exactly one evidence object.
    """
    source = f'''
import json, os, subprocess, sys
from pathlib import Path
package = Path('boat-cache-live')
(package / 'src').mkdir(parents=True, exist_ok=True)
(package / 'Cargo.toml').write_text({CRATE_TOML!r})
(package / 'src/main.rs').write_text({CRATE_MAIN!r})
for command in (['cargo', 'generate-lockfile', '--offline'], ['cargo', 'build', '--offline', '--locked']):
    subprocess.run(command, cwd=package, stdout=sys.stderr, check=True)
wrapper = os.environ['RUSTC_WRAPPER']
stats = json.loads(subprocess.check_output([wrapper, '--show-stats', '--stats-format=json']))
mounts = [line.split()[4] for line in Path('/proc/self/mountinfo').read_text().splitlines()]
print(json.dumps(dict(wrapper=wrapper, rw_mode=os.environ.get('SCCACHE_S3_RW_MODE'),
                     recache='SCCACHE_RECACHE' in os.environ,
                     tools_mounted='/motor-tools' in mounts, gateway_mounted='/motor-gateway' in mounts,
                     stats=stats)))
'''
    return 'python3 -c ' + shlex.quote(source)


def remote_hits(server_info):
    """Count S3 hits in sccache 0.18's ServerInfo, never local disk hits.

    Boat uses a single S3 backend. Its hit counts have two alternate breakdowns;
    count the language totals once, only after verifying the backend location.
    A multilevel configuration instead needs the S3 level's own hit counter.
    """
    if not isinstance(server_info, dict) or not isinstance(server_info.get('stats'), dict):
        return None
    stats = server_info['stats']
    levels = stats.get('multi_level')
    if isinstance(levels, list):
        counters = [level.get('hits') for level in levels if isinstance(level, dict)
                    and isinstance(level.get('location'), str) and level['location'].startswith('s3,')]
        if not counters:
            return None
    elif levels is None and str(server_info.get('cache_location', '')).startswith('s3,'):
        hits = stats.get('cache_hits')
        counts = hits.get('counts') if isinstance(hits, dict) else None
        if not isinstance(counts, dict):
            return None
        counters = list(counts.values())
    else:
        return None
    total = 0
    for count in counters:
        if type(count) is not int or count < 0:
            return None
        total += count
    return total


def passed(installed_modes, report, hits, removed):
    """What a passing live check needs, in the order a failure should be read."""
    problems = []
    if installed_modes != MODES:
        problems.append(f'installed modes {installed_modes}')
    if not removed:
        problems.append('cache provisioning was left in the sandbox')
    if report.get('wrapper') != '/motor-tools/sccache':
        problems.append(f"job wrapper {report.get('wrapper')!r}")
    if report.get('rw_mode') != 'READ_ONLY' or report.get('recache'):
        problems.append(f"job cache mode {report.get('rw_mode')!r} recache={report.get('recache')}")
    if not (report.get('tools_mounted') and report.get('gateway_mounted')):
        problems.append('job has no cache tool or gateway mount')
    if hits is None:
        problems.append('sccache reported no remote storage level')
    elif hits < MINIMUM_REMOTE_HITS:
        problems.append(f'{hits} remote sccache hits')
    return problems
