"""Run with python3 -m unittest -v script/runner/test_deploy.py."""
import json
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

DEPLOY = Path(__file__).with_name("deploy.sh").resolve()
policy_spec = importlib.util.spec_from_file_location('runner_permissions_policy', DEPLOY.with_name('permissions_policy.py'))
if policy_spec is None or policy_spec.loader is None:
    raise ImportError('cannot load permissions sudo policy renderer')
policy = importlib.util.module_from_spec(policy_spec)
policy_spec.loader.exec_module(policy)


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


class DeployTests(unittest.TestCase):
    def run_deploy(self, job, *, resume=True, drain="timeout", fail_start=False,
                    instance="main", default_state_root=False, decoys=False, drain_timeout=None, helper_valid=True):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            state_root = (root / "motor-windows/windows/state" if instance == "windows"
                          else root / "state")
            state = state_root / instance / "jobs/job"
            state.mkdir(parents=True)
            (root / 'admission').write_text('open')
            (state / "record.json").write_text(json.dumps(job))
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
                path.write_text(admin)
                path.chmod(0o755)
            systemctl = bin_dir / "systemctl"
            systemctl.write_text('''#!/usr/bin/env bash
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
            return result, (root / "log").read_text(), (root / "admission").read_text().strip()

    def job(self, **extra):
        return dict(dict(id="survivor-job", state="Running", terminal=None, started_ms=1), **extra)

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

    def test_proven_not_started_is_restart_recoverable(self):
        result, _, admission = self.run_deploy(dict(id="queued", state="Queued", terminal=None, started_ms=None, process=None))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(admission, "open")
