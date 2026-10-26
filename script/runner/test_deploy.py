"""Run with python3 -m unittest -v script/runner/test_deploy.py."""
import json
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import socket
import select
import threading
import time
import base64
import hashlib
import tempfile
import unittest
from unittest import mock

DEPLOY = Path(__file__).with_name("deploy.sh").resolve()
policy_spec = importlib.util.spec_from_file_location('runner_permissions_policy', DEPLOY.with_name('permissions_policy.py'))
if policy_spec is None or policy_spec.loader is None:
    raise ImportError('cannot load permissions sudo policy renderer')
policy = importlib.util.module_from_spec(policy_spec)
policy_spec.loader.exec_module(policy)
canary_spec = importlib.util.spec_from_file_location('deploy_canary', DEPLOY.with_name('deploy_canary.py'))
if canary_spec is None or canary_spec.loader is None:
    raise ImportError('cannot load deployment canary')
canary = importlib.util.module_from_spec(canary_spec)
canary_spec.loader.exec_module(canary)


class PermissionsPolicyTests(unittest.TestCase):
    def test_only_fixed_native_operations_are_granted_to_the_trusted_operator(self):
        template = DEPLOY.with_name('permissions-helper.sudoers').read_text()
        accounts = {'ubuntu':1000,'motor-job':997,'motor-cache':996,'alias':997,'root':0}
        rendered = policy.render(template, 'ubuntu', accounts.__getitem__)
        row = next(line for line in rendered.splitlines() if 'NOPASSWD:' in line)
        self.assertEqual(row.split('NOPASSWD: ',1)[1].split(', '), [
            '/usr/local/libexec/motor-runner-permissions physical-chmod owner-rwX -- *',
            '/usr/local/libexec/motor-runner-permissions physical-chmod private-rwX -- *'])
        for operator in ('motor-job','motor-cache','alias','root','ubuntu\nALL=(ALL) ALL'):
            with self.subTest(operator=operator), self.assertRaises(ValueError):
                policy.render(template, operator, accounts.__getitem__)
        with self.assertRaises(ValueError):
            policy.render(template.replace('physical-chmod owner-rwX', 'serve'), 'ubuntu', accounts.__getitem__)
        with self.assertRaises(ValueError):
            policy.render(template + '\n#include /tmp/untrusted\n', 'ubuntu', accounts.__getitem__)


class UnitCommandTests(unittest.TestCase):
    def test_repository_units_and_boat_wrapper_extract_only_serve_options(self):
        units = DEPLOY.with_name('units')
        for instance, base, dropin, binary in (
                ('main', 'motor-runner.service', None, 'motor-runner'),
                ('e2e', 'motor-runner@.service', 'motor-runner@e2e.service.d/binary.conf', 'motor-runner-e2e'),
                ('windows', 'motor-runner@.service', 'motor-runner@windows.service.d/windows.conf', 'motor-runner-windows')):
            with self.subTest(instance=instance):
                text = (units / base).read_text()
                if dropin:
                    text += '\n' + (units / dropin).read_text()
                command = canary.unit_command(text, instance)
                self.assertEqual(command[0], '/home/ubuntu/.cargo/bin/' + binary)
                self.assertEqual(command[1:3], ['--instance', instance])
                self.assertNotIn('serve', command)
                self.assertNotIn('%i', command)
        boat = DEPLOY.parents[1] / 'boat-image/motor-boat.service'
        command = canary.unit_command(boat.read_text() +
                                      '\nExecStartPost=/ignored serve --wrong\nExecStart=\n', 'boat')
        self.assertEqual(command[0], '/usr/bin/python3')
        self.assertEqual(command[1:3], ['--boat-profile', '--tenant-vm'])
        self.assertIn('--instance', command)
        self.assertIn('boat', command)
        self.assertNotIn('motor-boat-start.py', ' '.join(command[1:]))
        self.assertNotIn('--wrong', command)

    def test_extraction_preserves_quoted_words_and_rejects_missing_serve(self):
        self.assertEqual(canary.unit_command(
            'ExecStart=/binary serve --state-root "/space dir" --instance %i\n', 'e2e'),
            ['/binary', '--state-root', '/space dir', '--instance', 'e2e'])
        with self.assertRaises(ValueError):
            canary.unit_command('ExecStart=/binary connect\n', 'main')

    def test_repository_dropins_merge_by_name_and_instance_specific_file_wins(self):
        with tempfile.TemporaryDirectory() as tmp:
            units = Path(tmp)
            (units / 'motor-runner@.service').write_text('ExecStart=/template serve --instance %i\n')
            generic = units / 'motor-runner@.service.d'
            specific = units / 'motor-runner@e2e.service.d'
            generic.mkdir()
            specific.mkdir()
            (specific / '10-first.conf').write_text('ExecStart=/first serve --instance %i\n')
            (generic / '90-last.conf').write_text('ExecStart=/generic serve --instance %i\n')
            self.assertEqual(canary.repository_unit_command(units, 'e2e'),
                             ['/generic', '--instance', 'e2e'])
            (specific / '90-last.conf').write_text('ExecStart=/e2e serve --instance %i --caller-root /home=/host\n')
            self.assertEqual(canary.repository_unit_command(units, 'e2e'),
                             ['/e2e', '--instance', 'e2e', '--caller-root', '/home=/host'])


