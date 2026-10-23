"""Run the cache experiment over pinned operator SSH, never through Motor jobs."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time

from probe import ProbeError, provider_sizing_matches, sizing_error
from ssh_reach import SSHReach, endpoint, pin_host, ssh_argv

SOURCE = Path(__file__).resolve().parents[1] / 'cache-bench' / 'bench.py'
ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('motor_cache_bench', SOURCE)
if spec is None or spec.loader is None:
    raise ImportError('cannot load cache benchmark driver')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class Authorization(SSHReach):
    """Reuse SSH authorization retries without starting reachability measurements."""
    def __init__(self, runner):
        self.runner = runner


def validate(parser, args):
    options = (args.cache_image, args.cache_bundle, args.cache_bundle_sha256,
               args.cache_backends, args.cache_prefix)
    if not args.cache_bench:
        if any(options):
            parser.error('--cache-* options require --cache-bench')
        return
    if not all(options):
        parser.error('--cache-bench requires --cache-image, --cache-bundle, --cache-bundle-sha256, --cache-backends and --cache-prefix')
    import re
    if not re.fullmatch(r'[a-z0-9][a-z0-9-]{0,62}', args.cache_image):
        parser.error('invalid --cache-image named snapshot')
    if not re.fullmatch('[0-9a-f]{64}', args.cache_bundle_sha256):
        parser.error('--cache-bundle-sha256 requires the digest recorded on A')
    try:
        bench.prefix_arg(args.cache_prefix)
    except argparse.ArgumentTypeError:
        parser.error('invalid cache prefix')
    if args.max_minutes > 30:
        parser.error('--cache-bench uses the shared 30-minute sandbox TTL; --max-minutes must be <=30')


def run(runner, args):
    # Validate local inputs before creating a billable sandbox.
    configs = bench.load_backends(argparse.Namespace(backends=args.cache_backends))
    for config in configs:
        if Path(config['credentials']).exists():
            bench.credentials(Path(config['credentials']), config)
    if bench.sha(args.cache_bundle) != args.cache_bundle_sha256:
        raise ProbeError('cache bundle digest differs from independently recorded A digest')
    data = runner.data['cache_bench'] = {'source_image': args.cache_image,
                                        'bundle_sha256': args.cache_bundle_sha256, 'complete': False}
    item, runner.data['create'] = runner.create(args.cache_image)
    if not provider_sizing_matches(item, runner.size):
        raise ProbeError(sizing_error(runner.size))
    runner.wait_hydrated(item['id'])
    runner.checkpoint()
    with tempfile.TemporaryDirectory(prefix='.cache-ssh-', dir=runner.results_path.parent) as temporary:
        directory = Path(temporary)
        private = directory / 'identity'
        environment = {k: v for k, v in os.environ.items() if k != 'BOAT_API_KEY'}
        subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(private)],
                       check=True, capture_output=True, env=environment)
        response = Authorization(runner).authorize(item['id'], private.with_suffix('.pub').read_text().strip())
        known_hosts = directory / 'known_hosts'
        alias = 'boat-cache-bench'
        pin_host(known_hosts, alias, response)
        ssh = ssh_argv(endpoint(response), private, known_hosts, alias)

        def remote(text, source=None, checked=True):
            budget = max(1, int(runner.deadline - time.monotonic()) - 30)
            result = subprocess.run(ssh + [text], stdin=source or subprocess.DEVNULL,
                                    capture_output=True, env=environment, timeout=budget)
            if checked and result.returncode:
                # Do not persist stdin, command stdout or exception bodies on failures.
                raise ProbeError(f'cache-bench SSH step failed (exit {result.returncode})')
            return result

        paths = ' '.join(shlex.quote('/dev/shm/motor-cache-credentials-' + config['name']) for config in configs)
        try:
            remote('test ! -e /srv/cache-bench && sudo -n mkdir -m700 /srv/cache-bench && '
                   'sudo -n chown "$(id -u):$(id -g)" /srv/cache-bench')
            with args.cache_bundle.open('rb') as stream:
                remote('umask 077; cat > /srv/cache-bench/assets.tar.gz', stream)
            digest = shlex.quote(args.cache_bundle_sha256 + '  /srv/cache-bench/assets.tar.gz')
            remote(f"printf '%s\\n' {digest} | sha256sum --check --status && "
                   'tar -xzf /srv/cache-bench/assets.tar.gz -C /srv/cache-bench && '
                   'rm /srv/cache-bench/assets.tar.gz')
            with SOURCE.open('rb') as stream:
                remote('umask 077; cat > /dev/shm/motor-cache-bench.py', stream)
            remote('rm -f ' + paths + ' && rm -rf -- /dev/shm/motor-cache-results')
            remote_configs = []
            for config in configs:
                destination = '/dev/shm/motor-cache-credentials-' + config['name']
                path = Path(config['credentials'])
                if path.exists():
                    # Secrets enter the disposable sandbox over SSH stdin, never API commands.
                    with path.open('rb') as stream:
                        remote('umask 077; cat > ' + shlex.quote(destination), stream)
                remote_configs.append(dict(config, credentials=destination))
            manifest = directory / 'backends.json'
            manifest.write_text(json.dumps(remote_configs))
            with manifest.open('rb') as stream:
                remote('umask 077; cat > /dev/shm/motor-cache-backends.json', stream)
            prefix = shlex.quote(args.cache_prefix)
            # The operator SSH session is not a job: its PATH lacks the image's Cargo
            # bin and its default RUSTUP_HOME is the session user's own. Run A's
            # operator script exports both the same way, from the build server's
            # layout, which the Boat image shares (script/runner/toolchain.json).
            layout = json.loads((ROOT / 'script/runner/toolchain.json').read_text())
            tools = (f"PATH={shlex.quote(layout['cargo_home'] + '/bin')}:\"$PATH\" "
                     f"RUSTUP_HOME={shlex.quote(layout['rustup_home'])} ")
            # Name a missing tool here: the driver reports only an exception's
            # type, so a missing executable would surface as a bare FileNotFoundError.
            found = remote('env ' + tools + 'sh -c ' + shlex.quote(
                'for t in cargo rustc sccache; do command -v "$t" >/dev/null || echo "$t"; done'),
                checked=False)
            missing = found.stdout.decode(errors='replace').split()
            if missing or found.returncode:
                raise ProbeError('cache-bench tools missing in the sandbox: ' + (' '.join(missing) or 'check failed'))
            result = remote('env ' + tools + 'python3 /dev/shm/motor-cache-bench.py --run --placement B '
                            '--backends /dev/shm/motor-cache-backends.json '
                            f'--prefix {prefix} --output /dev/shm/motor-cache-results > /dev/null', checked=False)
            fetched = remote('cat /dev/shm/motor-cache-results/results.json', checked=False)
            if fetched.returncode == 0:
                data['result'] = json.loads(fetched.stdout)
                destination = runner.results_path.parent / 'cache-results'
                destination.mkdir(mode=0o700)
                bench.save(destination, data['result'])
            if result.returncode or not data.get('result', {}).get('complete'):
                raise ProbeError('cache benchmark failed; inspect cache-results/results.json if available')
            data['complete'] = True
        finally:
            try:
                removed = remote('rm -f ' + paths, checked=False)
                data['credential_file_removed'] = removed.returncode == 0
                if not data['credential_file_removed']:
                    data['complete'] = False
                    raise ProbeError('credential removal over SSH failed; shared cleanup must delete the sandbox')
            finally:
                runner.checkpoint()
    # The probe's module-level main runs shared cleanup even when an SSH step fails.
    runner.checkpoint()


def render(data):
    measurement = data['cache_bench']
    text = bench.render(measurement.get('result', {'backends': []}))
    return text + '\nBoat cleanup: ' + data.get('cleanup_summary', 'unmeasured') + '\n'
