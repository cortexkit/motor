#!/usr/bin/env python3
"""Exercise host config installation without modifying the test machine."""
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SOURCE = 'script/runner/journald/60-motor-retention.conf'
DEST = '/etc/systemd/journald.conf.d/60-motor-retention.conf'
# The build server and tenant images install 60-motor-retention.conf. Boat's
# base image (the provider's Ubuntu image) ships a journald drop-in that sorts
# after that name and sets a shorter retention, so the prepared Boat image
# installs the same 60-motor-retention.conf contents as zz-motor-retention.conf,
# which sorts after names starting with a digit or a lowercase letter.
BOAT_DEST = '/etc/systemd/journald.conf.d/zz-motor-retention.conf'
INOTIFY = '/etc/sysctl.d/60-motor-inotify.conf'


def module(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    if spec is None or spec.loader is None:
        raise ImportError('cannot load ' + relative)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


class JournalRetentionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_shared_file_has_the_declared_retention_policy(self):
        self.assertEqual(
            '# Host logs (the systemd journal) are kept for at most 14 days. The privacy page\n'
            '# states this limit, so every host type motor runs on carries it: the build\n'
            '# server, hosted hosts, tenant VMs and Boat sandboxes. The existing size cap\n'
            '# still applies, whichever limit is reached first. Install on a host with:\n'
            '#   sudo install -D -m 0644 60-motor-retention.conf /etc/systemd/journald.conf.d/60-motor-retention.conf\n'
            '#   sudo systemctl restart systemd-journald\n'
            '[Journal]\nMaxRetentionSec=14day\n', (ROOT / SOURCE).read_text())

    def assert_installed(self, dest=DEST, source=SOURCE):
        installed = self.root / dest.lstrip('/')
        self.assertEqual((ROOT / source).read_bytes(), installed.read_bytes())
        self.assertEqual(0o644, stat.S_IMODE(installed.stat().st_mode))

    def install(self, *args, **kwargs):
        if args[0] == 'install':
            self.assertEqual(('install', '-D', '-o', 'root', '-g', 'root', '-m', '0644',
                              str(ROOT / SOURCE), getattr(self, 'dest', DEST)), args)
            # Assert the deployment ownership above, but keep fixture files owned
            # by the test user so these tests need no root privileges.
            subprocess.run(['install', '-D', '-m', '0644', args[-2],
                            str(self.root / args[-1].lstrip('/'))], check=True)
        if args[:2] == ('systemd-analyze', 'cat-config'):
            self.assertEqual(('systemd-analyze', 'cat-config', 'systemd/journald.conf'), args)
            return self.config
        return ''

    def test_tenant_installs_exact_root_owned_0644_drop_in(self):
        guest = module('retention_guest', 'script/tenant-image/install-guest.py')
        with patch.object(guest, 'BUILD', ROOT), patch.object(guest, 'run', side_effect=self.install) as run:
            guest.install_journal_retention()
        self.assertEqual(1, run.call_count)
        self.assert_installed()
        self.assertIn('    install_journal_retention()\n', (ROOT / 'script/tenant-image/install-guest.py').read_text())

    def test_boat_installs_restarts_and_checks_effective_retention(self):
        boat = module('retention_boat', 'script/boat-image/install.py')
        self.dest = BOAT_DEST
        self.assertEqual(BOAT_DEST, boat.JOURNAL_RETENTION_DROP_IN)
        self.config = '# /etc/systemd/journald.conf\n[Journal]\nMaxRetentionSec=30day\n' + (ROOT / SOURCE).read_text()
        with patch.object(boat, 'run', side_effect=self.install) as run:
            boat.install_journal_retention()
        self.assertEqual([('install', '-D', '-o', 'root', '-g', 'root', '-m', '0644', str(ROOT / SOURCE), BOAT_DEST),
                          ('systemctl', 'restart', 'systemd-journald'),
                          ('systemd-analyze', 'cat-config', 'systemd/journald.conf')],
                         [call.args for call in run.call_args_list])
        self.assertTrue(run.call_args.kwargs['capture'])
        self.assert_installed(dest=BOAT_DEST)
        main = (ROOT / 'script/boat-image/install.py').read_text().split('def main():', 1)[1]
        self.assertLess(main.index('    install_journal_retention()'), main.index('parity.check('))

    def test_boat_refuses_missing_retention_and_later_overrides(self):
        boat = module('retention_boat_invalid', 'script/boat-image/install.py')
        self.dest = BOAT_DEST
        for config in ('[Journal]\n', '[Journal]\n# MaxRetentionSec=14day\n',
                       '[Other]\nMaxRetentionSec=14day\n',
                       '[Journal]\nMaxRetentionSec=14day\nMaxRetentionSec=30day\n',
                       '[Journal]\nMaxRetentionSec=14day\nMaxRetentionSec=\n'):
            with self.subTest(config=config):
                self.config = config
                with patch.object(boat, 'run', side_effect=self.install), \
                     self.assertRaisesRegex(ValueError, 'journald MaxRetentionSec'):
                    boat.install_journal_retention()

    def shell(self, body, check=0, answers=''):
        log = self.root / 'calls.jsonl'
        helper = self.root / 'install-fixture.py'
        helper.write_text('''import json, os, pathlib, subprocess, sys
args = sys.argv[1:]
with open(os.environ['CALLS'], 'a') as log:
    log.write(json.dumps(args) + '\\n')
if args[0] == 'install':
    command = args[:]
    for flag in ('-o', '-g'):
        index = command.index(flag)
        del command[index:index + 2]
    command[-1] = os.environ['FIXTURE'] + command[-1]
    subprocess.run(command, check=True)
''')
        harness = '''set -euo pipefail
sudo() { test "$1" = -n; shift; "$@"; }
install() { python3 "$HELPER" install "$@"; }
systemctl() { python3 "$HELPER" systemctl "$@"; }
sysctl() { python3 "$HELPER" sysctl "$@"; }
cd() { builtin cd "$CHECKOUT"; }
bash() { test "$*" = 'script/runner/install-tools.sh build-server runtime-only'; }
directory() { mkdir -p "$FIXTURE$1"; }
ok() { :; }
fail() { problems=$((problems + 1)); }
attrs() { printf 'root:root %s\\n' "$(stat -c %a "$1")"; }
checkout=$CHECKOUT
problems=0 skipped=0
'''
        env = dict(os.environ, HELPER=str(helper), FIXTURE=str(self.root),
                   CHECKOUT=str(ROOT), CALLS=str(log))
        result = subprocess.run(['bash', '-c', harness + f'check={check}\n' + body],
                                input=answers, text=True, capture_output=True, env=env, cwd=ROOT)
        self.assertEqual(0, result.returncode, result.stderr + result.stdout)
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def test_runner_setup_installs_both_configs_and_applies_them(self):
        script = (ROOT / 'script/runner/setup.sh').read_text()
        body = script.split('cd "/srv/build/src/$1"', 1)[1].split('\nREMOTE', 1)[0]
        calls = self.shell(body)
        self.assertEqual([
            ['install', '-D', '-o', 'root', '-g', 'root', '-m', '0644', SOURCE, DEST],
            ['systemctl', 'restart', 'systemd-journald'],
            ['install', '-D', '-o', 'root', '-g', 'root', '-m', '0644',
             'script/runner/sysctl/60-motor-inotify.conf', INOTIFY],
            ['sysctl', '-p', INOTIFY]], calls)
        self.assert_installed()
        self.assert_installed(INOTIFY, 'script/runner/sysctl/60-motor-inotify.conf')

    def hosted_functions(self):
        script = (ROOT / 'script/hosted-host/install.sh').read_text()
        functions = []
        for name in ('place', 'host_configs'):
            match = re.search(r'\n' + name + r'\(\) \{\n.*?\n\}', script, re.S)
            self.assertIsNotNone(match)
            assert match is not None
            functions.append(match[0])
        self.assertIn('\nhost_configs\n', script)
        # Redirect only installed paths; the install command is executed on
        # fixture files while its original destination is recorded separately.
        place = functions[0].replace('local kind=$1 src=$2 dest=$3 owner=$4 mode=$5',
            'local kind=$1 src=$2 original=$3 dest=$FIXTURE$3 owner=$4 mode=$5')
        place = place.replace('"$src" "$dest"\n', '"$src" "$original"\n')
        return place + functions[1] + '\nhost_configs\n'

    def test_hosted_installs_both_configs_and_applies_them(self):
        calls = self.shell(self.hosted_functions(), answers='y\ny\n')
        self.assertEqual([
            ['install', '-o', 'root', '-g', 'root', '-m', '644', str(ROOT / SOURCE), DEST],
            ['install', '-o', 'root', '-g', 'root', '-m', '644',
             str(ROOT / 'script/runner/sysctl/60-motor-inotify.conf'), INOTIFY],
            ['systemctl', 'restart', 'systemd-journald'], ['sysctl', '-p', INOTIFY]], calls)
        self.assert_installed()
        self.assert_installed(INOTIFY, 'script/runner/sysctl/60-motor-inotify.conf')

    def test_hosted_check_and_declined_configs_do_not_apply_changes(self):
        self.assertEqual([], self.shell(self.hosted_functions(), check=1))
        self.assertEqual([], self.shell(self.hosted_functions(), answers='n\nn\n'))
        self.assertFalse((self.root / DEST.lstrip('/')).exists())


if __name__ == '__main__':
    unittest.main()
