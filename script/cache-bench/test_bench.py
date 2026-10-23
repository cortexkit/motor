"""Linux fixtures: no Cargo compilation, provider credentials or external services."""
import argparse
import contextlib
from datetime import datetime, timezone
import hashlib
import http.client
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('cache_bench_test_subject', Path(__file__).with_name('bench.py'))
if spec is None or spec.loader is None:
    raise ImportError('cannot load cache benchmark driver')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
PREFIX = 'cache-bench/fixture/'


class _Response:
    status = 200
    def getheaders(self):
        return [('Content-Length', '2')]
    def read(self):
        return b'ok'


class _Connection:
    """Fails the first request sent on each instance that the test marks stale."""
    made = []
    stale = set()
    def __init__(self, host, timeout):
        self.number = len(_Connection.made)
        _Connection.made.append(self)
        self.sent = 0
    def request(self, *args):
        self.sent += 1
        if self.number in _Connection.stale and self.sent > 1:
            raise http.client.RemoteDisconnected('Remote end closed connection without response')
    def getresponse(self):
        return _Response()
    def close(self):
        pass


class TransportRetryTests(unittest.TestCase):
    def client(self):
        _Connection.made, _Connection.stale = [], set()
        return bench.S3('https://s3.de.io.cloud.ovh.net', 'ACCESS', 'SECRET',
                        {'bucket': 'motor-cache-test', 'addressing': 'path', 'region': 'de'})

    def test_reused_connection_closed_while_idle_is_retried_once_on_a_new_one(self):
        s3 = self.client()
        with patch.object(bench.http.client, 'HTTPSConnection', _Connection):
            s3.request('GET', '/motor-cache-test/a')
            _Connection.stale.add(0)
            status, _, data = s3.request('GET', '/motor-cache-test/b')
        self.assertEqual((status, data), (200, b'ok'))
        self.assertEqual(len(_Connection.made), 2)

    def test_failure_on_a_new_connection_is_not_retried_and_names_its_type(self):
        class Refused(_Connection):
            def request(self, *args):
                raise ConnectionRefusedError('refused https://secret-signed-url')
        s3 = self.client()
        with patch.object(bench.http.client, 'HTTPSConnection', Refused):
            with self.assertRaises(bench.BenchError) as raised:
                s3.request('GET', '/motor-cache-test/a')
        self.assertEqual(len(_Connection.made), 1)
        self.assertIn('GET ConnectionRefusedError on a new connection', str(raised.exception))
        self.assertNotIn('secret-signed-url', str(raised.exception))


