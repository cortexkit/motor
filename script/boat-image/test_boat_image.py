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
        self.assertEqual(self.pins["nats"], "2.15.0")
        self.assertIn("libsqlite3-dev", self.pins["packages"])
        self.assertIn("python3", self.pins["packages"])
        self.assertEqual(set(self.pins["rust_toolchains"]), {"1.89.0", "1.91.0", "1.96.0", "1.99.0"})

    def test_wrong_input_hash_is_refused(self):
        pins = copy.deepcopy(self.pins)
        pins["runner_inputs_sha256"]["script/runner/tools.txt"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "input changed"):
            install.validate(pins)

    def test_unpinned_versions_and_endpoint_port_are_refused(self):
        for key, value in (("bun", "latest"), ("probe_port", 9443)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                install.validate(dict(self.pins, **{key: value}))

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
        # The inotify path unit stayed armed on Boat's retired FUSE /etc.
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
        for unit in ('motor-boat-restored.service','motor-boat.service','motor-boat-host.service','motor-boat-identity.service','motor-boat-bridge.service'):
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
