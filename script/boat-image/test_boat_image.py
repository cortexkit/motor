#!/usr/bin/env python3
"""Portable recipe integrity tests, not proof of a functioning Boat host."""
import contextlib
import copy
import importlib.util
import inspect
import io
from importlib.machinery import SourceFileLoader
import json
import os
import pwd
import stat
import sys
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock, MagicMock

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("boat_image_install", HERE / "install.py")
if spec is None or spec.loader is None:
    raise ImportError("cannot load Boat recipe")
install = importlib.util.module_from_spec(spec)
spec.loader.exec_module(install)
start_spec = importlib.util.spec_from_file_location("boat_start", HERE / "motor-boat-start.py")
if start_spec is None or start_spec.loader is None:
    raise ImportError("cannot load Boat startup budget helper")
start = importlib.util.module_from_spec(start_spec)
start_spec.loader.exec_module(start)
network_spec = importlib.util.spec_from_loader('boat_network', SourceFileLoader('boat_network', str(HERE / 'motor-boat-network')))
if network_spec is None or network_spec.loader is None:
    raise ImportError('cannot load Boat network preflight')
network = importlib.util.module_from_spec(network_spec)
network_spec.loader.exec_module(network)
credentials_spec = importlib.util.spec_from_file_location('boat_credentials', HERE / 'motor-boat-credentials.py')
if credentials_spec is None or credentials_spec.loader is None:
    raise ImportError('cannot load Boat credential hygiene')
credentials = importlib.util.module_from_spec(credentials_spec)
credentials_spec.loader.exec_module(credentials)


