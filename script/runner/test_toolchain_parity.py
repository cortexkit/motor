import importlib.util
import contextlib
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tarfile
import unittest
import zipfile
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("toolchain_parity", Path(__file__).with_name("toolchain-parity.py"))
if spec is None or spec.loader is None:
    raise ImportError("cannot load toolchain parity checker")
parity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(parity)


class JobAccountTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location('job_account', parity.HERE / 'job-account.py')
        assert spec is not None and spec.loader is not None
        self.account = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.account)
        self.identity = {'uid': 999, 'gid': 986}

    def test_fresh_identity_is_created_with_exact_uid_and_gid(self):
        with patch.object(self.account.pwd, 'getpwuid', side_effect=KeyError), \
             patch.object(self.account.grp, 'getgrgid', side_effect=KeyError), \
             patch.object(self.account.pwd, 'getpwnam', side_effect=KeyError), \
             patch.object(self.account.grp, 'getgrnam', side_effect=KeyError), \
             patch.object(parity, 'output') as run:
            self.account.ensure(self.identity, run)
        self.assertEqual([('groupadd', '--system', '--gid', '986', 'motor-job'),
                          ('useradd', '--system', '--uid', '999', '--gid', '986', '--home-dir',
                           '/nonexistent', '--shell', '/usr/sbin/nologin', 'motor-job')],
                         [call.args for call in run.call_args_list])

    def test_numeric_collisions_name_the_owner_and_do_not_create_accounts(self):
        for namespace in ('uid', 'gid'):
            with self.subTest(namespace=namespace), \
                 patch.object(self.account.pwd, 'getpwuid', side_effect=KeyError if namespace == 'gid' else None,
                              return_value=('base-user',)), \
                 patch.object(self.account.grp, 'getgrgid', return_value=('base-group',)), \
                 patch.object(parity, 'output') as run:
                with self.assertRaisesRegex(ValueError, 'already owned by base-' + ('user' if namespace == 'uid' else 'group')):
                    self.account.ensure(self.identity, run)
                run.assert_not_called()


