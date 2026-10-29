import ast
import importlib.util
import contextlib
import copy
import ctypes
import errno
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tarfile
from types import SimpleNamespace
import unittest
import zipfile
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("toolchain_parity", Path(__file__).with_name("toolchain-parity.py"))
if spec is None or spec.loader is None:
    raise ImportError("cannot load toolchain parity checker")
parity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(parity)


class ParityBundleTests(unittest.TestCase):
    def test_declared_files_load_without_the_repository(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in parity.PARITY_FILES:
                shutil.copy2(parity.HERE / name, root / name)
            result = subprocess.run([sys.executable, '-I', str(root / 'toolchain-parity.py'), '--help'],
                                    cwd=root, text=True, capture_output=True)
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn('--capture', result.stdout)

    def test_declared_files_cover_sibling_imports(self):
        imported = set()
        original = importlib.util.spec_from_file_location

        def record(name, location, *args, **kwargs):
            path = Path(location).resolve()
            if path.parent == parity.HERE:
                imported.add(path.name)
            return original(name, location, *args, **kwargs)

        with patch.object(importlib.util, 'spec_from_file_location', side_effect=record):
            spec = importlib.util.spec_from_file_location('isolated_parity', parity.HERE / 'toolchain-parity.py')
            assert spec is not None and spec.loader is not None
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
        # The AST scan finds literal sibling imports even in uncalled functions
        # and ordinary imports that do not use spec_from_file_location.
        for name in module.PARITY_FILES:
            if not name.endswith('.py'):
                continue
            for node in ast.walk(ast.parse((parity.HERE / name).read_text())):
                if isinstance(node, ast.Constant) and isinstance(node.value, str):
                    if node.value.endswith('.py') and Path(node.value).name == node.value:
                        imported.add(node.value)
                if isinstance(node, (ast.Import, ast.ImportFrom)):
                    modules = [alias.name for alias in node.names] if isinstance(node, ast.Import) else [node.module or '']
                    for dependency in modules:
                        sibling = parity.HERE / (dependency.split('.')[0] + '.py')
                        if sibling.is_file():
                            imported.add(sibling.name)
        self.assertFalse(imported - set(module.PARITY_FILES),
                         f'undeclared sibling imports: {sorted(imported - set(module.PARITY_FILES))}')


class SyscallSurfaceTests(unittest.TestCase):
    def test_raw_probe_calls_fchmodat2_on_a_retained_directory_without_fallback(self):
        chmod_calls = []

        def syscall(number, *args):
            if number.value == 452:
                fd, path, mode, flags = args
                self.assertTrue(os.path.isdir(f'/proc/self/fd/{fd}'))
                self.assertEqual((b'', 0o700, 0x1000), (path, mode, flags))
                chmod_calls.append(fd)
            ctypes.set_errno(errno.ENOSYS)
            return -1

        with patch.object(parity.syscall_surface.ctypes, 'CDLL') as library:
            library.return_value.syscall.side_effect = syscall
            actual = parity.syscall_surface.capture()
        self.assertEqual(1, len(chmod_calls))
        self.assertEqual({'errno': errno.ENOSYS, 'result': 'ENOSYS'}, actual['fchmodat2.EMPTY_PATH'])
        self.assertEqual(31, len(actual))
        self.assertEqual(set(parity.syscall_surface.expectations(True)), set(actual))

    def test_landlock_probe_reports_abi_and_syscall_errors(self):
        for result, error, passes in ((1, 0, True), (8, 0, True), (0, 0, False),
                                      (-1, errno.ENOSYS, False), (-1, errno.EOPNOTSUPP, False)):
            with self.subTest(result=result, error=error):
                calls = []

                def syscall(number, *args):
                    if number.value == 444:
                        calls.append(args)
                        ctypes.set_errno(error)
                        return result
                    ctypes.set_errno(errno.ENOSYS)
                    return -1

                with patch.object(parity.syscall_surface.ctypes, 'CDLL') as library:
                    library.return_value.syscall.side_effect = syscall
                    actual = parity.syscall_surface.capture()
                self.assertEqual(1, len(calls))
                attributes, size, flags = calls[0]
                self.assertIsNone(attributes.value)
                self.assertEqual((0, 1), (size.value, flags.value))
                self.assertEqual({'abi': result, 'errno': error,
                                  'result': errno.errorcode.get(error, 'OK')},
                                 actual['landlock_create_ruleset.VERSION'])
                errors = parity.syscall_surface.differences(actual)
                landlock_errors = [message for message in errors if 'landlock_create_ruleset.VERSION' in message]
                self.assertEqual(passes, not landlock_errors, errors)

    def test_job_clone3_denial_is_required_but_not_expected_on_the_host(self):
        self.assertEqual({errno.ENOSYS}, parity.syscall_surface.expectations(True)['clone3.invalid'])
        self.assertEqual({errno.EINVAL}, parity.syscall_surface.expectations(False)['clone3.invalid'])

    def test_missing_probe_results_fail_closed(self):
        errors = parity.syscall_surface.differences({})
        self.assertIn('syscall fchmodat2.EMPTY_PATH', '\n'.join(errors))
        self.assertEqual(32, len(errors))


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
                 patch.object(self.account.pwd, 'getpwnam', side_effect=KeyError), \
                 patch.object(self.account.grp, 'getgrnam', side_effect=KeyError), \
                 patch.object(parity, 'output') as run:
                with self.assertRaisesRegex(ValueError, 'already owned by base-' + ('user' if namespace == 'uid' else 'group')):
                    self.account.ensure(self.identity, run)
                run.assert_not_called()

    def test_boat_system_identity_leaves_occupied_numbers_untouched(self):
        users = {'dnsmasq': SimpleNamespace(pw_uid=999, pw_gid=986)}
        groups = {'dnsmasq': SimpleNamespace(gr_gid=986)}
        commands = []

        def run(*args):
            commands.append(args)
            if args[0] == 'groupadd':
                self.assertEqual(('groupadd', '--system', 'motor-job'), args)
                groups['motor-job'] = SimpleNamespace(gr_gid=985)
            else:
                self.assertEqual(('useradd', '--system', '--gid', 'motor-job', '--home-dir',
                                  '/nonexistent', '--shell', '/usr/sbin/nologin', 'motor-job'), args)
                users['motor-job'] = SimpleNamespace(pw_uid=998, pw_gid=985)

        with patch.object(self.account.pwd, 'getpwnam', side_effect=users.__getitem__), \
             patch.object(self.account.grp, 'getgrnam', side_effect=groups.__getitem__), \
             patch.object(self.account.pwd, 'getpwuid', return_value=('dnsmasq',)), \
             patch.object(self.account.grp, 'getgrgid', return_value=('dnsmasq',)):
            self.account.ensure('system-assigned', run)
            self.account.ensure('system-assigned', run)
        self.assertEqual(2, len(commands))
        self.assertEqual((998, 985), (users['motor-job'].pw_uid, users['motor-job'].pw_gid))
        self.assertEqual((999, 986), (users['dnsmasq'].pw_uid, groups['dnsmasq'].gr_gid))

    def test_boat_existing_account_must_have_its_own_primary_group(self):
        with patch.object(self.account.pwd, 'getpwnam', return_value=SimpleNamespace(pw_gid=986)), \
             patch.object(self.account.grp, 'getgrnam', return_value=SimpleNamespace(gr_gid=985)), \
             patch.object(parity, 'output') as run:
            with self.assertRaisesRegex(ValueError, 'own primary group'):
                self.account.ensure('system-assigned', run)
            run.assert_not_called()


class CaptureTests(unittest.TestCase):
    def test_host_capture_measures_declared_policy_and_records_its_removal(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'bin').mkdir()
            policy = root / 'retention.conf'
            policy.write_bytes(b'[Journal]\nMaxRetentionSec=14day\n')
            policy.chmod(0o644)
            manifest = {'cargo_home': tmp, 'rustup_home': tmp, 'host_files': {str(policy): {}}}

            def output(*args, **kwargs):
                if args[0] == 'dpkg-query' or args[1:] == ('toolchain', 'list'):
                    return ''
                return 'stable (default)' if args[1:] == ('default',) else 'rustup 1.29.1'

            original_read = Path.read_text

            def read(path, *args, **kwargs):
                return 'ID=ubuntu\n' if str(path) == '/etc/os-release' else original_read(path, *args, **kwargs)

            with patch.object(parity, 'output', side_effect=output), \
                 patch.object(parity.shutil, 'which', return_value=None), \
                 patch.object(parity.syscall_surface, 'capture', return_value={}), \
                 patch.object(parity.seed, 'capture', return_value={}), \
                 patch.object(Path, 'read_text', read):
                present = parity.capture(manifest)['host_files']
                policy.unlink()
                missing = parity.capture(manifest)['host_files']
            self.assertEqual({str(policy): {'sha256': hashlib.sha256(b'[Journal]\nMaxRetentionSec=14day\n').hexdigest(),
                                           'uid': os.getuid(), 'gid': os.getgid(), 'mode': 0o644}}, present)
            self.assertEqual({str(policy): None}, missing)

    def test_host_file_capture_records_bytes_and_permissions_and_rejects_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'retention.conf'
            self.assertIsNone(parity.host_file(path))
            path.write_bytes(b'[Journal]\nMaxRetentionSec=14day\n')
            path.chmod(0o644)
            self.assertEqual({'sha256': hashlib.sha256(path.read_bytes()).hexdigest(),
                              'uid': os.getuid(), 'gid': os.getgid(), 'mode': 0o644}, parity.host_file(path))
            link = path.with_suffix('.link')
            link.symlink_to(path)
            self.assertIsNone(parity.host_file(link))

    def test_ipv6_loopback_probe_binds_and_connects(self):
        observed = parity.ipv6_loopback_capture()
        self.assertTrue(observed['connected'], observed)
        self.assertEqual(('complete', 0), (observed['step'], observed['errno']))

    def test_ipv6_loopback_probe_records_namespace_diagnostics(self):
        import socket
        values = {'/proc/cmdline': 'test-kernel ipv6.disable=1',
                  '/proc/sys/net/ipv6/conf/all/disable_ipv6': '1',
                  '/proc/sys/net/ipv6/conf/default/disable_ipv6': '1',
                  '/proc/sys/net/ipv6/conf/lo/disable_ipv6': '0'}
        with patch.object(parity, 'optional_text', side_effect=values.get), \
             patch.object(Path, 'exists', return_value=False) as exists, \
             patch.object(socket, 'socket', side_effect=OSError(errno.EAFNOSUPPORT, 'family refused')):
            observed = parity.ipv6_loopback_capture()
        self.assertEqual('test-kernel ipv6.disable=1', observed['cmdline'])
        self.assertFalse(observed['if_inet6_present'])
        exists.assert_called_once_with()
        self.assertEqual({'all': '1', 'default': '1', 'lo': '0'}, observed['disable_ipv6'])

    def test_ipv6_loopback_probe_records_socket_and_bind_failures(self):
        import socket
        for failure in ('socket', 'bind', 'connect'):
            with self.subTest(failure=failure), patch.object(socket, 'socket') as create:
                if failure == 'socket':
                    create.side_effect = OSError(errno.EAFNOSUPPORT, 'family refused')
                else:
                    endpoint = create.return_value.__enter__.return_value
                    getattr(endpoint, failure).side_effect = OSError(errno.EADDRNOTAVAIL, 'no loopback')
                observed = parity.ipv6_loopback_capture()
                self.assertFalse(observed['connected'])
                self.assertEqual(errno.EAFNOSUPPORT if failure == 'socket' else errno.EADDRNOTAVAIL,
                                 observed['errno'])
                self.assertEqual({'socket': 'socket(AF_INET6) listener', 'bind': 'bind ::1',
                                  'connect': 'connect ::1'}[failure], observed['step'])

    def test_job_identity_is_the_process_not_a_host_account_lookup(self):
        with patch.object(parity.os, 'getuid', return_value=997), patch.object(parity.os, 'getgid', return_value=996):
            self.assertEqual({'uid': 997, 'gid': 996}, parity.job_identity(True))
            self.assertEqual({'uid': 997, 'gid': 996}, parity.job_identity(True, 'boat'))

    def test_boat_host_capture_records_account_properties_and_login_defs_limit(self):
        import grp
        import pwd
        user = SimpleNamespace(pw_uid=798, pw_gid=797, pw_shell='/usr/sbin/nologin')
        with patch.object(pwd, 'getpwnam', return_value=user) as lookup, \
             patch.object(grp, 'getgrgid', return_value=SimpleNamespace(gr_name='motor-job')) as group, \
             patch.object(parity, 'optional_text', return_value='# UID_MIN 1000\nUID_MIN 800 # local policy\n') as text:
            self.assertEqual({'uid': 798, 'gid': 797, 'primary_group': 'motor-job',
                              'shell': '/usr/sbin/nologin', 'uid_min': 800}, parity.job_identity(False, 'boat'))
        lookup.assert_called_once_with('motor-job')
        group.assert_called_once_with(797)
        text.assert_called_once_with('/etc/login.defs')

    def test_missing_boat_host_account_is_recorded(self):
        import pwd
        with patch.object(pwd, 'getpwnam', side_effect=KeyError):
            self.assertIsNone(parity.job_identity(False, 'boat'))

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

    def test_package_alias_sorting_before_its_package_still_takes_its_version(self):
        # pn sorts before pnpm, so capture must measure pnpm first.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'bin').mkdir()
            (root / 'bin/rustup').write_text('proxy fixture')
            manifest = {'cargo_home': str(root), 'rustup_home': str(root), 'tools': {
                'pn': {'path': '/usr/local/bin/pn', 'version_from': 'pnpm'},
                'pnpm': {'path': '/usr/local/bin/pnpm'}}}
            def output(*args, **kwargs):
                if args[0] == 'dpkg-query' or args[1:] == ('toolchain', 'list'):
                    return ''
                if args[1:] == ('default',):
                    return 'stable (default)'
                return 'rustup 1.29.1'
            def version(path, env):
                if Path(path).name != 'pnpm':
                    raise AssertionError('only pnpm itself is executed: ' + str(path))
                return {'version': '11.1.2', 'exit_code': 0}
            original_read = Path.read_text
            def read(path, *args, **kwargs):
                return 'ID=ubuntu\n' if str(path) == '/etc/os-release' else original_read(path, *args, **kwargs)
            which = {name: '/usr/local/bin/' + name for name in ('pn', 'pnpm')}
            with patch.object(parity, 'output', side_effect=output), \
                 patch.object(parity.shutil, 'which', side_effect=lambda name, path=None: which.get(name)), \
                 patch.object(parity, 'binary_version', side_effect=version), \
                 patch.object(parity.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '', '')), \
                 patch.object(parity.syscall_surface, 'capture', return_value={}), \
                 patch.object(parity.seed, 'capture', return_value={}), \
                 patch.object(Path, 'read_text', read):
                tools = parity.capture(manifest)['tools']
        self.assertEqual(('11.1.2', 0), (tools['pn']['version'], tools['pn']['exit_code']))
        self.assertEqual(('11.1.2', 0), (tools['pnpm']['version'], tools['pnpm']['exit_code']))

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

    def observed(self, role='build-server', job=False):
        manifest = parity.role_manifest(self.manifest, role, job=True)
        bins = {}
        for name, expected in manifest['cargo_binaries'].items():
            bins[name] = {'symlink': 'rustup' if expected.get('proxy') else None,
                          'sha256': manifest['sccache']['build_server_sha256'].get(name, name),
                          'version': expected.get('version'), 'exit_code': 0}
        for name, expected in manifest['cargo_binaries'].items():
            if 'matches' in expected:
                bins[name]['sha256'] = bins[expected['matches']]['sha256']
        tools = {name: {'path': parity.tool_path(spec, role, job),
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
                'host_files': {} if job else copy.deepcopy(
                    parity.role_manifest(self.manifest, role)['host_files']),
                'job_environment': {**manifest['job_environment'], **manifest['job_environment_by_role'][role]},
                'runtime': {'proc': {name: True for name in ('self/mountinfo', 'cpuinfo', 'meminfo')},
                            'inotify': {'max_user_instances': 8192}, 'dev_shm_writable': True,
                            'tmpdir_exec': True, 'sshd_executable': True,
                            'ipv6_loopback': {'connected': True, 'errno': 0}},
                'job_identity': ({'uid': 998, 'gid': 985, 'uid_min': 1000,
                                  'primary_group': 'motor-job', 'shell': '/usr/sbin/nologin'}
                                 if role == 'boat' else dict(manifest['job_identity_by_role'][role])),
                'syscalls': {**{name: {'errno': 0} for name in parity.syscall_surface.SUCCESS},
                             'landlock_create_ruleset.VERSION': {'errno': 0, 'abi': 1},
                             'openat2.reject_symlink': {'errno': errno.ELOOP},
                             'mount_setattr.invalid': {'errno': errno.EINVAL},
                             'open_tree.invalid': {'errno': errno.EBADF},
                             'move_mount.invalid': {'errno': errno.EPERM},
                             'clone3.invalid': {'errno': errno.EINVAL}},
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

    def test_fchmodat2_enosys_fails_every_role_and_mode(self):
        for role in ('build-server', 'boat', 'tenant'):
            for job in (False, True):
                with self.subTest(role=role, job=job):
                    actual = self.observed(role)
                    actual['syscalls']['clone3.invalid']['errno'] = errno.ENOSYS if job else errno.EINVAL
                    self.assertFalse(any('syscall ' in error for error in
                                         parity.differences(self.manifest, actual, role, job)))
                    actual['syscalls']['fchmodat2.EMPTY_PATH']['errno'] = errno.ENOSYS
                    errors = parity.differences(self.manifest, actual, role, job)
                    self.assertEqual(["syscall fchmodat2.EMPTY_PATH: expected ['OK'], observed errno 38"],
                                     [error for error in errors if error.startswith('syscall ')])

    def test_landlock_requires_abi_one_on_every_linux_role_and_mode(self):
        for role in ('build-server', 'boat', 'tenant'):
            for job in (False, True):
                for abi, passes in ((1, True), (8, True), (0, False), (None, False), (-1, False), (True, False)):
                    with self.subTest(role=role, job=job, abi=abi):
                        actual = self.observed(role)
                        actual['syscalls']['clone3.invalid']['errno'] = errno.ENOSYS if job else errno.EINVAL
                        actual['syscalls']['landlock_create_ruleset.VERSION']['abi'] = abi
                        errors = parity.differences(self.manifest, actual, role, job)
                        self.assertEqual(passes,
                                         not any('landlock_create_ruleset.VERSION' in error for error in errors), errors)
                for error in (errno.ENOSYS, errno.EOPNOTSUPP, errno.EPERM):
                    with self.subTest(role=role, job=job, error=error):
                        actual = self.observed(role)
                        actual['syscalls']['clone3.invalid']['errno'] = errno.ENOSYS if job else errno.EINVAL
                        actual['syscalls']['landlock_create_ruleset.VERSION'] = {'errno': error, 'abi': -1}
                        errors = parity.differences(self.manifest, actual, role, job)
                        self.assertTrue(any('syscall landlock_create_ruleset.VERSION' in error for error in errors), errors)
                with self.subTest(role=role, job=job, missing=True):
                    actual = self.observed(role)
                    del actual['syscalls']['landlock_create_ruleset.VERSION']
                    errors = parity.differences(self.manifest, actual, role, job)
                    self.assertTrue(any('landlock_create_ruleset.VERSION' in error for error in errors), errors)

    def report(self, actual, job=False, role='build-server', remote_cache_on=False):
        captured = copy.deepcopy(actual)
        captured['syscalls']['clone3.invalid']['errno'] = errno.ENOSYS if job else errno.EINVAL
        with patch.object(parity, 'capture', return_value=captured):
            return parity.check(self.manifest, role=role, job=job, gnu_test_sha256='package-qualified-test',
                                cargo_registry=self.registry(), remote_cache_on=remote_cache_on)

    def test_journal_retention_is_required_on_every_linux_host_but_not_in_jobs(self):
        path = '/etc/systemd/journald.conf.d/60-motor-retention.conf'
        source = parity.HERE / 'journald/60-motor-retention.conf'
        self.assertEqual({'sha256': hashlib.sha256(source.read_bytes()).hexdigest(),
                          'uid': 0, 'gid': 0, 'mode': 0o644}, self.manifest['host_files'][path])
        for role in ('build-server', 'boat', 'tenant'):
            with self.subTest(role=role):
                # Boat installs the same file under a later-sorting name.
                [role_path] = parity.role_manifest(self.manifest, role)['host_files']
                actual = self.observed(role)
                if role in ('boat', 'tenant'):
                    actual.update({key: value for key, value in
                                   self.manifest['layout_by_role'].get(role, {}).get('host', {}).items()
                                   if key != 'host_files'})
                    actual['sccache_artifacts'] = {
                        'inputs_sha256': self.manifest['sccache']['inputs_sha256'],
                        'binaries': {name: actual['cargo_binaries'][name]['sha256']
                                     for name in self.manifest['sccache']['build_server_sha256']}}
                self.assertEqual([], parity.differences(self.manifest, actual, role=role))
                del actual['host_files'][role_path]
                errors = parity.differences(self.manifest, actual, role=role)
                self.assertEqual(1, len(errors))
                self.assertIn('host file ' + role_path, errors[0])
                job_actual = self.observed(role, job=True)
                job_actual['host_files'] = {}
                self.assertTrue(self.report(job_actual, role=role, job=True)['passed'])

    def test_journal_retention_content_owner_and_mode_drift_fail_parity(self):
        path = '/etc/systemd/journald.conf.d/60-motor-retention.conf'
        for key, value in (('sha256', '0' * 64), ('uid', 1000), ('gid', 1000), ('mode', 0o600)):
            with self.subTest(key=key):
                actual = self.observed()
                actual['host_files'][path][key] = value
                report = self.report(actual)
                self.assertFalse(report['passed'])
                self.assertEqual(1, len(report['errors']))
                self.assertIn('host file ' + path, report['errors'][0])

    def test_host_file_contract_requires_valid_pins_and_metadata(self):
        path = '/etc/systemd/journald.conf.d/60-motor-retention.conf'
        for key, value in (('sha256', 'bad'), ('uid', -1), ('gid', True), ('mode', 0o10000)):
            with self.subTest(key=key):
                manifest = copy.deepcopy(self.manifest)
                manifest['host_files'][path][key] = value
                with self.assertRaisesRegex(ValueError, 'invalid host file contract'):
                    parity.validate(manifest)
        for missing in (None, {}):
            manifest = copy.deepcopy(self.manifest)
            manifest['host_files'] = missing
            with self.assertRaisesRegex(ValueError, 'missing host file contract'):
                parity.validate(manifest)

    # Settings the machine running the tests may export are not part of any
    # role's declared job environment, but the cache capture records them
    # when it is run inside a real job. A test that reads them from the host
    # would pass on one machine and fail on another, so every test that calls
    # capture() itself clears them first.
    AMBIENT_CACHE_SETTINGS = ('RUSTC_WRAPPER', 'SCCACHE_DIR', 'SCCACHE_BUCKET', 'SCCACHE_LOG',
                              'SCCACHE_S3_RW_MODE', 'SCCACHE_MULTILEVEL_CHAIN',
                              'AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY')

    def hermetic(self, environ):
        """`environ` with the host's own cache settings removed."""
        return {name: value for name, value in environ.items()
                if name not in self.AMBIENT_CACHE_SETTINGS
                and not name.startswith(('SCCACHE_', 'AWS_'))}

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
        actual = self.observed('tenant', job=True)
        actual['compile_wrapper'] = {}
        self.assertEqual([], self.report(actual, job=True, role='tenant')['errors'])
        for name, value in (('PATH', '/tenant/cargo-home/bin:/usr/local/bin:/usr/bin:/bin'),
                            ('RUSTUP_HOME', '/home/ubuntu/.rustup'), ('RUSTC_WRAPPER', 'sccache')):
            with self.subTest(name=name):
                drift = copy.deepcopy(actual)
                drift['job_environment'][name] = value
                self.assertTrue(self.report(drift, job=True, role='tenant')['errors'])

    def test_every_linux_job_requires_ipv6_loopback(self):
        for role in ('build-server', 'tenant', 'boat'):
            for observed in ({}, {'connected': False, 'errno': errno.EAFNOSUPPORT},
                             {'connected': False, 'errno': errno.EADDRNOTAVAIL}):
                with self.subTest(role=role, observed=observed):
                    actual = self.observed(role, job=True)
                    actual['runtime']['ipv6_loopback'] = observed
                    self.assertEqual([
                        f'job IPv6 loopback bind/connect: expected success, observed {observed!r}'
                    ], self.report(actual, job=True, role=role)['errors'])

    def test_tenant_job_requires_sshd_and_runtime_capabilities(self):
        for name in ('dev_shm_writable', 'tmpdir_exec', 'sshd_executable'):
            with self.subTest(name=name):
                actual = self.observed('tenant', job=True)
                actual['runtime'][name] = False
                self.assertEqual([f'job {name}: expected True, observed False'],
                                 self.report(actual, job=True, role='tenant')['errors'])
        actual = self.observed('tenant', job=True)
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

    def test_tenant_host_runs_bun_outside_home_and_jobs_see_it_at_the_shared_path(self):
        # A tenant VM's /home is the tenant's own workspace root, bound over the
        # image's /home before the runner starts, so the image's Bun lives in
        # /usr/local/lib/motor and the runner mounts it at job_path for jobs.
        host = self.observed('tenant')
        host.update(self.manifest['layout_by_role']['tenant']['host'])
        host['sccache_artifacts'] = {'inputs_sha256': self.manifest['sccache']['inputs_sha256'],
                                     'binaries': {name: host['cargo_binaries'][name]['sha256']
                                                  for name in self.manifest['sccache']['build_server_sha256']}}
        self.assertEqual('/usr/local/lib/motor/bun/bin/bun', host['tools']['bun']['path'])
        self.assertEqual([], parity.differences(self.manifest, host, role='tenant'))
        host['tools']['bun']['path'] = '/home/ubuntu/.bun/bin/bun'
        self.assertEqual(["bun path: expected '/usr/local/lib/motor/bun/bin/bun', observed '/home/ubuntu/.bun/bin/bun'"],
                         parity.differences(self.manifest, host, role='tenant'))
        job = self.observed('tenant', job=True)
        job['compile_wrapper'] = {}
        self.assertEqual('/home/ubuntu/.bun/bin/bun', job['tools']['bun']['path'])
        self.assertEqual([], self.report(job, job=True, role='tenant')['errors'])
        for role in ('build-server', 'boat'):
            self.assertEqual('/home/ubuntu/.bun/bin/bun', self.observed(role)['tools']['bun']['path'])

    def test_tenant_bun_under_a_bound_workspace_root_is_refused(self):
        for path in ('/home/ubuntu/.bun/bin/bun', '/Users/shared/bun'):
            with self.subTest(path=path):
                manifest = copy.deepcopy(self.manifest)
                manifest['tools']['bun']['path_by_role']['tenant'] = path
                with self.assertRaisesRegex(ValueError, 'hidden by the tenant'):
                    parity.validate(manifest)

    def test_host_capture_searches_the_roles_bun_directory(self):
        searched = []
        class Searched(Exception):
            pass
        def which(name, path=None):
            searched.append(path)
            raise Searched
        with patch.object(parity, 'output', return_value='stub'), \
             patch.object(Path, 'iterdir', return_value=iter(())), \
             patch.object(Path, 'glob', return_value=iter(())), \
             patch.object(parity.shutil, 'which', side_effect=which), \
             self.assertRaises(Searched):
            parity.capture(parity.role_manifest(self.manifest, 'tenant'), role='tenant')
        self.assertEqual('/usr/local/lib/motor/bun/bin', searched[0].split(':')[1])

    def test_pinned_roles_check_exact_job_uid_and_gid(self):
        for role in ('build-server', 'tenant'):
            for field in ('uid', 'gid'):
                with self.subTest(role=role, field=field):
                    actual = self.observed(role, job=True)
                    actual['job_identity'][field] = 997
                    errors = self.report(actual, job=True, role=role)['errors']
                    self.assertEqual(1, len(errors))
                    self.assertIn('motor-job identity:', errors[0])

    def test_boat_parity_accepts_system_assigned_numbers(self):
        actual = self.observed('boat')
        actual['sccache_artifacts'] = {'inputs_sha256': self.manifest['sccache']['inputs_sha256'], 'binaries': {
            name: actual['cargo_binaries'][name]['sha256'] for name in self.manifest['sccache']['build_server_sha256']}}
        self.assertEqual([], parity.differences(self.manifest, actual, role='boat'))
        self.assertEqual([], self.report(actual, job=True, role='boat')['errors'])

    def test_boat_host_parity_requires_system_account_properties(self):
        for change, expected in ((None, 'missing non-root job account'),
                                 ({'uid': 0}, 'missing non-root job account'),
                                 ({'gid': 0}, 'missing non-root job account'),
                                 ({'uid': 1000}, 'uid must be below UID_MIN'),
                                 ({'uid_min': None}, 'uid must be below UID_MIN'),
                                 ({'primary_group': 'dnsmasq'}, 'motor-job primary group:'),
                                 ({'shell': '/bin/bash'}, 'login shell is not disabled')):
            with self.subTest(change=change):
                actual = self.observed('boat')
                if change is None:
                    actual['job_identity'] = None
                else:
                    actual['job_identity'].update(change)
                errors = parity.differences(self.manifest, actual, role='boat')
                identity_errors = [error for error in errors if error.startswith('motor-job')]
                self.assertEqual(1, len(identity_errors), errors)
                self.assertIn(expected, identity_errors[0])

    def test_role_identity_rules_survive_server_refresh(self):
        expected = {'boat': 'system-assigned', 'build-server': {'uid': 999, 'gid': 986},
                    'tenant': {'uid': 999, 'gid': 986}}
        self.assertEqual(expected, self.manifest['job_identity_by_role'])
        refreshed = parity.refresh(self.manifest, self.observed())
        self.assertEqual(expected, refreshed['job_identity_by_role'])
        self.assertNotIn('job_identity', refreshed)

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

    def test_file_package_is_required_on_every_linux_role(self):
        for role in ('build-server', 'tenant', 'boat'):
            with self.subTest(role=role):
                packages = self.manifest['apt_packages']
                self.assertIn('file', packages['required'] + packages['by_role'][role])
                actual = self.observed(role)
                actual['apt_packages'].pop('file')
                self.assertIn('missing apt package: file',
                              parity.differences(self.manifest, actual, role=role))

    def test_lsof_package_is_required_on_every_linux_role(self):
        for role in ('build-server', 'tenant', 'boat'):
            with self.subTest(role=role):
                packages = self.manifest['apt_packages']
                self.assertIn('lsof', packages['required'] + packages['by_role'][role])
                actual = self.observed(role)
                actual['apt_packages'].pop('lsof')
                self.assertIn('missing apt package: lsof',
                              parity.differences(self.manifest, actual, role=role))

    def test_zstd_and_ripgrep_packages_are_required_on_every_linux_role(self):
        for package in ('zstd', 'ripgrep'):
            for role in ('build-server', 'tenant', 'boat'):
                with self.subTest(package=package, role=role):
                    packages = self.manifest['apt_packages']
                    self.assertIn(package, packages['required'] + packages['by_role'][role])
                    actual = self.observed(role)
                    actual['apt_packages'].pop(package)
                    self.assertIn(f'missing apt package: {package}',
                                  parity.differences(self.manifest, actual, role=role))

    def test_musl_tools_is_required_only_on_the_build_server(self):
        manifest = self.manifest
        self.assertIn('musl-tools', manifest['apt_packages']['by_role']['build-server'])
        self.assertNotIn('musl-tools', manifest['apt_packages']['required'])
        setup = (parity.HERE / 'setup.sh').read_text()
        self.assertIn('build-essential clang lld musl-tools pkg-config', setup)
        server = self.observed('build-server')
        server['apt_packages'].pop('musl-tools')
        self.assertIn('missing apt package: musl-tools',
                      parity.differences(manifest, server, role='build-server'))
        for role in ('boat', 'tenant'):
            with self.subTest(role=role):
                self.assertNotIn('musl-tools', manifest['apt_packages']['by_role'][role])
                self.assertNotIn('missing apt package: musl-tools',
                                 parity.differences(manifest, self.observed(role), role=role))

    def test_build_server_job_visible_tools_are_in_the_catalogue(self):
        actual = self.observed()
        actual['local_bin_entries'].extend(
            ('bun-1.3.14', 'node22', 'corepack', 'npx', 'pn', 'pnpx', 'pnx'))
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

    def switched_on(self, role):
        """`role`'s job as it measures with the remote cache on."""
        actual = self.observed(role, job=True)
        actual['job_environment'].update(parity.remote_cache_environment.environment(role))
        if role != 'build-server':
            # No shared compile cache, so the runner's own sccache is the
            # wrapper; there is no supervisor file or backend beside it.
            actual['compile_wrapper'].update(
                resolved='/motor-tools/sccache', supervisor_cache=None, backend=None,
                backend_readonly=None, wrapper_readonly=True)
        return actual

    def test_every_role_qualifies_in_both_switch_states(self):
        for role in ('build-server', 'boat', 'tenant'):
            with self.subTest(role=role, state='off'):
                self.assertEqual([], self.report(self.observed(role, job=True), job=True, role=role)['errors'])
            with self.subTest(role=role, state='on'):
                report = self.report(self.switched_on(role), job=True, role=role, remote_cache_on=True)
                self.assertEqual([], report['errors'])
                self.assertTrue(report['remote_cache'])

    def test_switch_state_is_part_of_the_job_contract(self):
        # Leftovers of the other state are drift, not accepted extras, and the
        # remote delta is required only while the cache is on.
        for role in ('build-server', 'boat', 'tenant'):
            with self.subTest(role=role, state='on'):
                report = self.report(self.observed(role, job=True), job=True, role=role,
                                     remote_cache_on=True)
                self.assertFalse(report['passed'])
                self.assertIn("job SCCACHE_S3_RW_MODE: expected 'READ_ONLY'",
                              '\n'.join(report['errors']))
                if role != 'build-server':
                    self.assertIn('job RUSTC_WRAPPER: expected', '\n'.join(report['errors']))
            with self.subTest(role=role, state='off'):
                report = self.report(self.switched_on(role), job=True, role=role)
                self.assertFalse(report['passed'])
                # The delta's variables are not part of the switch-off contract.
                self.assertIn('job SCCACHE_S3_RW_MODE: expected None', '\n'.join(report['errors']))
                self.assertIn('job SCCACHE_S3_USE_SSL: expected None', '\n'.join(report['errors']))

    def test_switch_on_role_markers_are_checked(self):
        # The bucket and region come from the provisioning generation, so the
        # comparison pins them to the role's declared placeholder rather than
        # to a value this repository could not know.
        for role in ('build-server', 'boat', 'tenant'):
            for field, value in (('SCCACHE_ENDPOINT', 'http://127.0.0.1:1'),
                                 ('SCCACHE_BUCKET', 'other-bucket'),
                                 ('SCCACHE_S3_RW_MODE', 'READ_WRITE')):
                with self.subTest(role=role, field=field):
                    actual = self.switched_on(role)
                    actual['job_environment'][field] = value
                    report = self.report(actual, job=True, role=role, remote_cache_on=True)
                    self.assertIn('job ' + field + ': expected', '\n'.join(report['errors']))
            if role == 'build-server':
                actual = self.switched_on(role)
                del actual['job_environment']['SCCACHE_MULTILEVEL_CHAIN']
                report = self.report(actual, job=True, role=role, remote_cache_on=True)
                self.assertIn('job SCCACHE_MULTILEVEL_CHAIN', '\n'.join(report['errors']))
            else:
                # Boat and tenant VMs have no local level, so their switch-on
                # contract has no chain at all; a job that still carries one
                # must fail that contract.
                self.assertNotIn('SCCACHE_MULTILEVEL_CHAIN',
                                 self.switched_on(role)['job_environment'])
                actual = self.switched_on(role)
                actual['compile_wrapper']['resolved'] = '/srv/build/cargo-home/bin/sccache'
                report = self.report(actual, job=True, role=role, remote_cache_on=True)
                self.assertIn('compiler wrapper resolution', '\n'.join(report['errors']))
                actual = self.switched_on(role)
                actual['compile_wrapper']['wrapper_readonly'] = False
                report = self.report(actual, job=True, role=role, remote_cache_on=True)
                self.assertIn('compiler cache wrapper_readonly', '\n'.join(report['errors']))

    def test_switch_on_capture_collects_the_remote_variables(self):
        def capture(remote_cache_on):
            # The capture records the process environment of the job it runs
            # in. This test gives it one it controls, so the host's own cache
            # settings and wrapper never decide the result.
            environment = self.hermetic({
                'PATH': self.manifest['job_environment_by_role']['boat']['PATH'],
            })
            with patch.object(parity, 'output', return_value='stub'), \
                 patch.object(parity.Path, 'iterdir', return_value=iter(())), \
                 patch.object(parity.Path, 'glob', return_value=iter(())), \
                 patch.object(parity.shutil, 'which', return_value=None), \
                 patch.object(parity.os, 'environ', environment), \
                 patch.object(parity, 'runtime_capture', return_value={}), \
                 patch.object(parity.syscall_surface, 'capture', return_value={}):
                return parity.capture(parity.role_manifest(self.manifest, 'boat', True),
                                      job=True, role='boat', remote_cache_on=remote_cache_on)

        on, off = capture(True), capture(False)
        translated = {name: parity.remote_cache_environment.expand(value)
                      for name, value in parity.remote_cache_environment.JOB_ENVIRONMENT_BY_ROLE['boat'].items()}
        for name in translated:
            self.assertIn(name, on['job_environment'])
        # RUSTC_WRAPPER is a Boat baseline name that is absent (None) with the
        # switch off; every other remote name does not appear at all until it
        # is on.
        self.assertIsNone(off['job_environment']['RUSTC_WRAPPER'])
        declared = {**self.manifest['job_environment'],
                    **self.manifest['job_environment_by_role']['boat']}
        for name in set(translated) - set(declared):
            self.assertNotIn(name, off['job_environment'])
        # The capture resolves the wrapper the runner installs from PATH.
        self.assertIn('compile_wrapper', on)

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
             patch.object(parity, 'capture', side_effect=lambda manifest=None, job=False, role=None, **_: seen.append(manifest) or {}), \
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
        # The switch state is passed through; it defaults to the switch-off
        # contract, which is what this command line asks for.
        self.assertEqual({'job': True, 'role': 'tenant', 'remote_cache_on': False}, capture.call_args.kwargs)

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

    def test_pnpm_release_pin_names_the_registry_archive_and_its_commands(self):
        parity.validate(self.manifest)
        pnpm = self.manifest['tools']['pnpm']
        self.assertEqual(pnpm['source']['package'], 'pnpm@' + pnpm['version'])
        self.assertEqual(pnpm['layout']['root'], '/usr/local/lib/node_modules/pnpm')
        # The images install this release themselves; a pin that names another
        # package, a different registry or no integrity must not be range-checked
        # into acceptance.
        for change in ({'package': 'pnpm@11.2.0'},
                       {'registry': 'https://mirror.example/'},
                       {'sha512': 'latest'},
                       {'sha512': pnpm['source']['sha512'][:-4]},
                       {}):
            with self.subTest(change=change):
                manifest = copy.deepcopy(self.manifest)
                manifest['tools']['pnpm']['source'] = {**pnpm['source'], **change} if change else {}
                with self.assertRaisesRegex(ValueError, 'pnpm'):
                    parity.validate(manifest)

    def test_every_pnpm_command_must_be_an_alias_into_the_package_tree(self):
        parity.validate(self.manifest)
        self.assertEqual([name for name in parity.PNPM_COMMANDS], ['pnpm', 'pn', 'pnpx', 'pnx'])
        changes = {'pn': {'version_from': None},
                   'pnpx': {'realpath_suffix': '/lib/node_modules/corepack/dist/pnpx.js'},
                   'pnx': {'path': '/usr/bin/pnx'}}
        for name, change in changes.items():
            with self.subTest(command=name):
                manifest = copy.deepcopy(self.manifest)
                for key, value in change.items():
                    if value is None:
                        manifest['tools'][name].pop(key)
                    else:
                        manifest['tools'][name][key] = value
                with self.assertRaisesRegex(ValueError, 'pnpm command'):
                    parity.validate(manifest)
        with self.subTest(command='missing'):
            manifest = copy.deepcopy(self.manifest)
            del manifest['tools']['pnx']
            with self.assertRaisesRegex(ValueError, 'pnpm command'):
                parity.validate(manifest)
        with self.subTest(layout='unknown command'):
            manifest = copy.deepcopy(self.manifest)
            manifest['tools']['pnpm']['layout']['commands'].append('/usr/local/bin/pn2')
            with self.assertRaisesRegex(ValueError, 'pnpm install layout'):
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


class JournalRetentionHostFileTests(unittest.TestCase):
    """The Boat image installs the shared journald retention file under
    zz-motor-retention.conf so it sorts after the drop-in Boat's base image
    ships; the build server and tenant keep 60-motor-retention.conf. Each role
    must check the name it actually installs, with identical contents."""

    def test_each_role_checks_the_retention_file_it_installs(self):
        manifest = json.loads(parity.MANIFEST.read_text())
        shared = '/etc/systemd/journald.conf.d/60-motor-retention.conf'
        boat = '/etc/systemd/journald.conf.d/zz-motor-retention.conf'
        for role in ('build-server', 'tenant'):
            self.assertEqual([shared], list(parity.role_manifest(manifest, role)['host_files']))
        boat_files = parity.role_manifest(manifest, 'boat')['host_files']
        self.assertEqual([boat], list(boat_files))
        self.assertEqual(manifest['host_files'][shared], boat_files[boat])
        install = (parity.HERE.parent / 'boat-image' / 'install.py').read_text()
        self.assertIn(f'JOURNAL_RETENTION_DROP_IN = "{boat}"', install)


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
