"""Portable tests for the live remote compile-cache check.

They prove the argument handling, the contract shapes, the modes and the
redaction. They cannot prove a Boat sandbox's behaviour: the live section is an
operator check against a prepared image.
"""
import argparse
import contextlib
import io
import json
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
        self.assertIn('cargo build --offline --locked', command)
        self.assertIn('"$RUSTC_WRAPPER" --show-stats --stats-format=json', command)


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

    def test_remote_hits_read_sccache_level_counters(self):
        self.assertEqual(cache_live.remote_hits({'s3_hits': 3, 's3_misses': 9, 'compile_requests': 12}), 3)
        self.assertEqual(cache_live.remote_hits({'s3': {'hits': 2}}), None)
        self.assertEqual(cache_live.remote_hits({'compile_requests': 12}), None)
        self.assertEqual(cache_live.remote_hits({'s3_hits': True}), None)
        self.assertIsNone(cache_live.remote_hits(None))


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
