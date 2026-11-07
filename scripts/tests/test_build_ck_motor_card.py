"""Exercise card-build provenance and mainline checks without compiling Rust."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class BuildCardTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(os.environ.get("TMPDIR", ROOT / "target/scratch"))
        scratch.mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="card-build-", dir=scratch)
        self.addCleanup(directory.cleanup)
        self.base = Path(directory.name)
        self.root = self.base / "source"
        self.root.mkdir()
        scripts = self.root / "scripts"
        scripts.mkdir()
        (scripts / "build-ck-motor-card.sh").write_text((ROOT / "scripts/build-ck-motor-card.sh").read_text())
        (self.root / "Cargo.lock").write_bytes(b"lock fixture\n")
        self.bin = self.base / "bin"
        self.bin.mkdir()
        self.trace = self.base / "trace"
        self.target = self.base / "custom target"
        self.env = os.environ.copy()
        self.env.update(
            PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
            CARD_TRACE=str(self.trace), CARD_TARGET=str(self.target),
            GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
        )
        self.tool("rustc", 'print("host: x86_64-unknown-linux-gnu")\n')
        self.tool("cargo", """import json, os, pathlib, sys
if sys.argv[1] == 'metadata':
    print(json.dumps({'target_directory': os.environ['CARD_TARGET']}))
else:
    keys = ('MOTOR_BUILD_COMMIT', 'CK_BUILD_GIT_SHA', 'CK_BUILD_GIT_STATUS', 'CK_BUILD_LOCK_DIGEST')
    pathlib.Path(os.environ['CARD_TRACE']).write_text(json.dumps({
        'args': sys.argv[1:], 'env': {key: os.environ.get(key) for key in keys}}))
    if os.environ.get('FAIL_BUILD'):
        sys.exit(17)
    if os.environ.get('OMIT_BINARY'):
        sys.exit(0)
    binary = pathlib.Path(os.environ['CARD_TARGET']) / 'x86_64-unknown-linux-gnu/release/ck-motor'
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b'card binary fixture')
""")
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Card test")
        self.git("config", "user.email", "card-test@localhost")
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        self.commit = self.git("rev-parse", "HEAD").stdout.strip()
        self.remote = self.base / "origin.git"
        self.git("clone", "--bare", ".", str(self.remote))
        self.git("remote", "add", "origin", str(self.remote))

    def tool(self, name, body):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + body)
        path.chmod(0o755)

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, env=self.env,
                              text=True, capture_output=True, check=True)

    def run_build(self):
        return subprocess.run(["bash", "scripts/build-ck-motor-card.sh"],
                              cwd=self.root, env=self.env, text=True, capture_output=True)

    def test_builds_tip_with_exact_provenance_and_reports_binary_digest(self):
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        trace = json.loads(self.trace.read_text())
        self.assertEqual(trace["env"], {
            "MOTOR_BUILD_COMMIT": self.commit,
            "CK_BUILD_GIT_SHA": self.commit,
            "CK_BUILD_GIT_STATUS": "clean",
            "CK_BUILD_LOCK_DIGEST": hashlib.sha256(b"lock fixture\n").hexdigest(),
        })
        self.assertEqual(trace["args"], ["build", "--locked", "--release", "--target",
                                       "x86_64-unknown-linux-gnu", "-p", "motor-module", "--bin", "ck-motor"])
        self.assertIn(f"Building {self.commit} (origin/main tip)", result.stdout)
        self.assertIn(f"Binary: {self.target}/x86_64-unknown-linux-gnu/release/ck-motor", result.stdout)
        self.assertIn(hashlib.sha256(b"card binary fixture").hexdigest(), result.stdout)

    def test_fetches_and_accepts_an_older_main_commit(self):
        (self.root / "new-file").write_text("next main commit")
        self.git("add", ".")
        self.git("commit", "-qm", "advance main")
        tip = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("push", "origin", "main")
        self.git("update-ref", "-d", "refs/remotes/origin/main")
        self.git("reset", "--hard", self.commit)
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"Building {self.commit} (ancestor of origin/main; tip is {tip})", result.stdout)
        self.assertEqual(json.loads(self.trace.read_text())["env"]["CK_BUILD_GIT_SHA"], self.commit)

    def test_refuses_dirty_tracked_tree(self):
        (self.root / "Cargo.lock").write_text("modified")
        result = self.run_build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dirty tree", result.stderr)
        self.assertFalse(self.trace.exists())

    def test_refuses_untracked_files(self):
        (self.root / "untracked").touch()
        result = self.run_build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dirty tree", result.stderr)
        self.assertFalse(self.trace.exists())

    def test_refuses_off_main_head(self):
        (self.root / "new-file").write_text("off-main commit")
        self.git("add", ".")
        self.git("commit", "-qm", "off-main")
        result = self.run_build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HEAD is not on origin/main", result.stderr)
        self.assertFalse(self.trace.exists())

    def test_build_failure_does_not_report_a_binary(self):
        self.env["FAIL_BUILD"] = "1"
        result = self.run_build()
        self.assertEqual(result.returncode, 17)
        self.assertNotIn("Binary:", result.stdout)

    def test_missing_binary_does_not_report_a_successful_digest(self):
        self.env["OMIT_BINARY"] = "1"
        result = self.run_build()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("Binary:", result.stdout)


if __name__ == "__main__":
    unittest.main()
