"""Portable tests for operator-only cache mode and its cleanup/credential boundary."""
import argparse
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
try:
    import cache_bench
    spec = importlib.util.spec_from_file_location('probe_cache_test_subject', HERE / 'probe.py')
    if spec is None or spec.loader is None:
        raise ImportError('cannot load Boat probe')
    probe = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(probe)
finally:
    sys.path.pop(0)


def args(**changes):
    values = dict(cache_bench=True, cache_image='prepared-image', cache_bundle=Path('/bundle'),
                  cache_bundle_sha256='a' * 64, cache_backends=Path('/operator/backends.json'),
                  cache_prefix='cache-bench/fixture/', max_minutes=30)
    values.update(changes)
    return SimpleNamespace(**values)


class ArgumentTests(unittest.TestCase):
    def test_options_must_belong_to_cache_mode_and_image_digest_prefix_are_validated(self):
        cache_bench.validate(argparse.ArgumentParser(), args())
        for changes in ({'cache_bench': False}, {'cache_image': None}, {'cache_image': '../image'},
                        {'cache_bundle_sha256': 'main'}, {'cache_prefix': '/'}, {'max_minutes': 60},
                        {'cache_baseline': True}, {'cache_repositories': Path('/manifest')},
                        {'cache_samples': 2}):
            with self.subTest(changes=changes), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                cache_bench.validate(argparse.ArgumentParser(), args(**changes))

    def test_cache_measurement_incomplete_fails_shared_verdict(self):
        data = {'measurements_complete': True, 'cache_bench': {'complete': False}}
        self.assertFalse(probe.verdict(data)['passed'])
        data['cache_bench']['complete'] = True
        self.assertTrue(probe.verdict(data)['passed'])

    def test_main_dispatches_cache_mode_and_always_calls_shared_cleanup_on_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            argv = ['probe.py', '--key-stdin', '--output', str(Path(tmp) / 'out'), '--cache-bench',
                    '--cache-image', 'prepared-image', '--cache-bundle', '/bundle',
                    '--cache-bundle-sha256', 'a' * 64, '--cache-backends', '/file',
                    '--cache-prefix', 'cache-bench/fixture/']
            with patch.object(sys, 'argv', argv), patch.object(sys, 'stdin', io.StringIO('boat-fixture')), \
                    patch.object(cache_bench, 'run', side_effect=RuntimeError('fixture failure')) as run, \
                    patch.object(probe, 'cleanup') as cleanup, patch.object(probe.signal, 'signal'), \
                    patch.object(probe, 'render'), contextlib.redirect_stdout(io.StringIO()):
                status = probe.main()
            self.assertEqual(status, 1)
            self.assertEqual(run.call_count, 1)
            self.assertEqual(cleanup.call_count, 1)


