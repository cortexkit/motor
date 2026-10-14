"""Fleet JavaScript validation with local stand-ins for SSH and package managers."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent

MANAGER = '''#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ["REMOTE_ROOT"]) / "state/js-manager-caches"
for name in ["BUN_INSTALL", "PNPM_HOME", "NPM_CONFIG_CACHE", "pnpm_config_store_dir"]:
    assert name not in os.environ, name
for name in ["HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "BUN_INSTALL_CACHE_DIR", "npm_config_cache"]:
    assert pathlib.Path(os.environ[name]).is_relative_to(root), name
assert "--ignore-scripts" in sys.argv
manager = pathlib.Path(sys.argv[0]).name
flag = {"bun": "--cache-dir", "npm": "--cache", "pnpm": "--store-dir"}[manager]
assert pathlib.Path(sys.argv[sys.argv.index(flag) + 1]).is_relative_to(root)
assert ("ci" if manager == "npm" else "--frozen-lockfile") in sys.argv
with open(os.environ["CALLS"], "a") as stream:
    stream.write(json.dumps({"manager": manager, "cache": os.environ["BUN_INSTALL_CACHE_DIR"]}) + "\\n")
sys.exit(1 if os.environ.get("FAIL_MANAGER") == manager else 0)
'''

SSH = '''#!/usr/bin/env python3
import os, subprocess, sys
args = sys.argv[1:]
if args[:1] == ["-o"]: args = args[2:]
args = [arg.replace("/srv/build/review/motor-js-rollout", os.environ["REMOTE_ROOT"]) for arg in args[1:]]
sys.exit(subprocess.run(["bash", "-c", args[0]] if len(args) == 1 else args).returncode)
'''

SCP = '''#!/usr/bin/env python3
import os, shutil, sys
source, dest = sys.argv[-2:]
dest = dest.split(":", 1)[1].replace("/srv/build/review/motor-js-rollout", os.environ["REMOTE_ROOT"])
shutil.copyfile(source, dest)
'''


class NestedJavascriptCheck(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="motor-nested-js-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.repo = self.root / "projects/fixture"
        self.repo.mkdir(parents=True)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, script in [("bun", MANAGER), ("npm", MANAGER), ("pnpm", MANAGER), ("ssh", SSH), ("scp", SCP)]:
            path = self.bin / name
            path.write_text(script)
            path.chmod(0o755)
        for manager, lock in [("bun", "bun.lock"), ("npm", "package-lock.json"), ("pnpm", "pnpm-lock.yaml")]:
            directory = self.repo / manager
            directory.mkdir()
            (directory / lock).write_text("fixture lock")
            (directory / "package.json").write_text("{}")
        self.env = os.environ | {
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "HOME": str(self.root / "operator"),
            "REMOTE_ROOT": str(self.root / "remote"),
            "CALLS": str(self.root / "calls"),
            "BUN_INSTALL": "/operator/bun",
            "BUN_INSTALL_CACHE_DIR": "/operator/bun-cache",
            "XDG_CACHE_HOME": "/operator/xdg",
            "npm_config_cache": "/operator/npm",
            "NPM_CONFIG_CACHE": "/operator/npm-uppercase",
            "pnpm_config_store_dir": "/operator/pnpm",
            "PNPM_HOME": "/operator/pnpm-home",
        }
        for args in [["init", "-q"], ["add", "."], ["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture"]]:
            subprocess.run(["git", "-C", str(self.repo), *args], check=True, capture_output=True)

    def test_remote_confines_all_managers_and_continues_after_failure(self):
        remote = Path(self.env["REMOTE_ROOT"])
        remote.mkdir()
        commit = subprocess.check_output(["git", "-C", str(self.repo), "rev-parse", "HEAD"], text=True).strip()
        subprocess.run(["git", "-C", str(self.repo), "bundle", "create", str(remote / "fixture.bundle"), "--all"], check=True, capture_output=True)
        result = subprocess.run(["bash", str(HERE / "check_nested_js_remote.sh"), str(remote), "fixture", commit], input="bun\nnpm\npnpm\n", env=self.env | {"FAIL_MANAGER": "npm"}, text=True, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("PASS fixture bun/bun.lock", result.stdout)
        self.assertIn("FAIL fixture npm/package-lock.json", result.stdout)
        self.assertIn("PASS fixture pnpm/pnpm-lock.yaml", result.stdout)
        calls = [json.loads(line) for line in Path(self.env["CALLS"]).read_text().splitlines()]
        self.assertEqual([call["manager"] for call in calls], ["bun", "npm", "pnpm"])
        self.assertEqual(len({call["cache"] for call in calls}), 3)

    def test_mac_dispatch_propagates_failure_and_cleans_remote(self):
        result = subprocess.run(["bash", str(HERE / "check_nested_js.sh"), str(self.repo.parent)], env=self.env | {"FAIL_MANAGER": "npm"}, text=True, capture_output=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAIL fixture npm/package-lock.json", result.stdout)
        self.assertIn("PASS fixture pnpm/pnpm-lock.yaml", result.stdout)
        self.assertFalse(Path(self.env["REMOTE_ROOT"]).exists())


if __name__ == "__main__":
    unittest.main()
