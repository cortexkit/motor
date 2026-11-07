"""Exercise the build-script fence against small workspace fixtures."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class NoBuildScriptsTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(os.environ.get("TMPDIR", ROOT / "target/scratch"))
        scratch.mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="no-build-scripts-", dir=scratch)
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.put("scripts/no-build-scripts.py", (ROOT / "scripts/no-build-scripts.py").read_text())

    def put(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def check(self):
        return subprocess.run(
            [sys.executable, str(self.root / "scripts/no-build-scripts.py")],
            capture_output=True, text=True,
        )

    def test_accepts_crates_without_build_scripts(self):
        self.put("crates/example/Cargo.toml", '[package]\nname = "example"\n# build = "old.rs"\ndescription = "build = is not a key here"\n')
        self.put("crates/example/src/main.rs", "fn main() {}\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("no-build-scripts: OK", result.stdout)

    def test_refuses_build_rs_anywhere_under_crates(self):
        self.put("crates/example/nested/build.rs", "fn main() {}\n")
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/example/nested/build.rs", result.stderr)
        self.assertIn("per checkout path and per commit", result.stderr)

    def test_refuses_manifest_build_key_even_when_disabled(self):
        for key in ("build", '"build"', "'build'", "package.build"):
            with self.subTest(key=key):
                self.put("crates/example/Cargo.toml", f"{key} = false\n")
                result = self.check()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("crates/example/Cargo.toml: build key", result.stderr)

    def test_refuses_inline_package_build_key(self):
        self.put("crates/example/Cargo.toml", 'package = { name = "example", build = "custom.rs" }\n')
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/example/Cargo.toml: build key", result.stderr)


if __name__ == "__main__":
    unittest.main()