class RecipeTests(unittest.TestCase):
    def setUp(self):
        self.pins = json.loads((HERE / "pins.json").read_text())

    def test_pins_match_real_runner_inputs(self):
        install.validate(self.pins)
        manifest = json.loads(install.PARITY_PATH.read_text())
        self.assertEqual(manifest["tools"]["nats-server"]["version"], "nats-server: v2.15.0")
        self.assertIn("libsqlite3-dev", manifest["apt_packages"]["required"])
        self.assertIn("python3", manifest["apt_packages"]["required"])
        self.assertEqual(set(manifest["rust_toolchains"]), {
            name + "-x86_64-unknown-linux-gnu" for name in ("1.91.0", "1.96.0", "1.99.0", "stable")})

    def test_wrong_input_hash_is_refused(self):
        pins = copy.deepcopy(self.pins)
        pins["runner_inputs_sha256"]["script/runner/tools.txt"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "input changed"):
            install.validate(pins)

    def test_endpoint_port_is_refused(self):
        with self.assertRaises(ValueError):
            install.validate(dict(self.pins, probe_port=9443))

    def test_recipe_preinstalls_wasm32_unknown_unknown(self):
        manifest = json.loads(install.PARITY_PATH.read_text())
        with patch.object(install, "run", return_value="") as run:
            install.install_rust(manifest, "/rustup-init")
        commands = [call.args for call in run.call_args_list if call.args[:3] == ('rustup', 'toolchain', 'install')]
        self.assertEqual({command[3] for command in commands}, {name for name in manifest['rust_toolchains'] if not name.startswith('stable-')})
        for command in commands:
            components = [command[i + 1] for i, arg in enumerate(command) if arg == '--component']
            targets = [command[i + 1] for i, arg in enumerate(command) if arg == '--target']
            if command[3] in ('1.99.0-x86_64-unknown-linux-gnu', 'stable-x86_64-unknown-linux-gnu'):
                self.assertIn('wasm32-unknown-unknown', targets)
                self.assertIn('x86_64-pc-windows-gnu', targets)
                self.assertIn('x86_64-unknown-linux-musl', targets)
            self.assertIn('clippy', components)

    def test_recipe_disables_rustup_self_update_before_installing_toolchains(self):
        manifest = json.loads(install.PARITY_PATH.read_text())
        with patch.object(install, 'run', return_value='') as run:
            install.install_rust(manifest, '/rustup-init')
        calls = [call.args for call in run.call_args_list]
        disable = calls.index(('rustup', 'set', 'auto-self-update', 'disable'))
        installs = [i for i, args in enumerate(calls) if args[:3] == ('rustup', 'toolchain', 'install')]
        self.assertTrue(installs and disable < min(installs))
        self.assertEqual(calls[0][0], '/rustup-init')
        self.assertLess(0, disable)

    def test_recipe_uses_server_bun_home_and_preserves_job_path_order(self):
        unit = (HERE / 'motor-boat.service').read_text()
        self.assertIn('Environment=HOME=/home/ubuntu\n', unit)
        self.assertIn('Environment=PATH=/home/ubuntu/.bun/bin:/srv/build/cargo-home/bin:/usr/local/bin:/usr/bin:/bin:', unit)

    def test_pinned_git_is_built_and_precedes_ubuntu_git_for_runner_and_jobs(self):
        manifest = json.loads(install.PARITY_PATH.read_text())
        link_dir = manifest['tools']['git']['path_by_role']['boat'].rsplit('/', 1)[0]
        unit = (HERE / 'motor-boat.service').read_text()
        runner_path = next(line.split('=', 2)[2] for line in unit.splitlines() if line.startswith('Environment=PATH='))
        job_path = manifest['job_environment_by_role']['boat']['PATH']
        for path in (runner_path, job_path):
            entries = path.split(':')
            self.assertLess(entries.index(link_dir), entries.index('/usr/bin'), path)
        source = (HERE / 'install.py').read_text()
        self.assertIn('run("bash", str(ROOT / "script/runner/build-git.sh"))', source)
        self.assertIn('manifest["tools"]["git"]["source"]["build_packages"]', source)

    def test_binaryen_120_replaces_the_distro_tool_and_is_pinned(self):
        manifest = json.loads(install.PARITY_PATH.read_text())
        spec = manifest['tools']['wasm-opt']
        self.assertEqual('wasm-opt version 120', spec['version'])
        self.assertEqual('/usr/local/bin/wasm-opt', spec['path_by_role']['boat'])
        self.assertEqual('ddb097af51d1bdb17300d986b0de7d97422f1933dedb4c9eda3510e0bf4076cc',
                         spec['source']['sha256'])
        self.assertNotIn('binaryen', manifest['apt_packages']['required'])
        self.assertIn('binaryen', install.BOAT_BASE_PURGE)
        self.assertIn('script/runner/install-binaryen.py', install.RUNNER_INPUTS)
        self.assertIn("run('/usr/bin/python3', str(ROOT / 'script/runner/install-binaryen.py')",
                      (HERE / 'install.py').read_text())
        self.assertIn('"wasm-opt": ["wasm-opt", "--version"]', (HERE / 'install.py').read_text())

    def test_recipe_passes_exact_rustup_component_and_target_argv(self):
        manifest = json.loads(install.PARITY_PATH.read_text())
        with patch.object(install, 'run', return_value='') as run:
            install.install_rust(manifest, '/rustup-init')
        names = ('1.99.0-x86_64-unknown-linux-gnu', 'stable-x86_64-unknown-linux-gnu')
        actual = {call.args[3]: call.args for call in run.call_args_list
                  if call.args[:3] == ('rustup', 'toolchain', 'install') and call.args[3] in names}
        targets = ('--target', 'aarch64-pc-windows-msvc', '--target', 'wasm32-unknown-unknown',
                   '--target', 'wasm32-wasip1', '--target', 'x86_64-pc-windows-gnu',
                   '--target', 'x86_64-pc-windows-msvc', '--target', 'x86_64-unknown-linux-gnu',
                   '--target', 'x86_64-unknown-linux-musl')
        self.assertEqual(actual, {
            names[0]: ('rustup', 'toolchain', 'install', names[0], '--profile', 'minimal',
                       '--component', 'clippy', '--component', 'rustfmt', *targets),
        })
        root = manifest['rustup_home'] + '/toolchains/'
        run.assert_any_call('cp', '-a', '--reflink=auto', root + names[0] + '/.', root + names[1])

    def test_recipe_normalizes_preview_names_and_removes_extra_parts(self):
        host = 'x86_64-unknown-linux-gnu'
        self.assertEqual(install.rustup_component_name('clippy-preview-' + host, host), 'clippy')
        self.assertEqual(install.rustup_component_name('rustfmt-preview-' + host, host), 'rustfmt')
        manifest = json.loads(install.PARITY_PATH.read_text())
        name = '1.91.0-' + host
        def run(*args, **kwargs):
            if args == ('rustup', 'component', 'list', '--installed', '--toolchain', name):
                return '\n'.join([*manifest['rust_toolchains'][name]['components'],
                                  'rust-std-wasm32-unknown-unknown', 'rust-analyzer-' + host])
            return ''
        with patch.object(install, 'run', side_effect=run) as execute:
            install.install_rust(manifest, '/rustup-init')
        execute.assert_any_call('rustup', 'target', 'remove', '--toolchain', name, 'wasm32-unknown-unknown')
        execute.assert_any_call('rustup', 'component', 'remove', '--toolchain', name, 'rust-analyzer')

    def test_recipe_freezes_stable_when_upstream_has_moved(self):
        manifest = json.loads(install.PARITY_PATH.read_text())
        stable = 'stable-x86_64-unknown-linux-gnu'
        numbered = '1.99.0-x86_64-unknown-linux-gnu'
        state = copy.deepcopy(manifest['rust_toolchains'])
        state[stable]['rustc'] = 'rustc 1.99.1 (58cf8f9c3 2026-10-13)'
        def run(*args, **kwargs):
            if args[:3] == ('rustup', 'toolchain', 'list'):
                return '\n'.join(state)
            if args[:3] == ('rustup', 'toolchain', 'install'):
                name = args[3]
                state[name] = copy.deepcopy(manifest['rust_toolchains'][name])
                if name == stable:
                    state[name]['rustc'] = 'rustc 1.99.1 (58cf8f9c3 2026-10-13)'
            elif args[:3] == ('rustup', 'toolchain', 'uninstall'):
                state.pop(args[3], None)
            elif args[0] == 'cp':
                state[Path(args[-1]).name] = copy.deepcopy(state[Path(args[-2]).name])
            elif args[:3] == ('rustup', 'component', 'list'):
                return '\n'.join(state[args[-1]]['components'])
            elif args[:3] == ('rustup', 'component', 'remove'):
                state[args[4]]['components'].remove(args[5] + '-x86_64-unknown-linux-gnu')
            return ''
        with patch.object(install, 'run', side_effect=run):
            install.install_rust(manifest, '/rustup-init')
        self.assertEqual(state[stable], manifest['rust_toolchains'][stable])
        self.assertEqual(state[numbered], manifest['rust_toolchains'][numbered])

    def test_recipe_bootstraps_empty_rustup_and_removes_unlisted_toolchains(self):
        manifest = json.loads(install.PARITY_PATH.read_text())
        stable = 'stable-x86_64-unknown-linux-gnu'
        for listed, removed in (('no installed toolchains', [stable]),
                                ('1.89.0-x86_64-unknown-linux-gnu', ['1.89.0-x86_64-unknown-linux-gnu', stable])):
            def run(*args, **kwargs):
                return listed if args == ('rustup', 'toolchain', 'list') else ''
            with self.subTest(listed=listed), patch.object(install, 'run', side_effect=run) as execute:
                install.install_rust(manifest, '/rustup-init')
                self.assertEqual([call.args[3] for call in execute.call_args_list if call.args[:3] == ('rustup', 'toolchain', 'uninstall')], removed)

    def test_coreutils_aliases_point_at_gnu_executables(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp, 'cp')
            source.write_text('GNU coreutils fixture')
            alias = Path(tmp, 'gnucp')
            install.gnu_aliases({'gnu_coreutils': {'gnucp': str(alias)}})
            self.assertEqual(alias.resolve(), source)
            install.gnu_aliases({'gnu_coreutils': {'gnucp': str(alias)}})

    def test_checksum_parser_rejects_missing_ambiguous_and_malformed(self):
        sha = "a" * 64
        self.assertEqual(install.checksum(f"{sha} *file.tgz\n", "file.tgz"), sha)
        for text in ("bad file.tgz", f"{sha} other", f"{sha} file.tgz\n{sha} file.tgz"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                install.checksum(text, "file.tgz")

    def test_download_verifies_bytes_not_filename(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, target = Path(tmp, "source"), Path(tmp, "target")
            source.write_bytes(b"hello")
            expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            self.assertEqual(install.digest(source), expected)
            install.download(source.as_uri(), target, expected)
            self.assertEqual(target.read_bytes(), b"hello")
            with self.assertRaisesRegex(ValueError, "mismatch"):
                install.download(source.as_uri(), target, "0" * 64)

    def test_host_unit_has_explicit_boat_profile_and_no_enabled_bridge(self):
        unit = (HERE / "motor-boat.service").read_text()
        self.assertIn("--boat-profile", unit)
        # The runner's CPU counts are set at start (sandbox_cpus feeds sized_runner_args), not in the unit.
        self.assertNotIn("--max-weight", unit)
        self.assertNotIn("--rustc-slots", unit)
        self.assertNotIn("@VCPUS@", unit)
        self.assertIn("--job-network veth", unit)
        self.assertIn("User=root", unit)
        self.assertIn('--tenant-vm --tenant-socket-dir /run/motor-tenant', unit)
        self.assertIn('--reserve-daemon-core false', unit)
        self.assertIn('--free-space-floor-bytes 25769803776', unit)
        self.assertIn('--free-space-eviction-bytes 34359738368', unit)
        self.assertIn('RuntimeDirectoryMode=0700', unit)
        self.assertNotIn("[Install]", (HERE / "motor-boat-bridge.service").read_text())
        self.assertEqual(self.pins["probe_port"], 19443)

    def test_target_directories_exist_before_installing_binaries_or_units(self):
        with patch.object(install, 'run') as run:
            install.target_directories()
        self.assertEqual(run.call_args.args[:8], ('install','-d','-o','root','-g','root','-m','0755'))
        self.assertTrue({'/usr/local/bin','/usr/local/libexec','/etc/systemd/system','/etc/ssh/sshd_config.d','/etc/motor','/etc/motor/tenant'} <= set(run.call_args.args[8:]))

    def test_absent_tool_home_is_created_root_owned_before_tools_install(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(install, 'run') as run:
            home = Path(tmp, 'ubuntu')
            self.assertEqual(install.tool_home(home), {'path': str(home), 'created': True})
        run.assert_called_once_with('install', '-d', '-o', 'root', '-g', 'root', '-m', '0755', str(home))
        main = (HERE / 'install.py').read_text().split('def main():', 1)[1]
        self.assertLess(main.index('tool_home()'), main.index('tools(manifest)'))

    def test_tool_home_owned_by_another_account_is_refused_with_its_identity(self):
        if os.geteuid() == 0:
            self.skipTest('needs a directory a non-root account owns')
        uid = os.getuid()
        with tempfile.TemporaryDirectory() as tmp, patch.object(install, 'run') as run:
            home, proc = Path(tmp, 'ubuntu'), Path(tmp, 'proc')
            home.mkdir(mode=0o755)
            for pid, owner in (('123', uid), ('456', uid + 1)):
                (proc / pid).mkdir(parents=True)
                (proc / pid / 'status').write_text(f'Name:\tfixture\nUid:\t{owner}\t{owner}\t{owner}\t{owner}\n')
            (proc / 'self').mkdir()
            with self.assertRaises(ValueError) as refused:
                install.tool_home(home, proc)
        run.assert_not_called()
        message = str(refused.exception)
        self.assertIn(f'is owned by uid {uid} (', message)
        try:
            self.assertIn(pwd.getpwuid(uid).pw_name + ':x:' + str(uid), message)
        except KeyError:
            self.assertIn('no passwd entry', message)
        self.assertIn('running processes: [123]', message)

    def test_tool_home_must_be_a_root_directory_without_group_or_other_write(self):
        for mode, expected in ((0o040755, None), (0o040775, 'mode 0775'), (0o040757, 'mode 0757'),
                               (0o120777, 'physical directory')):
            home = Mock()
            home.lstat.return_value = Mock(st_uid=0, st_mode=mode)
            home.__str__ = lambda _self: '/home/ubuntu'
            with self.subTest(mode=oct(mode)), patch.object(install, 'run') as run:
                if expected is None:
                    self.assertEqual(install.tool_home(home), {'path': '/home/ubuntu', 'created': False})
                else:
                    with self.assertRaisesRegex(ValueError, expected):
                        install.tool_home(home)
                run.assert_not_called()

    def test_archives_are_extracted_without_recorded_owners_under_a_fixed_umask(self):
        seen = []
        def record(*args, **_kwargs):
            seen.append((args, os.umask(0o022)))
        previous = os.umask(0o077)
        try:
            with patch.object(install, 'run', side_effect=record):
                install.extract('/srv/build/downloads/node.tar.xz', '/usr/local', '-J', '--strip-components=1')
            self.assertEqual(os.umask(previous), 0o077)
        finally:
            os.umask(previous)
        self.assertEqual(seen, [(('tar', '--no-same-owner', '--no-same-permissions', '-J', '--strip-components=1',
                                  '-xf', '/srv/build/downloads/node.tar.xz', '-C', '/usr/local'), 0o022)])

    def test_rendered_helper_install_normalizes_every_parent_and_fixed_binary(self):
        calls = []
        with patch.object(install, 'run', side_effect=lambda *args, **kwargs: calls.append(args)):
            install.install_permission_helper('/srv/build/candidate')
        directory = calls[0]
        self.assertEqual(directory[:8], ('install','-d','-o','root','-g','root','-m','0755'))
        self.assertTrue({'/','/usr','/usr/local','/usr/local/libexec'} <= set(directory[8:]))
        self.assertEqual(calls[-1], ('install','-o','root','-g','root','-m','0755','/srv/build/candidate','/usr/local/libexec/motor-runner-permissions'))

    def test_admission_endpoint_waits_for_identity_without_baking_it_into_image(self):
        unit = (HERE / 'motor-boat-host.service').read_text()
        watcher = (HERE / 'motor-boat-identity.service').read_text()
        self.assertIn('ConditionPathExists=/etc/motor/tenant/id', unit)
        self.assertIn('motor-boat-start.py --host-exec --sandbox /etc/motor/sandbox.json', unit)
        self.assertIn('motor-boat-start.py --watch-identity', watcher)
        self.assertIn('WantedBy=multi-user.target', watcher)
        # The inotify path unit is gone: on Boat it armed on the temporary FUSE
        # /etc used during the snapshot restore and missed the real file.
        self.assertFalse((HERE / 'motor-boat-host.path').exists())
        self.assertIn('motor-boat-host.path', install.RETIRED_UNITS)
        self.assertNotIn('motor-boat-host.path', install.UNITS)

    def test_runner_mount_and_identity_watcher_wait_for_boat_restore(self):
        for name in ('motor-boat.service', 'motor-boat-host.service', 'motor-boat-identity.service'):
            text = (HERE / name).read_text()
            with self.subTest(unit=name):
                self.assertRegex(text, r'(?m)^Requires=.*\bmotor-boat-restored\.service\b')
                self.assertRegex(text, r'(?m)^After=.*\bmotor-boat-restored\.service\b')
        gate = (HERE / 'motor-boat-restored.service').read_text()
        self.assertIn('DefaultDependencies=no', gate)
        self.assertIn('RemainAfterExit=yes', gate)
        self.assertIn('motor-boat-start.py --wait-restored', gate)
        self.assertNotIn('[Install]', gate)
        writes = {}
        with patch.object(install, 'run'), patch.object(install.Path, 'mkdir'), \
             patch.object(install.Path, 'exists', return_value=True), \
             patch.object(install.Path, 'write_text', lambda path, text: writes.__setitem__(str(path), text)), \
             patch.object(install, 'tempfile'), patch.object(install, 'fcntl'), \
             patch.object(install.Path, 'read_bytes', return_value=b''):
            install.run.return_value = 'xfs'
            install.state(self.pins)
        mount = writes['/etc/systemd/system/var-lib-motor\\x2dboat.mount']
        self.assertIn('Requires=motor-boat-restored.service', mount)
        self.assertIn('After=motor-boat-restored.service', mount)
        for name in ('/etc/systemd/system/var-lib-motor\\x2dboat.mount', '/etc/systemd/system/Users.mount'):
            for directive in ('Requires', 'After'):
                values = next(line.split('=', 1)[1].split() for line in writes[name].splitlines() if line.startswith(directive + '='))
                self.assertNotIn('motor-boat-storage.service', values)
            self.assertIn('ConditionPathExists=!/run/motor-boat-storage.json', writes[name])
            self.assertNotIn('WantedBy=', writes[name])

    def test_storage_service_orders_all_storage_writers(self):
        for name in ('motor-boat.service', 'motor-boat-host.service'):
            unit = (HERE / name).read_text()
            for directive in ('Requires', 'After'):
                values = next(line.split('=', 1)[1].split() for line in unit.splitlines() if line.startswith(directive + '='))
                self.assertIn('motor-boat-storage.service', values)
                self.assertNotIn('Users.mount', values)
                self.assertNotIn(r'var-lib-motor\x2dboat.mount', values)
        storage = (HERE / 'motor-boat-storage.service').read_text()
        self.assertIn('DefaultDependencies=no', storage)
        self.assertIn('Requires=motor-boat-restored.service', storage)
        self.assertIn('After=motor-boat-restored.service', storage)
        self.assertIn('motor-boat-start.py --prepare-storage', storage)
        self.assertIn('motor-boat-storage.service', install.UNITS)

    def test_storage_rejects_boat_sysbind_even_when_mount_unit_would_be_active(self):
        root = '32 2 253:1 / / rw - ext4 /dev/vda1 rw\n'
        sysbind = root + '1200 32 253:1 /var/lib/motor-boat /var/lib/motor-boat rw - ext4 /dev/vda1 rw\n'
        for table in (root, sysbind):
            with self.subTest(table=table), self.assertRaisesRegex(RuntimeError, 'must be an XFS mount'):
                start.state_mount(start.mount_table(table))
        rows = start.mount_table('1200 32 7:0 / /var/lib/motor-boat rw - xfs /dev/loop0 rw\n')
        with patch.object(start.Path, 'stat', return_value=Mock(st_dev=1792)), \
             patch.object(start.Path, 'is_file', return_value=True), \
             patch.object(start.Path, 'read_text', return_value='/var/lib/wrong.xfs'), \
             patch.object(start.Path, 'samefile', return_value=False), \
             self.assertRaisesRegex(RuntimeError, 'not backed by'):
            start.state_mount(rows)

    def test_users_requires_exact_bind_not_just_any_xfs_mount(self):
        rows = start.mount_table('1 0 7:0 /elsewhere /Users rw - xfs /dev/loop0 rw\n')
        with patch.object(start.Path, 'samefile', return_value=False), self.assertRaisesRegex(RuntimeError, 'must bind'):
            start.users_mount(rows)

    def test_storage_rejects_stacked_targets_even_when_top_mount_is_correct(self):
        rows = start.mount_table('1 0 253:1 /var/lib/motor-boat /var/lib/motor-boat rw - ext4 /dev/vda1 rw\n'
                                 '2 1 7:0 / /var/lib/motor-boat rw - xfs /dev/loop0 rw\n')
        with self.assertRaisesRegex(RuntimeError, 'stacked mount targets'):
            start.state_mount(rows)
        users = start.mount_table('3 0 253:1 /Users /Users rw - ext4 /dev/vda1 rw\n'
                                  '4 3 7:0 /state/workspaces /Users rw - xfs /dev/loop0 rw\n')
        with patch.object(start.Path, 'samefile', return_value=True), self.assertRaisesRegex(RuntimeError, 'stacked mount targets'):
            start.users_mount(users)

    def test_storage_rejects_non_reflink_xfs_and_failed_clone(self):
        with patch.object(start, 'mounts', return_value=[]), patch.object(start, 'state_mount'), \
             patch.object(start, 'users_mount'), patch.object(start.subprocess, 'check_output', return_value='reflink=0'), \
             self.assertRaisesRegex(RuntimeError, 'reflink enabled'):
            start.check_storage()
        with tempfile.TemporaryDirectory() as tmp, patch.object(start, 'STATE', Path(tmp)), \
             patch.object(start, 'USERS', Path(tmp)), patch.object(start, 'mounts', return_value=[]), \
             patch.object(start, 'state_mount'), patch.object(start, 'users_mount'), \
             patch.object(start.subprocess, 'check_output', return_value='reflink=1'), \
             patch.object(start.fcntl, 'ioctl', side_effect=OSError(95, 'unsupported')), self.assertRaises(OSError):
            start.check_storage()

    def test_prepare_storage_never_mounts_over_nonempty_views(self):
        with tempfile.TemporaryDirectory() as tmp:
            state, users = Path(tmp, 'state'), Path(tmp, 'Users')
            state.mkdir()
            users.mkdir()
            (users / 'job-stdin').touch()
            with patch.object(start, 'STATE', state), patch.object(start, 'USERS', users), \
                 patch.object(start.Path, 'read_text', return_value=''), \
                 patch.object(start, 'restore_pending', return_value=None), patch.object(start, 'mounts', return_value=[]), \
                 patch.object(start.subprocess, 'check_output', return_value='0'), \
                 patch.object(start, 'state_mount', side_effect=RuntimeError('ext4')), \
                 patch.object(start, 'users_mount', side_effect=RuntimeError('ext4')), patch.object(start.subprocess, 'run') as run:
                with self.assertRaisesRegex(RuntimeError, 'non-empty directory'):
                    start.prepare_storage()
                run.assert_not_called()
            self.assertTrue((users / 'job-stdin').exists())

    def test_prepare_storage_uses_systemd_owners_and_records_readiness(self):
        with tempfile.TemporaryDirectory() as tmp:
            state, users, evidence = Path(tmp, 'state'), Path(tmp, 'Users'), Path(tmp, 'evidence')
            with patch.object(start, 'STATE', state), patch.object(start, 'USERS', users), \
                 patch.object(start.Path, 'read_text', return_value=''), \
                 patch.object(start, 'STORAGE_EVIDENCE', evidence), patch.object(start, 'restore_pending', return_value=None), \
                 patch.object(start, 'mounts', return_value=[]), \
                 patch.object(start, 'state_mount', side_effect=[RuntimeError('ext4'), ('state', 'xfs', 'loop')]), \
                 patch.object(start, 'users_mount', side_effect=RuntimeError('ext4')), \
                 patch.object(start, 'storage_identity', return_value=(1792, 128, 131)), \
                 patch.object(start.subprocess, 'check_output', return_value='0'), \
                 patch.object(start, 'check_storage', return_value={'reflink': True}), patch.object(start.subprocess, 'run') as run:
                start.prepare_storage()
            self.assertEqual([call.args[0] for call in run.call_args_list], [
                ['/usr/bin/systemctl', 'daemon-reload'],
                ['/usr/bin/systemctl', 'start', r'var-lib-motor\x2dboat.mount'],
                ['/usr/bin/systemctl', 'start', 'Users.mount'],
                ['/usr/bin/systemctl', 'is-active', '--quiet', r'var-lib-motor\x2dboat.mount', 'Users.mount']])
            self.assertGreater(json.loads(evidence.read_text())['verified_monotonic_ns'], 0)
            self.assertTrue((state / 'state/workspaces').is_dir())

    def test_storage_prepare_refuses_live_consumers_without_clearing_fence(self):
        with patch.object(start.Path, 'read_text', return_value=''), \
             patch.object(start, 'restore_pending', return_value=None), \
             patch.object(start.subprocess, 'check_output', return_value='123'), \
             patch.object(start.Path, 'unlink') as unlink, patch.object(start.subprocess, 'run') as run, \
             self.assertRaisesRegex(RuntimeError, 'refusing to change storage'):
            start.prepare_storage()
        unlink.assert_not_called()
        run.assert_not_called()

    def test_deferred_public_bind_cleanup_blocks_readiness_after_marker_removal(self):
        root = '32 2 253:1 / / rw - ext4 /dev/vda1 rw\n'
        public = '100 32 253:1 /Users /Users rw - ext4 /dev/vda1 rw\n'
        self.assertIn('not been retired', start.restore_pending(root + public, False))
        state = public.replace('/Users', '/var/lib/motor-boat')
        self.assertIn('not been retired', start.restore_pending(root + state, False))
        self.assertIsNone(start.restore_pending(root, False))
        now = [0]
        def pause(seconds):
            now[0] += seconds
        # Model marker removal at zero, deferred cleanup at three, then require
        # five uninterrupted seconds before storage can be established.
        def table():
            return root + (public if now[0] < 3 else '')
        with patch.object(start.Path, 'read_text', side_effect=table), \
             patch.object(start.RESTORE_ACTIVE.__class__, 'exists', return_value=False):
            start.wait_restored(clock=lambda: now[0], pause=pause)
        self.assertEqual(now[0], 3 + start.RESTORE_STABLE_S)

    def test_storage_identity_rejects_users_reverting_to_root_disk(self):
        root_device = Path('/').stat().st_dev
        state, users = Mock(), Mock()
        state.stat.return_value = Mock(st_dev=root_device + 1, st_ino=128)
        users.stat.return_value = Mock(st_dev=root_device, st_ino=131)
        with patch.object(start, 'STATE', state), patch.object(start, 'USERS', users), \
             self.assertRaisesRegex(RuntimeError, 'Boat storage view changed'):
            start.storage_identity()
        users.samefile.assert_not_called()

    def test_storage_identity_requires_same_workspace_directory_not_just_device(self):
        root_device = Path('/').stat().st_dev
        state, users = MagicMock(), Mock()
        state.stat.return_value = Mock(st_dev=root_device + 1, st_ino=128)
        users.stat.return_value = Mock(st_dev=root_device + 1, st_ino=131)
        users.samefile.return_value = False
        with patch.object(start, 'STATE', state), patch.object(start, 'USERS', users), \
             self.assertRaisesRegex(RuntimeError, 'Boat storage view changed'):
            start.storage_identity()
        users.samefile.assert_called_once_with(state / 'state/workspaces')

    @contextlib.contextmanager
    def storage_watch_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence, marker, health, lock = (Path(tmp, name) for name in ('evidence', 'ready', 'health.json', 'lock'))
            evidence.write_text(json.dumps({'identity': [1792, 128, 131]}))
            marker.touch()
            with patch.object(start, 'STORAGE_EVIDENCE', evidence), patch.object(start, 'ENDPOINT_READY', marker), \
                 patch.object(start, 'STORAGE_HEALTH', health), patch.object(start, 'STORAGE_LOCK', lock), \
                 patch.object(start, 'storage_identity', return_value=(1792, 128, 131)), \
                 patch.object(start, 'notify_systemd'):
                yield evidence, marker, health

    def test_storage_watch_starts_withdrawn_before_reading_evidence(self):
        with self.storage_watch_paths() as (_, marker, health):
            def read_evidence():
                self.assertFalse(marker.exists())
                self.assertEqual(json.loads(health.read_text())['status'], 'unhealthy')
                raise ValueError('bad evidence')
            with patch.object(start, 'STORAGE_EVIDENCE', Mock(read_text=read_evidence)), \
                 self.assertRaisesRegex(ValueError, 'bad evidence'):
                start.watch_storage()
            self.assertFalse(marker.exists())

    def test_storage_watch_withdraws_before_logging_even_when_logging_fails(self):
        with self.storage_watch_paths() as (_, marker, health):
            def pause(_):
                marker.touch()
            def log(*args, **kwargs):
                self.assertFalse(marker.exists())
                self.assertEqual(json.loads(health.read_text())['status'], 'unhealthy')
                raise BrokenPipeError('journal unavailable')
            with patch.object(start, 'storage_identity', side_effect=[(1792, 128, 131), RuntimeError('view lost')]), \
                 patch('builtins.print', side_effect=log), self.assertRaises(BrokenPipeError):
                start.watch_storage(pause=pause)
            self.assertFalse(marker.exists())

    def test_storage_watch_all_loop_exceptions_withdraw_and_escape(self):
        for source in ('evidence_json', 'evidence_missing', 'identity', 'pause', 'notify', 'health_write'):
            with self.subTest(source=source), self.storage_watch_paths() as (evidence, marker, health), contextlib.ExitStack() as stack:
                if source == 'evidence_json':
                    evidence.write_text('{')
                elif source == 'evidence_missing':
                    evidence.unlink()
                elif source == 'identity':
                    stack.enter_context(patch.object(start, 'storage_identity', side_effect=ValueError('stat failed')))
                elif source == 'notify':
                    stack.enter_context(patch.object(start, 'notify_systemd', side_effect=OSError('notify failed')))
                elif source == 'health_write':
                    original = start.write_storage_health
                    def write(status, identity=None, reason=None):
                        if status == 'healthy':
                            raise OSError('health write failed')
                        original(status, identity, reason)
                    stack.enter_context(patch.object(start, 'write_storage_health', side_effect=write))
                def pause(_):
                    marker.touch()
                    raise InterruptedError('pause failed')
                with self.assertRaises((OSError, ValueError)):
                    start.watch_storage(pause=pause)
                self.assertFalse(marker.exists())
                self.assertEqual(json.loads(health.read_text())['status'], 'unhealthy')

    def test_storage_watch_unlink_failure_is_fatal_and_cleanup_retries(self):
        with self.storage_watch_paths() as (_, marker, health):
            unlink, attempts = Path.unlink, []
            def remove(path, **kwargs):
                if path == marker:
                    attempts.append(path)
                    if len(attempts) == 1:
                        raise PermissionError('unlink failed')
                return unlink(path, **kwargs)
            with patch.object(Path, 'unlink', remove), self.assertRaises(PermissionError):
                start.watch_storage()
            self.assertGreaterEqual(len(attempts), 2)
            self.assertFalse(marker.exists())
            self.assertEqual(json.loads(health.read_text())['status'], 'unhealthy')

    def test_storage_watch_failed_revocation_write_cannot_leave_healthy_lease(self):
        with self.storage_watch_paths() as (_, marker, health):
            original, revocations = start.write_storage_health, []
            def write(status, identity=None, reason=None):
                if status == 'unhealthy':
                    revocations.append(status)
                    if len(revocations) > 1:
                        raise OSError('cannot write unhealthy state')
                original(status, identity, reason)
            def pause(_):
                self.assertEqual(json.loads(health.read_text())['status'], 'healthy')
                marker.touch()
                raise InterruptedError('stop watcher')
            with patch.object(start, 'write_storage_health', side_effect=write), self.assertRaises(InterruptedError):
                start.watch_storage(pause=pause)
            self.assertFalse(marker.exists())
            self.assertFalse(health.exists(), 'failed revocation must invalidate the old healthy lease')

    def test_storage_watch_heartbeats_only_after_completed_polls_and_never_publishes(self):
        with self.storage_watch_paths() as (_, marker, health):
            notifications, pauses = [], []
            def notify(message):
                notifications.append(message)
                self.assertFalse(marker.exists())
                status = 'unhealthy' if message == 'READY=1' else 'healthy'
                self.assertEqual(json.loads(health.read_text())['status'], status)
            def pause(seconds):
                pauses.append(seconds)
                self.assertFalse(marker.exists())
                state = json.loads(health.read_text())
                self.assertEqual(state['status'], 'healthy')
                self.assertEqual(state['identity'], [1792, 128, 131])
                self.assertGreater(state['expires_monotonic_ns'], start.time.monotonic_ns())
                self.assertLessEqual(state['expires_monotonic_ns'], start.time.monotonic_ns() + 5_000_000_000)
                self.assertEqual(health.stat().st_mode & 0o777, 0o600)
                if len(pauses) == 2:
                    raise InterruptedError('stop watcher')
            with self.assertRaises(InterruptedError):
                start.watch_storage(pause=pause, notify=notify)
            self.assertEqual(notifications, ['READY=1', 'WATCHDOG=1'])
            self.assertEqual(pauses, [1, 1])
            self.assertFalse(marker.exists())
            self.assertEqual(json.loads(health.read_text())['status'], 'unhealthy')

    def test_storage_watch_restart_requires_matching_identity_before_healthy(self):
        with self.storage_watch_paths() as (_, marker, health):
            for identity in ((1792, 128, 999), (1792, 128, 999)):
                marker.touch()
                with patch.object(start, 'storage_identity', return_value=identity), self.assertRaises(RuntimeError):
                    start.watch_storage()
                self.assertFalse(marker.exists())
                self.assertEqual(json.loads(health.read_text())['status'], 'unhealthy')
            def pause(_):
                self.assertEqual(json.loads(health.read_text())['status'], 'healthy')
                self.assertFalse(marker.exists())
                raise InterruptedError('stop recovered watcher')
            with self.assertRaises(InterruptedError):
                start.watch_storage(pause=pause)

    def test_storage_watch_clean_exit_in_poll_is_still_a_failure(self):
        with self.storage_watch_paths() as (_, marker, health):
            def pause(_):
                marker.touch()
                raise SystemExit(0)
            with self.assertRaisesRegex(RuntimeError, 'exited during a poll'):
                start.watch_storage(pause=pause)
            self.assertFalse(marker.exists())
            self.assertEqual(json.loads(health.read_text())['status'], 'unhealthy')

    def test_storage_watch_health_lease_expires_without_further_polls(self):
        with self.storage_watch_paths() as (_, marker, health), patch.object(start.time, 'monotonic_ns', return_value=100):
            start.write_storage_health('healthy', (1792, 128, 131))
            self.assertEqual(json.loads(health.read_text())['expires_monotonic_ns'], 5_000_000_100)
            start.withdraw_storage('storage_identity_changed')
            withdrawal = json.loads(marker.with_name('endpoint-withdrawal.json').read_text())
            self.assertEqual(withdrawal['rule'], 'storage_identity_changed')
            self.assertIn('runner_state', withdrawal['volumes'])
            self.assertIn('workspace', withdrawal['volumes'])
            self.assertEqual(json.loads(health.read_text())['expires_monotonic_ns'], 0)

    def test_storage_watch_unit_has_watchdog_cleanup_and_explicit_crash_limit(self):
        unit = (HERE / 'motor-boat-storage-watch.service').read_text()
        for setting in ('Type=notify', 'NotifyAccess=main', 'WatchdogSec=5', 'Restart=on-failure',
                        'RestartSec=1', 'StartLimitIntervalSec=60', 'StartLimitBurst=5',
                        'RuntimeDirectory=motor-boat-storage', 'RuntimeDirectoryMode=0700',
                        'RuntimeDirectoryPreserve=yes',
                        'ExecStopPost=/usr/bin/python3 /usr/local/libexec/motor-boat-start.py --withdraw-storage'):
            self.assertIn(setting, unit)

    def test_image_config_gates_the_endpoint_marker_writer_on_watcher_health(self):
        writes = {}
        with patch.object(install, 'target_directories'), patch.object(install, 'accounts'), \
             patch.object(install, 'install_units'), patch.object(install, 'state'), \
             patch.object(install, 'run', return_value='8') as run, patch.object(install.shutil, 'copy2') as copy, \
             patch.object(Path, 'mkdir'), patch.object(Path, 'unlink'), \
             patch.object(Path, 'read_text', return_value='Include /etc/ssh/sshd_config.d/*.conf\n'), \
             patch.object(Path, 'write_text', lambda path, text: writes.__setitem__(str(path), text)):
            install.configure(self.pins, 'revision')
        copy.assert_called_once_with(install.ROOT / 'script/runner/sysctl/60-motor-inotify.conf', '/etc/sysctl.d/60-motor-inotify.conf')
        run.assert_any_call('sysctl', '-p', '/etc/sysctl.d/60-motor-inotify.conf')
        config = json.loads(writes['/etc/motor/sandbox.json'])
        self.assertEqual(config['ready_marker'], str(start.ENDPOINT_READY))
        self.assertEqual(config['storage_health'], {'state_file': str(start.STORAGE_HEALTH),
            'lock_file': str(start.STORAGE_LOCK), 'identity_paths': [str(start.STATE), str(start.USERS)]})
        # The endpoint's readiness floor is the runner's admission floor on the
        # runner's state disk, the one the unit passes the runner.
        unit = (HERE / 'motor-boat.service').read_text()
        self.assertIn(f"--free-space-floor-bytes {config['disk_floor']['floor_bytes']} ", unit)
        self.assertIn(f"--state-root {config['disk_floor']['path']} ", unit)

    def test_storage_watch_notify_uses_systemd_datagram_socket(self):
        with tempfile.TemporaryDirectory() as tmp, start.socket.socket(start.socket.AF_UNIX, start.socket.SOCK_DGRAM) as receiver:
            address = str(Path(tmp, 'notify'))
            receiver.bind(address)
            receiver.settimeout(1)
            with patch.dict(start.os.environ, {'NOTIFY_SOCKET': address}):
                start.notify_systemd('WATCHDOG=1')
            self.assertEqual(receiver.recv(128), b'WATCHDOG=1')

    def test_storage_withdraw_entry_point_only_revokes(self):
        with self.storage_watch_paths() as (_, marker, health), \
             patch.object(sys, 'argv', ['start', '--withdraw-storage']):
            start.main()
            self.assertFalse(marker.exists())
            self.assertEqual(json.loads(health.read_text())['status'], 'unhealthy')

    def test_runner_refuses_unmounted_storage_before_spawning(self):
        with patch.object(sys, 'argv', ['start', 'serve']), \
             patch.object(start, 'check_storage', side_effect=RuntimeError('wrong storage')), \
             patch.object(start.subprocess, 'Popen') as spawn, self.assertRaisesRegex(RuntimeError, 'wrong storage'):
            start.main()
        spawn.assert_not_called()

    def test_host_refuses_unmounted_storage_before_exec(self):
        with patch.object(sys, 'argv', ['start', '--host-exec']), \
             patch.object(start, 'check_storage', side_effect=RuntimeError('wrong storage')), \
             patch.object(start.os, 'execv') as execute, self.assertRaisesRegex(RuntimeError, 'wrong storage'):
            start.main()
        execute.assert_not_called()

    def test_endpoint_unit_publishes_marker_directory_and_keeps_restarting(self):
        unit = (HERE / 'motor-boat-host.service').read_text()
        self.assertIn('RuntimeDirectory=motor', unit)
        self.assertIn('RuntimeDirectoryMode=0755', unit)
        self.assertIn('StartLimitIntervalSec=0', unit)
        self.assertIn('/run/motor/endpoint-revision', unit)

    def test_restore_gate_waits_for_boat_marker_and_fuse_system_paths(self):
        live = ('442 32 253:1 /etc /var/lib/ascii-lazy/sysbind/etc rw - ext4 /dev/vda1 rw\n'
                '32 2 253:1 / / rw,relatime shared:1 - ext4 /dev/vda1 rw\n'
                '1897 32 0:52 / /etc rw,relatime shared:1791 - fuse ascii-lazyfs-sys rw,user_id=0\n')
        reason = start.restore_pending(live, False)
        self.assertIn('/etc is still served by ascii-lazyfs-sys (fuse)', reason)
        # After the restore Boat mounts the real directory on top and retires
        # the FUSE view elsewhere; the later entry on /etc wins.
        done = live.replace(' /etc rw,relatime shared:1791', ' /var/lib/ascii-lazy/retired/etc rw,relatime shared:1791') + \
            '1224 32 253:1 /etc /etc rw,relatime - ext4 /dev/vda1 rw\n'
        self.assertIsNone(start.restore_pending(done, False))
        self.assertIn('restore marker', start.restore_pending(done, True))
        stacked = done + '1999 1224 0:90 / /etc rw - fuse ascii-lazyfs-sys rw\n'
        self.assertIsNotNone(start.restore_pending(stacked, False))
        escaped = start.mount_table('5 1 0:1 / /var/lib/motor\\040boat rw - ext4 /dev/vda1 rw\n')
        self.assertEqual(escaped[0][0], '/var/lib/motor boat')
        self.assertIsNone(start.serving_mount('/etcetera', start.mount_table('1224 32 253:1 /etc /etc rw - fuse x rw\n')))

    def test_restore_gate_reports_then_gives_up_after_its_deadline(self):
        now = [0.0]
        def pause(seconds):
            now[0] += seconds
        output = io.StringIO()
        with patch.object(start.Path, 'read_text', return_value='1 0 0:52 / /usr rw - fuse lazy rw\n'), \
             patch.object(start, 'RESTORE_ACTIVE', Mock(exists=Mock(return_value=False))), \
             contextlib.redirect_stdout(output), self.assertRaisesRegex(RuntimeError, 'did not finish'):
            start.wait_restored(clock=lambda: now[0], pause=pause)
        self.assertEqual(output.getvalue().count('waiting for Boat'), 1)
        self.assertGreaterEqual(now[0], start.RESTORE_DEADLINE_S)

    def test_identity_watcher_starts_endpoint_once_for_either_write_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            identity = Path(tmp, 'id')
            starts = []
            # Already present when the watcher starts: no wait at all.
            identity.write_text('bx_23456789')
            start.watch_identity(lambda: starts.append('start'), lambda: start.identity_present(identity),
                                 pause=lambda _: self.fail('waited for an existing file'))
            self.assertEqual(starts, ['start'])
            identity.unlink()
            # Appears later, by rename into the directory: polled until present.
            polls = []
            def pause(_):
                polls.append(1)
                if len(polls) == 3:
                    staged = Path(tmp, '.staged')
                    staged.write_text('bx_23456789')
                    staged.rename(identity)
            start.watch_identity(lambda: starts.append('start'), lambda: start.identity_present(identity), pause=pause)
            self.assertEqual((starts, len(polls)), (['start', 'start'], 3))
            # A directory or dangling link at the path is not an identity file.
            identity.unlink()
            identity.symlink_to(Path(tmp, 'missing'))
            self.assertFalse(start.identity_present(identity))

    def test_provider_config_and_profiles_are_redacted_then_removed(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            config = home / '.config/ascii/boat/config.json'
            config.parent.mkdir(parents=True)
            config.write_text(json.dumps({'apiKey':'canary-never-print', 'url':'metadata', 'nested':{'token':None}}))
            profile = home / '.bashrc'
            profile.write_text('export ASCII_TOKEN="canary-never-print"\nexport BOAT_CURRENT_ID_FILE=/neutral/path\nexport BOAT_FUTURE="canary-never-print"\nexport PATH=/kept\n')
            result = credentials.sanitize([str(home)], [profile])
            self.assertEqual(result['config_before'][0]['key_names'], ['apiKey','nested','nested.token','url'])
            self.assertEqual(result['config_before'][0]['nonempty_credential_field_names'], ['apiKey'])
            self.assertNotIn('canary-never-print', json.dumps(result))
            self.assertFalse(config.exists())
            self.assertEqual(profile.read_text(), ':\n:\n:\nexport PATH=/kept\n')
            self.assertEqual(credentials.clean_environment({'ASCII_TOKEN':'secret','BOAT_UNKNOWN':'secret','PATH':'kept'}), {'PATH':'kept'})

    def test_bootstrap_and_host_exec_clear_every_provider_prefix(self):
        class ExecObserved(Exception):
            pass
        with tempfile.TemporaryDirectory() as tmp:
            source, sized = Path(tmp, 'sandbox.json'), Path(tmp, 'run-sandbox.json')
            source.write_text(json.dumps({'vcpu_ceiling': 8, 'job_socket': '/run/motor-tenant/jobs.sock'}))
            def execv(program, args):
                self.assertEqual(program, '/usr/local/bin/motor-host')
                self.assertEqual(args, [program, '--sandbox', str(sized)])
                self.assertEqual(dict(start.os.environ), {'PATH':'kept'})
                raise ExecObserved()
            with patch.dict(start.os.environ, {'ASCII_TOKEN':'secret','BOAT_FUTURE':'secret','BOAT_ID':'id','PATH':'kept'}, clear=True), \
                 patch.object(start, 'check_storage'), patch.object(start, 'SIZED_SANDBOX', sized), \
                 patch.object(start, 'sandbox_cpus', return_value=4), \
                 patch.object(sys, 'argv', ['start','--host-exec','--sandbox',str(source)]), patch.object(start.os, 'execv', execv):
                with self.assertRaises(ExecObserved):
                    start.main()
            self.assertEqual(json.loads(sized.read_text()), {'vcpu_ceiling': 4, 'job_socket': '/run/motor-tenant/jobs.sock'})

    def test_runner_and_endpoint_cpu_counts_follow_the_running_sandbox(self):
        # An image built on an 8-vCPU large and cloned onto a 4-vCPU default must
        # admit 4, whatever count an older unit file still carries.
        self.assertEqual(start.sized_runner_args(['serve', '--max-weight', '8', '--rustc-slots', '8', '--job-network', 'veth'], 4),
                         ['serve', '--job-network', 'veth', '--max-weight', '4', '--rustc-slots', '4'])
        self.assertEqual(start.sized_runner_args(['serve'], 2), ['serve', '--max-weight', '2', '--rustc-slots', '2'])
        with tempfile.TemporaryDirectory() as tmp:
            source, sized = Path(tmp, 'sandbox.json'), Path(tmp, 'sized.json')
            source.write_text(json.dumps({'vcpu_ceiling': 8, 'ready_marker': '/run/motor/endpoint-ready'}))
            self.assertEqual(start.sized_host_args(['--sandbox', str(source)], 4, sized), ['--sandbox', str(sized)])
            self.assertEqual(json.loads(sized.read_text()), {'vcpu_ceiling': 4, 'ready_marker': '/run/motor/endpoint-ready'})
            self.assertEqual(sized.stat().st_mode & 0o777, 0o600)
            self.assertEqual(json.loads(source.read_text())['vcpu_ceiling'], 8)

    def test_runner_is_started_with_the_measured_cpu_count(self):
        launched = []
        class Stop(Exception):
            pass
        def popen(argv):
            launched.append(argv)
            raise Stop()
        with patch.object(sys, 'argv', ['start', 'serve', '--boat-profile']), patch.object(start, 'check_storage'), \
             patch.object(start.Path, 'read_text', return_value=json.dumps({'mem_available_bytes': 8 << 30})), \
             patch.object(start, 'sandbox_cpus', return_value=4), patch.object(start.subprocess, 'Popen', popen), \
             self.assertRaises(Stop):
            start.main()
        self.assertEqual(launched[0][:6], ['/usr/local/bin/motor-runner', 'serve', '--boat-profile',
                                           '--max-weight', '4', '--rustc-slots'])
        self.assertEqual(launched[0][6], '4')

    def test_network_preflight_uses_absolute_admin_paths_and_never_applies_rules(self):
        calls = []
        def run(*args, **kwargs):
            calls.append(args)
            return Mock(stdout='[{"dev":"eth0"}]' if args[0] == network.IP else '')
        template = install.ROOT / 'script/runner/motor-net.nft'
        with patch.object(network, 'run', run), patch.object(network.subprocess, 'run', return_value=Mock(returncode=1)), \
             patch.object(network.Path, 'is_file', return_value=True), \
             patch.object(sys, 'argv', ['network','--check','--template',str(template)]):
            network.main()
        self.assertTrue(all(call[0].startswith('/usr/sbin/') for call in calls))
        self.assertIn((network.NFT, '--check', '-f', '-'), calls)
        self.assertTrue(any(call[:2] == (network.UFW, '--dry-run') for call in calls))
        self.assertFalse(any('-w' in call for call in calls))
        self.assertFalse(any(call[:2] == (network.NFT, '-f') for call in calls))

    def test_unit_preflight_collects_all_statuses_and_journals_before_stopping(self):
        failure = install.subprocess.CalledProcessError(1, 'systemctl start')
        with patch.object(install, 'run', side_effect=failure), patch.object(install.subprocess, 'run') as collect:
            with self.assertRaises(install.subprocess.CalledProcessError):
                install.service_preflight()
        calls = [call.args[0] for call in collect.call_args_list]
        for unit in ('motor-boat-restored.service','motor-boat-storage.service',r'var-lib-motor\x2dboat.mount','Users.mount','motor-boat.service','motor-boat-host.service','motor-boat-identity.service','motor-boat-bridge.service'):
            self.assertIn(['/usr/bin/systemctl','status','--no-pager','-l',unit], calls)
            self.assertIn(['/usr/bin/journalctl','-u',unit,'-b','--no-pager','-n','200'], calls)
        self.assertEqual(calls[-1], ['/usr/bin/systemctl','stop','motor-boat.service'])

    def test_memory_budget_subtracts_measured_idle_and_margin(self):
        self.assertEqual(start.budget(16*(1<<30), 1<<30), 15569256448)
        self.assertEqual(start.budget(16*(1<<30)+123, 1<<30), 15569256448)
        with self.assertRaises(ValueError):
            start.budget(1<<30, 1<<29)

    def test_apt_index_failure_is_fatal_and_transient_retry_is_bounded(self):
        failed = Mock(returncode=100, stdout="", stderr="503 Service Unavailable")
        with patch.object(install.subprocess, 'run', return_value=failed) as run:
            pauses = []
            with self.assertRaisesRegex(RuntimeError, "refusing package installation"):
                install.apt_update(pause=pauses.append)
            self.assertEqual(run.call_count, 3)
            self.assertEqual(pauses, [1, 2])
            self.assertIn('APT::Update::Error-Mode=any', run.call_args.args[0])
        failed.stderr = 'signature verification failed'
        with patch.object(install.subprocess, 'run', return_value=failed) as run:
            with self.assertRaises(RuntimeError):
                install.apt_update(pause=lambda _: None)
            self.assertEqual(run.call_count, 1)

    def test_archive_preflight_retries_official_indexes_only(self):
        calls = []
        def fetch(url, timeout):
            calls.append(url)
            if 'security.ubuntu.com' in url:
                raise install.urllib.error.HTTPError(url, 503, 'unavailable', {}, None)
            response = Mock(status=200)
            response.read.return_value = b'-----BEGIN PGP SIGNED MESSAGE-----\n'
            response.__enter__ = Mock(return_value=response)
            response.__exit__ = Mock(return_value=False)
            return response
        evidence = install.archive_preflight(self.pins, fetch=fetch, pause=lambda _: None)
        self.assertEqual(evidence[0]['attempts'][0]['status'], 200)
        self.assertEqual(len(evidence[2]['attempts']), 3)
        self.assertEqual(evidence[2]['attempts'][-1]['status'], 503)
        self.assertTrue(all(url.startswith(('https://archive.ubuntu.com/', 'https://security.ubuntu.com/')) for url in calls))

    def test_unsigned_or_third_party_sources_and_missing_release_dates_refuse(self):
        self.assertIn('Dir::Etc::sourcelist=/dev/null', install.APT_OPTIONS)
        self.assertIn('Dir::Etc::sourceparts=/etc/apt/motor-boat.sources.d', install.APT_OPTIONS)
        self.assertIn('Acquire::AllowInsecureRepositories=false', install.APT_OPTIONS)
        self.assertIn('APT::Get::AllowUnauthenticated=false', install.APT_OPTIONS)
        with self.assertRaises(ValueError):
            install.validate(dict(self.pins, ubuntu_archive='https://untrusted.invalid/ubuntu'))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for suite in ('noble', 'noble-updates', 'noble-security'):
                (root / (suite + '_InRelease')).write_text('-----BEGIN PGP SIGNED MESSAGE-----\nDate: Wed, 01 Jan 2025 12:00:00 UTC\n')
            manifest = install.release_manifest(root)
            self.assertEqual(len(manifest), 3)
            self.assertEqual(manifest['noble_InRelease']['date'], 'Wed, 01 Jan 2025 12:00:00 UTC')
            (root / 'noble_InRelease').write_text('Date: Wed, 01 Jan 2025 12:00:00 UTC\n')
            with self.assertRaises(ValueError):
                install.release_manifest(root)


class BoatBaseLayoutTests(unittest.TestCase):
    """Boat's base image ships /usr/local owned by its sandbox account, with
    the provider's Go, rustup, Cargo and Ruby installs inside Ubuntu's skeleton
    directories. The fixture reproduces that layout; ownership is simulated
    through a table that Path.lstat reports and the injected chown updates, so
    the tests need not run as root and every re-own call is observable."""
    # A stand-in for Boat's uid 1000; any non-root uid shows the same.
    PROVIDER = install.BOAT_BASE_UID
    SKELETON = ("usr/local", "usr/local/bin", "usr/local/etc", "usr/local/games", "usr/local/include",
                "usr/local/lib", "usr/local/sbin", "usr/local/share", "usr/local/share/man", "usr/local/src",
                "usr/local/share/ca-certificates")
    # Every entry at or beneath these belongs to the provider unless ROOT_INSIDE names it.
    PROVIDER_TREES = ("usr/local/go", "usr/local/rustup", "usr/local/cargo", "usr/local/include/ruby-4.0.0",
                      "usr/local/lib/ruby", "usr/local/share/doc/ruby", "opt/provider")
    PROVIDER_ENTRIES = ("usr/local/man", "usr/local/bin/go", "usr/local/bin/cargo", "usr/local/bin/ruby",
                        "usr/local/lib/libruby.so.4.0", "usr/local/lib/provider-opt",
                        "usr/local/share/ca-certificates/provider.crt")
    # A root-owned file inside a provider tree goes with the tree.
    ROOT_INSIDE = ("usr/local/include/ruby-4.0.0/ruby/config.h",)
    # What the layout step removes, grouped as the inventory records it.
    REMOVED = {
        "/usr/local/bin/cargo": 1, "/usr/local/bin/go": 1, "/usr/local/bin/ruby": 1,
        "/usr/local/cargo": 3, "/usr/local/go": 5,
        "/usr/local/include/ruby-4.0.0": 5,
        "/usr/local/lib/libruby.so.4.0": 1, "/usr/local/lib/provider-opt": 1, "/usr/local/lib/ruby": 7,
        "/usr/local/rustup": 5,
        "/usr/local/share/ca-certificates": 1, "/usr/local/share/doc": 2,
    }

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.image = Path(os.path.realpath(temp.name), "image")
        directories = ("usr/local/go/bin", "usr/local/go/src", "usr/local/rustup/toolchains/stable/bin",
                       "usr/local/cargo/bin", "usr/local/include/ruby-4.0.0/ruby",
                       "usr/local/lib/ruby/4.0.0/x86_64-linux", "usr/local/share/doc/ruby", "opt/provider",
                       # Ours, root-owned, already present before the step.
                       "usr/bin", "usr/local/lib/git/2.53.0/bin", "home/ubuntu/.bun/bin", "home/ubuntu/.rustup",
                       "srv/build/cargo-home/bin")
        for relative in self.SKELETON + directories:
            (self.image / relative).mkdir(parents=True, exist_ok=True)
        for relative in ("usr/local/go/bin/go", "usr/local/go/src/main.go", "usr/local/cargo/bin/cargo",
                         "usr/local/rustup/toolchains/stable/bin/rustc", "usr/local/include/ruby-4.0.0/ruby.h",
                         "usr/local/include/ruby-4.0.0/ruby/version.h", "usr/local/include/ruby-4.0.0/ruby/config.h",
                         "usr/local/lib/ruby/4.0.0/set.rb", "usr/local/lib/ruby/4.0.0/x86_64-linux/rbconfig.rb",
                         "usr/local/lib/ruby/4.0.0/ruby", "usr/local/lib/ruby/4.0.0/gem",
                         "usr/local/lib/libruby.so.4.0", "usr/local/share/ca-certificates/provider.crt",
                         "usr/local/share/doc/ruby/README", "opt/provider/tool",
                         "usr/local/bin/motor-runner", "usr/local/lib/git/2.53.0/bin/git", "usr/bin/python3"):
            (self.image / relative).write_text("#!/bin/sh\n")
            (self.image / relative).chmod(0o755)
        (self.image / "usr/local/man").symlink_to("share/man")
        (self.image / "usr/local/bin/git").symlink_to("../lib/git/2.53.0/bin/git")
        # Provider command links into its trees, absolute and relative.
        (self.image / "usr/local/bin/go").symlink_to("/usr/local/go/bin/go")
        (self.image / "usr/local/bin/cargo").symlink_to("../cargo/bin/cargo")
        (self.image / "usr/local/bin/ruby").symlink_to("../lib/ruby/4.0.0/ruby")
        # Root-owned links into provider trees, in and outside /usr/local.
        (self.image / "usr/local/bin/gem").symlink_to("../lib/ruby/4.0.0/gem")
        (self.image / "usr/bin/go").symlink_to("/usr/local/go/bin/go")
        # Links out of /usr/local to a provider directory elsewhere: the
        # provider's own link is removed as a link, root's stays, and neither
        # leads the walk to opt/provider.
        (self.image / "usr/local/lib/provider-opt").symlink_to("../../../opt/provider")
        (self.image / "usr/local/share/opt-view").symlink_to("../../../opt/provider")
        (self.image / "bin").symlink_to("usr/bin")
        # Boat's /usr/local/share is also group-writable in this fixture.
        (self.image / "usr/local/share").chmod(0o775)
        self.owners = {}
        for path in self.entries():
            relative = str(path.relative_to(self.image))
            provider = (relative in self.SKELETON + self.PROVIDER_ENTRIES or any(
                relative == tree or relative.startswith(tree + "/") for tree in self.PROVIDER_TREES))
            self.owners[path] = (self.PROVIDER, self.PROVIDER) if provider and relative not in self.ROOT_INSIDE else (0, 0)
        self.chowned = []

    def entries(self):
        """Every path in the fixture, without following symlinks."""
        for directory, directories, files in os.walk(self.image):
            for name in ["", *directories, *files]:
                yield Path(directory, name) if name else Path(directory)

    def snapshot(self):
        return {path: path.lstat().st_mode for path in self.entries()}

    @staticmethod
    def inode(path):
        """The key lstat and lchown act on: every directory above the last
        component resolved, the last component itself not, so a path reached
        through a symlinked directory reports the owner of what it reaches."""
        path = Path(path)
        return Path(os.path.realpath(path.parent), path.name)

    def chown(self, path, uid, gid):
        self.chowned.append(Path(path))
        self.owners[self.inode(path)] = (uid, gid)

    def simulated(self):
        real = Path.lstat

        def lstat(path):
            metadata = real(path)
            uid, gid = self.owners.get(self.inode(path), (0, 0))
            return Mock(st_uid=uid, st_gid=gid, st_mode=metadata.st_mode)

        return patch.object(Path, "lstat", lstat)

    def check(self):
        # The report keeps only the first few offenders, in directory walk
        # order, which differs between filesystems; ask for all of them so a
        # named offender is found wherever the walk meets it.
        with self.simulated():
            return install.root_paths.check(install.root_paths.BOAT_ROOTS, self.image, 0,
                                            limit=None)

    def apply(self, **options):
        with self.simulated():
            return install.boat_base_layout(self.image, chown=self.chown, **options)

    def test_build_refuses_before_the_layout_step_and_passes_after_it(self):
        before = self.check()
        self.assertGreater(before["offender_count"], 0)
        for offender in ("usr/local/go", "usr/local/include/ruby-4.0.0", "usr/local/bin/ruby"):
            self.assertIn(f"{self.image}/{offender}: uid={self.PROVIDER} gid={self.PROVIDER}", before["offenders"])
        with self.simulated(), self.assertRaisesRegex(ValueError, "root executes from are not owned by uid 0"):
            install.root_paths.require(install.root_paths.BOAT_ROOTS, self.image, 0)
        report = self.apply()
        after = self.check()
        self.assertEqual((after["offender_count"], after["offenders"]), (0, []))
        with self.simulated():
            install.root_paths.require(install.root_paths.BOAT_ROOTS, self.image, 0)
        self.assertEqual(report["removed"], self.REMOVED)
        self.assertEqual(report["removed_entries"], sum(self.REMOVED.values()))
        for tree in self.PROVIDER_TREES[:-1] + self.PROVIDER_ENTRIES[1:]:
            self.assertFalse(os.path.lexists(self.image / tree), tree)
        self.assertFalse(os.path.lexists(self.image / self.ROOT_INSIDE[0]))
        # Root's links into removed entries go too; the provider's own links
        # were removed as entries and are not listed twice.
        self.assertEqual([(row["path"], row["uid"]) for row in report["unlinked"]],
                         [("/usr/local/bin/gem", 0), ("/usr/bin/go", 0)])
        # Our own installs beside the provider's survive untouched.
        for kept in ("usr/local/bin/motor-runner", "usr/local/bin/git", "usr/local/lib/git/2.53.0/bin/git",
                     "usr/local/share/doc", "usr/local/man", "usr/bin/python3"):
            self.assertTrue(os.path.lexists(self.image / kept), kept)
        self.assertEqual(stat.S_IMODE((self.image / "usr/local/share").lstat().st_mode), 0o755)

    def test_only_the_skeleton_is_re_owned(self):
        report = self.apply()
        named = {self.image / name.lstrip("/") for name in install.BOAT_BASE_USR_LOCAL["skeleton"]}
        self.assertTrue(set(self.chowned) <= named)
        self.assertEqual(len(self.chowned), len(set(self.chowned)))
        present = {path for path in named if os.path.lexists(path)}
        self.assertEqual(set(self.chowned), present)
        self.assertEqual({self.image / row["path"].lstrip("/") for row in report["reowned"]}, present)
        self.assertEqual(sorted(report["absent"]), ["/usr/local/lib/python3.12", "/usr/local/lib/python3.12/dist-packages"])

    def test_provider_entries_go_at_any_depth_while_unsafe_root_entries_still_fail_the_check(self):
        # The layout step removes what the sandbox account owns, wherever it is
        # under /usr/local. This root-owned directory is not a skeleton path,
        # so the step leaves its group-write bit set and the root-owned path
        # check must still report it by name.
        deep = self.image / "usr/local/lib/git/2.53.0/bin/provider-extra"
        deep.write_text("")
        self.owners[deep] = (self.PROVIDER, self.PROVIDER)
        writable = self.image / "usr/local/share/root-writable"
        writable.mkdir()
        writable.chmod(0o777)
        report = self.apply()
        self.assertFalse(os.path.lexists(deep))
        self.assertEqual(report["removed"]["/usr/local/lib/git"], 1)
        self.assertNotIn(writable, self.chowned)
        self.assertEqual(self.check()["offenders"], [f"{writable}: mode=0777 (group or other may write)"])

    def test_the_walk_never_follows_symlinks(self):
        self.apply()
        # Both links lead to a provider directory outside /usr/local; the
        # provider's link is removed as a link and root's stays, while the
        # directory they point at, and the provider file in it, are untouched.
        self.assertFalse(os.path.lexists(self.image / "usr/local/lib/provider-opt"))
        self.assertTrue((self.image / "usr/local/share/opt-view").is_symlink())
        self.assertTrue((self.image / "opt/provider/tool").is_file())

    def test_an_unexpected_owner_refuses_before_anything_changes(self):
        cases = (("usr/local/lib", "refusing to re-own /usr/local/lib: owned by uid 4321", 4321),
                 ("usr/local/lib/ruby/4.0.0/set.rb", "refusing to remove /usr/local/lib/ruby/4.0.0/set.rb: "
                  "owned by uid 1001", 1001),
                 ("usr/local/lib/git/2.53.0/bin/git", "refusing to leave in place "
                  "/usr/local/lib/git/2.53.0/bin/git: owned by uid 1001", 1001))
        for relative, message, uid in cases:
            with self.subTest(relative):
                before = self.snapshot()
                saved = self.owners[self.image / relative]
                self.owners[self.image / relative] = (uid, uid)
                with self.assertRaisesRegex(ValueError, message):
                    self.apply()
                self.assertEqual(self.chowned, [])
                self.assertEqual(self.snapshot(), before)
                self.owners[self.image / relative] = saved

    def test_a_removal_that_would_take_the_running_recipe_refuses_first(self):
        # As if the recipe ran on the provider's interpreter, whose standard
        # library sits in a tree the step would remove.
        stdlib = self.image / "usr/local/lib/ruby/4.0.0"
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, f"{stdlib} is among them and this recipe runs from it"):
            self.apply(protected={stdlib})
        self.assertEqual((self.chowned, self.snapshot()), ([], before))
        running = install.running_paths()
        self.assertIn(Path(os.path.realpath(sys.executable)), running)
        self.assertIn(install.ROOT, running)

    def test_reapplying_changes_nothing(self):
        self.apply()
        self.chowned.clear()
        again = self.apply()
        self.assertEqual((again["removed"], again["removed_entries"], again["unlinked"], again["reowned"],
                          self.chowned), ({}, 0, [], [], []))

    def test_the_recipe_settles_the_layout_before_installing_and_checks_after(self):
        main = inspect.getsource(install.main)
        step = main.index("boat_base_layout()")
        self.assertLess(main.index("target_directories()"), step)
        for later in ("tool_home()", "tools(manifest)", "service_preflight()",
                      "root_paths.require(root_paths.BOAT_ROOTS)"):
            self.assertLess(step, main.index(later), later)
        self.assertIn("boat_base_layout=base_layout", main)


if __name__ == "__main__":
    unittest.main()
