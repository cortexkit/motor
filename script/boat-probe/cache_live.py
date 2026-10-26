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

# Boat's own /tmp is never saved in a snapshot, which is why the contract's
# staging area lives there and not under /etc.
STAGING = '/tmp/ck-motor-cache'
DIRECTORY = '/run/motor/cache'
READ_KEY = DIRECTORY + '/read-key'
CONFIG = DIRECTORY + '/config.json'
SEED = 'cargo-seed.tar.gz'
SEED_MANIFEST = 'cargo-seed.json'
# The account every probe job runs as.
JOB_ACCOUNT = 'motor-job'
WORKSPACE = '/Users/boat-cache-live'
APP = WORKSPACE + '/app'
# What the contract installs, checked after the install and never assumed.
MODES = {'directory': '700', 'read-key': '400', 'config.json': '444'}
# The members of each file, exactly as the contract lists them. Unknown members
# are refused: one would leave the runner's parse failing and the cache off.
KEY_MEMBERS = ('access_key_id', 'generation', 'secret_access_key')
CONFIG_MEMBERS = ('bucket', 'enabled', 'endpoint', 'generation', 'prefix', 'region',
                  'storage_class', 'tenant')
# Least a real Rust job can fetch: one compile unit or more from the store.
MINIMUM_REMOTE_HITS = 1

# A Cargo package with real compile units. Every dependency is resolved offline
# from the operator's seed, so the job neither reaches the network nor writes the
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


def stage_command():
    """The shell that writes the job's package and Cargo home owned by the job
    account. Its standard input is the package files followed by the operator's
    seed, so no cache credential is part of the staged material a snapshot could
    keep."""
    return ' && '.join([
        f'install -d -m 0755 {APP}/src {WORKSPACE}/cargo {WORKSPACE}/target',
        f'cat > {APP}/Cargo.toml',
        f'cat > {APP}/src/main.rs',
        f'sudo -n chown -R {JOB_ACCOUNT}:{JOB_ACCOUNT} {WORKSPACE}',
        f'cat > {STAGING}/{SEED}',
    ])


def job_command():
    """The job's command, run with its package directory as the cwd.

    It builds offline from the job's own Cargo home, then prints the statistics
    of the wrapper the runner installed. Asking the same wrapper is what makes
    the numbers the job's own: a second sccache would start its own server and
    report a build that never ran."""
    return 'cargo build --offline --locked && "$RUSTC_WRAPPER" --show-stats --stats-format=json'


def remote_hits(stats):
    """Compiles the store answered, or None when no remote counter is reported.

    sccache names its storage levels (`s3_hits`, and the legacy
    `s3_<storage>_hits`). A build with the cache off reports no such counter at
    all, which is why a job that never saw the cache cannot pass as a miss."""
    if not isinstance(stats, dict):
        return None
    counters = [count for name, count in stats.items()
                if isinstance(count, int) and not isinstance(count, bool)
                and (name == 's3_hits' or (name.startswith('s3_') and name.endswith('_hits')))]
    return sum(counters) if counters else None


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
