import importlib.util
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("toolchain_parity", Path(__file__).with_name("toolchain-parity.py"))
if spec is None or spec.loader is None:
    raise ImportError("cannot load toolchain parity checker")
parity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(parity)


class CaptureTests(unittest.TestCase):
    def test_timeout_and_missing_binary_are_recorded(self):
        for error, message in ((subprocess.TimeoutExpired("tool", 30), "timeout"),
                               (FileNotFoundError("missing"), "missing")):
            with self.subTest(error=message), patch.object(parity.subprocess, "run", side_effect=error):
                self.assertEqual(parity.binary_version("/bin/tool", {}), {"version": None, "error": message})

    def test_sccache_inventory_never_runs_a_cache_process(self):
        with patch.object(parity.subprocess, "run") as run:
            for name in ("sccache", "sccache-0.18.0-stock"):
                self.assertEqual(parity.binary_version("/cargo/bin/" + name, {}),
                                 {"version": None, "error": "not-probed: sccache"})
            run.assert_not_called()

    def test_gnu_test_inventory_never_passes_a_version_flag(self):
        with patch.object(parity.subprocess, 'run') as run:
            self.assertEqual(parity.binary_version('/usr/bin/gnutest', {}),
                             {'version': None, 'error': 'not-probed: test has no version flag'})
            run.assert_not_called()

    def test_coreutils_prefixed_true_uses_canonical_argv0(self):
        with patch.object(parity.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, 'true (GNU coreutils) 9.7', '')) as run:
            parity.binary_version('/usr/bin/gnutrue', {})
        self.assertEqual(run.call_args.args[0], ['true', '--version'])
        self.assertEqual(run.call_args.kwargs['executable'], '/usr/bin/gnutrue')

    def test_capture_reads_live_installed_target_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'bin').mkdir()
            (root / 'bin/rustup').write_text('proxy fixture')
            name = '1.99.0-x86_64-unknown-linux-gnu'
            targets = root / 'toolchains' / name / 'lib/rustlib'
            targets.mkdir(parents=True)
            for target in ('x86_64-unknown-linux-gnu', 'wasm32-unknown-unknown'):
                (targets / target / 'lib').mkdir(parents=True)
                (targets / target / 'lib/libstd-fixture.rlib').write_bytes(b'std fixture')
            def output(*args, **kwargs):
                if args[1:] == ('toolchain', 'list'):
                    return name + ' (default)'
                if args[1:] == ('target', 'list', '--installed', '--toolchain', name):
                    return '\n'.join(sorted(path.name for path in targets.iterdir()))
                if args[1:] == ('component', 'list', '--installed', '--toolchain', name):
                    return 'cargo-x86_64-unknown-linux-gnu\nrustc-x86_64-unknown-linux-gnu'
                if args[1:] == ('default',):
                    return name + ' (default)'
                if args[1:] == ('show', 'profile'):
                    return 'minimal'
                if args[1:] == ('--version',):
                    return 'rustup 1.29.1' if args[0].endswith('/bin/rustup') else 'rustc 1.99.0 (b940084d7 2026-09-28)'
                if args[0] == 'dpkg-query':
                    return ''
                self.fail('unexpected command: ' + repr(args))
            original_read = Path.read_text
            def read(path, *args, **kwargs):
                if str(path) == '/etc/os-release':
                    return 'ID=ubuntu\n'
                return original_read(path, *args, **kwargs)
            with patch.object(parity, 'output', side_effect=output), patch.object(parity.shutil, 'which', return_value=None), \
                 patch.object(Path, 'read_text', read):
                before = parity.capture({'cargo_home': str(root), 'rustup_home': str(root)})
                (targets / 'wasm32-unknown-unknown/lib/libstd-fixture.rlib').unlink()
                incomplete = parity.capture({'cargo_home': str(root), 'rustup_home': str(root)})
                (targets / 'wasm32-unknown-unknown/lib').rmdir()
                (targets / 'wasm32-unknown-unknown').rmdir()
                after = parity.capture({'cargo_home': str(root), 'rustup_home': str(root)})
            self.assertIn('wasm32-unknown-unknown', before['rust_toolchains'][name]['targets'])
            self.assertTrue(before['rust_target_payloads'][name]['wasm32-unknown-unknown'])
            self.assertIn('wasm32-unknown-unknown', incomplete['rust_toolchains'][name]['targets'])
            self.assertFalse(incomplete['rust_target_payloads'][name]['wasm32-unknown-unknown'])
            self.assertNotIn('wasm32-unknown-unknown', after['rust_toolchains'][name]['targets'])


