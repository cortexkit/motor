"""Scratch-copy controls for motor's scratch and daemon fences."""
from pathlib import Path
import runpy
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
scratch = runpy.run_path(str(SCRIPTS / "test-scratch.py"))
daemon = runpy.run_path(str(SCRIPTS / "subc-daemon-env.py"))
dev = runpy.run_path(str(SCRIPTS / "test-dev-binaries.py"))


class SafetyFencesTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="motor-gate-")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.source = self.root / "crates/motor-module/tests/planted.rs"
        self.source.parent.mkdir(parents=True)
        allow = self.root / "scripts/test-scratch.allow"
        allow.parent.mkdir()
        allow.write_text("# no scratch exemptions\n")

    def test_scratch_rejects_planted_global_temp_root(self):
        self.source.write_text('fn planted() { let _ = std::env::temp_dir(); }')
        errors = scratch["violations"](self.root, minimum_files=1)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("planted.rs:1: planted: use cortexkit_test_support::scratch_root()", errors[0])

    def test_published_scratch_helper_is_accepted(self):
        self.source.write_text('fn fenced() { let _ = cortexkit_test_support::scratch_root(); }')
        self.assertEqual(scratch["violations"](self.root, minimum_files=1), [])
        self.assertEqual(scratch["self_check"](), [])

    def test_scratch_inventory_floor_refuses_empty_scan(self):
        self.assertEqual(scratch["violations"](self.root), [
            f'tree scan read 0 Rust files; expected at least {scratch["MINIMUM_FILES"]}'
        ])

    def test_daemon_rejects_planted_raw_binary(self):
        self.source.write_text('fn planted() { Command::new("/tmp/ck-subc"); }')
        errors = daemon["violations"](self.root, minimum_files=1)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("planted.rs:1: names the subc daemon binary", errors[0])
        self.assertIn("cortexkit_test_support::SubcDaemonBinary", errors[0])

    def test_daemon_rejects_raw_identifier_and_literals(self):
        for source in (
            'fn planted() { Command::new(&subc_binary); }',
            'const BINARY: &str = r#"ck-subc.exe"#;',
            'const BINARY: &str = "ck-subc";',
        ):
            with self.subTest(source=source):
                self.source.write_text(source)
                self.assertTrue(daemon["violations"](self.root, minimum_files=1))

    def test_published_daemon_helper_and_prose_are_accepted(self):
        self.source.write_text('''// Command::new("ck-subc")
fn fenced() {
    let binary = cortexkit_test_support::SubcDaemonBinary::build();
    let _ = binary.command();
    let _ = "spawn ck-subc";
}''')
        self.assertEqual(daemon["violations"](self.root, minimum_files=1), [])

    def test_daemon_inventory_floor_refuses_empty_scan(self):
        self.assertEqual(daemon["violations"](self.root), [
            f'tree scan read 0 Rust files; expected at least {daemon["MINIMUM_FILES"]}'
        ])

    def test_dev_binary_inventory_floor_refuses_empty_scan(self):
        self.assertEqual(dev["violations"](self.root), [
            f'only scanned 0 files; expected at least {dev["MINIMUM_FILES"]}'
        ])


if __name__ == "__main__":
    unittest.main()
