#!/usr/bin/env python3
"""Judge captured job observations without needing a privileged test process."""
import contextlib
import copy
import errno
import importlib.util
import io
import json
from pathlib import Path
import unittest
from unittest import mock


HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("job_sandbox_probe", HERE / "job-sandbox-probe.py")
assert spec is not None and spec.loader is not None
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
CAPTURE = HERE / "test-fixtures/job-sandbox-passing.json"


class JobSandboxProbeTests(unittest.TestCase):
    def setUp(self):
        # Unmodified stdout from a real four-vCPU server job. The UUID and PSI
        # counters are evidence, not values calculated by the parser under test.
        self.passing = json.loads(CAPTURE.read_text())

    def refused(self, observation, property_name):
        with self.assertRaisesRegex(RuntimeError, property_name + ": saw"):
            probe.judge(json.dumps(observation))

    def test_captured_real_job_passes(self):
        observed = probe.judge(CAPTURE.read_text())
        self.assertEqual("bash", observed["pid1"]["comm"])
        self.assertEqual(999, observed["files"]["memory.pressure"]["uid"])
        self.assertEqual(30, observed["pressure_write"]["errno"])

    def test_cgroup_requires_full_slice_prefix_lowercase_uuid_and_suffix(self):
        original = self.passing["cgroup"]
        for value in (original.replace("motor.slice", "other.slice", 1),
                      original.replace("motor-jobs.slice", "other.slice"),
                      original.replace("motor-job-", "other-job-"),
                      original.replace("01a1903f", "01A1903F"),
                      original.replace("01a1903f-339f-7722-9136-c484784ca3e1", "not-a-uuid"),
                      original.replace(".service", ".scope"), original + "0::/extra\n",
                      original + "\n", "1:memory:/motor.slice\n"):
            with self.subTest(cgroup=value):
                self.refused(dict(self.passing, cgroup=value), "cgroup path")

    def test_cgroup_and_both_parent_slice_directories_are_root_owned(self):
        for path in self.passing["directories"]:
            for field in ("uid", "gid"):
                with self.subTest(path=path, field=field):
                    changed = copy.deepcopy(self.passing)
                    changed["directories"][path][field] = 999
                    self.refused(changed, "root-owned cgroup directory .*")

    def test_memory_pressure_is_the_only_job_owned_control_file_with_mode_0644(self):
        # A mixed or foreign owner, or a different mode, is refused.
        for changes in (dict(uid=0), dict(gid=0), dict(uid=1234, gid=1234), dict(mode=0o600)):
            with self.subTest(changes=changes):
                changed = copy.deepcopy(self.passing)
                changed["files"]["memory.pressure"].update(changes)
                self.refused(changed, "memory.pressure owned by the job account or root, mode 0644")
        for owner in (999, 1234):
            changed = copy.deepcopy(self.passing)
            changed["files"]["memory.max"]["uid"] = owner
            self.refused(changed, "root-owned cgroup control file memory.max")

    def test_root_owned_memory_pressure_is_accepted(self):
        # Some hosts' systemd leaves memory.pressure root-owned (seen on Boat);
        # the contract accepts it, since the read-only mount still holds.
        changed = copy.deepcopy(self.passing)
        changed["files"]["memory.pressure"].update(uid=0, gid=0)
        probe.judge(json.dumps(changed))

    def test_cgroup_write_must_fail_because_mount_is_read_only(self):
        for write in (dict(wrote=True), dict(errno=errno.EACCES, error="Permission denied"),
                      dict(errno=errno.EINVAL, error="Invalid PSI trigger"), {}):
            with self.subTest(write=write):
                self.refused(dict(self.passing, pressure_write=write), "read-only cgroup .*")

    def test_no_psi_is_refused_even_if_the_other_sandbox_properties_pass(self):
        for key in ("system_pressure", "job_pressure"):
            for value in (dict(errno=errno.ENOENT, error="No such file or directory"),
                          dict(errno=errno.EACCES, error="Permission denied"), ""):
                with self.subTest(key=key, value=value):
                    self.refused(dict(self.passing, **{key: value}), "PSI readable .*")

    def test_pid1_must_be_bash_and_run_as_the_job_account(self):
        for field, value in (("comm", "systemd"), ("uids", [0] * 4), ("gids", [0] * 4)):
            changed = copy.deepcopy(self.passing)
            changed["pid1"][field] = value
            self.refused(changed, "PID 1 bash wrapper as motor-job")

    def test_probe_itself_must_run_as_motor_job(self):
        for field, value in (("name", "user"), ("uid", 0), ("euid", 0), ("gid", 0), ("egid", 0)):
            changed = copy.deepcopy(self.passing)
            changed["identity"][field] = value
            self.refused(changed, "motor-job identity")

    def test_boat_job_account_numbers_are_not_pinned(self):
        changed = copy.deepcopy(self.passing)
        changed["identity"].update(uid=123, euid=123, account_uid=123,
                                   gid=456, egid=456, account_gid=456)
        changed["pid1"].update(uids=[123] * 4, gids=[456] * 4)
        changed["files"]["memory.pressure"].update(uid=123, gid=456)
        probe.judge(json.dumps(changed))

    def test_missing_or_malformed_observations_cannot_pass(self):
        for output in ("", "not JSON", "[]"):
            with self.subTest(output=output), self.assertRaisesRegex(RuntimeError, "job sandbox output"):
                probe.judge(output)
        for field in self.passing:
            changed = dict(self.passing)
            del changed[field]
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                probe.judge(json.dumps(changed))

    def test_probe_failure_prints_evidence_and_names_missing_psi(self):
        changed = dict(self.passing, system_pressure=dict(errno=2, error="No such file or directory"))
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(probe, "observe", return_value=changed), \
             contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            self.assertEqual(1, probe.main())
        self.assertEqual(changed, json.loads(out.getvalue()))
        self.assertIn("PSI readable /proc/pressure/memory: saw", err.getvalue())
        self.assertNotIn(probe.SUCCESS, err.getvalue())

    def test_nonzero_job_exit_cannot_pass_with_good_observations(self):
        with self.assertRaisesRegex(RuntimeError, "probe job completion"):
            probe.judge_job(dict(code=1, stdout=CAPTURE.read_text(), stderr="crashed"))

    def test_write_observation_actually_attempts_to_write(self):
        path = mock.MagicMock()
        self.assertEqual(dict(wrote=True), probe.pressure_write(path))
        path.open.assert_called_once_with("w")
        path.open.return_value.__enter__.return_value.write.assert_called_once_with("some 150000 1000000\n")
        path.open.side_effect = OSError(errno.EROFS, "Read-only file system")
        self.assertEqual(errno.EROFS, probe.pressure_write(path)["errno"])


if __name__ == "__main__":
    unittest.main()