class ParityTests(unittest.TestCase):
    def setUp(self):
        self.manifest = json.loads(parity.MANIFEST.read_text())

    def observed(self):
        manifest = self.manifest
        bins = {}
        for name, expected in manifest['cargo_binaries'].items():
            bins[name] = {'symlink': 'rustup' if expected.get('proxy') else None,
                          'sha256': manifest['sccache']['build_server_sha256'].get(name, name),
                          'version': expected.get('version'), 'exit_code': 0}
        for name, expected in manifest['cargo_binaries'].items():
            if 'matches' in expected:
                bins[name]['sha256'] = bins[expected['matches']]['sha256']
        tools = {name: {'path': spec['path'], 'realpath': spec['path'] + spec.get('realpath_suffix', ''),
                        'version': spec.get('version', spec.get('observed_build_server_version')), 'exit_code': 0}
                 for name, spec in manifest['tools'].items()}
        tools.update({name: {'path': path, 'version': name[3:] + ' (GNU coreutils) 9.7', 'exit_code': 0}
                      for name, path in manifest['gnu_coreutils'].items()})
        for name, row in tools.items():
            if name in manifest['gnu_coreutils']:
                row['package'] = 'gnu-coreutils: ' + row['path']
        tools['gnutest'].update(version=None, sha256='package-qualified-test')
        return {**{key: copy.deepcopy(manifest[key]) for key in ('rust_default', 'rust_profile', 'rust_toolchains', 'cargo_home', 'rustup_home')},
                'rustup_version': 'rustup ' + manifest['rustup'], 'cargo_binaries': bins, 'tools': tools,
                'rust_target_payloads': {name: {target: True for target in spec['targets']} for name, spec in manifest['rust_toolchains'].items()},
                'apt_packages': {name: 'distro-version' for name in manifest['apt_packages']['required'] + ['coreutils', 'gnu-coreutils']},
                'sccache_inputs_sha256': manifest['sccache']['inputs_sha256'],
                'job_environment': manifest['job_environment'].copy(),
                'mountinfo': '1 0 0:0 / /home/ubuntu/.rustup ro - ext4 /dev/example rw'}

    def report(self, actual, job=False):
        with patch.object(parity, 'capture', return_value=actual):
            return parity.check(self.manifest, job=job, gnu_test_sha256='package-qualified-test')

    def test_measured_contract_passes(self):
        self.assertTrue(self.report(self.observed())['passed'])
        self.assertTrue(self.report(self.observed(), job=True)['passed'])

    def test_gnu_dd_and_test_exact_server_outputs_are_accepted(self):
        actual = self.observed()
        actual['tools']['gnudd'].update(version='dd (coreutils) 9.7', exit_code=0, package='gnu-coreutils: /usr/bin/gnudd')
        actual['tools']['gnutest'].update(version='', exit_code=0, package='gnu-coreutils: /usr/bin/gnutest')
        self.assertTrue(self.report(actual)['passed'])
        actual['tools']['gnudd']['package'] = ''
        self.assertTrue(self.report(actual)['passed'])
        actual['tools']['gnutest']['package'] = 'coreutils-from-uutils: /usr/bin/gnutest'
        self.assertFalse(self.report(actual)['passed'])

    def test_job_versionless_gnu_test_requires_fresh_host_hash(self):
        actual = self.observed()
        actual['tools']['gnutest']['package'] = ''
        self.assertTrue(self.report(actual, job=True)['passed'])
        actual['tools']['gnutest']['sha256'] = 'different-file'
        self.assertFalse(self.report(actual, job=True)['passed'])

    def test_missing_image_wasm_target_fails_check(self):
        actual = self.observed()
        actual['rust_toolchains']['1.99.0-x86_64-unknown-linux-gnu']['targets'].remove('wasm32-unknown-unknown')
        report = self.report(actual)
        self.assertFalse(report['passed'])
        self.assertEqual(len(report['errors']), 1)
        self.assertIn('1.99.0-x86_64-unknown-linux-gnu targets:', report['errors'][0])

    def test_toolchain_component_and_default_drift_fail(self):
        for key in ('rust_default', 'rust_profile'):
            with self.subTest(key=key):
                actual = self.observed()
                actual[key] = 'unexpected'
                self.assertFalse(self.report(actual)['passed'])
        actual = self.observed()
        actual['rust_toolchains']['stable-x86_64-unknown-linux-gnu']['components'].append('clippy-x86_64-unknown-linux-gnu')
        self.assertFalse(self.report(actual)['passed'])

    def test_missing_target_payload_fails_even_with_rustup_receipt(self):
        actual = self.observed()
        actual['rust_target_payloads']['1.99.0-x86_64-unknown-linux-gnu']['wasm32-unknown-unknown'] = False
        report = self.report(actual)
        self.assertEqual(report['errors'], ['1.99.0-x86_64-unknown-linux-gnu: missing libstd payload for wasm32-unknown-unknown'])
        self.assertFalse(report['passed'])

    def test_cargo_versions_proxies_default_and_stock_cache_are_checked(self):
        for name, key, value in (('cargo-nextest', 'version', 'cargo-nextest 0.0.1'),
                                 ('rust-analyzer', 'symlink', None),
                                 ('ckdev-mutate', 'sha256', 'wrong-default'),
                                 ('sccache', 'sha256', 'stock')):
            with self.subTest(name=name):
                actual = self.observed()
                actual['cargo_binaries'][name][key] = value
                self.assertFalse(self.report(actual)['passed'])

    def test_tools_and_packages_are_checked(self):
        for name in ('bun', 'wasm-opt', 'x86_64-w64-mingw32-gcc', 'gnucp'):
            with self.subTest(name=name):
                actual = self.observed()
                del actual['tools'][name]
                self.assertFalse(self.report(actual)['passed'])
        actual = self.observed()
        del actual['apt_packages']['libsqlite3-dev']
        self.assertFalse(self.report(actual)['passed'])

    def test_readonly_rustup_and_exact_job_path_are_checked(self):
        actual = self.observed()
        actual['mountinfo'] = actual['mountinfo'].replace('ro -', 'rw -')
        self.assertIn('job rustup home is not mounted read-only', self.report(actual, job=True)['errors'])
        actual = self.observed()
        actual['job_environment']['PATH'] = '/usr/local/bin:/usr/bin:/bin'
        self.assertFalse(self.report(actual, job=True)['passed'])

    def test_boat_cache_artifact_hashes_bind_the_build_record_to_live_binaries(self):
        actual = self.observed()
        actual['sccache_artifacts'] = {'inputs_sha256': self.manifest['sccache']['inputs_sha256'], 'binaries': {
            name: actual['cargo_binaries'][name]['sha256'] for name in self.manifest['sccache']['build_server_sha256']}}
        self.assertEqual(parity.differences(self.manifest, actual, role='boat'), [])
        actual['cargo_binaries']['sccache']['sha256'] = 'replaced-after-build'
        self.assertTrue(any('sccache built artifact hash' in error for error in parity.differences(self.manifest, actual, role='boat')))

    def test_unpinned_versions_and_inconsistent_components_are_refused(self):
        manifest = copy.deepcopy(self.manifest)
        manifest['tools']['bun']['version'] = 'latest'
        with self.assertRaisesRegex(ValueError, 'unpinned version: bun'):
            parity.validate(manifest)
        manifest = copy.deepcopy(self.manifest)
        manifest['rust_toolchains']['1.99.0-x86_64-unknown-linux-gnu']['targets'].remove('wasm32-unknown-unknown')
        with self.assertRaisesRegex(ValueError, 'targets and components disagree'):
            parity.validate(manifest)

    def test_check_cli_returns_nonzero_on_drift(self):
        with patch.object(parity.sys, 'argv', ['toolchain-parity.py', '--check']), \
             patch.object(parity, 'check', return_value={'passed': False, 'errors': ['missing target']}), \
             contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(parity.main(), 1)
        self.assertFalse(json.loads(out.getvalue())['passed'])


