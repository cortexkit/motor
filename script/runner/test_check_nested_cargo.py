import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
CHECK = ROOT / 'script/runner/check_nested_cargo.sh'


class NestedCargoAuditTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.projects = self.root / 'projects'
        self.repo = self.projects / 'fixture'
        self.repo.mkdir(parents=True)
        self.git('init', '-q')
        self.env = dict(os.environ, RUSTC_WRAPPER='', CARGO_BUILD_RUSTC_WRAPPER='',
                        CARGO_HOME=str(self.root / 'cargo-home'))
        for name in list(self.env):
            if name.startswith('GIT_CONFIG_'):
                self.env.pop(name)

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.repo), *args], stderr=subprocess.PIPE)

    def package(self, directory, stale=False):
        path = self.repo / directory
        (path / 'src').mkdir(parents=True)
        name = 'root-probe' if directory == '.' else directory.replace('/', '-')
        (path / 'src/lib.rs').write_text('pub fn fixture() {}')
        (path / 'Cargo.toml').write_text(f'[package]\nname="{name}"\nversion="0.2.0"\nedition="2021"\n[workspace]\n')
        version = '0.1.0' if stale else '0.2.0'
        (path / 'Cargo.lock').write_text(f'version=4\n[[package]]\nname="{name}"\nversion="{version}"\n')

    def commit(self):
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@invalid', 'commit', '-qm', 'snapshot')

    def run_check(self, *args):
        result = subprocess.run(['bash', str(CHECK), str(self.projects), *args], env=self.env,
                                text=True, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def test_stale_nested_warns_root_is_excluded_and_later_lock_passes(self):
        self.package('.', stale=True)
        self.package('a-stale', stale=True)
        self.package('b-ready')
        (self.repo / '.cargo').mkdir()
        (self.repo / '.cargo/config.toml').write_text('invalid repository config [')
        (self.repo / 'rust-toolchain.toml').write_text('[toolchain]\nchannel="not-a-real-toolchain"\n')
        self.commit()
        self.package('untracked', stale=True)
        before = self.git('status', '--porcelain')
        output = self.run_check()
        self.assertIn('WARN fixture/a-stale/Cargo.lock:', output)
        self.assertIn('--locked', output)
        self.assertIn('PASS fixture/b-ready/Cargo.lock', output)
        self.assertNotIn('fixture/Cargo.lock', output)
        self.assertNotIn('untracked', output)
        self.assertEqual(self.git('status', '--porcelain'), before)

    def test_invalid_and_over_limit_nested_locks_skip_without_failure(self):
        self.package('.')
        self.package('a-linked')
        (self.repo / 'a-linked/Cargo.lock').unlink()
        (self.repo / 'a-linked/Cargo.lock').symlink_to('../Cargo.lock')
        (self.repo / 'b-missing').mkdir()
        (self.repo / 'b-missing/Cargo.lock').write_text('version=4\n')
        for i in range(128):
            path = self.repo / f'lock-{i:03}'
            path.mkdir()
            (path / 'Cargo.lock').write_text('version=4\n')
        self.commit()
        output = self.run_check()
        self.assertIn('SKIP fixture/a-linked/Cargo.lock: requires tracked regular', output)
        self.assertIn('SKIP fixture/b-missing/Cargo.lock: requires tracked regular', output)
        self.assertIn('exceeds 128 tracked Cargo.lock limit', output)
        self.assertIn('advisory', output)

    def test_timeout_keeps_cargo_error_and_continues(self):
        self.package('a-stalled')
        self.package('b-ready')
        self.commit()
        tools = self.root / 'bin'
        tools.mkdir()
        cargo = tools / 'cargo'
        cargo.write_text('#!/bin/sh\ncase "$*" in *a-stalled*) echo "cargo error before stall" >&2; sleep 20;; *) exit 0;; esac\n')
        cargo.chmod(0o755)
        self.env['PATH'] = str(tools) + os.pathsep + self.env['PATH']
        output = self.run_check('--timeout', '0.1')
        self.assertIn('WARN fixture/a-stalled/Cargo.lock: timed out', output)
        self.assertIn('cargo error before stall', output)
        self.assertIn('PASS fixture/b-ready/Cargo.lock', output)


if __name__ == '__main__':
    unittest.main()