class SetupCallerRootsTests(unittest.TestCase):
    def test_roots_match_users_and_unnamed_reflink_probe_leaves_them_empty(self):
        import fcntl
        import stat

        script = DEPLOY.with_name('setup-caller-roots.sh')
        body = script.read_text().split("<<'PY'\n", 1)[1].rsplit('\nPY', 1)[0]
        self.assertIn('"$script_dir/setup-caller-roots.sh"', DEPLOY.with_name('setup.sh').read_text())
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            users = root / 'Users'
            users.mkdir(mode=0o751)
            body = body.replace('/srv/build', str(root))
            opened = []
            real_open = os.open
            tmpfile = getattr(os, 'O_TMPFILE')

            def open_unnamed(path, flags, mode):
                self.assertEqual(flags & tmpfile, tmpfile)
                opened.append(Path(path))
                return real_open(path, flags, mode)

            def clone(dst, operation, src):
                self.assertEqual(operation, 0x40049409)
                os.write(dst, os.pread(src, 4096, 0))
                os.lseek(dst, 0, os.SEEK_SET)

            with mock.patch('os.open', side_effect=open_unnamed), mock.patch.object(fcntl, 'ioctl', side_effect=clone):
                exec(compile(body, str(script), 'exec'), {})
            self.assertEqual(opened, [users, root / 'callers/main/home', users, root / 'callers/e2e/home'])
            for instance in ('main', 'e2e'):
                host = root / 'callers' / instance / 'home'
                self.assertEqual(list(host.iterdir()), [])
                self.assertEqual(stat.S_IMODE(host.stat().st_mode), 0o751)
                self.assertEqual((host.stat().st_uid, host.stat().st_gid),
                                 (users.stat().st_uid, users.stat().st_gid))
            leftover = root / 'callers/main/home/leftover'
            leftover.write_text('preserve')
            with self.assertRaisesRegex(SystemExit, 'requires an empty directory'):
                exec(compile(body, str(script), 'exec'), {})
            self.assertEqual(leftover.read_text(), 'preserve')


