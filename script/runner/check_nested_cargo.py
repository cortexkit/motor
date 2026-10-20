#!/usr/bin/env python3
"""Advisory offline audit of nested Cargo locks in fleet default-branch snapshots.

This never gates job admission or deployment. It uses the invoking host's Cargo
cache, so missing sources are warnings, not proof that a lockfile is stale.
"""

import argparse
import math
import os
from pathlib import Path
import signal
import stat
import subprocess
import tempfile

MAX_LOCKFILES = 128


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.PIPE)


def fetch(manifest, directory, seconds):
    env = dict(os.environ, CARGO_NET_OFFLINE='true', RUSTUP_AUTO_INSTALL='0', RUSTUP_NO_UPDATE_CHECK='1',
               RUSTC_WRAPPER='', CARGO_BUILD_RUSTC_WRAPPER='')
    for name in list(env):
        if name.startswith('GIT_CONFIG_') or name in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE'):
            env.pop(name)
    command = ['cargo', 'fetch', '--locked', '--offline', '--manifest-path', str(manifest)]
    # A neutral cwd ignores repository .cargo/config and rust-toolchain files,
    # matching trusted runner fetches instead of running uploaded configuration.
    process = subprocess.Popen(command, cwd=directory, env=env, start_new_session=True,
                               stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True)
    try:
        _, error = process.communicate(timeout=seconds)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        _, error = process.communicate()
        return f'timed out after {seconds:g}s: {error.strip()}'
    return error.strip() if process.returncode else None


def audit(repo, seconds, emit=print):
    try:
        branch = git(repo, 'symbolic-ref', '--quiet', 'refs/remotes/origin/HEAD').decode().strip()
    except subprocess.CalledProcessError:
        branch = 'HEAD'
    commit = git(repo, 'rev-parse', '--verify', branch).decode().strip()
    files = {}
    for entry in git(repo, 'ls-tree', '-rz', commit).split(b'\0'):
        if entry:
            header, name = entry.split(b'\t', 1)
            files[Path(os.fsdecode(name))] = header.startswith((b'100644 blob ', b'100755 blob '))
    locks = sorted(path for path in files if path.name == 'Cargo.lock' and path != Path('Cargo.lock'))
    if not locks:
        return
    with tempfile.TemporaryDirectory(prefix='motor-nested-cargo-') as scratch:
        root = Path(scratch)
        checkout = root / 'checkout'
        subprocess.run(['git', 'clone', '-q', '--shared', '--no-checkout', str(repo), str(checkout)], check=True)
        subprocess.run(['git', '-C', str(checkout), 'checkout', '-q', '--detach', commit], check=True)
        neutral = root / 'fetch'
        neutral.mkdir()
        count = int(Path('Cargo.lock') in files)
        for lock in locks:
            count += 1
            label = f'{repo.name}/{lock}'
            if count > MAX_LOCKFILES:
                emit(f'SKIP {label}: exceeds 128 tracked Cargo.lock limit (advisory)')
                continue
            manifest = lock.with_name('Cargo.toml')
            regular = lambda path: stat.S_ISREG((checkout / path).lstat().st_mode)
            if not (files[lock] and files.get(manifest) and regular(lock) and regular(manifest)):
                emit(f'SKIP {label}: requires tracked regular lock and sibling manifest (advisory)')
                continue
            try:
                error = fetch(checkout / manifest, neutral, seconds)
            except OSError as failure:
                error = str(failure)
            if error:
                emit(f'WARN {label}: ' + ' | '.join(error.splitlines()))
            else:
                emit(f'PASS {label}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('projects', type=Path, nargs='?', default=Path.home() / 'Work/Projects/CortexKit')
    parser.add_argument('--timeout', type=float, default=30, help='per-lock fetch deadline in seconds')
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or args.timeout <= 0 or not args.projects.is_dir():
        parser.error('provide an existing projects directory and a positive timeout')
    for repo in sorted(args.projects.resolve().iterdir()):
        if not repo.is_dir():
            continue
        try:
            top = git(repo, 'rev-parse', '--show-toplevel').decode().strip()
        except subprocess.CalledProcessError:
            continue
        if Path(top).resolve() != repo.resolve():
            continue
        try:
            audit(repo, args.timeout)
        except (OSError, subprocess.CalledProcessError) as error:
            detail = getattr(error, 'stderr', b'') or b''
            print(f'WARN {repo.name}: audit unavailable: {error} {os.fsdecode(detail).strip()}')
    print('Nested Cargo audit complete; warnings/skips are advisory and do not block deployment.')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
