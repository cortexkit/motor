"""Exercise the source file size fence against small git fixtures."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("source_file_size", ROOT / "scripts/source-file-size.py")
size = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(size)


class SourceFileSizeTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(os.environ.get("TMPDIR", ROOT / "target/scratch"))
        scratch.mkdir(parents=True, exist_ok=True)
        directory = tempfile.TemporaryDirectory(prefix="source-file-size-", dir=scratch)
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        self.exceptions = size.EXCEPTIONS
        self.addCleanup(setattr, size, "EXCEPTIONS", self.exceptions)
        size.EXCEPTIONS = {}

    def put(self, name, lines, track=True):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("x\n" * lines)
        if track:
            subprocess.run(["git", "-C", str(self.root), "add", name], check=True)

    def test_files_at_the_limit_pass_and_over_it_fail(self):
        self.put("crates/a/src/small.rs", size.LIMIT)
        self.assertEqual(size.violations(self.root), [])
        self.put("crates/a/src/big.rs", size.LIMIT + 1)
        self.assertEqual(len(size.violations(self.root)), 1)
        self.assertIn("crates/a/src/big.rs", size.violations(self.root)[0])

    def test_every_source_language_counts(self):
        for name in ("a.py", "b.sh", "c.ps1", "d.swift"):
            self.put(f"script/{name}", size.LIMIT + 1)
        self.assertEqual(len(size.violations(self.root)), 4)

    def test_tests_generated_untracked_and_other_files_are_exempt(self):
        for name in ("crates/a/src/tests.rs", "crates/a/src/engine_tests.rs", "crates/a/tests/it/main.rs",
                     "script/test_x.py", "test-vectors/v/gen.rs", "crates/a/src/generated_table.rs",
                     "docs/long.md"):
            self.put(name, size.LIMIT + 1)
        self.put("crates/a/src/untracked.rs", size.LIMIT + 1, track=False)
        self.assertEqual(size.violations(self.root), [])

    def test_an_exception_may_shrink_but_never_grow(self):
        size.EXCEPTIONS = {"crates/a/src/engine.rs": (7000, "older than the limit")}
        self.put("crates/a/src/engine.rs", 7000)
        self.assertEqual(size.violations(self.root), [])
        self.put("crates/a/src/engine.rs", 7001)
        self.assertIn("may not grow past 7000", size.violations(self.root)[0])

    def test_a_stale_exception_is_reported(self):
        size.EXCEPTIONS = {"crates/a/src/gone.rs": (7000, "older than the limit")}
        self.assertIn("remove the entry", size.violations(self.root)[0])

    def test_the_repository_passes(self):
        size.EXCEPTIONS = self.exceptions
        self.assertEqual(size.violations(ROOT), [])


if __name__ == "__main__":
    unittest.main()
