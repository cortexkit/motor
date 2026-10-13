import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "test-binary-fold.py"
spec = importlib.util.spec_from_file_location("test_binary_fold", SCRIPT)
fold = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fold)


class TestBinaryFold(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.tests = self.root / "crates/example/tests"
        self.tests.mkdir(parents=True)

    def write(self, name):
        path = self.tests / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#[test] fn example() {}\n")
        return path.relative_to(self.root).as_posix()

    def test_top_level_untracked_file_is_rejected_with_remedy(self):
        path = self.write("new_suite.rs")
        self.assertEqual(fold.violations(self.root, set()), [
            f"{path}: move the file into tests/it/ as a module, or add it to the allowlist "
            "with the reason it needs its own process"
        ])

    def test_directory_entry_point_requires_allowlist(self):
        path = self.write("new_suite/main.rs")
        self.assertEqual(fold.violations(self.root, set()), [f"{path}: {fold.REMEDY}"])
        self.assertEqual(fold.violations(self.root, {path}), [])

    def test_folded_modules_and_nested_fixture_mains_are_not_entry_points(self):
        entry = self.write("it/main.rs")
        self.write("it/new_suite.rs")
        self.write("it/nested/main.rs")
        self.write("compile_fail/probe/src/main.rs")
        self.assertEqual(fold.violations(self.root, {entry}), [])

    def test_explicit_directory_entry_point_requires_allowlist(self):
        path = self.write("isolated/entry.rs")
        entries = [self.root / path]
        self.assertEqual(fold.violations(self.root, set(), entries), [f"{path}: {fold.REMEDY}"])
        self.assertEqual(fold.violations(self.root, {path}, entries), [])

    def test_allowlist_requires_reason_and_unique_path(self):
        allow = self.root / "allow"
        for invalid in ["crates/example/tests/test.rs\n", "p\t\n", "p\treason\np\treason\n"]:
            allow.write_text(invalid)
            with self.assertRaises(ValueError):
                fold.read_allowlist(allow)
        allow.write_text("# exceptions\np\tNeeds an isolated environment\n")
        self.assertEqual(fold.read_allowlist(allow), {"p"})


if __name__ == "__main__":
    unittest.main()