class ArgumentsTests(unittest.TestCase):
    def parse(self, *argv):
        with contextlib.redirect_stderr(io.StringIO()):
            return bench.parse_args(argv)

    def test_run_and_purge_require_scoped_prefix_and_credential_file(self):
        common = ['--credentials', '/operator/file', '--backend-name', 'ovh-standard', '--region', 'de',
                  '--bucket', 'motor-cache-test', '--addressing', 'path', '--prefix', PREFIX]
        args = self.parse('--run', '--placement', 'B', '--output', '/evidence', *common)
        self.assertEqual(args.credentials, Path('/operator/file'))
        self.assertEqual(self.parse('--purge', *common).prefix, PREFIX)
        for argv in (['--run'], ['--purge'], ['--purge', *common, '--placement', 'A'],
                     ['--run', *common], ['--purge', *common, '--commit', 'a' * 40]):
            with self.subTest(argv=argv), self.assertRaises(SystemExit):
                self.parse(*argv)

    def test_prefix_cannot_cover_bucket_or_escape_namespace(self):
        for prefix in ('', '/', 'cache-bench/', 'other/run/', 'cache-bench/../',
                       'cache-bench/run', 'cache-bench/run/?x', 'cache-bench/a/b/'):
            with self.subTest(prefix=prefix), self.assertRaises(argparse.ArgumentTypeError):
                bench.prefix_arg(prefix)

    def test_prepare_requires_full_commit_not_placeholder_path(self):
        argv = ['--prepare', '--source', '/repo', '--cargo-home', '/seed',
                '--toolchain', '1.99.0', '--bundle', '/assets.tar.gz']
        self.assertEqual(self.parse(*argv, '--commit', 'a' * 40).jobs, 8)
        for commit in ('main', 'HEAD', '/srv/cache-bench/motor', 'a' * 7):
            with self.subTest(commit=commit), self.assertRaises(SystemExit):
                self.parse(*argv, '--commit', commit)
        with self.assertRaises(SystemExit):
            self.parse(*argv, '--commit', 'a' * 40, '--jobs', '0')

    def test_credentials_errors_never_echo_contents(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'credentials'
            path.write_text(bench.ENDPOINT + '\nACCESS_FIXTURE\nSECRET_FIXTURE\n')
            path.chmod(0o600)
            self.assertEqual(bench.credentials(path), (bench.ENDPOINT, 'ACCESS_FIXTURE', 'SECRET_FIXTURE'))
            for mode, contents in ((0o644, path.read_text()), (0o600, 'SECRET_FIXTURE\n')):
                path.chmod(mode)
                path.write_text(contents)
                with self.assertRaises(bench.BenchError) as raised:
                    bench.credentials(path)
                self.assertNotIn('SECRET_FIXTURE', str(raised.exception))

    def test_r2_key_value_format_is_not_executed_and_checks_bucket_region(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'r2'
            path.write_text('AWS_SECRET_ACCESS_KEY=secret=with=equals\nR2_REGION=auto\n'
                            'AWS_ACCESS_KEY_ID=private-id\nR2_ENDPOINT=https://example.test\nR2_BUCKET=motor-cache-test\n')
            path.chmod(0o600)
            config = {'bucket': 'motor-cache-test', 'region': 'auto'}
            self.assertEqual(bench.credentials(path, config), ('https://example.test', 'private-id', 'secret=with=equals'))
            with self.assertRaises(bench.BenchError) as error:
                bench.credentials(path, dict(config, bucket='cortexkitmotor'))
            self.assertNotIn('private-id', str(error.exception))
            self.assertNotIn('secret=with=equals', str(error.exception))
            path.write_text(path.read_text() + 'AWS_ACCESS_KEY_ID=duplicate\n')
            with self.assertRaises(bench.BenchError):
                bench.credentials(path)

    def test_children_do_not_inherit_real_credentials_or_build_overrides(self):
        hostile = {'AWS_SECRET_ACCESS_KEY': 'secret', 'BOAT_API_KEY': 'boat', 'SCCACHE_BUCKET': 'bad',
                   'RUSTFLAGS': '--bad', 'CARGO_HOME': '/bad', 'RUSTC_WRAPPER': 'bad',
                   'CARGO_BUILD_RUSTC_WRAPPER': 'bad', 'GIT_CONFIG_COUNT': '1', 'PATH': '/usr/bin'}
        with patch.dict(os.environ, hostile, clear=True):
            env = bench.clean_env()
        for key in ('AWS_SECRET_ACCESS_KEY', 'BOAT_API_KEY', 'SCCACHE_BUCKET', 'RUSTFLAGS', 'GIT_CONFIG_COUNT'):
            self.assertNotIn(key, env)
        self.assertEqual(env['CARGO_HOME'], '/srv/cache-bench/cargo-home')
        self.assertEqual(env['RUSTC_WRAPPER'], '')
        self.assertEqual(env['CARGO_BUILD_RUSTC_WRAPPER'], '')
        self.assertEqual(env['CARGO_NET_OFFLINE'], 'true')

    def test_manifest_backend_names_are_unique_and_properties_cannot_be_mixed(self):
        valid = {'name': 'r2', 'bucket': 'motor-cache-test', 'credentials': 'missing-file', 'region': 'auto', 'addressing': 'path'}
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'backends.json'
            path.write_text(json.dumps([valid]))
            args = self.parse('--run', '--backends', str(path), '--placement', 'B', '--output', '/out', '--prefix', PREFIX)
            self.assertEqual(bench.load_backends(args)[0]['credentials'], str(Path(tmp) / 'missing-file'))
            path.write_text(json.dumps([valid, valid]))
            with self.assertRaisesRegex(bench.BenchError, 'unique'):
                bench.load_backends(args)
            with self.assertRaises(SystemExit):
                self.parse('--purge', '--backends', str(path), '--prefix', PREFIX, '--region', 'auto')
        for value in (None, {}, dict(valid, name=None), dict(valid, addressing='auto')):
            with self.subTest(value=value), self.assertRaises(bench.BenchError):
                bench.backend_config(value)


class ParsingTests(unittest.TestCase):
    def test_stats_preserve_raw_and_report_language_counters_and_durations(self):
        raw = {'stats': {'compile_requests': 15, 'cache_hits': {'counts': {'Rust': 8}, 'adv_counts': {'rust rlib': 8}},
                         'cache_misses': {'counts': {'Rust': 4, 'C/C++': 1}},
                         'cache_errors': {'counts': {'Rust': 1}},
                         'cache_read_hit_duration': {'secs': 2, 'nanos': 5},
                         'cache_read_miss_duration': {'secs': 0, 'nanos': 4}}}
        parsed = bench.parse_stats(json.dumps(raw))
        self.assertEqual((parsed['requests'], parsed['hits'], parsed['misses'], parsed['errors']), (15, 8, 5, 1))
        self.assertEqual(parsed['read_hit_duration'], {'secs': 2, 'nanos': 5})
        self.assertEqual(parsed['raw'], raw)
        with self.assertRaises((KeyError, bench.BenchError)):
            bench.parse_stats('{"stats":{}}')

    def test_latency_counts_exactly_200_real_gets_and_nearest_rank_percentiles(self):
        class Fake:
            events = []
            bucket = 'motor-cache-test'
            def objects(self, prefix):
                return [prefix + 'a', prefix + 'b']
            def request(self, method, path):
                self.events.append({'method': method, 'status': 200, 'bytes_downloaded': 11,
                                    'seconds': (len(self.events) + 1) / 1000})
                return 200, {}, b'cache entry'
        result = bench.latency_probe(Fake(), PREFIX)
        self.assertEqual((result['samples'], result['distinct_objects']), (200, 2))
        self.assertEqual(result['p50_get_ms'], 100)
        self.assertEqual(result['p95_get_ms'], 190)
        self.assertEqual(result['transfer']['bytes_downloaded'], 2200)
        self.assertEqual(result['transfer']['request_count'], 200)

    def test_sigv4_uses_selected_region_and_signs_storage_class(self):
        headers = bench.signed_headers('PUT', '/motor-cache-test/a', '', b'', 'id', 'secret',
                                       now=datetime(2026, 1, 2, tzinfo=timezone.utc),
                                       extra={'x-amz-storage-class': 'EXPRESS_ONEZONE'})
        self.assertEqual(headers['host'], 's3.de.io.cloud.ovh.net')
        self.assertEqual(headers['x-amz-content-sha256'], hashlib.sha256(b'').hexdigest())
        auth = headers['authorization']
        self.assertIn('Credential=id/20260102/de/s3/aws4_request', auth)
        self.assertIn('x-amz-date;x-amz-storage-class', auth)
        self.assertNotIn('secret', str(headers))
        self.assertEqual(bench.canonical_query('z=a%2Fb&a=hello+world'), 'a=hello%20world&z=a%2Fb')

    def test_transport_selects_region_and_virtual_host_or_path_without_real_network(self):
        for style in ('path', 'virtual-host'):
            with self.subTest(style=style), patch.object(bench.http.client, 'HTTPSConnection') as connect:
                response = connect.return_value.getresponse.return_value
                response.status, response.read.return_value = 200, b'entry'
                response.getheaders.return_value = [('Content-Length', '5')]
                s3 = bench.S3('https://example.test', 'id', 'secret', {'bucket': 'cortexkitmotor', 'region': 'auto', 'addressing': style})
                s3.request('GET', '/cortexkitmotor/cache-bench/run/hash')
                call = connect.return_value.request.call_args
                self.assertIn('/auto/s3/aws4_request', call.args[3]['authorization'])
                self.assertEqual(call.args[1], '/cache-bench/run/hash' if style == 'virtual-host' else
                                 '/cortexkitmotor/cache-bench/run/hash')
                self.assertEqual(call.args[3]['host'], 'cortexkitmotor.example.test' if style == 'virtual-host'
                                 else 'example.test')

    def test_rtt_probe_runs_30_seconds_and_times_tcp_and_one_byte_get_separately(self):
        clock = [0.0]
        def sleep(value):
            clock[0] += value
        class Client:
            events = []
            bucket = 'motor-cache-test'
            connect_address = ('example.test', 443)
            def request(self, method, path, **kwargs):
                self.kwargs = kwargs
                clock[0] += .01
                return 206, {}, b'x'
        client = Client()
        def connect(*args, **kwargs):
            clock[0] += .005
            return contextlib.nullcontext()
        with patch.object(bench.time, 'perf_counter', side_effect=lambda: clock[0]), \
                patch.object(bench.time, 'sleep', side_effect=sleep), \
                patch.object(bench.socket, 'create_connection', side_effect=connect):
            result = bench.rtt_probe(client, 'real-cache-key')
        self.assertGreaterEqual(result['seconds'], 30)
        self.assertLess(result['seconds'], 30.1)
        self.assertAlmostEqual(result['tcp_connect']['p50_ms'], 5)
        self.assertAlmostEqual(result['tiny_get']['p95_ms'], 10)
        self.assertEqual(client.kwargs['extra'], {'range': 'bytes=0-0'})
        self.assertTrue(client.kwargs['fresh'])

    def test_summary_distinguishes_misses_from_transport_errors(self):
        result = bench.transfer_summary([{'method': 'GET', 'status': status, 'bytes_downloaded': 3}
                                         for status in (200, 404, 403, 500, 0)])
        self.assertEqual(result['errors'], 3)
        self.assertEqual(result['not_found'], 1)
        self.assertEqual(result['bytes_downloaded'], 15)

    def test_render_no_cache_is_not_a_fake_miss(self):
        run = {'name': 'no-cache', 'wall_seconds': 1.25, 'sccache': None,
               'transfer': {'bytes_downloaded': 0, 'request_count': 0}}
        text = bench.render({'runs': [run]})
        self.assertIn('| no-cache | 1.250 | n/a |', text)
        self.assertIn('Complete: False', text)


class GatewayTests(unittest.TestCase):
    class S3:
        def __init__(self):
            self.calls = []
            self.bucket = 'motor-cache-test'
            self.config = {'region': 'de', 'storage_class': 'EXPRESS_ONEZONE'}
        def request(self, method, path, body=b'', **kwargs):
            self.calls.append((method, path, body, kwargs))
            return 200, {'content-length': '5', 'etag': 'test'}, b'cache' if method != 'HEAD' else b''

    def test_read_only_rejects_writes_and_other_prefixes_before_s3(self):
        s3 = self.S3()
        with bench.Gateway(s3, PREFIX, False) as gateway:
            connection = http.client.HTTPConnection('127.0.0.1', gateway.server.server_port)
            for method, path in (('PUT', '/' + s3.bucket + '/' + PREFIX + 'hash'),
                                 ('GET', '/' + s3.bucket + '/other/hash'),
                                 ('GET', '/' + s3.bucket + '/' + PREFIX + '../hash')):
                connection.request(method, path, body=b'')
                response = connection.getresponse()
                self.assertEqual(response.status, 403)
                response.read()
                connection.close()
            self.assertEqual(s3.calls, [])
            self.assertEqual(gateway.denied, 3)
            self.assertEqual(gateway.env()['SCCACHE_S3_RW_MODE'], 'READ_ONLY')
            self.assertEqual(gateway.env()['AWS_SECRET_ACCESS_KEY'], 'cache-bench-dummy')

    def test_fill_forwards_exact_object_and_reports_head_size(self):
        s3 = self.S3()
        with bench.Gateway(s3, PREFIX, True) as gateway:
            connection = http.client.HTTPConnection('127.0.0.1', gateway.server.server_port)
            path = '/' + s3.bucket + '/' + PREFIX + 'hash'
            connection.request('PUT', path, body=b'entry')
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(response.read(), b'cache')
            connection.request('HEAD', path)
            response = connection.getresponse()
            self.assertEqual(response.getheader('Content-Length'), '5')
            response.read()
            connection.close()
        self.assertEqual([call[:3] for call in s3.calls], [('PUT', path, b'entry'), ('HEAD', path, b'')])
        self.assertEqual(s3.calls[0][3]['extra'], {'x-amz-storage-class': 'EXPRESS_ONEZONE'})
        self.assertEqual(s3.calls[1][3]['extra'], {})

    def test_probe_observes_gateway_and_forces_fresh_upstream_tls(self):
        s3 = self.S3()
        with bench.Gateway(s3, PREFIX, False) as gateway:
            client = bench.GatewayClient(gateway)
            try:
                status, _, body = client.request('GET', '/' + s3.bucket + '/' + PREFIX + 'hash',
                                                 extra={'range': 'bytes=0-0'}, fresh=True)
            finally:
                client.close()
        self.assertEqual(status, 200)
        self.assertEqual(body, b'cache')
        self.assertTrue(s3.calls[0][3]['fresh'])
        self.assertEqual(s3.calls[0][3]['extra'], {'range': 'bytes=0-0'})
        self.assertEqual(len(client.events), 1)

    def test_authentication_error_body_cannot_leak_access_id_to_sccache(self):
        s3 = self.S3()
        with patch.object(s3, 'request', return_value=(403, {}, b'<AccessKeyId>PRIVATE-ID</AccessKeyId>')), \
                bench.Gateway(s3, PREFIX, False) as gateway:
            client = bench.GatewayClient(gateway)
            try:
                status, _, body = client.request('GET', '/' + s3.bucket + '/' + PREFIX + 'hash')
            finally:
                client.close()
        self.assertEqual(status, 403)
        self.assertEqual(body, b'')


class StorageClassTests(unittest.TestCase):
    class S3:
        def __init__(self):
            self.events, self.calls = [], []
            self.bucket = 'motor-cache-test'
            self.endpoint = bench.ENDPOINT
            self.config = {'storage_class': 'EXPRESS_ONEZONE'}
        def objects(self, prefix):
            return [prefix + 'a', prefix + 'b']
        def request(self, method, path, query='', extra=None):
            self.calls.append((method, path, query, extra))
            if method == 'PUT':
                return 200, {}, b'<CopyObjectResult><ETag>same</ETag></CopyObjectResult>'
            tier = 'EXPRESS_ONEZONE' if '/high-performance/' in path else 'STANDARD'
            return 200, {'x-amz-storage-class': tier, 'content-length': '7', 'etag': 'same'}, b''

    def test_head_audit_reports_actual_class_not_requested_label(self):
        s3 = self.S3()
        audit = bench.verify_class(s3, PREFIX)
        self.assertEqual(audit['returned'], ['STANDARD'])
        self.assertFalse(audit['matches_requested'])
        self.assertEqual(len(audit['sampled_objects']), 2)

    def test_absent_head_header_requires_independent_attributes_not_an_assumption(self):
        s3 = self.S3()
        with patch.object(s3, 'request', side_effect=[
                (200, {'content-length': '3', 'etag': 'same'}, b''),
                (200, {}, b'<GetObjectAttributesOutput><StorageClass>STANDARD</StorageClass></GetObjectAttributesOutput>')]) as request:
            result = bench.head_object(s3, 'key')
        self.assertEqual(result['effective_storage_class'], 'STANDARD')
        self.assertIsNone(result['storage_class_header'])
        self.assertEqual(request.call_args.kwargs['extra'], {'x-amz-object-attributes': 'StorageClass'})
        with patch.object(s3, 'request', side_effect=[(200, {'content-length': '3'}, b''), (403, {}, b'')]):
            self.assertIsNone(bench.head_object(s3, 'key')['effective_storage_class'])


class WorkloadTests(unittest.TestCase):
    def test_each_sample_cleans_target_builds_then_compiles_tests_and_stops_daemon(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(bench.shutil, 'rmtree') as clean, \
                patch.object(bench, 'command') as command, patch.object(bench.subprocess, 'run') as run:
            command.side_effect = ['', '', json.dumps({'stats': {'compile_requests': 2, 'cache_hits': {},
                                                                  'cache_misses': {}, 'cache_errors': {}}}), '']
            run.return_value = subprocess.CompletedProcess([], 0)
            result = bench.workload('sample', {'X': 'same'}, Path(tmp), True)
        clean.assert_called_once_with(bench.TARGET, ignore_errors=True)
        self.assertEqual([call.args[0] for call in run.call_args_list],
                         [['cargo', 'build', '--locked', '--workspace'],
                          ['cargo', 'test', '--no-run', '--locked', '--workspace']])
        self.assertEqual(command.call_args_list[-1].args[0], ['sccache', '--stop-server'])
        self.assertEqual(result['sccache']['requests'], 2)

    def test_failure_stops_daemon_and_cannot_return_valid_timing(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(bench.shutil, 'rmtree'), \
                patch.object(bench, 'command') as command, patch.object(bench.subprocess, 'run') as run:
            run.return_value = subprocess.CompletedProcess([], 1)
            with self.assertRaises(bench.BenchError):
                bench.workload('sample', {}, Path(tmp), True)
        self.assertEqual(command.call_args_list[-1].args[0], ['sccache', '--stop-server'])

    def test_failed_target_removal_never_runs_cargo(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(bench, 'TARGET', Path(tmp)), \
                patch.object(bench.shutil, 'rmtree'), patch.object(bench.subprocess, 'run') as run:
            with self.assertRaisesRegex(bench.BenchError, 'cold target removal failed'):
                bench.workload('sample', {}, Path(tmp), False)
        run.assert_not_called()


class ScheduleTests(unittest.TestCase):
    def exercise(self, placement):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'fixed'
            root.mkdir()
            checkout = root / 'motor'
            checkout.mkdir()
            (root / 'manifest.json').write_text(json.dumps({
                'commit': 'a' * 40, 'toolchain': '1.99.0', 'jobs': 8, 'versions': {'same': True},
                'checkout': str(checkout), 'cargo_home': str(root / 'cargo-home'),
                'lock_sha256': 'sha', 'source_sha256': 'source', 'driver_sha256': 'sha'}))
            calls = []

            class S3:
                events = []
                bucket = 'motor-cache-test'
                config = {'region': 'de'}
                def objects(self, prefix):
                    return [] if placement == 'A' else [prefix + 'standard/object']

            def workload(name, env, output, cached):
                calls.append((name, env.copy(), cached))
                return {'name': name, 'wall_seconds': 1.0, 'sccache': {
                    'hits': 3, 'misses': 1, 'errors': 0, 'read_errors': 0, 'write_errors': 0} if cached else None}

            output = Path(tmp) / 'evidence'
            with patch.multiple(bench, ROOT=root, CHECKOUT=checkout, CARGO_HOME=root / 'cargo-home'), \
                    patch.object(bench, 'versions', return_value={'same': True}), \
                    patch.object(bench, 'sha', return_value='sha'), patch.object(bench, 'source_sha', return_value='source'), \
                    patch.object(bench.shutil, 'which', return_value='/sccache'), \
                    patch.object(bench, 'workload', side_effect=workload), \
                    patch.object(bench, 'verify_class', return_value={'sampled_objects': [{'key': PREFIX + 'hash'}]}), \
                    patch.object(bench, 'rtt_probe', return_value={'failures': 0}) as rtt, \
                    patch.object(bench, 'latency_probe', return_value={'samples': 200, 'p50_get_ms': 1,
                        'p95_get_ms': 2, 'transfer': {'bytes_downloaded': 400}}) as latency, \
                    contextlib.redirect_stdout(io.StringIO()):
                bench.benchmark(argparse.Namespace(placement=placement, output=output, prefix=PREFIX), S3())
            result = json.loads((output / 'results.json').read_text())
            self.assertTrue(result['complete'])
            self.assertEqual(len(latency.call_args_list), 2)
            self.assertEqual(rtt.call_count, 2)
            return calls

    def test_a_fills_its_own_backend_and_reads_three_times_plus_local_floor(self):
        calls = self.exercise('A')
        names = [call[0] for call in calls]
        self.assertEqual(len(names), 7)
        self.assertEqual(names[:2], ['no-cache', 'remote-fill'])
        self.assertEqual(names[-2:], ['local-fill', 'local-warm'])
        self.assertEqual(calls[1][1]['SCCACHE_S3_RW_MODE'], 'READ_WRITE')
        self.assertEqual(sum('read-only' in name for name in names), 3)

    def test_b_never_fills_or_writes_and_has_three_reads_per_backend(self):
        calls = self.exercise('B')
        self.assertEqual(len(calls), 4)
        self.assertEqual(calls[0][0], 'no-cache')
        for _, env, cached in calls[1:]:
            self.assertTrue(cached)
            self.assertEqual(env['SCCACHE_S3_RW_MODE'], 'READ_ONLY')
        prefixes = [call[1]['SCCACHE_S3_KEY_PREFIX'] for call in calls[1:]]
        self.assertEqual(prefixes.count(PREFIX), 3)


class MatrixTests(unittest.TestCase):
    def test_available_subset_fills_independently_and_missing_keys_are_explicitly_skipped(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / 'creds'
            path.write_text('https://example.test\nprivate-id\nprivate-secret\n')
            path.chmod(0o600)
            configs = [dict(name=name, credentials=str(path if index < 2 else root / 'missing'),
                            bucket='motor-cache-test', region='de', addressing='path', storage_class=storage)
                       for index, (name, storage) in enumerate((('ovh-standard', 'STANDARD'),
                           ('ovh-high-performance', 'EXPRESS_ONEZONE'), ('r2', None), ('hetzner-fsn1', None)))]
            def benchmark(args, s3):
                return {'class_audit': {'matches_requested': True, 'returned': [s3.config['storage_class']]},
                        'warnings': [], 'rtt': {'direct': {'failures': 0}, 'gateway': {'failures': 0}}, 'runs': []}
            args = argparse.Namespace(output=root / 'A', prefix=PREFIX, placement='A')
            with patch.object(bench, 'benchmark', side_effect=benchmark) as measured, \
                    contextlib.redirect_stdout(io.StringIO()) as stdout:
                self.assertTrue(bench.matrix(args, configs))
            self.assertEqual([call.args[0].prefix for call in measured.call_args_list],
                             [PREFIX + 'ovh-standard/', PREFIX + 'ovh-high-performance/'])
            result = json.loads((args.output / 'results.json').read_text())
            self.assertEqual([row['status'] for row in result['backends']], ['passed', 'passed', 'skipped', 'skipped'])
            self.assertNotIn('private-id', stdout.getvalue())
            self.assertNotIn('private-secret', stdout.getvalue())
            b = dict(result, placement='B', backends=[dict(row, placement='B') for row in result['backends']])
            (root / 'B.json').write_text(json.dumps(b))
            bench.main(['--combine', str(args.output / 'results.json'), str(root / 'B.json'),
                        '--output', str(root / 'combined')])
            combined = json.loads((root / 'combined/results.json').read_text())
            self.assertEqual(len(combined['backends']), 8)
            markdown = (root / 'combined/results.md').read_text()
            self.assertIn('| A | ovh-standard | passed |', markdown)
            self.assertIn('| B | ovh-standard | passed |', markdown)
            self.assertIn('GET gateway p50/p95', markdown)

    def test_documented_backend_manifest_includes_hetzner_bucket_and_all_four_variants(self):
        configs = json.loads(Path(__file__).with_name('backends.example.json').read_text())
        self.assertEqual(len(configs), 4)
        for config in configs:
            bench.backend_config(config)
        self.assertEqual(configs[-1]['bucket'], 'cortexkitmotor')
        self.assertEqual(configs[2]['region'], 'auto')


if __name__ == '__main__':
    unittest.main()
