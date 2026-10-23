#!/usr/bin/env python3
"""Portable regression tests for the installed-daemon evidence tools."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import fixtures
import qualification


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.records = [
            {"type": "accepted", "job_id": "a", "platform": "macos"},
            {"type": "started", "seq": 0},
            {"type": "output", "seq": 1, "stream": "stdout", "bytes": "AP8K"},
            {"type": "output", "seq": 2, "stream": "stderr", "bytes": "ZXJyCg=="},
            {"type": "terminal", "job_id": "a", "outcome": {"type": "exit", "code": 23}},
        ]
        (self.root / "guest.stdout").write_bytes(b"\0\xff\n")
        (self.root / "guest.stderr").write_bytes(b"err\n")
        (self.root / "guest.status.json").write_text(json.dumps({"returncode": 23, "elapsed_seconds": 1.25}))
        (self.root / "cli.status").write_text("23")

    def write(self):
        path = self.root / "records.jsonl"
        path.write_text("".join(json.dumps(row) + "\n" for row in self.records))
        return path

    def test_independent_binary_streams_status_and_replays(self):
        self.records.insert(4, self.records[2].copy())
        self.write()
        result = qualification.verify(self.root)
        self.assertEqual(result["stdout"]["bytes"], 3)
        self.assertEqual((self.root / "output.stderr").read_bytes(), b"err\n")
        self.assertEqual(result["guest"]["returncode"], 23)

    def test_lost_reordered_or_changed_replay_is_not_parity(self):
        original = json.loads(json.dumps(self.records))
        for change in (lambda: self.records.pop(2),
                       lambda: self.records.__setitem__(slice(2, 4), list(reversed(self.records[2:4]))),
                       lambda: self.records.insert(4, dict(self.records[2], bytes="YQ=="))):
            self.records = json.loads(json.dumps(original))
            change()
            with self.assertRaisesRegex(ValueError, "sequence|replay"):
                qualification.decode(self.write())

    def test_byte_status_platform_and_missing_evidence_fail_closed(self):
        self.write()
        (self.root / "guest.stdout").write_bytes(b"wrong")
        with self.assertRaisesRegex(ValueError, "byte mismatch"):
            qualification.verify(self.root)
        (self.root / "guest.stdout").write_bytes(b"\0\xff\n")
        (self.root / "cli.status").write_text("0")
        with self.assertRaisesRegex(ValueError, "CLI exit"):
            qualification.verify(self.root)
        (self.root / "cli.status").write_text("23")
        self.records[-1]["outcome"]["code"] = 0
        with self.assertRaisesRegex(ValueError, "guest status"):
            self.write(); qualification.verify(self.root)
        self.records[0]["platform"] = "linux"
        with self.assertRaisesRegex(ValueError, "Accepted.platform"):
            qualification.decode(self.write())
        self.records = self.records[1:]
        with self.assertRaisesRegex(ValueError, "missing Accepted"):
            qualification.decode(self.write())

    def test_guest_capture_preserves_actual_execution_and_waits_for_collection(self):
        capture = self.root / "capture"
        source = "import os; os.write(1,b'\\x00\\xff\\n'); os.write(2,b'err\\n'); raise SystemExit(23)"
        import shlex
        command = shlex.join([sys.executable, "-c", source])
        runner = "import guest_capture,sys; sys.exit(guest_capture.run(sys.argv[1],sys.argv[2],shell='/bin/sh',timeout=10))"
        child = subprocess.Popen([sys.executable, "-c", runner, command, str(capture)], cwd=HERE,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 10
            while not (capture / "complete").exists():
                self.assertLess(time.monotonic(), deadline)
                self.assertIsNone(child.poll())
                time.sleep(0.01)
            self.assertIsNone(child.poll(), "job must not exit before host copies its evidence")
            self.assertEqual((capture / "guest.stdout").read_bytes(), b"\0\xff\n")
            self.assertEqual((capture / "guest.stderr").read_bytes(), b"err\n")
            self.assertEqual(json.loads((capture / "guest.status.json").read_text())["returncode"], 23)
            (capture / "collected").touch()
            stdout, stderr = child.communicate(timeout=10)
            self.assertEqual((child.returncode, stdout, stderr), (23, b"\0\xff\n", b"err\n"))
        finally:
            if child.poll() is None:
                child.kill(); child.communicate()


class FixtureTests(unittest.TestCase):
    def test_generated_committed_fixtures_and_exact_private_cache_boundary(self):
        with tempfile.TemporaryDirectory() as directory:
            original = fixtures.command
            def with_hostile_commit_policy(*args, cwd):
                result = original(*args, cwd=cwd)
                if args[:2] == ("git", "init"):
                    original("git", "config", "commit.gpgsign", "true", cwd=cwd)
                    hook = cwd / ".git/hooks/pre-commit"
                    hook.parent.mkdir(parents=True, exist_ok=True)
                    hook.write_text("#!/bin/sh\nexit 99\n")
                    hook.chmod(0o755)
                return result
            with patch.dict(os.environ, {"GIT_DIR": str(Path(directory) / "wrong-git-dir"),
                                         "GIT_INDEX_FILE": str(Path(directory) / "wrong-index")}), \
                    patch.object(fixtures, "command", side_effect=with_hostile_commit_policy):
                result = fixtures.create(Path(directory) / "fixtures")
            self.assertFalse((Path(directory) / "wrong-index").exists())
            for name in ("workspace", "sibling", "private_good", "private_missing"):
                self.assertEqual(original("git", "status", "--porcelain", cwd=result[name]), "")
            package = json.loads((Path(result["workspace"]) / "package.json").read_text())
            self.assertEqual(package["dependencies"]["@esbuild/darwin-arm64"], "0.25.5")
            self.assertEqual(package["dependencies"]["motor-qualification-sibling"], "file:../sibling")
            cache = next((Path(result["client_cargo_home"]) / "git/db").iterdir())
            self.assertEqual(original("git", "cat-file", "-t", result["bundled_commit"], cwd=cache), "commit")
            negative = subprocess.run(["git", "cat-file", "-t", result["missing_commit"]], cwd=cache, capture_output=True)
            self.assertNotEqual(negative.returncode, 0)
            self.assertIn(result["missing_commit"], (Path(result["private_missing"]) / "Cargo.lock").read_text())


if __name__ == "__main__":
    unittest.main()
