"""Check release entry points pass checkout-derived version facts to Cargo."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class ReleaseBuildTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(os.environ.get("TMPDIR", ROOT / "target/scratch"))
        scratch.mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="release-build-", dir=scratch)
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.bin = self.root / "fixture-tools/bin"
        self.bin.mkdir(parents=True)
        self.trace = self.root / "trace"
        self.env = os.environ.copy()
        self.env.update(PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                        RELEASE_TRACE=str(self.trace), MOTOR_BUILD_COMMIT="stale-caller-value",
                        GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        subprocess.run(["git", "init", "-q"], cwd=self.root, env=self.env, check=True)
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release-test@localhost")
        self.git("commit", "--allow-empty", "-qm", "fixture")
        self.commit = self.git("rev-parse", "HEAD").stdout.strip()
        self.tool("cargo", """import json, os, pathlib, sys
args = sys.argv[1:]
if 'build' in args:
    with open(os.environ['RELEASE_TRACE'], 'a') as out:
        out.write(json.dumps({'args': args, 'commit': os.environ.get('MOTOR_BUILD_COMMIT')}) + '\\n')
    binary = 'motor-winagent' if '--manifest-path' in args else 'motor-runner'
    path = pathlib.Path('target/x86_64-pc-windows-msvc/release') / (binary + '.exe')
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b'binary fixture')
else:
    print('cargo fixture')
""")
        for name in ("rustc", "rustup", "sudo", "musl-gcc"):
            self.tool(name, f"print('{name} fixture')\n")

    def put(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def tool(self, name, body):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + body)
        path.chmod(0o755)

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, env=self.env,
                              text=True, capture_output=True, check=True)

    def test_windows_release_overrides_caller_commit_from_its_checkout(self):
        relative = "script/windows-runner/build-release.sh"
        self.put(relative, (ROOT / relative).read_text())
        self.put("script/windows-runner/cache-manifest.py", "print('cache fixture')\n")
        self.put("script/runner/toolchain.json", json.dumps({"windows_cross": {
            "cargo_xwin": "fixture", "vs_manifest": "fixture", "sdk": "fixture",
            "crt": "fixture", "arch": "x86_64", "variant": "desktop",
            "cache": "fixture-cache", "tools": "fixture-tools",
            "cache_tree_sha256": "a" * 64, "package_manifest_sha256": "b" * 64,
        }}))
        result = subprocess.run(["bash", relative], cwd=self.root, env=self.env,
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [json.loads(line) for line in self.trace.read_text().splitlines()]
        self.assertEqual(len(builds), 2)
        self.assertEqual(builds[0]["commit"], self.commit)
        self.assertIn("motor-runner", builds[0]["args"])
        provenance = json.loads((self.root / "target/windows-release-provenance.json").read_text())
        self.assertEqual(provenance["binary_source_commit"], self.commit)

    def test_tenant_musl_release_recipe_embeds_checkout_commit(self):
        readme = (ROOT / "script/tenant-image/README.md").read_text()
        recipe = next(block.split("```", 1)[0] for block in readme.split("```sh\n")[1:]
                      if "cargo build --release" in block.split("```", 1)[0])
        result = subprocess.run(["bash", "-ec", recipe], cwd=self.root, env=self.env,
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [json.loads(line) for line in self.trace.read_text().splitlines()]
        self.assertEqual(len(builds), 1)
        self.assertEqual(builds[0]["commit"], self.commit)
        self.assertIn("x86_64-unknown-linux-musl", builds[0]["args"])

    def test_server_gate_exports_checkout_commit_before_its_steps(self):
        source = (ROOT / "script/runner/server-gate.sh").read_text()
        setup, steps = source.split("\nstatus=0\n", 1)
        self.assertIn("step clippy cargo", steps)
        result = subprocess.run(["bash", "-c", setup + '\nprintf "%s" "$MOTOR_BUILD_COMMIT"'],
                                cwd=self.root, env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, self.commit)
        self.assertIn("export MOTOR_BUILD_COMMIT", setup)


if __name__ == "__main__":
    unittest.main()
