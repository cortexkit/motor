"""Portable tests for the live remote compile-cache check.

They prove the argument handling, the contract shapes, the modes and the
redaction. They cannot prove a Boat sandbox's behaviour: the live section is an
operator check against a prepared image.
"""
import argparse
import contextlib
import io
import json
import subprocess
import tempfile
from typing import Any
from pathlib import Path
import sys
import unittest
from unittest.mock import MagicMock, patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
try:
    import cache_live
finally:
    sys.path.pop(0)

# A fake pair in the contract's shape. These strings are not credentials.
FAKE = {'generation': 'gen-7', 'tenant': 'acct-01KXJJHK4V6DB42W1YAN2XD9QN',
        'endpoint': 'https://store.example.test', 'region': 'de', 'bucket': 'motor-cache',
        'prefix': 'tenant/acct-01KXJJHK4V6DB42W1YAN2XD9QN/', 'storage_class': 'STANDARD',
        'enabled': True, 'access_key_id': 'FAKEACCESSKEYID', 'secret_access_key': 'FAKESECRETKEY'}

# sccache 0.18 serializes ServerInfo; counts and adv_counts describe the same hits.
SERVER_INFO = {'cache_location': 's3, name: motor-cache, prefix: tenant/test/',
               'stats': {'cache_hits': {'counts': {'Rust': 2}, 'adv_counts': {'rust rlib': 2}},
                         'multi_level': None}}


def write_inputs(root, config=None, key=None):
    root.mkdir(parents=True, exist_ok=True)
    (root / 'config.json').write_text(json.dumps(config or {name: FAKE[name] for name in cache_live.CONFIG_MEMBERS}))
    (root / 'read-key').write_text(json.dumps(key or {
        name: FAKE[name] for name in cache_live.KEY_MEMBERS}))
    return root


