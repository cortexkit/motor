#!/usr/bin/env python3
"""Remove provider CLI credentials and profile exports; emit names/booleans only."""
import argparse
import json
import os
from pathlib import Path
import pwd
import re
import stat
import subprocess

PROVIDER = re.compile(r'(?m)^\s*(?:export\s+)?(ASCII_TOKEN|BOAT_[A-Za-z0-9_]*)\s*=.*(?:\n|$)')
ACCESS_SOURCE = r'''
import json, os, pathlib, re

def provider_access(paths, pids):
    out = {'files': {}, 'processes': {}, 'inherited_names': sorted(k for k in os.environ if k == 'ASCII_TOKEN' or k.startswith('BOAT_'))}
    for name in paths:
        try:
            text = pathlib.Path(name).read_text()
            assignments = re.findall(r'(?m)^\s*(?:export\s+)?ASCII_TOKEN\s*=\s*(.*)$', text)
            token = any(value.strip().strip(";'\" ") for value in assignments)
            out['files'][name] = {'readable': True, 'ascii_token_assignment': token}
        except OSError as error:
            out['files'][name] = {'readable': False, 'error': type(error).__name__}
    for pid in pids:
        try:
            data = pathlib.Path('/proc') / str(pid) / 'environ'
            names = {part.split(b'=',1)[0] for part in data.read_bytes().split(b'\0') if b'=' in part}
            out['processes'][str(pid)] = {'readable': True, 'ascii_token_present': b'ASCII_TOKEN' in names}
        except OSError as error:
            out['processes'][str(pid)] = {'readable': False, 'error': type(error).__name__}
    return out
'''


def provider_name(name):
    return name == 'ASCII_TOKEN' or name.startswith('BOAT_')


def clean_environment(environment):
    return {name: value for name, value in environment.items() if not provider_name(name)}


def config_summary(path):
    result = {'path': str(path), 'present': path.exists() or path.is_symlink()}
    if not result['present']:
        return result
    if path.is_symlink() or not stat.S_ISREG(path.stat().st_mode):
        result['inspection_error'] = 'not a regular config file'
        return result
    keys, credentials, types = [], [], {}
    def visit(value, prefix=''):
        if isinstance(value, dict):
            for key, child in value.items():
                name = prefix + str(key)
                keys.append(name)
                types[name] = type(child).__name__
                if re.search(r'key|token|secret|password|credential', str(key), re.I) and child not in (None, '', False):
                    credentials.append(name)
                visit(child, name + '.')
        elif isinstance(value, list):
            for child in value:
                visit(child, prefix + '[].')
    try:
        visit(json.loads(path.read_text()))
        result.update(key_names=sorted(keys), nonempty_credential_field_names=sorted(credentials),
                      has_nonempty_credential_fields=bool(credentials), field_types=types)
    except (OSError, ValueError) as error:
        result['inspection_error'] = type(error).__name__
    return result


def profiles_for(homes):
    profiles = [Path(home) / name for home in homes for name in ('.bashrc','.profile','.bash_profile')]
    profiles.extend([Path('/etc/environment'), Path('/etc/profile'), Path('/etc/bash.bashrc')])
    profiles.extend(Path('/etc/profile.d').glob('*.sh'))
    return sorted(set(profiles))


def process_inventory():
    rows = []
    for directory in sorted(Path('/proc').iterdir()):
        if not directory.name.isdecimal():
            continue
        try:
            names = {part.split(b'=',1)[0].decode(errors='replace') for part in (directory/'environ').read_bytes().split(b'\0') if b'=' in part}
            if 'ASCII_TOKEN' in names or directory.name == '1':
                rows.append({'pid': int(directory.name), 'uid': directory.stat().st_uid,
                    'comm': (directory/'comm').read_text().strip(),
                    'cgroup': (directory/'cgroup').read_text().strip(),
                    'provider_environment_names': sorted(name for name in names if provider_name(name))})
        except (OSError, ValueError) as error:
            if directory.name == '1':
                rows.append({'pid':1, 'inspection_error':type(error).__name__, 'provider_environment_names':None})
    return rows


def job_access(paths, processes):
    command = ACCESS_SOURCE + '\nimport sys; print(json.dumps(provider_access(json.loads(sys.argv[1]),json.loads(sys.argv[2]))))'
    result = subprocess.run(['/usr/sbin/runuser','-u','motor-job','--','/usr/bin/python3','-c',command,
                             json.dumps([str(path) for path in paths]), json.dumps([p['pid'] for p in processes])],
                            capture_output=True, text=True, timeout=30, cwd='/', env=clean_environment(os.environ))
    if result.returncode:
        raise RuntimeError('motor-job outside-sandbox credential access check failed')
    return json.loads(result.stdout)


def sanitize(homes, profiles):
    configs = [Path(home) / '.config/ascii/boat/config.json' for home in homes]
    summaries = [config_summary(path) for path in configs]
    removed = []
    for path in configs:
        if path.exists() or path.is_symlink():
            path.unlink()
            removed.append(str(path))
    changes = {}
    for path in profiles:
        if not path.exists():
            continue
        if not stat.S_ISREG(path.stat().st_mode):
            raise RuntimeError('provider profile must be a regular file: ' + str(path))
        text = path.read_text()
        names = sorted(set(PROVIDER.findall(text)))
        if names:
            if path.is_symlink():
                raise RuntimeError('refusing to modify a linked provider profile: ' + str(path))
            # A shell conditional still needs a body after its export is removed.
            replacement = '' if path == Path('/etc/environment') else ':\n'
            path.write_text(PROVIDER.sub(replacement, text))
            changes[str(path)] = names
    if any(path.exists() or path.is_symlink() for path in configs):
        raise RuntimeError('provider CLI config still present after deletion')
    return {'config_before': summaries, 'removed_configs': removed, 'removed_profile_export_names': changes,
            'configs_absent_after': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error('requires root')
    homes = sorted({p.pw_dir for p in pwd.getpwall() if p.pw_dir.startswith('/')})
    profiles = profiles_for(home for home in homes if home == '/root' or home.startswith('/home/'))
    configs = [Path(home)/'.config/ascii/boat/config.json' for home in homes]
    processes = process_inventory()
    paths = [*configs, *profiles]
    before = job_access(paths, processes)
    result = sanitize(homes, profiles)
    result.update(processes=processes, outside_job_before=before, outside_job_after=job_access(paths, processes),
                  paths_checked=[str(p) for p in paths])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    args.output.chmod(0o600)
    print(json.dumps(result))


if __name__ == '__main__':
    main()
