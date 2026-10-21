#!/usr/bin/env python3
"""Check, inside a real job, that a worktree-scoped empty pushurl resets the list.

Git 2.46 made an empty value of remote.<name>.url or remote.<name>.pushurl
clear the URLs collected from earlier config (git commit 9badf97c42, "remote:
allow resetting url list"). Older git, such as Ubuntu 24.04's 2.43, appends the
empty string as one more URL instead, so the inherited push URL survives. Code
that resets a pushurl at --worktree scope and then adds one URL therefore sees
three push URLs on old git and exactly one on the build server's git.

Prints one JSON object and exits nonzero when the job's git keeps more than
the one URL.
"""

import json
import os
from pathlib import Path
import subprocess
import tempfile

INHERITED = 'https://example.invalid/inherited-push.git'
REFUSED = 'https://example.invalid/refused-push.git'


def git(cwd, *args, env):
    result = subprocess.run(['git', *args], cwd=cwd, env=env, text=True, capture_output=True, check=True)
    return result.stdout


def evaluate(observed):
    """`observed` is the stdout of `git remote get-url --push --all`."""
    urls = observed.splitlines()
    return {'passed': urls == [REFUSED], 'expected': [REFUSED], 'observed': urls}


def run_probe():
    try:
        report = measure()
    except subprocess.CalledProcessError as error:
        # A git that rejects any step is a failed probe, still reported as JSON.
        report = {'passed': False, 'command': error.cmd, 'exit_code': error.returncode,
                  'stderr': error.stderr}
    print(json.dumps(report))
    return 0 if report['passed'] else 1


def measure():
    # Measure only the job's git binary: no inherited per-command, global or
    # system configuration may add or reset URLs.
    env = {name: value for name, value in os.environ.items() if not name.startswith('GIT_')}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1', GIT_TERMINAL_PROMPT='0')
    with tempfile.TemporaryDirectory(prefix='git-pushurl-probe-') as scratch:
        main = Path(scratch) / 'main'
        main.mkdir()
        git(main, 'init', '-q', env=env)
        git(main, '-c', 'user.name=Pushurl probe', '-c', 'user.email=probe@invalid',
            'commit', '-q', '--allow-empty', '-m', 'probe', env=env)
        git(main, 'remote', 'add', 'origin', 'https://example.invalid/fetch.git', env=env)
        git(main, 'config', 'remote.origin.pushurl', INHERITED, env=env)
        git(main, 'config', 'extensions.worktreeConfig', 'true', env=env)
        worktree = Path(scratch) / 'linked'
        git(main, 'worktree', 'add', '-q', '--detach', str(worktree), env=env)
        # The sequence that failed on Boat: clear the inherited values at
        # worktree scope, then add the single URL this worktree should use.
        git(worktree, 'config', '--worktree', '--replace-all', 'remote.origin.pushurl', '', env=env)
        git(worktree, 'config', '--worktree', '--add', 'remote.origin.pushurl', REFUSED, env=env)
        observed = git(worktree, 'remote', 'get-url', '--push', '--all', 'origin', env=env)
        return {'git_version': git(worktree, '--version', env=env).strip(), **evaluate(observed)}


if __name__ == '__main__':
    raise SystemExit(run_probe())
