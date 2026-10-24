#!/usr/bin/env python3
"""Exercise the interactive unit installer without writing host system units."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HERE = Path(__file__).resolve().parent


class InstallUnitsTests(unittest.TestCase):
    def test_system_policy_is_installed_privileged_and_user_units_stay_in_home(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / 'home'
            home.mkdir()
            tools = root / 'bin'
            tools.mkdir()
            log = root / 'commands'
            for name in ('sudo', 'systemctl'):
                command = tools / name
                command.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$UNIT_TEST_LOG"\n')
                command.chmod(0o755)
            env = dict(os.environ, HOME=str(home), PATH=str(tools) + ':' + os.environ['PATH'],
                       UNIT_TEST_LOG=str(log))
            result = subprocess.run(['bash', str(HERE / 'install-units.sh')],
                                    input='y\n' * 20, text=True, capture_output=True, env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            commands = log.read_text()
            self.assertIn('install -o root -g root -m 0644 ' +
                          str(HERE / 'units/system/motor-jobs.slice') +
                          ' /etc/systemd/system/motor-jobs.slice', commands)
            self.assertIn('/etc/systemd/system/user@' + str(os.getuid()) +
                          '.service.d/motor-oom.conf', commands)
            self.assertIn('systemctl daemon-reload', commands)
            self.assertIn('--user daemon-reload', commands)
            self.assertNotIn('restart', commands)
            self.assertIn('Drain all runner instances', result.stdout)
            for name in ('motor-runner.service', 'motor-runner@.service'):
                self.assertEqual((home / '.config/systemd/user' / name).read_bytes(),
                                 (HERE / 'units' / name).read_bytes())
            self.assertFalse((home / '.config/systemd/user/system').exists())


if __name__ == '__main__':
    unittest.main()