class CaptureTests(unittest.TestCase):
    def test_job_identity_is_the_process_not_a_host_account_lookup(self):
        with patch.object(parity.os, 'getuid', return_value=997), patch.object(parity.os, 'getgid', return_value=996):
            self.assertEqual({'uid': 997, 'gid': 996}, parity.job_identity(True))

    def test_runtime_capture_executes_in_tmpdir_and_records_noexec_failure(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, TMPDIR=tmp), \
             patch.object(parity.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
            # tempfile caches its selected directory, so reset it for this probe.
            with patch.object(tempfile, 'tempdir', tmp):
                runtime = parity.runtime_capture()
        self.assertTrue(runtime['tmpdir_exec'])
        self.assertTrue(str(run.call_args.args[0][0]).startswith(tmp + '/'))
        with patch.object(parity.subprocess, 'run', side_effect=PermissionError('noexec')):
            self.assertFalse(parity.runtime_capture()['tmpdir_exec'])

    def test_timeout_and_missing_binary_are_recorded(self):
        for error, message in ((subprocess.TimeoutExpired("tool", 30), "timeout"),
                               (FileNotFoundError("missing"), "missing")):
            with self.subTest(error=message), patch.object(parity.subprocess, "run", side_effect=error):
                self.assertEqual(parity.binary_version("/bin/tool", {}), {"version": None, "error": message})

    def test_package_alias_version_comes_from_its_pinned_package(self):
        observed = {'pnpm': {'version': '11.1.2', 'exit_code': 0}}
        with patch.object(parity, 'binary_version', side_effect=AssertionError('alias must not be executed')):
            self.assertEqual({'version': '11.1.2', 'exit_code': 0},
                             parity.tool_version('/usr/local/bin/pnpx', {}, observed, 'pnpm'))
        self.assertEqual({'version': None, 'exit_code': 1},
                         parity.tool_version('/usr/local/bin/pnpx', {}, {}, 'pnpm'))

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
            # A plain --capture, without a manifest, measures git too.
            self.assertIn('git', before['tools'])


class ParityTests(unittest.TestCase):
    def setUp(self):
        self.manifest = json.loads(parity.MANIFEST.read_text())

    def registry(self):
        checksum = 'a' * 64
        path = 'registry/cache/index.crates.io-fixture/cortexkit-cow-0.1.0.crate'
        return {'schema': 1, 'scope': 'tenant-1-private-fleet', 'crate_count': 1,
                'crates': [{'registry': 'index.crates.io-fixture', 'name': 'cortexkit-cow',
                            'version': '0.1.0', 'checksum': checksum, 'bytes': 7}],
                'files': {path: {'sha256': checksum, 'bytes': 7}}}

    def observed(self, role='build-server'):
        manifest = parity.role_manifest(self.manifest, role, job=True)
        bins = {}
        for name, expected in manifest['cargo_binaries'].items():
            bins[name] = {'symlink': 'rustup' if expected.get('proxy') else None,
                          'sha256': manifest['sccache']['build_server_sha256'].get(name, name),
                          'version': expected.get('version'), 'exit_code': 0}
        for name, expected in manifest['cargo_binaries'].items():
            if 'matches' in expected:
                bins[name]['sha256'] = bins[expected['matches']]['sha256']
        tools = {name: {'path': spec.get('path_by_role', {}).get(role, spec['path']),
                        'realpath': spec['path'] + spec.get('realpath_suffix',
                                                             spec.get('realpath_suffix_by_role', {}).get(role, '')),
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
                'apt_packages': {name: 'distro-version' for name in manifest['apt_packages']['required']
                                 + manifest['apt_packages']['by_role'][role] + ['coreutils', 'gnu-coreutils']},
                'sccache_inputs_sha256': manifest['sccache']['inputs_sha256'],
                'job_environment': {**manifest['job_environment'], **manifest['job_environment_by_role'][role]},
                'runtime': {'proc': {name: True for name in ('self/mountinfo', 'cpuinfo', 'meminfo')},
                            'inotify': {'max_user_instances': 8192}, 'dev_shm_writable': True,
                            'tmpdir_exec': True, 'sshd_executable': True},
                'job_identity': dict(manifest['job_identity']),
                'compile_wrapper': {
                    **manifest['compile_wrapper'],
                    'resolved': '/motor-tools/sccache' if role == 'boat' else '/motor-tools/prefrontal-build-slot',
                    'supervisor_cache': '/motor-tools/sccache' if role == 'build-server' else None,
                    'wrapper_readonly': True, 'backend_readonly': True,
                },
                'local_bin_entries': sorted(name for name, spec in manifest['tools'].items()
                                            if spec['path'].startswith('/usr/local/bin/')),
                'mountinfo': '1 0 0:0 / ' + manifest['rustup_home'] + ' ro - ext4 /dev/example rw',
                'cargo_registry': self.registry()}

    def report(self, actual, job=False, role='build-server'):
        with patch.object(parity, 'capture', return_value=copy.deepcopy(actual)):
            return parity.check(self.manifest, role=role, job=job, gnu_test_sha256='package-qualified-test',
                                cargo_registry=self.registry())

    def test_missing_registry_crate_fails_boat_qualification(self):
        actual = self.observed('boat')
        actual['cargo_registry']['crates'] = []
        report = self.report(actual, job=True, role='boat')
        self.assertFalse(report['passed'])
        self.assertEqual(report['cargo_registry']['missing_crates'], 1)
        self.assertEqual(report['cargo_registry']['missing_examples'], ['index.crates.io-fixture/cortexkit-cow 0.1.0'])

    def test_boat_qualification_requires_operator_seed_inventory(self):
        with patch.object(parity, 'capture', return_value=self.observed('boat')):
            report = parity.check(self.manifest, role='boat', job=True)
        self.assertFalse(report['passed'])
        self.assertIn('Cargo registry parity requires the operator-supplied server seed manifest', report['errors'])

    def test_boat_job_has_no_compile_cache_environment(self):
        actual = self.observed('boat')
        actual['compile_wrapper'] = {}
        self.assertTrue(self.report(actual, job=True, role='boat')['passed'])
        actual['job_environment'].update(RUSTC_WRAPPER='sccache', SCCACHE_DIR='/motor-cache')
        report = self.report(actual, job=True, role='boat')
        self.assertFalse(report['passed'])
        self.assertIn("job RUSTC_WRAPPER: expected None, observed 'sccache'", report['errors'])

    def test_measured_contract_passes(self):
        self.assertTrue(self.report(self.observed())['passed'])
        self.assertTrue(self.report(self.observed(), job=True)['passed'])

    def test_tenant_role_checks_the_job_layout_without_compile_cache(self):
        actual = self.observed('tenant')
        actual['compile_wrapper'] = {}
        self.assertEqual([], self.report(actual, job=True, role='tenant')['errors'])
        for name, value in (('PATH', '/tenant/cargo-home/bin:/usr/local/bin:/usr/bin:/bin'),
                            ('RUSTUP_HOME', '/home/ubuntu/.rustup'), ('RUSTC_WRAPPER', 'sccache')):
            with self.subTest(name=name):
                drift = copy.deepcopy(actual)
                drift['job_environment'][name] = value
                self.assertTrue(self.report(drift, job=True, role='tenant')['errors'])

    def test_tenant_job_requires_sshd_and_runtime_capabilities(self):
        for name in ('dev_shm_writable', 'tmpdir_exec', 'sshd_executable'):
            with self.subTest(name=name):
                actual = self.observed('tenant')
                actual['runtime'][name] = False
                self.assertEqual([f'job {name}: expected True, observed False'],
                                 self.report(actual, job=True, role='tenant')['errors'])
        actual = self.observed('tenant')
        actual['runtime']['proc']['meminfo'] = False
        actual['runtime']['inotify']['max_user_instances'] = 128
        self.assertEqual(['/proc/meminfo readable: expected True, observed False',
                          'inotify max_user_instances: expected at least 8192, observed 128'],
                         self.report(actual, job=True, role='tenant')['errors'])

    def test_tenant_host_requires_openssh_package_and_cache_provenance(self):
        actual = self.observed('tenant')
        actual.update(self.manifest['layout_by_role']['tenant']['host'])
        actual['sccache_artifacts'] = {'inputs_sha256': self.manifest['sccache']['inputs_sha256'],
                                     'binaries': {name: actual['cargo_binaries'][name]['sha256']
                                                  for name in self.manifest['sccache']['build_server_sha256']}}
        self.assertEqual([], parity.differences(self.manifest, actual, role='tenant'))
        del actual['apt_packages']['openssh-server']
        self.assertEqual(['missing apt package: openssh-server'],
                         parity.differences(self.manifest, actual, role='tenant'))
        actual['cargo_binaries']['sccache']['sha256'] = 'replaced'
        self.assertIn('sccache built artifact hash', '\n'.join(parity.differences(self.manifest, actual, role='tenant')))

    def test_every_role_checks_exact_job_uid_and_gid(self):
        for role in ('build-server', 'boat', 'tenant'):
            for field in ('uid', 'gid'):
                with self.subTest(role=role, field=field):
                    actual = self.observed(role)
                    actual['job_identity'][field] = 997
                    errors = self.report(actual, job=True, role=role)['errors']
                    self.assertEqual(1, len(errors))
                    self.assertIn('motor-job identity:', errors[0])

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

    def test_build_server_compiler_wrapper_reaches_the_trusted_cache_wrapper(self):
        self.assertTrue(self.report(self.observed(), job=True)['passed'])
        for key, value in (('sccache_sha256', 'untrusted-script'), ('backend', '/tmp/sccache'),
                           ('sccache_path', '/usr/local/bin/sccache'), ('backend_readonly', False)):
            actual = self.observed()
            actual['compile_wrapper'][key] = value
            self.assertFalse(self.report(actual, job=True)['passed'])

    def test_server_supervisor_cache_cannot_bypass_the_trusted_wrapper(self):
        actual = self.observed()
        actual['compile_wrapper']['supervisor_cache'] = '/srv/build/cargo-home/bin/sccache'
        self.assertFalse(self.report(actual, job=True)['passed'])

    def test_trusted_compiler_wrapper_hash_matches_runner_source(self):
        source = parity.HERE.parents[1] / 'crates/motor-runner/src/sccache-wrapper.sh'
        self.assertEqual(parity.hashlib.sha256(source.read_bytes()).hexdigest(), self.manifest['compile_wrapper']['sccache_sha256'])

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

    def test_build_server_requires_apt_binaryen_but_boat_does_not(self):
        manifest = self.manifest
        self.assertIn('binaryen', manifest['apt_packages']['by_role']['build-server'])
        self.assertNotIn('binaryen', manifest['apt_packages']['required'])
        self.assertNotIn('binaryen', manifest['apt_packages']['by_role']['boat'])
        setup = (parity.HERE / 'setup.sh').read_text()
        self.assertIn('ca-certificates unzip xz-utils binaryen unattended-upgrades', setup)
        server = self.observed('build-server')
        server['apt_packages'].pop('binaryen')
        self.assertIn('missing apt package: binaryen',
                      parity.differences(manifest, server, role='build-server'))
        boat_errors = parity.differences(manifest, self.observed('boat'), role='boat')
        self.assertNotIn('missing apt package: binaryen', boat_errors)

    def test_build_server_job_visible_tools_are_in_the_catalogue(self):
        actual = self.observed()
        actual['local_bin_entries'].extend(
            ('bun-1.3.14', 'node22', 'corepack', 'npx', 'pnpx', 'pnx'))
        errors = self.report(actual)['errors']
        self.assertFalse(errors, errors)

    def test_uncatalogued_local_bin_entries_fail_except_host_services(self):
        actual = self.observed()
        actual['local_bin_entries'].extend(
            ('unlisted-tool', 'motor-host-agent', 'firecracker-worker', 'jailer-worker', 'firecracker', 'jailer'))
        errors = self.report(actual)['errors']
        inventory_errors = [error for error in errors if error.startswith('uncatalogued job-visible /usr/local/bin entries:')]
        self.assertEqual(['uncatalogued job-visible /usr/local/bin entries: firecracker, jailer, unlisted-tool'],
                         inventory_errors)

    def test_local_bin_capture_includes_only_executable_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            executable = root / 'visible'
            executable.write_text('#!/bin/sh\nexit 0\n')
            executable.chmod(0o755)
            (root / 'non-executable').write_text('data')
            (root / 'directory').mkdir()
            (root / 'visible-link').symlink_to(executable)
            self.assertEqual(['visible', 'visible-link'], parity.local_bin_entries(root))

    def test_readonly_rustup_and_exact_job_path_are_checked(self):
        actual = self.observed()
        actual['mountinfo'] = actual['mountinfo'].replace('ro -', 'rw -')
        self.assertIn('job rustup home is not mounted read-only', self.report(actual, job=True)['errors'])
        actual = self.observed()
        actual['job_environment']['PATH'] = '/usr/local/bin:/usr/bin:/bin'
        self.assertFalse(self.report(actual, job=True)['passed'])

    def test_boat_cache_artifact_hashes_bind_the_build_record_to_live_binaries(self):
        actual = self.observed('boat')
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

    def test_capture_cli_measures_the_tools_the_manifest_names(self):
        seen = []
        with patch.object(parity.sys, 'argv', ['toolchain-parity.py', '--capture']), \
             patch.object(parity, 'capture', side_effect=lambda manifest=None, job=False: seen.append(manifest) or {}), \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(parity.main(), 0)
        self.assertEqual(1, len(seen))
        self.assertIn('bun-1.3.14', seen[0]['tools'])

    def test_capture_cli_honors_tenant_and_job_without_rewriting_path(self):
        with patch.object(parity.sys, 'argv', ['toolchain-parity.py', '--capture', '--job', '--role', 'tenant']), \
             patch.object(parity, 'capture', return_value={}) as capture, \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(0, parity.main())
        self.assertEqual('/tenant/cargo-home', capture.call_args.args[0]['cargo_home'])
        self.assertEqual({'job': True}, capture.call_args.kwargs)

    def test_check_cli_fails_when_git_version_differs_from_the_server(self):
        actual = self.observed()
        actual['tools']['git']['version'] = 'git version 2.43.0'
        with patch.object(parity.sys, 'argv', ['toolchain-parity.py', '--check']), \
             patch.object(parity, 'capture', return_value=actual), \
             contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(parity.main(), 1)
        self.assertEqual(["git version: expected 'git version 2.53.0', observed 'git version 2.43.0'"],
                         json.loads(out.getvalue())['errors'])

    def test_boat_git_is_the_servers_release_ahead_of_the_distribution_git(self):
        self.assertEqual([], self.report(self.observed('boat'), job=True, role='boat')['errors'])
        actual = self.observed('boat')
        actual['tools']['git'].update(version='git version 2.43.0')
        self.assertEqual(["git version: expected 'git version 2.53.0', observed 'git version 2.43.0'"],
                         self.report(actual, job=True, role='boat')['errors'])
        # Ubuntu's own git found first: wrong directory and not the pinned build.
        actual = self.observed('boat')
        actual['tools']['git'].update(path='/usr/bin/git', realpath='/usr/bin/git', version='git version 2.43.0')
        errors = self.report(actual, job=True, role='boat')['errors']
        self.assertIn("git path: expected '/usr/local/bin/git', observed '/usr/bin/git'", errors)
        self.assertIn('git: wrong resolved executable path', errors)

    def test_boat_wasm_opt_must_match_the_build_server_major_version(self):
        actual = self.observed('boat')
        self.assertEqual([], self.report(actual, job=True, role='boat')['errors'])
        actual['tools']['wasm-opt']['version'] = 'wasm-opt version 120 (version_120)'
        self.assertEqual([], self.report(actual, job=True, role='boat')['errors'])
        actual['tools']['wasm-opt']['version'] = 'wasm-opt version 116'
        self.assertEqual(["wasm-opt major version: expected '120', observed '116'"],
                         self.report(actual, job=True, role='boat')['errors'])

    def test_binaryen_source_pin_and_install_layout_are_versioned_together(self):
        parity.validate(self.manifest)
        for change in ({'version': 'wasm-opt version 121'},
                       {'source': dict(self.manifest['tools']['wasm-opt']['source'], sha256='latest')},
                       {'source': dict(self.manifest['tools']['wasm-opt']['source'], install_dir='/usr/local/lib')}):
            with self.subTest(change=change):
                manifest = copy.deepcopy(self.manifest)
                manifest['tools']['wasm-opt'].update(change)
                with self.assertRaisesRegex(ValueError, 'Binaryen'):
                    parity.validate(manifest)

    def test_git_source_pin_follows_the_measured_version(self):
        parity.validate(self.manifest)
        for change in ({'version': 'git version 2.54.0'}, {'version': 'git version 2.53'},
                       {'source': dict(self.manifest['tools']['git']['source'], sha256='latest')},
                       {'source': dict(self.manifest['tools']['git']['source'], install_dir='/usr/local')},
                       {'source': dict(self.manifest['tools']['git']['source'], build_packages=[])},
                       {'path_by_role': {'boat': '/usr/bin/git'}}):
            with self.subTest(change=change):
                manifest = copy.deepcopy(self.manifest)
                manifest['tools']['git'].update(change)
                with self.assertRaisesRegex(ValueError, 'git'):
                    parity.validate(manifest)


class InstallerTests(unittest.TestCase):
    def test_cargo_installer_consumes_manifest_and_places_pinned_defaults(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = json.loads(parity.MANIFEST.read_text())
            archive_dir = root / 'archive-fixtures'
            archive_dir.mkdir()
            bun_payload = b'#!/bin/sh\necho 1.3.14\n'
            bun_archive = archive_dir / 'bun.zip'
            with zipfile.ZipFile(bun_archive, 'w') as archive:
                archive.writestr('bun-linux-x64/bun', bun_payload)
            node_payload = b'#!/bin/sh\necho v22.22.1\n'
            node_archive = archive_dir / 'node.tar.xz'
            with tarfile.open(node_archive, 'w:xz') as archive:
                entry = tarfile.TarInfo('node-v22.22.1-linux-x64/bin/node')
                entry.size = len(node_payload)
                entry.mode = 0o755
                archive.addfile(entry, io.BytesIO(node_payload))
            for name, archive_path in (('bun-1.3.14', bun_archive), ('node22', node_archive)):
                manifest['tools'][name]['archive']['sha256'] = hashlib.sha256(archive_path.read_bytes()).hexdigest()
            (root / 'toolchain.json').write_text(json.dumps(manifest))
            script = root / 'install-tools.sh'
            script.write_text((parity.HERE / 'install-tools.sh').read_text().replace('/srv/build', str(root)))
            fake_bin = root / 'fake-bin'
            fake_bin.mkdir()
            cargo = fake_bin / 'cargo'
            cargo.write_text('#!/usr/bin/env python3\nimport json, os, pathlib, sys\n'
                             'args = sys.argv[1:]\n'
                             'open(os.environ["TEST_CARGO_LOG"], "a").write("install\\n")\n'
                             'manifest = json.loads(pathlib.Path(os.environ["TEST_MANIFEST"]).read_text())\n'
                             'tool = next(row for row in manifest["cargo_installs"] if row["crate"] == args[-1])\n'
                             'root = pathlib.Path(args[args.index("--root")+1] if "--root" in args else os.environ["CARGO_HOME"])\n'
                             '(root / "bin").mkdir(parents=True, exist_ok=True)\n'
                              'for name in tool["bins"]: (root / "bin" / name).write_text("binary:" + " ".join(args))\n')
            cargo.chmod(0o755)
            curl = fake_bin / 'curl'
            curl.write_text('#!/usr/bin/env python3\nimport os, pathlib, shutil, sys\n'
                            'args = sys.argv[1:]\n'
                            'url = next(arg for arg in args if arg.startswith("https://"))\n'
                            'target = pathlib.Path(args[args.index("-o") + 1])\n'
                            'source = pathlib.Path(os.environ["TEST_ARCHIVE_DIR"]) / ("bun.zip" if "bun-linux-x64.zip" in url else "node.tar.xz")\n'
                            'shutil.copyfile(source, target)\n')
            curl.chmod(0o755)
            env = dict(os.environ, TEST_MANIFEST=str(root / 'toolchain.json'), TEST_ARCHIVE_DIR=str(archive_dir),
                       TEST_CARGO_LOG=str(root / 'cargo-installs.log'),
                       TOOLCHAIN_INSTALL_ROOT=str(root / 'sysroot'),
                       PATH=str(fake_bin) + os.pathsep + os.environ['PATH'])
            result = subprocess.run(['bash', str(script), 'boat'], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            bins = root / 'cargo-home/bin'
            self.assertIn('--locked --version =0.9.138 cargo-nextest', (bins / 'cargo-nextest').read_text())
            default = bins / 'ckdev-mutate'
            default_rev = next(t['rev'] for t in manifest['cargo_installs'] if t.get('default'))
            self.assertEqual(default.read_bytes(), (bins / ('ckdev-mutate-' + default_rev[:8])).read_bytes())
            self.assertTrue((bins / 'worker-codegen').is_file())
            sysroot = root / 'sysroot'
            self.assertEqual(str(sysroot / 'usr/local/lib/bun/1.3.14/bin/bun'),
                             os.readlink(sysroot / 'usr/local/bin/bun-1.3.14'))
            self.assertEqual(bun_payload, (sysroot / 'usr/local/lib/bun/1.3.14/bin/bun').read_bytes())
            self.assertEqual(str(sysroot / 'usr/local/lib/nodejs/22.22.1/bin/node'),
                             os.readlink(sysroot / 'usr/local/bin/node22'))
            self.assertEqual(node_payload,
                             (sysroot / 'usr/local/lib/nodejs/22.22.1/bin/node').read_bytes())
            cargo_log = (root / 'cargo-installs.log').read_text()
            runtime = subprocess.run(['bash', str(script), 'build-server', 'runtime-only'], env=env,
                                     capture_output=True, text=True)
            self.assertEqual(runtime.returncode, 0, runtime.stderr)
            self.assertEqual(cargo_log, (root / 'cargo-installs.log').read_text())
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


FAKE_GIT = '''#!/bin/sh
case "$*" in
  --version) echo 'git version 2.53.0' ;;
  '--version --build-options') printf 'git version 2.53.0\\nlibcurl: 8.18.0\\n' ;;
  grep*) grep -q motor pcre.txt ;;
  *) exit 2 ;;
esac
'''

# Stands in for git's own Makefile: records each target and installs the fake
# git under the prefix and DESTDIR that build-git.sh passes.
FAKE_GIT_MAKEFILE = '''all:
\techo all >> $(TEST_MAKE_LOG)
install:
\techo install $(prefix) $(sysconfdir) $(USE_LIBPCRE2) >> $(TEST_MAKE_LOG)
\tmkdir -p $(DESTDIR)$(prefix)/bin $(DESTDIR)$(prefix)/libexec/git-core
\tfor name in git git-receive-pack git-upload-pack git-upload-archive git-shell scalar; do cp fake-git $(DESTDIR)$(prefix)/bin/$$name; done
\tcp fake-git $(DESTDIR)$(prefix)/libexec/git-core/git-remote-https
\tcp fake-git $(DESTDIR)$(prefix)/libexec/git-core/git-http-push
'''


class GitBuildTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.source = self.root / 'git-2.53.0.tar.xz'
        with tarfile.open(self.source, 'w:xz') as archive:
            for name, text, mode in (('Makefile', FAKE_GIT_MAKEFILE, 0o644), ('fake-git', FAKE_GIT, 0o755)):
                entry = tarfile.TarInfo('git-2.53.0/' + name)
                payload = text.encode()
                entry.size, entry.mode = len(payload), mode
                archive.addfile(entry, io.BytesIO(payload))
        self.manifest = json.loads(parity.MANIFEST.read_text())
        self.manifest['tools']['git']['source']['sha256'] = hashlib.sha256(self.source.read_bytes()).hexdigest()
        fake_bin = self.root / 'fake-bin'
        fake_bin.mkdir()
        curl = fake_bin / 'curl'
        curl.write_text('#!/bin/sh\nwhile [ "$1" != -o ]; do shift; done\ncp "$TEST_SOURCE" "$2"\n')
        curl.chmod(0o755)
        self.env = dict(os.environ, TEST_SOURCE=str(self.source), TEST_MAKE_LOG=str(self.root / 'make.log'),
                        GIT_INSTALL_ROOT=str(self.root / 'sysroot'), TMPDIR=str(self.root),
                        PATH=str(fake_bin) + os.pathsep + os.environ['PATH'])

    def build(self):
        scripts = self.root / 'runner'
        scripts.mkdir(exist_ok=True)
        (scripts / 'toolchain.json').write_text(json.dumps(self.manifest))
        (scripts / 'build-git.sh').write_text((parity.HERE / 'build-git.sh').read_text())
        return subprocess.run(['bash', str(scripts / 'build-git.sh')], env=self.env, capture_output=True, text=True)

    def test_pinned_source_is_built_once_and_linked_ahead_of_the_distribution_git(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        install_dir = self.manifest['tools']['git']['source']['install_dir']
        self.assertEqual(['all', f'install {install_dir} /etc 1'],
                         (self.root / 'make.log').read_text().splitlines())
        sysroot = self.root / 'sysroot'
        for name in ('git', 'git-receive-pack', 'git-upload-pack', 'git-upload-archive', 'git-shell', 'scalar'):
            self.assertEqual(install_dir + '/bin/' + name, os.readlink(sysroot / 'usr/local/bin' / name))
            self.assertTrue((sysroot / install_dir.lstrip('/') / 'bin' / name).is_file())
        # Reapplying the recipe with unchanged inputs reuses the build.
        again = self.build()
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertIn('already built from the pinned source', again.stdout)
        self.assertEqual(2, len((self.root / 'make.log').read_text().splitlines()))

    def test_source_with_another_digest_is_refused_before_building(self):
        self.manifest['tools']['git']['source']['sha256'] = '0' * 64
        result = self.build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('source SHA-256 mismatch', result.stderr)
        self.assertFalse((self.root / 'make.log').exists())
        self.assertFalse((self.root / 'sysroot/usr/local/bin/git').exists())


PUSHURL_SPEC = importlib.util.spec_from_file_location('git_pushurl_probe', parity.HERE / 'git-pushurl-probe.py')
assert PUSHURL_SPEC is not None and PUSHURL_SPEC.loader is not None
pushurl_probe = importlib.util.module_from_spec(PUSHURL_SPEC)
PUSHURL_SPEC.loader.exec_module(pushurl_probe)


class GitPushurlProbeTests(unittest.TestCase):
    def test_only_the_single_worktree_url_passes(self):
        refused, inherited = pushurl_probe.REFUSED, pushurl_probe.INHERITED
        self.assertTrue(pushurl_probe.evaluate(refused + '\n')['passed'])
        # Git before 2.46 keeps the inherited URL and the empty reset value.
        old = pushurl_probe.evaluate('\n'.join((inherited, '', refused)) + '\n')
        self.assertFalse(old['passed'])
        self.assertEqual([inherited, '', refused], old['observed'])

    def test_probe_passes_with_a_git_that_resets_on_an_empty_value(self):
        version = subprocess.run(['git', '--version'], capture_output=True, text=True, check=True).stdout
        release = tuple(int(part) for part in version.split()[2].split('.')[:2])
        if release < (2, 46):
            self.skipTest('this host has git before 2.46, which keeps the inherited push URL: ' + version)
        with contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(0, pushurl_probe.run_probe())
        report = json.loads(out.getvalue())
        self.assertEqual([pushurl_probe.REFUSED], report['observed'])


if __name__ == "__main__":
    unittest.main()
