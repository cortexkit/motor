#!/usr/bin/env python3
"""Exercise the interactive unit installer without writing host system units."""
import os
from pathlib import Path
import shutil
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

    def test_unflagged_unit_respects_ever_enabled_history(self):
        for instance, spelling in (('main', 'default'), ('e2e', 'default'),
                                   ('main', 'separate'), ('e2e', 'equals'), ('windows', 'separate')):
            for capability in ('absent', 'legacy', 'pass'):
                for ever_enabled in (False, True):
                    with self.subTest(instance=instance, spelling=spelling, capability=capability,
                                      ever_enabled=ever_enabled), tempfile.TemporaryDirectory() as directory:
                        root = Path(directory)
                        source = root / 'runner'
                        shutil.copytree(HERE / 'units', source / 'units')
                        for name in ('install-units.sh', 'deploy_canary.py'):
                            shutil.copyfile(HERE / name, source / name)
                        relative = {'main': 'motor-runner.service',
                                    'e2e': 'motor-runner@e2e.service.d/binary.conf',
                                    'windows': 'motor-runner@windows.service.d/windows.conf'}[instance]
                        unit = source / 'units' / relative
                        state_root = (root / 'data/cortexkit/motor-runner' if spelling == 'default'
                                      else root / 'explicit-state')
                        if spelling != 'default':
                            text = unit.read_text().replace(
                                ' --state-root /srv/build/motor-windows/windows/state', '')
                            option = ' --state-root' + ('=' if spelling == 'equals' else ' ') + str(state_root)
                            unit.write_text('\n'.join(line + option if line.startswith('ExecStart=') and
                                                      ' serve ' in line else line for line in text.splitlines()) + '\n')
                        if ever_enabled:
                            state = state_root / instance
                            state.mkdir(parents=True)
                            (state / 'caller-roots-ever-enabled').touch()
                        tools = root / 'bin'
                        tools.mkdir()
                        for name in ('sudo', 'systemctl'):
                            tool = tools / name
                            tool.write_text('#!/bin/sh\nexit 0\n')
                            tool.chmod(0o755)
                        env = dict(os.environ, HOME=str(root / 'home'), XDG_DATA_HOME=str(root / 'data'),
                                   PATH=str(tools) + ':' + os.environ['PATH'])
                        env.pop('RUNNER_STATE_ROOT', None)
                        if spelling != 'default':
                            env['RUNNER_STATE_ROOT'] = str(root / 'wrong-state')
                        for name in ('main', 'e2e', 'windows'):
                            binary = root / ('binary-' + name)
                            env['RUNNER_BINARY_' + name] = str(binary)
                            if name == instance and capability == 'absent':
                                continue
                            binary.write_text('#!/bin/sh\n' + (
                                "echo 'usage: motor-runner serve|connect' >&2\nexit 1\n"
                                if name == instance and capability == 'legacy' else 'exit 0\n'))
                            binary.chmod(0o755)
                        result = subprocess.run(['bash', str(source / 'install-units.sh')],
                                                input='y\n' * 20, text=True, capture_output=True, env=env)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        dest = root / 'home/.config/systemd/user'
                        refused = ever_enabled and capability != 'pass'
                        self.assertEqual((dest / relative).exists(), not refused, result.stderr)
                        if refused:
                            self.assertIn('caller roots were enabled for ' + instance, result.stderr)
                            other = ('motor-runner@e2e.service.d/binary.conf' if instance == 'main'
                                     else 'motor-runner.service')
                            self.assertTrue((dest / other).exists())

    def test_flagged_unit_uses_its_own_binary_and_refusals_leave_other_units_installable(self):
        for instance in ('main', 'e2e'):
            for capability in ('absent', 'legacy', 'refuse', 'pass'):
                with self.subTest(instance=instance, capability=capability), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    source = root / 'runner'
                    shutil.copytree(HERE / 'units', source / 'units')
                    for name in ('install-units.sh', 'deploy_canary.py'):
                        shutil.copyfile(HERE / name, source / name)
                    relative = ('motor-runner.service' if instance == 'main' else
                                'motor-runner@e2e.service.d/binary.conf')
                    flagged = source / 'units' / relative
                    text = flagged.read_text().splitlines()
                    text = [line + ' --caller-root /home=/srv/build/callers/' + instance + '/home'
                            if line.startswith('ExecStart=') and ' serve ' in line else line
                            for line in text]
                    flagged.write_text('\n'.join(text) + '\n')
                    tools = root / 'bin'
                    tools.mkdir()
                    for name in ('sudo', 'systemctl'):
                        path = tools / name
                        path.write_text('#!/bin/sh\nexit 0\n')
                        path.chmod(0o755)
                    env = dict(os.environ, HOME=str(root / 'home'),
                               PATH=str(tools) + ':' + os.environ['PATH'])
                    log = root / 'checks'
                    for name in ('main', 'e2e', 'windows'):
                        binary = root / ('binary-' + name)
                        env['RUNNER_BINARY_' + name] = str(binary)
                        if name == instance and capability == 'absent':
                            continue
                        binary.write_text(f'''#!/bin/bash
echo '{name} '"$*" >> {str(log)!r}
if [[ "$*" = *--caller-root* ]]; then
  case {capability if name == instance else 'pass'} in
    legacy) echo 'usage: motor-runner serve|connect' >&2; exit 1;;
    refuse) echo 'fixture instance refusal' >&2; exit 1;;
  esac
fi
''')
                        binary.chmod(0o755)
                    result = subprocess.run(['bash', str(source / 'install-units.sh')],
                                            input='y\n' * 20, text=True, capture_output=True, env=env)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    dest = root / 'home/.config/systemd/user'
                    self.assertEqual((dest / relative).exists(), capability == 'pass')
                    other = ('motor-runner@e2e.service.d/binary.conf' if instance == 'main'
                             else 'motor-runner.service')
                    self.assertEqual((dest / other).read_bytes(), (source / 'units' / other).read_bytes())
                    self.assertTrue((dest / 'motor-runner@windows.service.d/windows.conf').exists())
                    if capability != 'absent':
                        self.assertIn(f'{instance} check-options --instance {instance}', log.read_text())
                        self.assertIn('--caller-root /home=/srv/build/callers/' + instance + '/home',
                                      log.read_text())
                    if capability != 'pass':
                        self.assertIn('skipped ' + relative, result.stderr)
                        self.assertIn({'absent': 'binary is absent', 'legacy': 'lacks check-options',
                                       'refuse': 'fixture instance refusal'}[capability], result.stderr)


if __name__ == '__main__':
    unittest.main()
