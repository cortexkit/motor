"""Exercise gate orchestration without compiling or contacting a runner."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class GateTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(os.environ.get("TMPDIR", ROOT / "target/scratch"))
        scratch.mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="gate-tests-", dir=scratch)
        self.addCleanup(directory.cleanup)
        self.base = Path(directory.name)
        self.root = self.base / "source"
        self.root.mkdir()
        self.bin = self.base / "bin"
        self.bin.mkdir()
        self.trace = self.base / "trace"
        self.lanes = self.base / "lanes"
        self.env = os.environ.copy()
        self.env.update(
            PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
            PYTHON=sys.executable,
            MOTOR_GATE_ROOT=str(self.lanes),
            GATE_TRACE=str(self.trace),
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
        )
        self.put(".gitignore", (ROOT / ".gitignore").read_text())
        self.put("scripts/gate.sh", (ROOT / "scripts/gate.sh").read_text())
        self.put("scripts/check-cortexkit-layout.sh", (ROOT / "scripts/check-cortexkit-layout.sh").read_text())
        for name in ("test-binary-fold", "unreachable-modules", "test-dev-binaries", "test-scratch", "subc-daemon-env"):
            self.put(f"scripts/{name}.py", "print('safety check')\n")
        self.put("crates/motor-protocol/tests/generate_vectors.py", "print('vectors checked')\n")
        for name in ("script/runner/test_toolchain_parity.py", "script/boat-image/test_boat_image.py",
                     "script/tenant-image/test_tenant_image.py"):
            self.put(name, "print('pins checked')\n")
        self.tool("uname", "#!/bin/sh\necho Linux\n")
        self.tool("rustc", "#!/bin/sh\necho 'rustc fixture'\n")
        self.tool("rustup", "#!/bin/sh\necho 'target installed'\n")
        self.tool("cargo", """#!/usr/bin/env python3
import json, os, sys
with open(os.environ['GATE_TRACE'], 'a') as out:
    out.write(json.dumps(['cargo', *sys.argv[1:]]) + '\\n')
print('cargo fixture')
if os.environ.get('FAIL_CARGO') and os.environ['FAIL_CARGO'] in ' '.join(sys.argv[1:]):
    sys.exit(7)
""")
        self.tool("ck", """#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys, time
args = sys.argv[1:]
cwd = pathlib.Path(args[args.index('--cwd') + 1])
with open(os.environ['GATE_TRACE'], 'a') as out:
    out.write(json.dumps(['ck', *args]) + '\\n')
(cwd.parent / (cwd.name + '.started')).touch()
expected = int(os.environ.get('EXPECT_PARALLEL', '0'))
if expected:
    deadline = time.monotonic() + 5
    while len(list(cwd.parent.glob('*.started'))) < expected:
        if time.monotonic() > deadline:
            sys.exit(9)
        time.sleep(.01)
if cwd.name == os.environ.get('FAIL_LANE'):
    sys.exit(8)
if os.environ.get('EXECUTE_REMOTE'):
    sys.exit(subprocess.call(['bash', '-c', args[-1]], cwd=cwd))
