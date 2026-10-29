#!/usr/bin/env python3
"""Qualification readiness tests with mocked service states and in-memory pipes."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import queue
import shutil
import struct
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch
from typing import Any, BinaryIO, Optional, Union

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("windows_qualification_jobs", HERE / "qualify-jobs.py")
assert SPEC is not None and SPEC.loader is not None
JOBS: Any = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(JOBS)


def _wire(value):
    payload = json.dumps(value).encode()
    return struct.pack(">I", len(payload)) + payload


def status_reply(ready=True, detail=None):
    agent = dict(ready=ready, parent_agent_sha256="A" * 64, release_agent_sha256="A" * 64, detail=detail)
    return _wire(dict(type="status_reply", health=dict(windows_guest_agent=agent)))


def hello(version=1, platform="windows", agent_ready=True, agent_detail=None):
    return _wire(dict(type="hello", protocol_version=version, platforms=[platform])) + status_reply(agent_ready, agent_detail)



class Bridge:
    def __init__(self, wire=b"", diagnostic=b"", running=False, hung=False):
        self.stdin = io.BytesIO()
        self.stdout = io.BytesIO(wire)
        self.diagnostic = diagnostic
        self.returncode = None if running else 1
        self.hung = hung
        self.killed = False
        self.reaped = False
        self.log: Optional[BinaryIO] = None

    def poll(self):
        return self.returncode

    def terminate(self):
        self.returncode = 0

    def kill(self):
        self.killed = True
        self.returncode = -9

    def wait(self, timeout):
        if self.hung and not self.killed:
            raise subprocess.TimeoutExpired("fake bridge", timeout)
        self.reaped = True
        return self.returncode


class Clock:
    def __init__(self):
        self.now = 0.0

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class QualificationReadinessTests(unittest.TestCase):
    def test_real_jobs_check_bun_even_with_a_custom_gate(self):
        source = (HERE / "qualify-jobs.py").read_text()
        self.assertIn('command = BUN_VERSION_CHECK + " || exit $?; " + ', source)

    @unittest.skipUnless(shutil.which("bash"), "requires Bash for the Git Bash version guard")
    def test_bun_version_check_rejects_wrong_missing_and_failed_bun(self):
        for version, code in (("1.4.2", 0), ("1.4.2\r", 0), ("1.4.1", 0), ("1.4.20", 0), ("", 127), ("1.4.2", 1)):
            with self.subTest(version=version, code=code):
                command = f"bun() {{ printf '%s\\n' '{version}'; return {code}; }}; " + JOBS.BUN_VERSION_CHECK
                result = subprocess.run(["bash", "-c", command], capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, version.rstrip("\r") == "1.4.2" and code == 0, result.stderr)

    def test_real_jobs_check_nextest_even_with_a_custom_gate(self):
        source = (HERE / "qualify-jobs.py").read_text()
        self.assertIn('+ NEXTEST_VERSION_CHECK + " || exit $?; " + ', source)

    @unittest.skipUnless(shutil.which("bash"), "requires Bash for the Git Bash version guard")
    def test_nextest_version_check_rejects_wrong_missing_and_failed_nextest(self):
        expected = "cargo-nextest 0.9.138 (fc97e97bb 2026-06-21)"
        for version, code, passes in ((expected, 0, True), (expected + "\r", 0, True),
                                      (expected.replace("0.9.138", "0.9.137"), 0, False),
                                      (expected.replace("0.9.138", "0.9.1380"), 0, False),
                                      ("", 127, False), (expected, 1, False)):
            with self.subTest(version=version, code=code):
                command = f"cargo() {{ test \"$*\" = 'nextest --version' || return 1; printf '%s\\n' '{version}'; return {code}; }}; " + JOBS.NEXTEST_VERSION_CHECK
                result = subprocess.run(["bash", "-c", command], capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, passes, result.stderr)
                if passes:
                    self.assertIn(expected, result.stdout)

    def test_real_jobs_check_python_even_with_a_custom_gate(self):
        source = (HERE / "qualify-jobs.py").read_text()
        self.assertIn('+ PYTHON_CHECK + " || exit $?; " + ', source)

    @unittest.skipUnless(shutil.which("bash"), "requires Bash for the Git Bash Python check")
    def test_python_check_rejects_wrong_missing_failed_and_incomplete_stdlib(self):
        for version3, version, code, smoke, passes in (
            ("Python 3.14.8", "Python 3.14.8", 0, 0, True),
            ("Python 3.14.8\r", "Python 3.14.8\r", 0, 0, True),
            ("Python 3.13.17", "Python 3.14.8", 0, 0, False),
            ("Python 3.14.7", "Python 3.14.8", 0, 0, False),
            ("Python 3.14.80", "Python 3.14.8", 0, 0, False),
            ("Python 3.14.8", "Python 3.13.17", 0, 0, False),
            ("Python 3.14.8", "Python 3.14.7", 0, 0, False),
            ("Python 3.14.8", "", 0, 0, False),
            ("", "Python 3.14.8", 127, 0, False),
            ("Python 3.14.8", "Python 3.14.8", 1, 0, False),
            ("Python 3.14.8", "Python 3.14.8", 0, 1, False),
        ):
            with self.subTest(version3=version3, version=version, code=code, smoke=smoke):
                command = (
                    "python3() { if test \"$*\" = --version; then "
                    f"printf '%s\\n' '{version3}'; return {code}; fi; "
                    "test \"$#\" -eq 2 && test \"$1\" = -c && "
                    "test \"$2\" = 'import json, hashlib, subprocess, tomllib, compression.zstd' || return 99; "
                    f"return {smoke}; }}; "
                    f"python() {{ test \"$*\" = --version || return 99; printf '%s\\n' '{version}'; }}; "
                ) + JOBS.PYTHON_CHECK
                result = subprocess.run(["bash", "-c", command], capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, passes, result.stderr)
                if passes:
                    self.assertIn("Python 3.14.8", result.stdout)

    def test_real_jobs_check_nats_and_stable_even_with_a_custom_gate(self):
        source = (HERE / "qualify-jobs.py").read_text()
        self.assertIn('+ NATS_VERSION_CHECK + " || exit $?; " + ', source)
        self.assertIn('+ STABLE_VERSION_CHECK + " || exit $?; " + ', source)

    @unittest.skipUnless(shutil.which("bash"), "requires Bash for the Git Bash NATS check")
    def test_nats_version_check_rejects_wrong_missing_and_failed_tools(self):
        for version, code, passes in (
            ("nats-server: v2.15.0", 0, True),
            ("nats-server: v2.15.0\r", 0, True),
            ("nats-server: v2.14.0", 0, False),
            ("nats-server: v2.15.00", 0, False),
            ("", 127, False),
            ("nats-server: v2.15.0", 1, False),
        ):
            with self.subTest(version=version, code=code):
                command = (
                    "nats-server() { test \"$*\" = --version || return 99; "
                    f"printf '%s\\n' '{version}'; return {code}; }}; "
                ) + JOBS.NATS_VERSION_CHECK
                result = subprocess.run(["bash", "-c", command], capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, passes, result.stderr)
                if passes:
                    self.assertIn("nats-server: v2.15.0", result.stdout)

    @unittest.skipUnless(shutil.which("bash"), "requires Bash for the Git Bash stable check")
    def test_stable_version_check_rejects_wrong_missing_and_failed_tools(self):
        for version, code, passes in (
            ("rustc 1.99.0 (abc123 2026-09-01)", 0, True),
            ("rustc 1.99.0 (abc123 2026-09-01)\r", 0, True),
            ("rustc 1.98.0 (old)", 0, False),
            ("rustc 1.99.00 (wrong)", 0, False),
            ("rustc 1.99.0-nightly (wrong)", 0, False),
            ("", 127, False),
            ("rustc 1.99.0 (abc123 2026-09-01)", 1, False),
        ):
            with self.subTest(version=version, code=code):
                command = (
                    "rustc() { test \"$*\" = '+stable --version' || return 99; "
                    f"printf '%s\\n' '{version}'; return {code}; }}; "
                ) + JOBS.STABLE_VERSION_CHECK
                result = subprocess.run(["bash", "-c", command], capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, passes, result.stderr)
                if passes:
                    self.assertIn("rustc 1.99.0 ", result.stdout)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "bridge.log"
        self.runner = self.root / "bin/motor-runner.exe"
        self.bridges: list[Bridge] = []

    def spawn(self, *args, **kwargs):
        bridge = self.bridges.pop(0)
        kwargs["stderr"].write(bridge.diagnostic)
        kwargs["stderr"].flush()
        bridge.log = kwargs["stderr"]
        return bridge

    def test_service_status_queries_scm_not_pipe_presence(self):
        response = subprocess.CompletedProcess([], 0, "Running\r\n", "")
        with patch.object(JOBS.subprocess, "run", return_value=response) as run:
            self.assertEqual(JOBS.runner_service_status(), "Running")
        assert run.call_args is not None
        command = run.call_args.args[0]
        self.assertIn("Get-Service -Name MotorRunner", command[-1])
        self.assertEqual(run.call_args.kwargs["timeout"], 10)

    def test_ready_status_requires_a_matching_guest_agent_identity(self):
        ready = dict(health=dict(windows_guest_agent=dict(ready=True, parent_agent_sha256="A" * 64, release_agent_sha256="A" * 64)))
        self.assertTrue(JOBS.require_windows_guest_agent_ready(ready)["ready"])
        mismatch = "windows_guest_agent_mismatch parent_agent_sha256=" + "B" * 64 + " release_agent_sha256=" + "A" * 64
        with self.assertRaisesRegex(JOBS.RunnerBridgeRefused, mismatch):
            JOBS.require_windows_guest_agent_ready(dict(health=dict(windows_guest_agent=dict(ready=False, detail=mismatch))))
        with self.assertRaisesRegex(JOBS.RunnerBridgeRefused, "identity status missing"):
            JOBS.require_windows_guest_agent_ready(dict(health={}))

    def test_service_status_query_failure_is_explicit(self):
        response = subprocess.CompletedProcess([], 1, "", "service not installed")
        with patch.object(JOBS.subprocess, "run", return_value=response):
            with self.assertRaisesRegex(RuntimeError, "cannot query runner service MotorRunner.*service not installed"):
                JOBS.runner_service_status()

    def test_stopped_service_fails_before_bridge_launch(self):
        with patch.object(JOBS, "runner_service_status", return_value="Stopped"), patch.object(JOBS.subprocess, "Popen") as launch:
            with self.assertRaisesRegex(JOBS.RunnerBridgeUnavailable, "runner service not running.*Stopped"):
                JOBS.Channel(self.runner, self.root, self.log)
        launch.assert_not_called()

    def test_channel_eof_reports_stopped_service_and_bridge_log(self):
        bridge = Bridge(diagnostic=b'Error: Os { code: 2, kind: NotFound }')
        self.bridges = [bridge]
        with patch.object(JOBS, "runner_service_status", side_effect=["Running", "Stopped"]), patch.object(JOBS.subprocess, "Popen", side_effect=self.spawn):
            with self.assertRaisesRegex(JOBS.RunnerBridgeUnavailable, "runner service not running.*Stopped.*runner bridge closed.*code: 2"):
                JOBS.Channel(self.runner, self.root, self.log)
        self.assertTrue(bridge.reaped)
        assert bridge.log is not None
        self.assertTrue(bridge.log.closed)

    def test_channel_rejects_wrong_hello_version_and_reaps_bridge(self):
        bridge = Bridge(hello(version=2), running=True)
        self.bridges = [bridge]
        channel = None
        try:
            with patch.object(JOBS, "runner_service_status", return_value="Running"), patch.object(JOBS.subprocess, "Popen", side_effect=self.spawn):
                with self.assertRaisesRegex(RuntimeError, "wrong runner Hello version/platform"):
                    channel = JOBS.Channel(self.runner, self.root, self.log)
        finally:
            if channel is not None:
                channel.close()
        self.assertTrue(bridge.reaped)
        assert bridge.log is not None
        self.assertTrue(bridge.log.closed)

    def test_channel_rejects_wrong_platform(self):
        bridge = Bridge(hello(platform="linux"), running=True)
        self.bridges = [bridge]
        with patch.object(JOBS, "runner_service_status", return_value="Running"), patch.object(JOBS.subprocess, "Popen", side_effect=self.spawn):
            with self.assertRaisesRegex(RuntimeError, "wrong runner Hello version/platform"):
                JOBS.Channel(self.runner, self.root, self.log)
        self.assertTrue(bridge.reaped)

    def test_channel_close_reaps_a_hung_bridge(self):
        bridge = Bridge(hello(), running=True, hung=True)
        self.bridges = [bridge]
        with patch.object(JOBS, "runner_service_status", return_value="Running"), patch.object(JOBS.subprocess, "Popen", side_effect=self.spawn):
            channel = JOBS.Channel(self.runner, self.root, self.log)
            channel.close()
        self.assertTrue(bridge.killed)
        self.assertTrue(bridge.reaped)

    def test_hello_queue_timeout_is_bounded_and_explicit(self):
        channel = JOBS.Channel.__new__(JOBS.Channel)
        channel.messages = Mock()
        channel.messages.get.side_effect = queue.Empty
        with self.assertRaisesRegex(TimeoutError, "runner did not deliver hello within 2 seconds"):
            channel.until("hello", timeout=2)

    @contextlib.contextmanager
    def readiness_environment(self, states: Union[str, list[str]] = "Running"):
        clock = Clock()
        status = Mock(return_value=states) if isinstance(states, str) else Mock(side_effect=states)
        output = io.StringIO()
        with patch.object(JOBS, "runner_service_status", status), patch.object(JOBS.subprocess, "Popen", side_effect=self.spawn) as launch, patch.object(JOBS.time, "monotonic", clock.monotonic), patch.object(JOBS.time, "sleep", clock.sleep), contextlib.redirect_stdout(output):
            yield clock, launch, output

    def test_running_service_waits_past_missing_pipe_for_real_hello(self):
        missing = Bridge(diagnostic=b'Error: Os { code: 2, kind: NotFound }')
        serving = Bridge(hello(), running=True)
        self.bridges = [missing, serving]
        with self.readiness_environment() as (_, launch, output):
            JOBS.wait_for_service_ready(self.root, self.runner, self.root, timeout=10)
        self.assertEqual(launch.call_count, 2)
        evidence = json.loads((self.root / "service-readiness.json").read_text())
        self.assertTrue(evidence["ready"])
        self.assertIn("NotFound", evidence["observations"][0]["bridge_error"])
        self.assertTrue(evidence["observations"][1]["hello_validated"])
        self.assertIn("parent-verification-complete windows-protocol-hello", output.getvalue())
        self.assertTrue(all(bridge.reaped and bridge.log is not None and bridge.log.closed for bridge in (missing, serving)))

    def test_start_pending_is_not_readiness(self):
        serving = Bridge(hello(), running=True)
        self.bridges = [serving]
        with self.readiness_environment(["StartPending", "Running", "Running"]) as (clock, launch, _):
            JOBS.wait_for_service_ready(self.root, self.runner, self.root, timeout=10)
        self.assertEqual(launch.call_count, 1)
        self.assertGreaterEqual(clock.now, 0.5)
        evidence = json.loads((self.root / "service-readiness.json").read_text())
        self.assertNotIn("hello_validated", evidence["observations"][0])
        self.assertTrue(evidence["ready"])

    def test_guest_agent_mismatch_fails_readiness_before_jobs(self):
        detail = "windows_guest_agent_mismatch parent_agent_sha256=" + "B" * 64 + " release_agent_sha256=" + "A" * 64
        mismatch = Bridge(hello(agent_ready=False, agent_detail=detail), running=True)
        self.bridges = [mismatch]
        with self.readiness_environment() as (_, launch, output):
            with self.assertRaisesRegex(JOBS.RunnerBridgeRefused, "Windows guest agent not ready.*windows_guest_agent_mismatch"):
                JOBS.wait_for_service_ready(self.root, self.runner, self.root, timeout=10)
        self.assertEqual(launch.call_count, 1)
        evidence = json.loads((self.root / "service-readiness.json").read_text())
        self.assertFalse(evidence["ready"])
        self.assertIn("parent_agent_sha256=" + "B" * 64, evidence["failure"])
        self.assertIn("release_agent_sha256=" + "A" * 64, evidence["failure"])
        self.assertNotIn("PASS", output.getvalue())
        self.assertTrue(mismatch.reaped)

    def test_stop_during_startup_is_clear_and_retained(self):
        with self.readiness_environment(["StartPending", "Stopped"]) as (_, launch, _):
            with self.assertRaisesRegex(JOBS.RunnerBridgeUnavailable, "runner service not running.*Stopped"):
                JOBS.wait_for_service_ready(self.root, self.runner, self.root, timeout=10)
        launch.assert_not_called()
        evidence = json.loads((self.root / "service-readiness.json").read_text())
        self.assertFalse(evidence["ready"])
        self.assertIn("Stopped", evidence["failure"])

    def test_running_without_valid_hello_times_out(self):
        missing = [Bridge(diagnostic=b"pipe NotFound") for _ in range(2)]
        self.bridges = missing.copy()
        with self.readiness_environment() as (clock, launch, output):
            with self.assertRaisesRegex(TimeoutError, "did not provide a valid Windows Hello within 1 seconds"):
                JOBS.wait_for_service_ready(self.root, self.runner, self.root, timeout=1)
        self.assertEqual(launch.call_count, 2)
        self.assertEqual(clock.now, 1)
        evidence = json.loads((self.root / "service-readiness.json").read_text())
        self.assertFalse(evidence["ready"])
        self.assertEqual(len(evidence["observations"]), 2)
        self.assertNotIn("PASS", output.getvalue())
        self.assertTrue(all(bridge.reaped for bridge in missing))

    def test_non_service_pipe_refusal_is_not_retried_as_startup(self):
        refused = Bridge(diagnostic=b"local pipe is not owned by the running MotorRunner service")
        self.bridges = [refused]
        with self.readiness_environment() as (_, launch, output):
            with self.assertRaisesRegex(JOBS.RunnerBridgeRefused, "refused a non-service pipe"):
                JOBS.wait_for_service_ready(self.root, self.runner, self.root, timeout=10)
        self.assertEqual(launch.call_count, 1)
        self.assertTrue(refused.reaped)
        self.assertNotIn("PASS", output.getvalue())

    def test_wrong_platform_is_not_retried_as_startup(self):
        wrong = Bridge(hello(platform="linux"), running=True)
        self.bridges = [wrong]
        with self.readiness_environment() as (_, launch, _):
            with self.assertRaisesRegex(RuntimeError, "wrong runner Hello version/platform"):
                JOBS.wait_for_service_ready(self.root, self.runner, self.root, timeout=10)
        self.assertEqual(launch.call_count, 1)
        self.assertTrue(wrong.reaped)

    def test_stopped_service_fails_before_creating_job_fixture(self):
        with patch.object(JOBS, "runner_service_status", return_value="Stopped"), patch.object(JOBS, "fixture") as create:
            with self.assertRaisesRegex(JOBS.RunnerBridgeUnavailable, "runner service not running"):
                JOBS.run(self.root, self.runner, self.root, "cargo test")
        create.assert_not_called()

    def test_qualification_waits_for_service_protocol_before_real_jobs(self):
        script = (HERE / "qualify.ps1").read_text()
        start = script.rindex("Start-Service MotorRunner\n")
        wait = script.index("Wait-RunnerQualificationService\n", start)
        jobs = script.index('& $Python "$Root\\bin\\qualify-jobs.py"', start)
        self.assertLess(start, wait)
        self.assertLess(wait, jobs)
        barrier = script.split("function Wait-RunnerQualificationService {", 1)[1].split("function Copy-QualificationEvidence", 1)[0]
        self.assertIn("'--wait-ready'", barrier)
        self.assertIn("WaitForExit(180000)", barrier)
        self.assertIn("$result.exitCode -ne 0", barrier)

    def test_readiness_cli_does_not_submit_real_jobs(self):
        evidence = self.root / "ready-only"
        with patch.object(JOBS, "wait_for_service_ready") as ready, patch.object(JOBS, "run") as real_jobs:
            JOBS.main(["--root", str(self.root), "--evidence", str(evidence), "--wait-ready"])
        ready.assert_called_once_with(self.root, self.runner, evidence)
        real_jobs.assert_not_called()

    def test_gate_success_is_the_windows_exit_outcome(self):
        # The live Ally gate exited 0 and reported windows_exit; a Unix exit
        # check refused it.
        JOBS.require_gate_success({"outcome": {"type": "windows_exit", "code": 0}})
        for outcome in ({"type": "windows_exit", "code": 1}, {"type": "exit", "code": 0}, None):
            with self.assertRaisesRegex(RuntimeError, "gate failed"):
                JOBS.require_gate_success({"outcome": outcome})

    def test_preparation_cases_submit_tagged_dependency_and_run_sh_only_snapshot(self):
        def git(repo, *args):
            return subprocess.check_output(["git", "-C", str(repo), *args], stderr=subprocess.STDOUT).decode().strip()

        channels = [Mock(), Mock()]
        for channel in channels:
            channel.until.return_value = {"outcome": {"type": "windows_exit", "code": 0}}
        with patch.object(JOBS, "git", side_effect=git), patch.object(JOBS, "Channel", side_effect=channels):
            JOBS.qualify_preparation_cases(self.root, self.runner, self.root)
        requests = [channel.send.call_args_list[0].args[0] for channel in channels]
        tagged, no_lock = requests
        self.assertIn("cargo build --offline --locked", tagged["command"])
        dep = tagged["git_dependencies"][0]
        self.assertEqual(dep["url"], JOBS.GIT_DEP_URL)
        # The runner uploads `bundles` then each dependency's bundle, and needs
        # consecutive indexes, so the dependency must not also be in `bundles`.
        uploads = tagged["bundles"] + [d["bundle"] for d in tagged["git_dependencies"]]
        self.assertEqual([u["index"] for u in uploads], list(range(len(uploads))))
        self.assertEqual(dep["bundle"]["repository"], JOBS.GIT_DEP_URL)
        dep_commit = dep["commits"][0]
        lock = (self.root / "git-tag-fixture/Cargo.lock").read_text()
        self.assertIn(f"git+{JOBS.GIT_DEP_URL}?tag=1.0.11#{dep_commit}", lock)
        self.assertEqual(git(self.root / "git-tag-dependency", "rev-parse", "1.0.11"), dep_commit)
        bundle = self.root / "git-tag-offline-dependency.bundle"
        self.assertEqual(git(self.root / "git-tag-dependency", "bundle", "list-heads", str(bundle)),
                         f"{dep_commit} refs/ck-motor/dep/{dep_commit}")
        for channel, request in zip(channels, requests):
            self.assertTrue(request["command"].startswith(JOBS.BUN_VERSION_CHECK + " || exit $?; "))
            chunks = [call.args[0] for call in channel.send.call_args_list[1:]]
            for manifest in request["bundles"] + [d["bundle"] for d in request["git_dependencies"]]:
                data = b"".join(JOBS.base64.b64decode(chunk["data"]) for chunk in chunks if chunk["index"] == manifest["index"])
                self.assertEqual(len(data), manifest["length"])
                self.assertEqual(JOBS.hashlib.sha256(data).hexdigest(), manifest["sha256"])
            channel.close.assert_called_once()
        self.assertIn("sh run.sh", no_lock["command"])
        self.assertEqual(no_lock["git_dependencies"], [])
        self.assertEqual(len(no_lock["bundles"]), 1)
        self.assertEqual(git(self.root / "no-lockfile-fixture", "ls-tree", "--name-only", "HEAD"), "run.sh")

    def test_preparation_case_rejects_nonzero_guest_exit(self):
        channel = Mock()
        channel.until.return_value = {"outcome": {"type": "windows_exit", "code": 101}}
        with patch.object(JOBS, "fixture_bundle", side_effect=lambda repo, bundle, commit: bundle.write_bytes(b"bundle")), patch.object(JOBS, "Channel", return_value=channel):
            with self.assertRaisesRegex(RuntimeError, "gate failed"):
                JOBS.run_preparation_case(self.root, self.runner, self.root, "failed", self.root, "commit", "tree", "sh run.sh")
        channel.close.assert_called_once()

    def test_preparation_only_cli_submits_only_the_two_preparation_cases(self):
        evidence = self.root / "preparation-evidence"
        with patch.object(JOBS, "qualify_preparation_cases") as cases, patch.object(JOBS, "run") as full:
            JOBS.main(["--root", str(self.root), "--evidence", str(evidence), "--preparation-only"])
        cases.assert_called_once_with(self.root, self.runner, evidence)
        full.assert_not_called()

    def test_fixture_bundle_refs_match_snapshot_transport_and_import_commits(self):
        def git(repo, *args):
            return subprocess.check_output(["git", "-C", str(repo), *args], stderr=subprocess.STDOUT).decode().strip()

        repo = self.root / "fixture"
        bundle = self.root / "fixture.bundle"
        with patch.object(JOBS, "git", side_effect=git):
            commit, tree = JOBS.fixture(repo)
            JOBS.fixture_bundle(repo, bundle, commit)
        heads = git(repo, "bundle", "list-heads", str(bundle)).splitlines()
        self.assertEqual(len(heads), 1, heads)
        oid, ref = heads[0].split(" ", 1)
        self.assertEqual(oid, commit)
        self.assertRegex(ref, r"^refs/ck-motor/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/snapshot$")
        self.assertEqual(git(repo, "for-each-ref", "--format=%(refname)", "refs/ck-motor/"), "")
        self.assertGreater(bundle.stat().st_size, 65536)
        git(repo, "bundle", "verify", str(bundle))
        mirror = self.root / "mirror.git"
        mirror.mkdir()
        git(mirror, "init", "--bare")
        git(mirror, "fetch", "--no-tags", "--no-write-fetch-head", str(bundle), ref)
        # Both request commit fields equal commit; verify it and its tree from
        # an empty mirror, not from the source repository that already had them.
        self.assertEqual(git(mirror, "cat-file", "-t", commit), "commit")
        self.assertEqual(git(mirror, "rev-parse", commit + "^{tree}"), tree)
        print("PASS fixture-bundle-list-heads " + heads[0], flush=True)

    def test_fixture_bundle_cleans_temporary_ref_after_creation_failure(self):
        def git(repo, *args):
            if args[:2] == ("bundle", "create"):
                raise RuntimeError("fixture bundle creation failed")
            return subprocess.check_output(["git", "-C", str(repo), *args], stderr=subprocess.STDOUT).decode().strip()

        repo = self.root / "fixture"
        with patch.object(JOBS, "git", side_effect=git):
            commit, _ = JOBS.fixture(repo)
            with self.assertRaisesRegex(RuntimeError, "fixture bundle creation failed"):
                JOBS.fixture_bundle(repo, self.root / "fixture.bundle", commit)
        self.assertEqual(git(repo, "for-each-ref", "--format=%(refname)", "refs/ck-motor/"), "")


if __name__ == "__main__":
    unittest.main()