class InstallerTests(unittest.TestCase):
    def test_cargo_installer_consumes_manifest_and_places_pinned_defaults(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = json.loads(parity.MANIFEST.read_text())
            (root / 'toolchain.json').write_text(json.dumps(manifest))
            script = root / 'install-tools.sh'
            script.write_text((parity.HERE / 'install-tools.sh').read_text().replace('/srv/build', str(root)))
            fake_bin = root / 'fake-bin'
            fake_bin.mkdir()
            cargo = fake_bin / 'cargo'
            cargo.write_text('#!/usr/bin/env python3\nimport json, os, pathlib, sys\n'
                             'args = sys.argv[1:]\n'
                             'manifest = json.loads(pathlib.Path(os.environ["TEST_MANIFEST"]).read_text())\n'
                             'tool = next(row for row in manifest["cargo_installs"] if row["crate"] == args[-1])\n'
                             'root = pathlib.Path(args[args.index("--root")+1] if "--root" in args else os.environ["CARGO_HOME"])\n'
                             '(root / "bin").mkdir(parents=True, exist_ok=True)\n'
                             'for name in tool["bins"]: (root / "bin" / name).write_text("binary:" + " ".join(args))\n')
            cargo.chmod(0o755)
            result = subprocess.run(['bash', str(script)], env=dict(os.environ, TEST_MANIFEST=str(root / 'toolchain.json'),
                                    PATH=str(fake_bin) + os.pathsep + os.environ['PATH']), capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            bins = root / 'cargo-home/bin'
            self.assertIn('--locked --version =0.9.138 cargo-nextest', (bins / 'cargo-nextest').read_text())
            default = bins / 'ckdev-mutate'
            self.assertEqual(default.read_bytes(), (bins / 'ckdev-mutate-9d6b663d').read_bytes())
            self.assertTrue((bins / 'worker-codegen').is_file())
            for tool in manifest['cargo_installs']:
                if 'git' in tool:
                    self.assertIn('--rev ' + tool['rev'], (bins / ('ckdev-mutate-' + tool['rev'][:8])).read_text())

    def test_fresh_sccache_build_installs_stock_without_running_either_binary(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'toolchain.json').write_text(parity.MANIFEST.read_text())
            script = root / 'build-sccache.sh'
            script.write_text((parity.HERE / 'build-sccache.sh').read_text())
            (root / 'patches').mkdir()
            (root / 'patches/sccache-0.18.0-read-only-hits.patch').write_text('patch fixture')
            fake_bin = root / 'fake-bin'
            fake_bin.mkdir()
            cargo_home = root / 'cargo'
            (cargo_home / 'bin').mkdir(parents=True)
            cargo = fake_bin / 'cargo'
            cargo.write_text('#!/usr/bin/env python3\nimport os, pathlib, sys\n'
                             'args = sys.argv[1:]\n'
                             'if args[0] == "install":\n'
                             '    source = pathlib.Path(os.environ["CARGO_HOME"]) / "registry/src/index/sccache-0.18.0"\n'
                             '    source.mkdir(parents=True)\n'
                             '    path = pathlib.Path(args[args.index("--root")+1]) / "bin/sccache"\n'
                             'else: path = pathlib.Path(args[args.index("--target-dir")+1]) / "release/sccache"\n'
                             'path.parent.mkdir(parents=True, exist_ok=True)\n'
                             'path.write_text("#!/bin/sh\\nexit 77\\n# " + args[0] + "\\n")\n'
                             'path.chmod(0o755)\n')
            cargo.chmod(0o755)
            patcher = fake_bin / 'patch'
            patcher.write_text('#!/bin/sh\nexit 0\n')
            patcher.chmod(0o755)
            result = subprocess.run(['sh', str(script)], env=dict(os.environ, CARGO_HOME=str(cargo_home), TMPDIR=str(root),
                                    PATH=str(fake_bin) + os.pathsep + os.environ['PATH']), capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotEqual((cargo_home / 'bin/sccache').read_bytes(), (cargo_home / 'bin/sccache-0.18.0-stock').read_bytes())


if __name__ == "__main__":
    unittest.main()
