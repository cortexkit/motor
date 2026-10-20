#!/usr/bin/env python3
"""Portable recipe integrity tests, not proof of a functioning Boat host."""
import contextlib
import copy
import importlib.util
import io
from importlib.machinery import SourceFileLoader
import json
import sys
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock

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
        self.assertEqual({command[3] for command in commands}, set(manifest['rust_toolchains']))
        for command in commands:
            components = [command[i + 1] for i, arg in enumerate(command) if arg == '--component']
            targets = [command[i + 1] for i, arg in enumerate(command) if arg == '--target']
            if command[3] in ('1.99.0-x86_64-unknown-linux-gnu', 'stable-x86_64-unknown-linux-gnu'):
                self.assertIn('wasm32-unknown-unknown', targets)
                self.assertIn('x86_64-pc-windows-gnu', targets)
                self.assertIn('x86_64-unknown-linux-musl', targets)
            if command[3].startswith('stable-'):
                self.assertNotIn('clippy', components)
                self.assertNotIn('rustfmt', components)
            else:
                self.assertIn('clippy', components)

    def test_recipe_uses_server_bun_home_and_preserves_job_path_order(self):
        unit = (HERE / 'motor-boat.service').read_text()
        self.assertIn('Environment=HOME=/home/ubuntu\n', unit)
        self.assertIn('Environment=PATH=/home/ubuntu/.bun/bin:/srv/build/cargo-home/bin:/usr/local/bin:/usr/bin:/bin:', unit)

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
            names[1]: ('rustup', 'toolchain', 'install', names[1], '--profile', 'minimal', *targets),
        })

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

    def test_recipe_bootstraps_empty_rustup_and_removes_unlisted_toolchains(self):
        manifest = json.loads(install.PARITY_PATH.read_text())
        for listed, removed in (('no installed toolchains', []),
                                ('1.89.0-x86_64-unknown-linux-gnu', ['1.89.0-x86_64-unknown-linux-gnu'])):
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
        self.assertIn("--max-weight @VCPUS@", unit)
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

    def test_storage_watch_withdraws_readiness_and_stays_failed_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence, marker = Path(tmp, 'evidence'), Path(tmp, 'ready')
            evidence.write_text(json.dumps({'identity': [1792, 128, 131]}))
            marker.touch()
            observations = []
            def pause(_):
                observations.append(marker.exists())
                marker.touch()  # A restarting endpoint cannot restore admission.
                if len(observations) == 3:
                    raise InterruptedError('stop watcher')
            with patch.object(start, 'STORAGE_EVIDENCE', evidence), patch.object(start, 'ENDPOINT_READY', marker), \
                 patch.object(start, 'storage_identity', side_effect=[(1792, 128, 131), RuntimeError('view lost'), (1792, 128, 131)]), \
                 self.assertRaises(InterruptedError):
                start.watch_storage(pause=pause)
            self.assertEqual(observations, [True, False, False])

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
        def execv(program, args):
            self.assertEqual(program, '/usr/local/bin/motor-host')
            self.assertEqual(args, [program, '--sandbox', '/etc/motor/sandbox.json'])
            self.assertEqual(dict(start.os.environ), {'PATH':'kept'})
            raise ExecObserved()
        with patch.dict(start.os.environ, {'ASCII_TOKEN':'secret','BOAT_FUTURE':'secret','BOAT_ID':'id','PATH':'kept'}, clear=True), \
             patch.object(start, 'check_storage'), \
             patch.object(sys, 'argv', ['start','--host-exec','--sandbox','/etc/motor/sandbox.json']), patch.object(start.os, 'execv', execv):
            with self.assertRaises(ExecObserved):
                start.main()

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


if __name__ == "__main__":
    unittest.main()