class InputTests(unittest.TestCase):
    def test_contract_shapes_are_accepted(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            root = write_inputs(Path(tmp))
            config, key = cache_live.read_inputs(root / 'config.json', root / 'read-key')
        self.assertEqual(config['generation'], key['generation'])
        self.assertEqual(config['prefix'], 'tenant/acct-01KXJJHK4V6DB42W1YAN2XD9QN/')

    def test_an_extra_member_a_generation_mismatch_or_a_non_boolean_switch_is_refused(self):
        import tempfile
        cases = [
            {'extra': 'x'},
            {'generation': 'another'},
        ]
        with tempfile.TemporaryDirectory() as tmp:
            for case in cases:
                with self.subTest(case=case):
                    root = write_inputs(Path(tmp) / 'case')
                    if 'extra' in case:
                        write_inputs(root, config={**json.loads((root / 'config.json').read_text()), 'extra': 'x'})
                    else:
                        write_inputs(root, config={**json.loads((root / 'config.json').read_text()), **case})
                    with self.assertRaises(ValueError):
                        cache_live.read_inputs(root / 'config.json', root / 'read-key')
            root = write_inputs(Path(tmp) / 'switch')
            write_inputs(root, config={**json.loads((root / 'config.json').read_text()), 'enabled': 'yes'})
            with self.assertRaises(ValueError):
                cache_live.read_inputs(root / 'config.json', root / 'read-key')

    def test_the_report_description_never_contains_key_material(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            root = write_inputs(Path(tmp))
            described = json.dumps(cache_live.describe(root / 'config.json'))
        self.assertIn('store.example.test', described)
        self.assertNotIn('FAKESECRETKEY', described)
        self.assertNotIn('FAKEACCESSKEYID', described)


class CommandTests(unittest.TestCase):
    def test_install_uses_the_contract_paths_and_modes(self):
        command = cache_live.install_command()
        self.assertIn('/run/motor/cache', command)
        self.assertIn('-m 0400', command)
        self.assertIn('-m 0444', command)
        self.assertIn('-m 0700', command)
        self.assertIn('-o root -g root', command)
        # The key is installed from the same staging directory the probe removes.
        self.assertIn(cache_live.STAGING + '/read-key', command)

    def test_removal_covers_every_copy_the_check_makes(self):
        removal = cache_live.remove_command()
        for path in (cache_live.READ_KEY, cache_live.CONFIG, cache_live.STAGING + '/read-key',
                     cache_live.STAGING + '/config.json'):
            self.assertIn(path, removal)
        absence = cache_live.absent_command()
        for path in (cache_live.READ_KEY, cache_live.CONFIG, cache_live.STAGING + '/read-key',
                     cache_live.STAGING + '/config.json'):
            self.assertIn('test ! -e ' + path, absence)

    def test_the_job_asks_its_own_wrapper_for_statistics(self):
        command = cache_live.job_command()
        self.assertIn('--offline', command)
        self.assertIn('--locked', command)
        self.assertIn('RUSTC_WRAPPER', command)
        self.assertIn('--show-stats', command)

    def test_job_command_builds_offline_and_emits_one_report_from_its_wrapper(self):
        import os
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cargo = root / 'cargo'
            cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$CALLS"\n'
                             'test -f Cargo.toml && test -f src/main.rs || exit 9\n'
                             'echo cargo-chatter\n')
            wrapper = root / 'sccache'
            wrapper.write_text('#!/bin/sh\necho \'' + json.dumps(SERVER_INFO) + '\'\n')
            cargo.chmod(0o755)
            wrapper.chmod(0o755)
            result = subprocess.run(['sh', '-c', cache_live.job_command()], cwd=root,
                                    env={**os.environ, 'PATH': str(root) + os.pathsep + os.environ['PATH'],
                                         'CALLS': str(root / 'calls'), 'RUSTC_WRAPPER': str(wrapper),
                                         'SCCACHE_S3_RW_MODE': 'READ_ONLY'},
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertEqual(report['wrapper'], str(wrapper))
            self.assertEqual(report['rw_mode'], 'READ_ONLY')
            self.assertEqual(report['stats'], SERVER_INFO)
            self.assertEqual((root / 'calls').read_text().splitlines(),
                             ['generate-lockfile --offline', 'build --offline --locked'])
            self.assertIn('cargo-chatter', result.stderr)


class VerdictTests(unittest.TestCase):
    def test_zero_or_missing_remote_hits_fail(self):
        report = {'wrapper': '/motor-tools/sccache', 'rw_mode': 'READ_ONLY', 'recache': False,
                  'tools_mounted': True, 'gateway_mounted': True}
        self.assertTrue(cache_live.passed(cache_live.MODES, report, 1, True) == [])
        self.assertTrue(any('0 remote sccache hits' in problem
                            for problem in cache_live.passed(cache_live.MODES, report, 0, True)))
        self.assertTrue(any('no remote storage level' in problem
                            for problem in cache_live.passed(cache_live.MODES, report, None, True)))
        for name, value in (('wrapper', None), ('rw_mode', 'READ_WRITE'), ('recache', True),
                            ('tools_mounted', False), ('gateway_mounted', False)):
            with self.subTest(name=name):
                self.assertTrue(cache_live.passed(cache_live.MODES, {**report, name: value}, 4, True))

    def test_a_wrong_mode_or_a_leftover_key_fails(self):
        report = {'wrapper': '/motor-tools/sccache', 'rw_mode': 'READ_ONLY', 'recache': False,
                  'tools_mounted': True, 'gateway_mounted': True}
        self.assertTrue(cache_live.passed({**cache_live.MODES, 'read-key': '444'}, report, 4, True))
        self.assertTrue(cache_live.passed(cache_live.MODES, report, 4, False))

    def test_remote_hits_use_pinned_server_info_without_double_counting(self):
        self.assertEqual(cache_live.remote_hits(SERVER_INFO), 2)
        self.assertEqual(cache_live.remote_hits({**SERVER_INFO, 'cache_location': 'Local disk: /cache'}), None)
        for counts, expected in (({}, 0), ({'Rust': 0}, 0), ({'Rust': True}, None),
                                 ({'Rust': -1}, None), ({'Rust': '2'}, None)):
            info = {**SERVER_INFO, 'stats': {'cache_hits': {'counts': counts}, 'multi_level': None}}
            self.assertEqual(cache_live.remote_hits(info), expected)
        for info in (None, {}, {'s3_hits': 2}, {'stats': {'cache_hits': {'counts': {'Rust': 9}}}}):
            self.assertIsNone(cache_live.remote_hits(info))

    def test_multilevel_hits_exclude_the_disk_level(self):
        disk = {'name': 'L0 (disk)', 'location': 'Local disk: /cache', 'hits': 9}
        s3 = {'name': 'L1 (s3)', 'location': SERVER_INFO['cache_location'], 'hits': 2}
        def info(levels):
            return {'stats': {'cache_hits': {'counts': {'Rust': 11}}, 'multi_level': levels}}
        self.assertEqual(cache_live.remote_hits(info([disk, s3])), 2)
        self.assertEqual(cache_live.remote_hits(info([disk, {**s3, 'hits': 0}])), 0)
        self.assertIsNone(cache_live.remote_hits(info([disk])))
        self.assertIsNone(cache_live.remote_hits(info([{**s3, 'hits': True}])))


class ArgumentTests(unittest.TestCase):
    def test_the_inputs_directory_is_runner_host_only(self):
        parser = argparse.ArgumentParser()
        parser.add_argument('--runner-host', action='store_true')
        parser.add_argument('--cache-inputs', type=Path)
        self.assertTrue(parser.parse_args(['--runner-host']).cache_inputs is None)
        self.assertEqual(parser.parse_args(['--runner-host', '--cache-inputs', '/x']).cache_inputs, Path('/x'))


class SectionTests(unittest.TestCase):
    def host(self, inputs=None):
        runner = MagicMock()
        runner.data = {'checks': [], 'phases': []}
        runner.probe.cache_inputs = inputs
        host = probe_runner_host().RunnerHost.__new__(probe_runner_host().RunnerHost)
        host.probe = runner.probe
        host.data = runner.data
        host._phase_context = None
        host._sections = []
        host.budget_seconds = None
        host.check = MagicMock(side_effect=lambda name, passed, evidence, status=None, reason=None: host.data['checks'].append(
            {'name': name, 'status': status or ('PASS' if passed else 'FAIL'), 'reason': reason}))
        host.section = MagicMock()
        host.section.return_value.__enter__ = MagicMock()
        host.section.return_value.__exit__ = MagicMock(return_value=False)
        host.phase_evidence = MagicMock()
        host.phase_evidence.return_value.__enter__ = MagicMock()
        host.phase_evidence.return_value.__exit__ = MagicMock(return_value=False)
        return host

    def test_without_the_operator_files_the_section_is_inconclusive_not_a_pass(self):
        host = self.host()
        host.verify_cache_live('bx_1', Path('/tmp'), 'ssh-ed25519 AAAA', Path('/bin/true'), Path('/bin/true'))
        self.assertEqual([check['name'] for check in host.data['checks']], ['cache_live.provisioning_supplied'])
        self.assertEqual(host.data['checks'][0]['status'], 'INCONCLUSIVE')
        self.assertIn('did not run', host.data['checks'][0]['reason'])


class ProvisioningTests(unittest.TestCase):
    def host(self):
        module = probe_runner_host()
        probe = MagicMock()
        probe.cache_config = probe.cache_read_key = None
        probe.deadline = module.time.monotonic() + 1800
        host: Any = module.RunnerHost(probe)
        host.route = MagicMock(return_value=['ssh', 'pinned'])
        host.execute = MagicMock(return_value={'stdout': '700 /run/motor/cache\n400 /run/motor/cache/read-key\n444 /run/motor/cache/config.json\n'})
        return module, host

    def test_fresh_and_repeated_uploads_create_private_staging_and_contract_modes(self):
        module, host = self.host()
        host.route.return_value = ['sh', '-c']
        def execute(sid, command):
            result = subprocess.run(['sh', '-c', command.replace('sudo -n ', '').replace('-o root -g root ', '')],
                                    capture_output=True, text=True, check=True)
            return {'stdout': result.stdout}
        host.execute.side_effect = execute
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            inputs = write_inputs(root / 'inputs')
            (inputs / 'config.json').chmod(0o444)
            (inputs / 'read-key').chmod(0o400)
            staging, installed = root / 'staged', root / 'installed'
            with patch.multiple(cache_live, STAGING=str(staging), DIRECTORY=str(installed),
                                CONFIG=str(installed / 'config.json'), READ_KEY=str(installed / 'read-key')):
                for _ in range(2):
                    result = host.install_cache_provisioning('bx', root, 'public', inputs)
                    self.assertEqual(result['modes'], cache_live.MODES)
                    self.assertEqual(staging.stat().st_mode & 0o777, 0o700)
                    self.assertEqual((staging / 'read-key').stat().st_mode & 0o777, 0o600)
                    self.assertEqual((installed / 'read-key').read_bytes(), (inputs / 'read-key').read_bytes())
                    (staging / 'config.json').chmod(0o444)
                    (staging / 'read-key').chmod(0o400)

    def test_staging_is_reset_before_readonly_inputs_are_streamed_and_installed(self):
        module, host = self.host()
        events = []
        host.execute.side_effect = lambda sid, command: events.append(command) or {
            'stdout': '700 /run/motor/cache\n400 /run/motor/cache/read-key\n444 /run/motor/cache/config.json\n'}
        def sent(argv, **kwargs):
            events.append(argv[-1])
            self.assertEqual(kwargs['stdin'].read(1), b'{')
            return subprocess.CompletedProcess(argv, 0, b'', b'')
        with tempfile.TemporaryDirectory() as tmp:
            root = write_inputs(Path(tmp))
            (root / 'config.json').chmod(0o444)
            (root / 'read-key').chmod(0o400)
            with patch.object(module.subprocess, 'run', side_effect=sent):
                installed = host.install_cache_provisioning('bx', Path(tmp), 'public', root)
        self.assertIn('rm -', events[0])
        self.assertIn(cache_live.STAGING, events[0])
        self.assertIn('install -d -m 0700', events[0])
        self.assertIn('cat > ' + cache_live.STAGING + '/config.json', events[1])
        self.assertEqual(installed['modes'], cache_live.MODES)

    def test_failed_upload_reports_ssh_stderr_without_echoing_the_input(self):
        module, host = self.host()
        with tempfile.TemporaryDirectory() as tmp:
            root = write_inputs(Path(tmp))
            with patch.object(module.subprocess, 'run', return_value=subprocess.CompletedProcess(
                    [], 1, b'', b'sh: /tmp/ck-motor-cache/config.json: Permission denied\n')):
                with self.assertRaisesRegex(ValueError, 'config.json: Permission denied') as error:
                    host.install_cache_provisioning('bx', Path(tmp), 'public', root)
        self.assertNotIn('FAKESECRETKEY', str(error.exception))

    def test_run_validates_the_actual_inputs_before_creating_a_sandbox(self):
        module, host = self.host()
        with tempfile.TemporaryDirectory() as tmp:
            root = write_inputs(Path(tmp))
            (root / 'read-key').unlink()
            host.probe.cache_inputs = root
            host._run = MagicMock()
            with self.assertRaisesRegex(ValueError, 'cache provisioning file refused'):
                host.run()
        host._run.assert_not_called()

    def test_live_job_uses_the_motor_client_and_parses_both_output_layers(self):
        module, host = self.host()
        host.probe.deadline = module.time.monotonic() + 1800
        host.execute.return_value = {'stdout': '4\n'}
        report = {'stats': SERVER_INFO, 'wrapper': '/motor-tools/sccache',
                  'rw_mode': 'READ_ONLY', 'recache': False, 'tools_mounted': True, 'gateway_mounted': True}
        outer = {'code': 0, 'stdout': json.dumps(report), 'stderr': '',
                 'terminal': {'outcome': {'type': 'exit', 'code': 0}}}
        with tempfile.TemporaryDirectory() as tmp, \
             patch.object(module, 'tunnel') as tunnel, \
             patch.object(module, 'process', return_value={'exit_code': 0, 'stdout': json.dumps(outer)}) as process:
            tunnel.return_value.__enter__.return_value = '127.0.0.1:19443'
            result = host.cached_job('bx', Path(tmp), 'public', Path('repo'), Path('client'), cache_live.job_command())
        self.assertEqual(result['result'], outer)
        self.assertEqual(json.loads(result['result']['stdout']), report)
        args = process.call_args.args[0]
        self.assertEqual(args[:3], ['client', '127.0.0.1:19443', 'repo'])
        self.assertEqual(args[-1], '4')

    def test_verification_needs_only_the_pair_and_rejects_a_failed_job_even_with_hits(self):
        module, host = self.host()
        report = {'stats': SERVER_INFO, 'wrapper': '/motor-tools/sccache',
                  'rw_mode': 'READ_ONLY', 'recache': False, 'tools_mounted': True, 'gateway_mounted': True}
        for code in (0, 1):
            with self.subTest(code=code), tempfile.TemporaryDirectory() as tmp:
                root = write_inputs(Path(tmp))
                host.data['checks'] = []
                host.probe.cache_inputs = root
                host.install_cache_provisioning = MagicMock(return_value={'modes': cache_live.MODES})
                host.remove_cache_provisioning = MagicMock(return_value=True)
                host.cached_job = MagicMock(return_value={'result': {
                    'code': code, 'stdout': json.dumps(report), 'stderr': '',
                    'terminal': {'outcome': {'type': 'exit', 'code': code}}}})
                host.wait_cache_admission = MagicMock(return_value=True)
                host.verify_cache_live('bx', Path(tmp), 'public', Path('repo'), Path('client'))
                checks = {row['name']: row['status'] for row in host.data['checks']}
                self.assertEqual(checks['cache_live.job'], 'PASS' if code == 0 else 'FAIL')
                self.assertEqual(checks['cache_live.remote_hits'], 'PASS')
                self.assertEqual(host.data['cache_live']['remote_hits'], 2)
                host.remove_cache_provisioning.assert_called_once()

    def test_failed_install_still_removes_every_provisioning_copy(self):
        module, host = self.host()
        with tempfile.TemporaryDirectory() as tmp:
            host.probe.cache_inputs = write_inputs(Path(tmp))
            host.install_cache_provisioning = MagicMock(side_effect=ValueError('install failed'))
            host.remove_cache_provisioning = MagicMock(return_value=True)
            with self.assertRaisesRegex(ValueError, 'install failed'):
                host.verify_cache_live('bx', Path(tmp), 'public', Path('repo'), Path('client'))
        host.remove_cache_provisioning.assert_called_once()

    def test_unadmitted_cache_never_builds_and_still_removes_the_key(self):
        module, host = self.host()
        with tempfile.TemporaryDirectory() as tmp:
            host.probe.cache_inputs = write_inputs(Path(tmp))
            host.install_cache_provisioning = MagicMock(return_value={'modes': cache_live.MODES})
            host.wait_cache_admission = MagicMock(return_value=False)
            host.cached_job = MagicMock()
            host.remove_cache_provisioning = MagicMock(return_value=True)
            host.verify_cache_live('bx', Path(tmp), 'public', Path('repo'), Path('client'))
        host.cached_job.assert_not_called()
        host.remove_cache_provisioning.assert_called_once()
        self.assertEqual(host.data['checks'][-1]['status'], 'FAIL')

    def test_watcher_admission_is_observed_before_building_and_has_a_deadline(self):
        module, host = self.host()
        ready = {'wrapper': '/motor-tools/sccache', 'rw_mode': 'READ_ONLY', 'recache': False,
                 'tools_mounted': True, 'gateway_mounted': True}
        def result(report):
            return {'result': {'code': 0, 'stdout': json.dumps(report),
                               'terminal': {'outcome': {'type': 'exit', 'code': 0}}}}
        clock = [0.0]
        host.data['cache_live'] = {}
        host.cached_job = MagicMock(side_effect=[result({}), result(ready)])
        with patch.object(module.time, 'monotonic', side_effect=lambda: clock[0]), \
             patch.object(module.time, 'sleep', side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)):
            self.assertTrue(host.wait_cache_admission('bx', Path('.'), 'public', Path('repo'), Path('client')))
        self.assertEqual(host.cached_job.call_count, 2)
        self.assertEqual(clock[0], 1)
        self.assertEqual(host.cached_job.call_args.kwargs['deadline'], cache_live.ADMISSION_SECONDS)
        host.cached_job = MagicMock(return_value=result({}))
        clock[0] = 0
        with patch.object(module.time, 'monotonic', side_effect=lambda: clock[0]), \
             patch.object(module.time, 'sleep', side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)):
            self.assertFalse(host.wait_cache_admission('bx', Path('.'), 'public', Path('repo'), Path('client')))
        self.assertEqual(clock[0], cache_live.ADMISSION_SECONDS)
        self.assertEqual(host.data['checks'][-1]['status'], 'FAIL')


def probe_runner_host():
    """The probe's runner-host module, imported the way the probe imports it, so
    its module-level constants are available to the tests."""
    sys.path.insert(0, str(HERE))
    try:
        import runner_host
    finally:
        sys.path.pop(0)
    return runner_host


if __name__ == '__main__':
    unittest.main()