""")
        self.git("init", "-q")
        self.git("config", "user.name", "Gate test")
        self.git("config", "user.email", "gate-test@localhost")
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")

    def put(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def tool(self, name, text):
        path = self.bin / name
        path.write_text(text)
        path.chmod(0o755)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, env=self.env)

    def run_gate(self, *args, **env):
        return subprocess.run(
            ["bash", "scripts/gate.sh", *args], cwd=self.root,
            env={**self.env, **env}, text=True, capture_output=True, timeout=30,
        )

    def commands(self, tool):
        if not self.trace.exists():
            return []
        return [row for line in self.trace.read_text().splitlines()
                if (row := json.loads(line))[0] == tool]

    def test_fast_never_submits_or_builds(self):
        result = self.run_gate("--fast")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("gate fast PASS", result.stdout)
        self.assertEqual(self.commands("ck"), [])
        self.assertEqual(self.commands("cargo"), [
            ["cargo", "fmt", "--version"], ["cargo", "fmt", "--all", "--check"],
            ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
        ])

    def test_fast_failure_stops_before_submission(self):
        result = self.run_gate(FAIL_CARGO="metadata")
        self.assertEqual(result.returncode, 7, result.stdout + result.stderr)
        self.assertIn("gate fast FAIL", result.stdout)
        self.assertEqual(self.commands("ck"), [])
        self.assertFalse(self.lanes.exists())

    def test_all_remote_lanes_start_in_parallel_and_report_failure(self):
        result = self.run_gate(EXPECT_PARALLEL="5", FAIL_LANE="windows-clippy")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        for lane in ("linux-lint", "linux-tests", "musl", "script-tests"):
            self.assertIn(f"gate {lane} PASS", result.stdout)
        self.assertIn("gate windows-clippy FAIL exit=8", result.stdout)
        self.assertEqual(len(self.commands("ck")), 5)
        for command in self.commands("ck"):
            self.assertEqual(command[1:5], ["motor", "run", "--vcpus", "16"])

    def test_selected_lane_keeps_source_history_and_reuses_checkout(self):
        self.put("tracked", "original")
        self.put("deleted", "remove me")
        self.git("add", ".")
        self.git("commit", "-qm", "source history")
        ancestor = self.git("rev-parse", "HEAD").decode().strip()
        self.put("tracked", "staged")
        self.git("add", "tracked")
        self.put("tracked", "unstaged")
        (self.root / "deleted").unlink()
        self.put("new source", "untracked")
        (self.root / ".git/info/exclude").write_text("staged-input\n")
        self.put("staged-input", "staged despite ignore rules")
        self.git("add", "-f", "staged-input")
        self.put(".cortexkit/internal-docs/private", "ignored")
        self.put("target/cache", "ignored")
        before = self.git("status", "--porcelain=v1")
        result = self.run_gate("--lane", "linux-lint")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        checkout = self.lanes / "linux-lint"
        self.assertEqual((checkout / "tracked").read_text(), "unstaged")
        self.assertEqual((checkout / "new source").read_text(), "untracked")
        self.assertEqual((checkout / "staged-input").read_text(), "staged despite ignore rules")
        self.assertFalse((checkout / "deleted").exists())
        self.assertFalse((checkout / "target/cache").exists())
        self.assertFalse((checkout / ".cortexkit/internal-docs/private").exists())
        subprocess.run(["git", "merge-base", "--is-ancestor", ancestor, "HEAD"], cwd=checkout, check=True)
        self.assertEqual(self.git("status", "--porcelain=v1"), before)
        (checkout / "target").mkdir()
        (checkout / "target/warm-cache").write_text("keep")
        (self.root / "new source").unlink()
        result = self.run_gate("--lane", "linux-lint")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((checkout / "new source").exists())
        self.assertEqual((checkout / "target/warm-cache").read_text(), "keep")

    def test_serial_job_continues_after_a_failed_lane(self):
        result = self.run_gate("--serial", "--lane", "linux-lint", "--lane", "windows-clippy",
                               EXECUTE_REMOTE="1", FAIL_CARGO="check --workspace")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("gate linux-lint FAIL exit=7", result.stdout)
        self.assertIn("gate windows-clippy PASS", result.stdout)
        self.assertEqual(len(self.commands("ck")), 1)
        command = self.commands("ck")[0]
        self.assertEqual(command[command.index("--cwd") + 1], str(self.root))
        self.assertFalse(self.lanes.exists())

    def test_rejects_unowned_checkout(self):
        (self.lanes / "linux-lint").mkdir(parents=True)
        result = self.run_gate("--lane", "linux-lint")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("Refusing to reset", result.stderr)
        self.assertEqual(self.commands("ck"), [])

    def test_invalid_selection_never_runs_checks(self):
        for args in (("--fast", "--lane", "musl"), ("--lane", "missing"),
                     ("--lane", "musl", "--lane", "musl")):
            with self.subTest(args=args):
                result = self.run_gate(*args)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(self.commands("cargo"), [])
        self.assertEqual(self.commands("ck"), [])


if __name__ == "__main__":
    unittest.main()