class SSHTests(unittest.TestCase):
    def exercise(self, failed=False, multi=False, timed_out=False):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            credential = root / 'creds'
            credential.write_text(cache_bench.bench.ENDPOINT + '\nACCESS_FIXTURE\nSECRET_FIXTURE\n')
            credential.chmod(0o600)
            bundle = root / 'bundle'
            bundle.write_bytes(b'offline assets fixture')
            manifest = root / 'backends.json'
            manifest.write_text(json.dumps([{'name': 'fixture', 'credentials': str(credential),
                                             'bucket': 'motor-cache-test', 'region': 'de', 'addressing': 'path'}]))
            options = args(cache_bundle=bundle, cache_bundle_sha256=cache_bench.bench.sha(bundle),
                           cache_backends=manifest)
            repositories = [dict(name=name, bundle=str(bundle), bundle_sha256=options.cache_bundle_sha256,
                                 prefix='cache-bench/fixture-' + name + '/') for name in ('motor', 'prefrontal')]
            if multi:
                options.cache_repositories = root / 'repositories.json'
                options.cache_profile = 'hourly'
                options.cache_baseline = True
                options.cache_samples = 2
            runner = SimpleNamespace(data={}, results_path=root / 'results.json', deadline=time.monotonic() + 1800,
                                     size='large', checkpoint=MagicMock(), wait_hydrated=MagicMock(),
                                     create=MagicMock(return_value=({'id': 'bx_fixture', 'vcpu': 8, 'memoryGB': 16}, {})))
            calls = []

            def execute(argv, **kwargs):
                calls.append((argv, kwargs))
                stdout, code = b'', 0
                if argv[0] == 'ssh-keygen':
                    path = Path(argv[-1])
                    path.write_text('private-fixture')
                    path.with_suffix('.pub').write_text('ssh-ed25519 Zml4dHVyZQ==')
                elif ' --run ' in argv[-1]:
                    if timed_out:
                        raise subprocess.TimeoutExpired(argv, kwargs['timeout'])
                    code = int(failed)
                elif argv[-1] == 'cat /dev/shm/motor-cache-results/results.json':
                    stdout = json.dumps({'complete': not failed, 'runs': [], 'backends': [
                        dict(name='fixture', placement='B', status='passed', result={})]}).encode()
                return subprocess.CompletedProcess(argv, code, stdout, b'')

            authorization = {'hostKey': 'ssh-ed25519 Zml4dHVyZQ==', 'sshEndpoint': '192.0.2.1:22'}
            with patch.object(cache_bench.subprocess, 'run', side_effect=execute), \
                    patch.object(cache_bench.bench, 'load_repositories', return_value=repositories), \
                    patch.object(cache_bench.SSHReach, 'authorize', return_value=authorization):
                if failed or timed_out:
                    with self.assertRaisesRegex(cache_bench.ProbeError, 'cache benchmark failed'):
                        cache_bench.run(runner, options)
                else:
                    cache_bench.run(runner, options)
            self.assertTrue(runner.data['cache_bench']['credential_file_removed'])
            self.assertEqual(runner.data['cache_bench']['complete'], not (failed or timed_out))
            runner.create.assert_called_once_with('prepared-image')
            runner.wait_hydrated.assert_called_once_with('bx_fixture')
            self.assertFalse(list(root.glob('.cache-ssh-*')))
            self.assertNotIn('SECRET_FIXTURE', json.dumps(runner.data))
            commands = [call[0][-1] for call in calls if call[0][0] == 'ssh']
            self.assertTrue(any('--placement B' in command for command in commands))
            if multi:
                runs = [command for command in commands if ' --run ' in command]
                self.assertEqual(len(runs), 2)
                self.assertTrue(all('--profile hourly --samples 2 --baseline' in command for command in runs))
                self.assertEqual([e['repository'] for e in runner.data['cache_bench']['result']['backends']], ['motor', 'prefrontal'])
            self.assertEqual(commands[-1], 'rm -f /dev/shm/motor-cache-credentials-fixture')
            for argv, kwargs in calls:
                self.assertNotIn('SECRET_FIXTURE', str(argv))
                self.assertNotIn('SECRET_FIXTURE', str(kwargs.get('env', {})))

    def test_pinned_ssh_uses_stdin_for_credentials_and_removes_file_after_success(self):
        self.exercise()

    def test_failed_benchmark_removes_credentials_and_keeps_partial_result(self):
        self.exercise(failed=True)

    def test_two_repositories_share_one_fresh_sandbox_and_keep_hourly_profile(self):
        self.exercise(multi=True)

    def test_timeout_fetches_partial_results_before_removing_credentials(self):
        self.exercise(timed_out=True)

    def test_invalid_bundle_does_not_create_billable_sandbox(self):
        runner = SimpleNamespace(create=MagicMock())
        with patch.object(cache_bench.bench, 'load_backends', return_value=[]), \
                patch.object(cache_bench.bench, 'sha', return_value='b' * 64):
            with self.assertRaisesRegex(cache_bench.ProbeError, 'bundle digest differs'):
                cache_bench.run(runner, args())
        runner.create.assert_not_called()


if __name__ == '__main__':
    unittest.main()