class DeployTests(unittest.TestCase):
    def run_deploy(self, job, *, resume=True, drain="timeout", fail_start=False,
                    instance="main", default_state_root=False, decoys=False, drain_timeout=None,
                    helper_valid: 'bool | str' = True, canary='pass', canary_timeout=None, quarantine=None,
                    caller_roots=False, check_options='pass', ever_enabled=False, home_canary='pass',
                    installed_options=None, different_state_override=False):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            state_root = (root / "motor-windows/windows/state" if instance == "windows"
                          else root / "state")
            state = state_root / instance / "jobs/job"
            state.mkdir(parents=True)
            if ever_enabled:
                (state_root / instance / 'caller-roots-ever-enabled').touch()
            unit = root / 'installed-unit'
            options = (installed_options or '--instance %i').replace('$STATE_ROOT', str(state_root))
            if caller_roots:
                options += ' --caller-root /home=/srv/build/callers/' + instance + '/home'
            unit.write_text('ExecStart=/old-runner serve --obsolete\nExecStart=\n' +
                            'ExecStart=/installed-runner serve ' + options + '\n')
            original_unit = unit.read_bytes()
            (root / 'admission').write_text('open')
            (state / "record.json").write_text(json.dumps(job))
            if quarantine is not None:
                folder = state_root / instance / 'quarantine'
                folder.mkdir()
                for name in quarantine:
                    (folder / name).write_text('{}')
            if decoys:
                for decoy in (root / "state/windows/jobs/wrong-root",
                              state_root / "main/jobs/wrong-instance"):
                    decoy.mkdir(parents=True)
                    (decoy / "record.json").write_text(json.dumps(
                        self.job(id=decoy.name, process=None)))
            admin = '''#!/usr/bin/env bash
echo "$*" >> "$LOG"
case "$1" in
--version) echo motor-runner-test;;
permission-helper-digest)
  if [ "$HELPER_VALID" = legacy ]; then echo 'usage: motor-runner serve|connect' >&2; exit 1; fi
  echo native-digest;;
check-permissions-helper) [ "$HELPER_VALID" = true ] || exit 1;;
check-options)
  case "$CHECK_OPTIONS" in
    legacy) echo 'usage: motor-runner serve|connect' >&2; exit 1;;
    refuse) echo 'fixture refusal: incompatible caller root' >&2; exit 1;;
  esac;;
drain)
  echo closed > "$ADMISSION"
  if [ "$DRAIN" = complete ]; then
    echo 'runner drain ended without a deadline timeout; leaving 1 restart-recoverable jobs to reattach'
    exit 0
  fi
  if [ "$DRAIN" = timeout ]; then
    echo 'drain deadline exceeded; surviving jobs will be recovered on restart' >&2
  else
    echo 'drain transport deadline exceeded' >&2
  fi
  exit 1;;
resume)
  [ "$RESUME" = true ] || { echo 'unknown frame resume' >&2; exit 1; }
  echo open > "$ADMISSION";;
esac
'''
            candidate, live = root / "candidate", root / "live"
            for path in (candidate, live):
                path.write_text(admin + f'\n# {path.name}\n')
                path.chmod(0o755)
            original_live = live.read_bytes()
            python = bin_dir / 'python3'
            python.write_text(f'''#!/usr/bin/env bash
if [[ "$1" = */deploy_canary.py && "${{2:-}}" != --unit-command ]]; then
  echo "canary $*" >> "$LOG"
  if [[ "$*" = *--home-compile-check* ]]; then CANARY=$HOME_CANARY; fi
  [ "$CANARY" != inconclusive ] || {{ echo 'canary inconclusive: no free permit' >&2; exit 75; }}
  [ "$CANARY" != invalid-config ] || exit 2
  [ "$CANARY" = pass ] || {{ echo "canary $CANARY" >&2; exit 1; }}
else
  exec {sys.executable!r} "$@"
fi
''')
            python.chmod(0o755)
            systemctl = bin_dir / "systemctl"
            systemctl.write_text('''#!/usr/bin/env bash
if [ "$2" = cat ]; then cat "$INSTALLED_UNIT"; exit; fi
echo "systemctl $*" >> "$LOG"
if [ "$2" = start ]; then
  if [ "$FAIL_START" = true ] && [ ! -f "$START_FAILED" ]; then
    touch "$START_FAILED"; exit 1
  fi
  echo open > "$ADMISSION"
fi
''')
            systemctl.chmod(0o755)
            env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}",
                       RUNNER_LIVE_BINARY=str(live), RUNNER_STATE_ROOT=str(state_root),
                       LOG=str(root / "log"), ADMISSION=str(root / "admission"),
                       RESUME=str(resume).lower(), DRAIN=drain,
                       FAIL_START=str(fail_start).lower(), START_FAILED=str(root / "failed"))
            env['HELPER_VALID'] = str(helper_valid).lower()
            env['CANARY'] = canary
            env['HOME_CANARY'] = home_canary
            env['CHECK_OPTIONS'] = check_options
            env['INSTALLED_UNIT'] = str(unit)
            if different_state_override:
                env['RUNNER_STATE_ROOT'] = str(root / 'wrong-state')
            env.pop('RUNNER_CANARY_TIMEOUT_S', None)
            if canary_timeout is not None:
                env['RUNNER_CANARY_TIMEOUT_S'] = str(canary_timeout)
            if default_state_root:
                env.pop("RUNNER_STATE_ROOT")
            env.pop("RUNNER_DRAIN_TIMEOUT_S", None)
            if drain_timeout is not None:
                env["RUNNER_DRAIN_TIMEOUT_S"] = str(drain_timeout)
            command = ["bash", str(DEPLOY), str(candidate)]
            if instance != "main":
                command.append(instance)
            result = subprocess.run(command, env=env,
                                    capture_output=True, text=True, timeout=10)
            log = (root / 'log').read_text()
            log += f'live restored: {live.read_bytes() == original_live}\n'
            log += f'unit unchanged: {unit.read_bytes() == original_unit}\n'
            log += f'staged executable: {live.with_name("live.new").exists()}\n'
            log += f'rollback copies: {len(list(root.glob("live.rollback-*")))}\n'
            return result, log, (root / "admission").read_text().strip()

    def job(self, **extra):
        return dict(dict(id="survivor-job", state="Running", terminal=None, started_ms=1), **extra)

    def test_check_options_gate_refuses_before_drain_without_touching_unit_or_executable(self):
        for flagged, sentinel, capability in ((False, False, 'refuse'), (True, False, 'legacy'),
                                             (False, True, 'legacy')):
            with self.subTest(flagged=flagged, sentinel=sentinel, capability=capability):
                result, log, admission = self.run_deploy(
                    self.job(), drain='complete', caller_roots=flagged,
                    ever_enabled=sentinel, check_options=capability)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn('candidate', result.stderr)
                self.assertIn('fixture refusal' if capability == 'refuse' else
                              'downgrade to a pre-mapping binary is unsupported', result.stderr)
                self.assertNotIn('drain --', log)
                self.assertNotIn('systemctl', log)
                self.assertIn('live restored: True', log)
                self.assertIn('unit unchanged: True', log)
                self.assertIn('staged executable: False', log)
                self.assertIn('rollback copies: 0', log)
                self.assertEqual(admission, 'open')

    def test_legacy_check_options_without_mapping_history_proceeds(self):
        result, log, _ = self.run_deploy(self.job(), drain='complete', check_options='legacy')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('check-options --instance main\n', log)
        self.assertIn('drain --', log)

    def test_installed_state_root_controls_sentinel_even_with_an_environment_override(self):
        result, log, _ = self.run_deploy(
            self.job(), drain='complete', check_options='legacy', ever_enabled=True,
            installed_options='--instance %i --state-root $STATE_ROOT', different_state_override=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('downgrade to a pre-mapping binary is unsupported', result.stderr)
        self.assertNotIn('drain --', log)

    def test_check_options_receives_last_installed_execstart_and_runs_before_drain(self):
        for instance in ('main', 'e2e', 'windows'):
            with self.subTest(instance=instance):
                result, log, _ = self.run_deploy(self.job(), drain='complete', instance=instance)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f'check-options --instance {instance}\n', log)
                self.assertNotIn('--obsolete', log)
                self.assertLess(log.index('check-options'), log.index('drain --'))

    def test_home_canary_is_conditional_and_a_home_only_failure_rolls_back(self):
        for flagged in (False, True):
            with self.subTest(flagged=flagged):
                result, log, _ = self.run_deploy(self.job(), drain='complete',
                                               caller_roots=flagged, home_canary='exit-1')
                self.assertEqual(result.returncode, 1 if flagged else 0, result.stderr)
                self.assertEqual(log.count('canary '), 2 if flagged else 1)
                if flagged:
                    self.assertIn('--home-compile-check', log)
                    self.assertIn('Mode: /home', result.stderr)
                    self.assertIn('flag-off rollback', result.stderr)
                    self.assertIn('forget-caller-root', result.stderr)
                    self.assertIn('live restored: True', log)

    def test_home_canary_capacity_is_inconclusive_and_names_mode(self):
        result, log, _ = self.run_deploy(self.job(), drain='complete', caller_roots=True,
                                       home_canary='inconclusive')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Mode: /home', result.stderr)
        self.assertIn('live restored: False', log)
        self.assertEqual(log.count('canary '), 2)

    def test_swap_and_rollback_warn_with_quarantine_folder_listing(self):
        for canary in ('pass', 'exit-1'):
            for instance in ('main', 'e2e'):
                with self.subTest(canary=canary, instance=instance):
                    result, _, _ = self.run_deploy(self.job(), drain='complete', canary=canary,
                                                  instance=instance, quarantine=['damaged-job.json'])
                    self.assertEqual(result.returncode, 0 if canary == 'pass' else 1)
                    self.assertIn('WARNING: runner quarantine is not empty:', result.stderr)
                    self.assertIn(f'/{instance}/quarantine', result.stderr)
                    self.assertIn('damaged-job.json', result.stderr)

    def test_empty_or_missing_quarantine_does_not_warn(self):
        for quarantine in (None, []):
            with self.subTest(quarantine=quarantine):
                result, _, _ = self.run_deploy(self.job(), drain='complete', quarantine=quarantine)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn('WARNING: runner quarantine', result.stderr)

    def test_timeout_recoverable_unit_proceeds_to_swap(self):
        result, log, admission = self.run_deploy(self.job(request={}, process=dict(unit="fake-unit", pid=42)))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("survivor-job: restart-recoverable", result.stderr)
        self.assertIn("all survivors are restart-recoverable", result.stderr)
        self.assertIn("systemctl --user stop", log)
        self.assertEqual(admission, "open")

    def test_missing_helper_grant_aborts_before_drain_or_stop(self):
        result, log, admission = self.run_deploy(self.job(), drain='complete', helper_valid=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('check-permissions-helper', log)
        self.assertNotIn('drain --', log)
        self.assertNotIn('systemctl', log)
        self.assertEqual(admission, 'open')

    def test_legacy_candidate_without_native_helper_still_drains_normally(self):
        result, log, admission = self.run_deploy(self.job(), drain='complete', helper_valid='legacy')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('check-permissions-helper', log)
        self.assertIn('drain --', log)
        self.assertEqual(admission, 'open')

    def test_completed_drain_reports_reattach_count_and_uses_ten_minute_default(self):
        result, log, admission = self.run_deploy(
            self.job(process=dict(unit="fake-unit", pid=42)), drain="complete")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("leaving 1 restart-recoverable jobs to reattach", result.stdout)
        self.assertIn("--drain-timeout-s 600 --exit-after-drain false", log)
        self.assertNotIn("survivor", result.stderr)
        self.assertIn("systemctl --user stop", log)
        self.assertEqual(admission, "open")

    def test_drain_timeout_override_is_preserved(self):
        result, log, _ = self.run_deploy(
            self.job(process=dict(unit="fake-unit", pid=42)), drain="complete", drain_timeout=90)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--drain-timeout-s 90 --exit-after-drain false", log)

    def test_timeout_started_windows_unit_reopens_admission(self):
        job = self.job(request=dict(platform="windows"),
                       process=dict(unit="fake-unit", pid=42))
        result, log, admission = self.run_deploy(job)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("survivor-job: UNSAFE", result.stderr)
        self.assertIn("Deploy aborted: unsafe survivors", result.stderr)
        self.assertIn("resume --instance main", log)
        self.assertNotIn("systemctl", log)
        self.assertEqual(admission, "open")

    def test_windows_instance_inventories_its_own_jobs(self):
        job = self.job(id="windows-queued", state="Queued", started_ms=None,
                       request=dict(platform="windows"), process=None)
        result, log, admission = self.run_deploy(job, instance="windows", decoys=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("windows-queued: restart-recoverable", result.stderr)
        self.assertNotIn("wrong-root", result.stderr)
        self.assertNotIn("wrong-instance", result.stderr)
        self.assertIn("drain --instance windows --state-root", log)
        self.assertIn("systemctl --user stop motor-runner@windows.service", log)
        self.assertIn("systemctl --user start motor-runner@windows.service", log)
        self.assertEqual(admission, "open")

    def test_windows_instance_defaults_to_host_state_root(self):
        result, log, admission = self.run_deploy(
            self.job(process=None), instance="windows", default_state_root=True,
            drain="transport")
        self.assertNotEqual(result.returncode, 0)
        flags = "--instance windows --state-root /srv/build/motor-windows/windows/state"
        self.assertIn(f"drain {flags} --drain-timeout-s", log)
        self.assertIn(f"resume {flags}\n", log)
        self.assertNotIn("systemctl", log)
        self.assertEqual(admission, "open")

    def test_timeout_unsafe_survivor_reopens_admission(self):
        result, log, admission = self.run_deploy(self.job(process=dict(unit=None, pid=os.getpid())))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("survivor-job: UNSAFE", result.stderr)
        self.assertIn("resume --instance main", log)
        self.assertNotIn("systemctl --user stop", log)
        self.assertEqual(admission, "open")

    def test_legacy_resume_refusal_reports_manual_recovery_without_signal(self):
        result, log, admission = self.run_deploy(self.job(process=None), resume=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("survivor-job: UNSAFE", result.stderr)
        self.assertIn("Manual recovery:", result.stderr)
        self.assertNotIn("systemctl", log)
        self.assertEqual(admission, "closed")

    def test_unacknowledged_drain_reopens_admission(self):
        result, log, admission = self.run_deploy(self.job(process=None), drain="transport")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resume --instance main", log)
        self.assertEqual(admission, "open")

    def test_failed_swap_start_rolls_back_and_reopens(self):
        result, log, admission = self.run_deploy(self.job(process=dict(unit="fake", pid=42)), fail_start=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(log.count("systemctl --user start"), 2)
        self.assertIn("resume --instance main", log)
        self.assertEqual(admission, "open")

    def test_post_swap_canary_runs_on_main_and_e2e_before_success(self):
        for instance in ('main', 'e2e'):
            with self.subTest(instance=instance):
                result, log, admission = self.run_deploy(
                    self.job(), drain='complete', instance=instance)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f'--instance {instance} --timeout-s 120', log)
                self.assertLess(log.index('systemctl --user is-active'), log.index('canary '))
                self.assertIn('live restored: False', log)
                self.assertEqual(admission, 'open')

    def test_post_swap_canary_failure_stops_candidate_and_restores_old_executable(self):
        for canary in ('refused-before-start', 'exit-1', 'deadline-exceeded', 'invalid-config'):
            with self.subTest(canary=canary):
                result, log, admission = self.run_deploy(
                    self.job(), drain='complete', canary=canary)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('Post-swap isolated canary failed; rolling back', result.stderr)
                self.assertIn('restored rollback executable', result.stderr)
                self.assertNotIn('deployed motor-runner', result.stdout)
                self.assertEqual(log.count('systemctl --user stop'), 2)
                self.assertEqual(log.count('systemctl --user start'), 2)
                self.assertIn('live restored: True', log)
                self.assertEqual(admission, 'open')

    def test_capacity_inconclusive_keeps_candidate_without_rollback(self):
        result, log, admission = self.run_deploy(
            self.job(), drain='complete', canary='inconclusive')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('canary inconclusive: no free permit', result.stderr)
        self.assertIn('keeping the new runner (isolation not verified)', result.stderr)
        self.assertEqual(log.count('systemctl --user stop'), 1)
        self.assertEqual(log.count('systemctl --user start'), 1)
        self.assertIn('live restored: False', log)
        self.assertEqual(admission, 'open')

    def test_canary_deadline_override_is_preserved(self):
        result, log, _ = self.run_deploy(self.job(), drain='complete', canary_timeout=180)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--timeout-s 180', log)

    def test_windows_explicitly_skips_the_linux_isolated_canary(self):
        result, log, _ = self.run_deploy(self.job(), drain='complete', instance='windows')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('canary ', log)
        self.assertIn('explicitly skipped for windows', result.stderr)

    def test_proven_not_started_is_restart_recoverable(self):
        result, _, admission = self.run_deploy(dict(id="queued", state="Queued", terminal=None, started_ms=None, process=None))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(admission, "open")


class CanaryTests(unittest.TestCase):
    def exercise(self, outcome: 'str | list[str]' = 'pass', instance='main', *,
                 timeout_s: 'int | None' = None, invocations=1, gate_jobs=True,
                 home_compile_check=False):
        outcomes = outcome if isinstance(outcome, list) else [outcome]
        timeout_s = timeout_s or (1 if outcomes[-1] in (
            'timeout', 'no-terminal', 'runner_full', 'runner_draining', 'queue_wait_exceeded') else 5)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            state = root / instance
            state.mkdir()
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(state / 'runner.sock'))
            listener.listen()
            listener.settimeout(0.1)
            errors = []
            requests = []
            stop = threading.Event()

            def handle(peer, outcome):
                hello = canary.receive(peer)
                self.assertTrue(hello['started'])
                self.assertTrue(hello['gate_jobs'])
                self.assertNotIn('default_branch_tip', hello)
                canary.send(peer, dict(type='hello', protocol_version=1, gate_jobs=gate_jobs))
                request = canary.receive(peer)
                requests.append(request)
                self.assertEqual(request['weight_hint'], 1)
                if gate_jobs:
                    self.assertTrue(request['gate_job'])
                else:
                    self.assertNotIn('gate_job', request)
                self.assertEqual(request['timeout'], 5)
                self.assertGreaterEqual(request['queue_wait_limit_s'], 1)
                self.assertLessEqual(request['queue_wait_limit_s'], min(10, timeout_s))
                self.assertEqual(request['command'], 'test "$(id -un)" = motor-job' +
                                 (' && rustc "$PWD/canary.rs"' if home_compile_check else ''))
                self.assertEqual(request['env'], {})
                self.assertNotEqual(request.get('workspace_kind'), 'head')
                if outcome in ('runner_full', 'runner_draining', 'refused',
                               'bundle_rejected', 'tree_hash_mismatch'):
                    reason = 'workspace_setup_failed' if outcome == 'refused' else outcome
                    canary.send(peer, dict(type='refusal', job_id=request['job_id'], reason=reason))
                    return
                if outcome == 'timeout':
                    # Wait for the bounded client to close; no synthetic
                    # success frame can accidentally satisfy the canary.
                    while peer.recv(4096):
                        pass
                    return
                job = request['job_id']
                canary.send(peer, dict(type='accepted', job_id=job))
                chunk = canary.receive(peer)
                bundle = base64.b64decode(chunk['data'])
                self.assertEqual(chunk['type'], 'bundle_chunk')
                self.assertEqual(len(bundle), request['bundles'][0]['length'])
                self.assertEqual(hashlib.sha256(bundle).hexdigest(), request['bundles'][0]['sha256'])
                bundle_path = root / 'received.bundle'
                bundle_path.write_bytes(bundle)
                refs = subprocess.check_output(['git', 'bundle', 'list-heads', str(bundle_path)], text=True)
                self.assertIn(request['snapshot_commit'], refs)
                self.assertEqual(request['integration_tip'], request['snapshot_commit'])
                if home_compile_check:
                    self.assertEqual(request['workspace_key'],
                                     f'/home/motor-deploy-canary/{instance}/workspace')
                    checkout = root / f'checkout-{len(requests)}'
                    checkout.mkdir()
                    subprocess.run(['git', '-C', str(checkout), 'init', '--quiet'],
                                   check=True, capture_output=True)
                    subprocess.run(['git', '-C', str(checkout), 'fetch', '--quiet',
                                    str(bundle_path), refs.split()[1]], check=True, capture_output=True)
                    subprocess.run(['git', '-C', str(checkout), 'checkout', '--quiet',
                                    request['snapshot_commit']], check=True, capture_output=True)
                    self.assertEqual((checkout / 'canary.rs').read_text(),
                                     'fn main() { let _: () = 1; }\n')
                if outcome in ('withdraw', 'withdraw-delayed', 'withdraw-race'):
                    before = time.monotonic()
                    canary.send(peer, dict(type='heartbeat',
                                           queue_wait_ms=request['queue_wait_limit_s'] * 1000 -
                                           (200 if outcome == 'withdraw-delayed' else 0)))
                    peer.settimeout(2)
                    self.assertEqual(canary.next_record(peer),
                                     dict(type='withdraw', job_id=job))
                    if outcome == 'withdraw-delayed':
                        self.assertGreater(time.monotonic() - before, 0.1)
                    if outcome == 'withdraw-race':
                        canary.send(peer, dict(type='withdraw_reply', outcome='started'))
                    else:
                        canary.send(peer, dict(type='withdraw_reply', outcome='withdrawn'))
                        canary.send(peer, dict(type='terminal', job_id=job,
                                              outcome=dict(type='refused_before_start',
                                                           reason='queue_wait_exceeded')))
                        return
                if outcome == 'untimed':
                    canary.send(peer, dict(type='heartbeat',
                                           queue_wait_ms=request['queue_wait_limit_s'] * 1000 - 200))
                    canary.send(peer, dict(type='heartbeat'))
                    until = time.monotonic() + 0.4
                    while select.select([peer], [], [], max(0, until - time.monotonic()))[0]:
                        self.assertEqual(canary.receive(peer)['type'], 'heartbeat',
                                         'untimed stream must not withdraw')
                        if time.monotonic() >= until:
                            break
                if outcome in ('queue_wait_exceeded', 'setup-failed'):
                    reason = ('workspace_setup_failed' if outcome == 'setup-failed'
                              else outcome)
                    canary.send(peer, dict(type='terminal', job_id=job,
                                          outcome=dict(type='refused_before_start', reason=reason)))
                    return
                if outcome == 'heartbeats':
                    for started in (False, True):
                        if started:
                            canary.send(peer, dict(type='started', job_id=job))
                        received = []
                        limit = time.monotonic() + 3
                        while len(received) < 2 and time.monotonic() < limit:
                            canary.send(peer, dict(type='heartbeat'))
                            if select.select([peer], [], [], 0.1)[0]:
                                self.assertEqual(canary.receive(peer)['type'], 'heartbeat')
                                received.append(time.monotonic())
                        self.assertEqual(len(received), 2)
                        self.assertGreater(received[1] - received[0], 0.5)
                        self.assertLess(received[1] - received[0], 2)
                if outcome == 'queued':
                    while peer.recv(4096):
                        pass
                    return
                if outcome not in ('no-start', 'heartbeats'):
                    canary.send(peer, dict(type='started', job_id=job))
                if outcome == 'no-terminal':
                    while peer.recv(4096):
                        pass
                    return
                if home_compile_check and outcome not in ('no-diagnostic', 'exit-0'):
                    path = (request['workspace_key'] if outcome != 'host-diagnostic' else
                            '/srv/build/callers/main/home/motor-deploy-canary/main/workspace')
                    diagnostic = (f'error[E0308]: mismatched types\n --> {path}/canary.rs:1:25\n'
                                  if outcome != 'wrong-error' else
                                  f'error: could not read {path}/canary.rs\n').encode()
                    for seq, data in enumerate((diagnostic[:20], diagnostic[20:])):
                        canary.send(peer, dict(type='output', seq=seq, stream='stderr',
                                               bytes=base64.b64encode(data).decode()))
                terminal = dict(type='exit', code=(1 if outcome == 'exit-1' or
                                (home_compile_check and outcome != 'exit-0') else 0))
                canary.send(peer, dict(type='terminal', job_id=job, outcome=terminal))

            def server():
                try:
                    with listener:
                        while not stop.is_set():
                            try:
                                peer, _ = listener.accept()
                            except socket.timeout:
                                continue
                            with peer:
                                index = min(len(requests), len(outcomes) - 1)
                                handle(peer, outcomes[index])
                except Exception as error:
                    errors.append(error)
            thread = threading.Thread(target=server, daemon=True)
            thread.start()
            results = []
            try:
                for _ in range(invocations):
                    results.append(subprocess.run(
                        [sys.executable, str(DEPLOY.with_name('deploy_canary.py')),
                         '--state-root', str(root), '--instance', instance,
                         '--timeout-s', str(timeout_s)] +
                        (['--home-compile-check'] if home_compile_check else []),
                        capture_output=True, text=True, timeout=timeout_s + 5))
            finally:
                stop.set()
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
            if errors:
                raise errors[0]
            if invocations > 1:
                for result in results:
                    self.assertEqual(result.returncode, 0, result.stderr)
            return results[-1], requests

    def test_home_compile_check_requires_exit_one_and_caller_path_rustc_error(self):
        for outcome in ('pass', 'exit-0', 'no-diagnostic', 'host-diagnostic', 'wrong-error', 'no-start'):
            with self.subTest(outcome=outcome):
                result, _ = self.exercise(outcome, home_compile_check=True)
                self.assertEqual(result.returncode, 0 if outcome == 'pass' else 1, result.stderr)
                if outcome == 'pass':
                    self.assertIn('/home compile-check passed', result.stdout)

    def test_canary_transfers_a_real_bundle_and_requires_isolated_start_and_zero_exit(self):
        for instance in ('main', 'e2e'):
            with self.subTest(instance=instance):
                result, requests = self.exercise(instance=instance)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('started and exited 0', result.stdout)
                self.assertIn(f'/{instance}/', requests[0]['workspace_key'])

    def test_canary_rejects_admission_refusal_setup_failure_nonzero_and_missing_start(self):
        for outcome in ('refused', 'setup-failed', 'bundle_rejected', 'tree_hash_mismatch',
                        'exit-1', 'no-start'):
            with self.subTest(outcome=outcome):
                result, _ = self.exercise(outcome)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotEqual(result.returncode, canary.INCONCLUSIVE)
                self.assertIn('canary', result.stderr)

    def test_canary_deadline_bounds_a_daemon_that_never_accepts(self):
        result, _ = self.exercise('timeout')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('deployment canary deadline exceeded', result.stderr)

    def test_canary_sends_heartbeats_while_queued_and_started_despite_runner_heartbeats(self):
        result, _ = self.exercise('heartbeats', timeout_s=8)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_canary_omits_gate_mark_without_runner_capability(self):
        result, _ = self.exercise(gate_jobs=False)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_accepted_but_queued_at_deadline_is_inconclusive(self):
        result, _ = self.exercise('queued', timeout_s=2)
        self.assertEqual(result.returncode, canary.INCONCLUSIVE, result.stderr)
        self.assertIn('isolation not verified', result.stderr)

    def test_queue_wait_budget_sends_withdraw_and_retries_capacity(self):
        for outcome in ('withdraw', 'withdraw-delayed'):
            with self.subTest(outcome=outcome):
                result, requests = self.exercise([outcome, 'pass'], timeout_s=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(requests), 2)
                self.assertIn('queue_wait_exceeded', result.stderr)

    def test_missing_queue_duration_clears_withdrawal_deadline(self):
        result, _ = self.exercise('untimed')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_withdrawal_that_races_start_still_requires_successful_execution(self):
        result, _ = self.exercise('withdraw-race')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_capacity_and_draining_refusals_are_inconclusive_not_fatal(self):
        for reason in ('runner_full', 'runner_draining', 'queue_wait_exceeded'):
            with self.subTest(reason=reason):
                result, _ = self.exercise(reason)
                self.assertEqual(result.returncode, canary.INCONCLUSIVE, result.stderr)
                self.assertIn('canary inconclusive: no free permit', result.stderr)
                self.assertNotIn('started and exited 0', result.stdout)

    def test_capacity_retry_can_pass_and_reuses_paths_with_fresh_job_ids(self):
        result, requests = self.exercise(['runner_full', 'queue_wait_exceeded', 'pass'], timeout_s=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('started and exited 0', result.stdout)
        self.assertEqual(len(requests), 3)
        self.assertEqual(len({request['job_id'] for request in requests}), 3)
        self.assertEqual(len({request['workspace_key'] for request in requests}), 1)
        self.assertEqual(len({request['repository_root'] for request in requests}), 1)

    def test_capacity_does_not_hide_a_later_failure_or_missing_terminal(self):
        for outcome in ('setup-failed', 'exit-1', 'timeout', 'no-terminal'):
            with self.subTest(outcome=outcome):
                result, _ = self.exercise(['runner_full', outcome], timeout_s=3)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertNotIn('canary inconclusive', result.stderr)

    def test_fixed_workspace_and_repository_are_reused_across_deployments(self):
        result, requests = self.exercise(['pass', 'pass'], instance='e2e', invocations=2)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(requests), 2)
        for request in requests:
            self.assertEqual(request['workspace_key'], '/Users/motor-deploy-canary/e2e/workspace')
            self.assertEqual(request['repository_root'], '/Users/motor-deploy-canary/e2e/workspace-source')
        self.assertNotEqual(requests[0]['job_id'], requests[1]['job_id'])
