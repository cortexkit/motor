#!/usr/bin/env python3
"""Operator-only, isolated cargo/sccache experiment. Python 3.10+, stdlib only."""
from __future__ import annotations

import argparse
import contextlib
from datetime import datetime, timezone
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import socket
import stat
import subprocess
import tarfile
import threading
import time
from urllib.parse import quote, urlencode, urlsplit, parse_qsl
import xml.etree.ElementTree as ET

ROOT = Path('/srv/cache-bench')
CHECKOUT = ROOT / 'motor'
CARGO_HOME = ROOT / 'cargo-home'
TARGET = ROOT / 'target'
ENDPOINT = 'https://s3.de.io.cloud.ovh.net'
REGION = 'de'


class BenchError(Exception):
    pass


class BenchTimeout(BenchError):
    pass


def credentials(path, config=None):
    """Never include file contents or parser exceptions in errors."""
    mode = path.stat().st_mode
    if path.is_symlink() or not stat.S_ISREG(mode) or mode & 0o077:
        raise BenchError('credentials must be a regular owner-only file (chmod 600)')
    lines = path.read_text().splitlines()
    if lines and lines[0].startswith('https://'):
        if len(lines) != 3:
            raise BenchError('bare credential format requires exactly three lines')
    else:
        values = {}
        for line in lines:
            key, separator, value = line.partition('=')
            if not separator or key in values:
                raise BenchError('invalid KEY=VALUE credential format')
            values[key] = value
        if set(values) != {'R2_ENDPOINT', 'R2_BUCKET', 'R2_REGION', 'AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY'}:
            raise BenchError('KEY=VALUE credential format requires the five documented R2/AWS fields')
        if config and (values['R2_BUCKET'] != config['bucket'] or values['R2_REGION'] != config['region']):
            raise BenchError('credential bucket/region disagrees with backend manifest')
        lines = [values['R2_ENDPOINT'], values['AWS_ACCESS_KEY_ID'], values['AWS_SECRET_ACCESS_KEY']]
    if any(not line or any(c.isspace() for c in line) for line in lines):
        raise BenchError('credential values must be nonempty and contain no whitespace')
    endpoint = urlsplit(lines[0])
    if (endpoint.scheme != 'https' or not endpoint.hostname or endpoint.username or endpoint.password or
            endpoint.path not in ('', '/') or endpoint.query or endpoint.fragment):
        raise BenchError('credential endpoint must be an HTTPS origin without userinfo, path or query')
    return lines[0].rstrip('/'), lines[1], lines[2]


def backend_config(value):
    if not isinstance(value, dict):
        raise BenchError('each backend must be a JSON object')
    if any(not isinstance(value.get(key), str) for key in ('name', 'credentials', 'region', 'addressing', 'bucket')) or (
            value.get('storage_class') is not None and not isinstance(value['storage_class'], str)):
        raise BenchError('backend fields must be strings (storage_class may be omitted)')
    if (set(value) - {'name', 'credentials', 'region', 'addressing', 'storage_class', 'bucket'} or
            not re.fullmatch(r'[a-z0-9][a-z0-9-]{0,63}', value.get('name', '')) or
            not re.fullmatch(r'[a-zA-Z0-9-]{1,64}', value.get('region', '')) or
            not re.fullmatch(r'[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]', value['bucket']) or
            value.get('addressing') not in ('path', 'virtual-host') or not value.get('credentials') or
            value.get('storage_class') is not None and not re.fullmatch(r'[A-Z0-9_]{1,64}', value['storage_class'])):
        raise BenchError('invalid backend config: require name, credential path, bucket, region, addressing and optional storage_class')
    return dict(value)


def load_backends(args):
    if args.backends:
        values = json.loads(args.backends.read_text())
        if not isinstance(values, list) or not values:
            raise BenchError('backend manifest must be a nonempty JSON array')
    else:
        values = [{'name': args.backend_name, 'credentials': str(args.credentials), 'region': args.region, 'bucket': args.bucket,
                   'addressing': args.addressing, 'storage_class': args.storage_class}]
    configs = [backend_config(value) for value in values]
    if len({value['name'] for value in configs}) != len(configs):
        raise BenchError('backend names must be unique (including storage class variants)')
    for value in configs:
        path = Path(value['credentials']).expanduser()
        if not path.is_absolute() and args.backends:
            path = args.backends.resolve().parent / path
        value['credentials'] = str(path.absolute())
    return configs


def bundle_manifest(path):
    with tarfile.open(path) as archive:
        stream = archive.extractfile('manifest.json')
        if stream is None:
            raise BenchError('bundle has no manifest')
        return json.load(stream)


def load_repositories(path):
    values = json.loads(path.read_text())
    if not isinstance(values, list) or not values:
        raise BenchError('repositories must be a nonempty JSON array')
    result = []
    for item in values:
        if (not isinstance(item, dict) or set(item) != {'name', 'bundle', 'bundle_sha256', 'prefix'} or
                item['name'] not in ('motor', 'prefrontal', 'broca') or
                not re.fullmatch('[0-9a-f]{64}', item['bundle_sha256'])):
            raise BenchError('repository requires name, bundle, bundle_sha256 and prefix')
        prefix_arg(item['prefix'])
        bundle = Path(item['bundle']).expanduser()
        if not bundle.is_absolute():
            bundle = path.resolve().parent / bundle
        if sha(bundle) != item['bundle_sha256']:
            raise BenchError('repository bundle digest differs from independently recorded digest')
        manifest = bundle_manifest(bundle)
        if manifest.get('repository', 'motor') != item['name']:
            raise BenchError('bundle repository differs from repository manifest')
        result.append(dict(item, bundle=str(bundle)))
    if len({r['name'] for r in result}) != len(result) or len({r['prefix'] for r in result}) != len(result):
        raise BenchError('repository names and prefixes must be unique')
    return result


def prefix_arg(value):
    if not re.fullmatch(r'cache-bench/[a-zA-Z0-9][a-zA-Z0-9_-]{0,79}/', value):
        raise argparse.ArgumentTypeError('prefix must be cache-bench/UNIQUE-RUN/ (trailing slash required)')
    return value


def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def source_sha():
    digest = hashlib.sha256()
    for path in sorted(CHECKOUT.rglob('*')):
        if path.is_symlink() or path.is_file():
            relative = str(path.relative_to(CHECKOUT))
            value = os.readlink(path) if path.is_symlink() else sha(path)
            digest.update(json.dumps([relative, value]).encode() + b'\n')
    return digest.hexdigest()


def clean_env():
    # A child needs tool discovery, not the operator's credentials or build overrides.
    env = {k: os.environ[k] for k in ('PATH', 'HOME', 'RUSTUP_HOME', 'LD_LIBRARY_PATH') if k in os.environ}
    env.setdefault('RUSTUP_HOME', str(Path.home() / '.rustup'))
    env.update(HOME=str(ROOT / 'home'), CARGO_HOME=str(CARGO_HOME), CARGO_TARGET_DIR=str(TARGET), CARGO_NET_OFFLINE='true',
               CARGO_INCREMENTAL='0', CARGO_BUILD_INCREMENTAL='false', RUSTC_WRAPPER='',
               CARGO_BUILD_RUSTC_WRAPPER='', LC_ALL='C', TMPDIR=str(ROOT / 'tmp'))
    return env


def command(argv, env=None, cwd=None):
    result = subprocess.run(argv, env=env, cwd=cwd, capture_output=True)
    if result.returncode:
        raise BenchError('command failed: ' + ' '.join(map(str, argv[:3])) +
                         '; see build logs if this was a workload command')
    return result.stdout.decode().strip()


def versions(env):
    return {'rustc': command(['rustc', '-vV'], env), 'cargo': command(['cargo', '-V'], env),
            'rust_sysroot': command(['rustc', '--print', 'sysroot'], env),
            'sccache': command(['sccache', '--version'], env),
            'architecture': platform.machine(), 'system': platform.system()}


def prepare(args):
    if args.bundle.exists():
        raise BenchError('bundle output already exists; choose a new file')
    if ROOT.resolve() != ROOT or ROOT.is_symlink() or ROOT.exists() and (not ROOT.is_dir() or any(ROOT.iterdir())):
        raise BenchError('/srv/cache-bench must be new or empty, never a symlink; archive existing results first')
    ROOT.mkdir(mode=0o700, parents=True, exist_ok=True)
    ROOT.chmod(0o700)
    (ROOT / 'tmp').mkdir()
    (ROOT / 'home').mkdir()
    CHECKOUT.mkdir()
    env = clean_env()
    env['RUSTUP_TOOLCHAIN'] = args.toolchain
    info = versions(env)
    if not re.search(r'\b0\.18\.0\b', info['sccache']):
        raise BenchError('sccache 0.18.0 is required')
    if command(['git', '-C', str(args.source), 'rev-parse', args.commit + '^{commit}']) != args.commit:
        raise BenchError('commit must resolve exactly to its full pinned SHA')
    archive = ROOT / 'source.tar'
    with archive.open('wb') as stream:
        subprocess.run(['git', '-C', str(args.source), 'archive', args.commit], stdout=stream, check=True)
    # git archive contains tracked source only, never a warm target or untracked secrets.
    command(['tar', '-xf', str(archive), '-C', str(CHECKOUT)])
    archive.unlink()
    if not (CHECKOUT / 'Cargo.lock').is_file():
        raise BenchError('pinned source has no Cargo.lock')
    CARGO_HOME.mkdir()
    # Do not copy cargo config, credentials, bin/ or a host-specific build cache.
    for relative in ('registry/index', 'registry/cache', 'registry/src', 'git/db', 'git/checkouts'):
        source = args.cargo_home / relative
        if source.exists():
            shutil.copytree(source, CARGO_HOME / relative, symlinks=False)
    manifest = {'schema': 1, 'repository': args.repository, 'commit': args.commit, 'toolchain': args.toolchain,
                'jobs': args.jobs, 'versions': info, 'checkout': str(CHECKOUT),
                'cargo_home': str(CARGO_HOME), 'lock_sha256': sha(CHECKOUT / 'Cargo.lock'),
                'source_sha256': source_sha(), 'driver_sha256': sha(Path(__file__))}
    (ROOT / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    with tarfile.open(args.bundle, 'w:gz') as bundle:
        for name in ('motor', 'cargo-home', 'manifest.json'):
            bundle.add(ROOT / name, arcname=name)
    print(json.dumps({'bundle_sha256': sha(args.bundle), 'manifest': manifest}, indent=2))


def canonical_query(query):
    return '&'.join(f'{quote(k, safe="-_.~")}={quote(v, safe="-_.~")}'
                    for k, v in sorted(parse_qsl(query, keep_blank_values=True)))


def signed_headers(method, path, query, body, access, secret, now=None, extra=None,
                   host='s3.de.io.cloud.ovh.net', region=REGION):
    """SigV4 for path-style S3, with TLS verified by HTTPSConnection."""
    now = now or datetime.now(timezone.utc)
    stamp, day = now.strftime('%Y%m%dT%H%M%SZ'), now.strftime('%Y%m%d')
    digest = hashlib.sha256(body).hexdigest()
    headers = {'host': host, 'x-amz-content-sha256': digest, 'x-amz-date': stamp}
    headers.update(extra or {})
    names = ';'.join(sorted(headers))
    canonical = '\n'.join([method, path, canonical_query(query),
                           ''.join(f'{k}:{headers[k]}\n' for k in sorted(headers)), names, digest])
    scope = f'{day}/{region}/s3/aws4_request'
    to_sign = '\n'.join(['AWS4-HMAC-SHA256', stamp, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    key = ('AWS4' + secret).encode()
    for value in (day, region, 's3', 'aws4_request'):
        key = hmac.new(key, value.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    headers['authorization'] = f'AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={names}, Signature={signature}'
    return headers


class _Transport(Exception):
    """A request that failed below HTTP, before or while reading the response."""
    def __init__(self, message, reused, timed_out=False):
        super().__init__(message)
        self.message, self.reused = message, reused
        self.timed_out = timed_out


class S3:
    """One persistent TLS connection per calling thread, no redirects or SDK retries."""
    def __init__(self, endpoint, access, secret, config):
        self.access, self.secret = access, secret
        self.endpoint, self.config = endpoint, config
        self.bucket = config['bucket']
        origin = urlsplit(endpoint)
        self.host = (self.bucket + '.' if config['addressing'] == 'virtual-host' else '') + origin.netloc
        self.connect_address = (urlsplit('https://' + self.host).hostname, origin.port or 443)
        self.local = threading.local()
        self.lock = threading.Lock()
        self.events = []

    def request(self, method, path, query='', body=b'', extra=None, fresh=False, timeout=120):
        if self.config['addressing'] == 'virtual-host':
            path = path.removeprefix('/' + self.bucket) or '/'
        try:
            return self._send(method, path, query, body, extra, fresh, timeout)
        except _Transport as first:
            # A kept-alive connection can sit idle through a whole build, and
            # providers close idle connections after different times (OVH and
            # Hetzner sooner than R2). Every request the bench sends is
            # idempotent, so a reused connection that fails is retried once on
            # a new one; a failure on a new connection is real.
            if not first.reused:
                raise (BenchTimeout if first.timed_out else BenchError)(first.message) from None
            try:
                return self._send(method, path, query, body, extra, True, timeout)
            except _Transport as second:
                raise (BenchTimeout if second.timed_out else BenchError)(second.message) from None

    def _send(self, method, path, query, body, extra, fresh, timeout):
        if fresh and getattr(self.local, 'connection', None):
            self.local.connection.close()
            self.local.connection = None
        reused = getattr(self.local, 'connection', None) is not None
        if not reused:
            self.local.connection = http.client.HTTPSConnection(self.host, timeout=timeout)
        connection = self.local.connection
        if connection is None:
            raise BenchError('object-storage connection was not initialized')
        connection.timeout = timeout
        if connection.sock is not None:
            connection.sock.settimeout(timeout)
        start = time.perf_counter()
        status, data = 0, b''
        try:
            connection.request(method, path + ('?' + query if query else ''), body,
                               signed_headers(method, path, query, body, self.access, self.secret, extra=extra,
                                              host=self.host, region=self.config['region']))
            response = connection.getresponse()
            status = response.status
            headers = {k.lower(): v for k, v in response.getheaders()}
            data = response.read()
            return status, headers, data
        except (OSError, http.client.HTTPException) as error:
            connection.close()
            self.local.connection = None
            # The exception's class and the method are safe to report; its text
            # may carry the signed URL, so it stays withheld.
            raise _Transport(f'object-storage transport failure: {method} {type(error).__name__}'
                             f' on a {"reused" if reused else "new"} connection (details withheld)',
                              reused, isinstance(error, TimeoutError)) from None
        finally:
            with self.lock:
                self.events.append({'method': method, 'status': status, 'bytes_downloaded': len(data),
                                    'seconds': time.perf_counter() - start})

    def objects(self, prefix):
        token = ''
        while True:
            query = urlencode({'list-type': '2', 'prefix': prefix, **({'continuation-token': token} if token else {})})
            status, _, data = self.request('GET', '/' + self.bucket, query)
            if status != 200:
                raise BenchError(f'ListObjectsV2 returned HTTP {status}')
            root = ET.fromstring(data)
            ns = {'s': 'http://s3.amazonaws.com/doc/2006-03-01/'}
            for content in root.findall('s:Contents', ns):
                key = content.findtext('s:Key', namespaces=ns)
                if not key or not key.startswith(prefix):
                    raise BenchError('storage returned a key outside the requested prefix')
                yield key
            if root.findtext('s:IsTruncated', namespaces=ns) != 'true':
                return
            next_token = root.findtext('s:NextContinuationToken', namespaces=ns)
            if not next_token or token == next_token:
                raise BenchError('invalid storage pagination')
            token = next_token


def object_path(key, bucket):
    return '/' + bucket + '/' + quote(key, safe='/-_.~')


def head_object(s3, key):
    status, headers, _ = s3.request('HEAD', object_path(key, s3.bucket))
    observed = headers.get('x-amz-storage-class')
    effective = observed
    if status == 200 and observed is None and s3.endpoint == ENDPOINT:
        # Do not assume a missing HEAD header proves the requested storage class.
        code, _, data = s3.request('GET', object_path(key, s3.bucket), 'attributes',
                                  extra={'x-amz-object-attributes': 'StorageClass'})
        if code == 200:
            effective = next((node.text for node in ET.fromstring(data).iter()
                              if node.tag.rsplit('}', 1)[-1] == 'StorageClass'), None)
    if status != 200:
        raise BenchError(f'HEAD failed (HTTP {status})')
    return {'key': key, 'storage_class_header': observed, 'effective_storage_class': effective,
            'class_source': 'HEAD' if observed else ('GetObjectAttributes' if effective else 'not reported'),
            'bytes': int(headers['content-length']), 'etag': headers.get('etag')}


def verify_class(s3, prefix):
    keys = sorted(key for key in s3.objects(prefix) if not key.endswith('/.sccache_check'))
    if not keys:
        raise BenchError('fill produced no real cache objects')
    objects = [head_object(s3, key) for key in keys[:20]]
    requested = s3.config.get('storage_class')
    return {'requested': requested, 'total_objects': len(keys), 'sampled_objects': objects,
            'returned': sorted({obj['effective_storage_class'] or 'not reported' for obj in objects}),
            'matches_requested': None if requested is None else all(
                obj['effective_storage_class'] == requested for obj in objects)}


def transfer_summary(events):
    return {'request_count': len(events), 'bytes_downloaded': sum(e['bytes_downloaded'] for e in events),
            'get_count': sum(e['method'] == 'GET' for e in events),
            'put_count': sum(e['method'] == 'PUT' for e in events),
            'errors': sum(e['status'] == 0 or e['status'] >= 500 or
                          400 <= e['status'] < 500 and e['status'] != 404 for e in events),
            'not_found': sum(e['status'] == 404 for e in events)}


class Gateway:
    """sccache sees dummy credentials; this operator process signs upstream requests."""
    def __init__(self, s3, prefix, writable):
        self.s3, self.prefix, self.writable = s3, prefix, writable
        self.denied = 0
        gateway = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, format, *args):
                pass

            def dispatch(self):
                parsed = urlsplit(self.path)
                allowed = (not parsed.query and parsed.path.startswith('/' + gateway.s3.bucket + '/' + gateway.prefix) and
                           '%' not in parsed.path and '..' not in parsed.path.split('/') and
                           self.command in ({'GET', 'HEAD', 'PUT'} if gateway.writable else {'GET', 'HEAD'}))
                if not allowed or self.headers.get('Transfer-Encoding'):
                    gateway.denied += 1
                    self.send_error(403)
                    return
                try:
                    length = int(self.headers.get('Content-Length', '0'))
                    if not 0 <= length <= 1024 * 1024 * 1024:
                        raise ValueError()
                    body = self.rfile.read(length)
                    extra = {}
                    if self.command == 'PUT' and gateway.s3.config.get('storage_class'):
                        extra['x-amz-storage-class'] = gateway.s3.config['storage_class']
                    if self.headers.get('Range'):
                        extra['range'] = self.headers['Range']
                    fresh = self.headers.get('X-Cache-Bench-Fresh') == '1'
                    probe = self.headers.get('X-Cache-Bench-Probe') == '1'
                    status, headers, data = gateway.s3.request(self.command, parsed.path, body=body, extra=extra,
                                                              fresh=fresh, timeout=5 if fresh or probe else 120)
                    if status >= 300:
                        # S3 authentication error XML can echo the access ID; never forward it to logs.
                        data = b''
                    self.send_response(status)
                    # Forward cache metadata, never upstream authorization or an error body.
                    for key, value in headers.items():
                        if key.lower() in {'etag', 'last-modified', 'content-type', 'content-range'}:
                            self.send_header(key, value)
                    self.send_header('Content-Length', headers.get('content-length', '0')
                                     if self.command == 'HEAD' else str(len(data)))
                    self.end_headers()
                    if self.command != 'HEAD':
                        self.wfile.write(data)
                except BenchTimeout:
                    self.send_error(504, 'object-storage timeout')
                except (ValueError, BenchError, OSError):
                    self.send_error(502, 'object-storage request failed')

            do_GET = do_HEAD = do_PUT = do_DELETE = do_POST = dispatch

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def env(self):
        return {'SCCACHE_BUCKET': self.s3.bucket, 'SCCACHE_REGION': self.s3.config['region'],
                'SCCACHE_ENDPOINT': f'http://127.0.0.1:{self.server.server_port}',
                'SCCACHE_S3_USE_SSL': 'false', 'SCCACHE_S3_NO_CREDENTIALS': 'false',
                'SCCACHE_S3_ENABLE_VIRTUAL_HOST_STYLE': 'false',
                'SCCACHE_S3_KEY_PREFIX': self.prefix, 'SCCACHE_S3_RW_MODE': 'READ_WRITE' if self.writable else 'READ_ONLY',
                'AWS_ACCESS_KEY_ID': 'cache-bench-dummy', 'AWS_SECRET_ACCESS_KEY': 'cache-bench-dummy',
                'AWS_EC2_METADATA_DISABLED': 'true'}


def count(value):
    if isinstance(value, dict):
        # adv_counts is the same events grouped more finely, not additional hits.
        return sum(value.get('counts', {}).values())
    if isinstance(value, (int, float)):
        return value
    raise BenchError('invalid sccache counter shape')


def parse_stats(raw):
    data = json.loads(raw)
    stats = data['stats']
    return {'requests': stats['compile_requests'], 'hits': count(stats['cache_hits']),
            'misses': count(stats['cache_misses']), 'errors': count(stats['cache_errors']),
            'read_errors': stats.get('cache_read_errors', 0), 'write_errors': stats.get('cache_write_errors', 0),
            'requests_executed': stats.get('requests_executed'),
            'read_hit_duration': stats.get('cache_read_hit_duration'),
            'read_miss_duration': stats.get('cache_read_miss_duration'), 'raw': data}


def percentile(values, fraction):
    if not values:
        raise BenchError('no latency samples')
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


class GatewayClient:
    """Measure from the caller side of the gateway, including its own overhead."""
    def __init__(self, gateway):
        self.gateway = gateway
        self.bucket = gateway.s3.bucket
        self.connect_address = ('127.0.0.1', gateway.server.server_port)
        self.events = []
        self.connection = None

    def objects(self, prefix):
        return self.gateway.s3.objects(prefix)

    def request(self, method, path, query='', body=b'', extra=None, fresh=False, timeout=120):
        if fresh and self.connection:
            self.connection.close()
            self.connection = None
        if self.connection is None:
            self.connection = http.client.HTTPConnection(*self.connect_address, timeout=timeout)
        self.connection.timeout = timeout
        if self.connection.sock is not None:
            self.connection.sock.settimeout(timeout)
        start = time.perf_counter()
        status, data = 0, b''
        try:
            headers = dict(extra or {})
            headers['X-Cache-Bench-Probe'] = '1'
            if fresh:
                headers['X-Cache-Bench-Fresh'] = '1'
            self.connection.request(method, path, body, headers)
            response = self.connection.getresponse()
            status, data = response.status, response.read()
            return status, dict(response.getheaders()), data
        except (OSError, http.client.HTTPException) as error:
            self.connection.close()
            self.connection = None
            raise (BenchTimeout if isinstance(error, TimeoutError) else BenchError)('gateway probe transport failure') from None
        finally:
            self.events.append({'method': method, 'status': status, 'bytes_downloaded': len(data),
                                'seconds': time.perf_counter() - start})

    def close(self):
        if self.connection:
            self.connection.close()


def rtt_probe(client, key, seconds=30):
    tcp, tiny = [], []
    before_events = len(client.events)
    start = time.perf_counter()
    until = start + seconds
    while time.perf_counter() < until:
        timeout = max(.01, min(5, until - time.perf_counter()))
        for kind, records in (('tcp', tcp), ('tiny', tiny)):
            before = time.perf_counter()
            record: dict = {'ok': False, 'timeout': False}
            try:
                if kind == 'tcp':
                    with socket.create_connection(client.connect_address, timeout=timeout):
                        pass
                else:
                    status, _, body = client.request('GET', object_path(key, client.bucket), extra={'range': 'bytes=0-0'},
                                                     fresh=True, timeout=timeout)
                    record['timeout'] = status in (408, 504)
                    if status != 206 or len(body) != 1:
                        raise BenchError('tiny GET did not return one ranged byte')
                record['ok'] = True
            except (OSError, BenchError) as error:
                record['timeout'] = record['timeout'] or isinstance(error, (TimeoutError, BenchTimeout))
            record['seconds'] = time.perf_counter() - before
            records.append(record)
        time.sleep(min(.1, max(0, until - time.perf_counter())))
    def summary(values):
        good = [r['seconds'] for r in values if r['ok']]
        return {'samples': len(good), 'records': values,
                'p50_ms': percentile(good, .5) * 1000 if good else None,
                'p95_ms': percentile(good, .95) * 1000 if good else None}
    return {'seconds': time.perf_counter() - start, 'failures': sum(not r['ok'] for r in tcp + tiny),
            'tcp_connect': summary(tcp), 'tiny_get': summary(tiny),
            'transfer': transfer_summary(client.events[before_events:])}


def latency_probe(s3, prefix, samples=200):
    keys = [key for key in s3.objects(prefix) if not key.endswith('/.sccache_check')]
    if not keys:
        raise BenchError('no real cache objects for latency probe; fill must succeed first')
    before = len(s3.events)
    timings, records = [], []
    for index in range(samples):
        start = time.perf_counter()
        record: dict = {'ok': False, 'timeout': False}
        try:
            status, _, _ = s3.request('GET', object_path(keys[index % len(keys)], s3.bucket), timeout=5)
            record['status'] = status
            record['ok'] = status == 200
            record['timeout'] = status in (408, 504)
        except (OSError, BenchError) as error:
            record['timeout'] = isinstance(error, (TimeoutError, BenchTimeout))
        record['seconds'] = time.perf_counter() - start
        records.append(record)
        if record['ok']:
            timings.append(record['seconds'])
    return {'samples': samples, 'distinct_objects': min(samples, len(keys)),
            'p50_get_ms': percentile(timings, .50) * 1000 if timings else None,
            'p95_get_ms': percentile(timings, .95) * 1000 if timings else None,
            'failures': sum(not r['ok'] for r in records), 'records': records,
            'transfer': transfer_summary(s3.events[before:]), 'get_seconds': timings}


def dependency_timings(env):
    """Sum Cargo unit durations, not wall time: parallel dependency units overlap."""
    path = TARGET / 'cargo-timings/cargo-timing.html'
    if not path.exists():
        return None
    parts = path.read_text().split('const UNIT_DATA =')
    if len(parts) != 2:
        raise BenchError('Cargo timing HTML has no recognized UNIT_DATA; calibration cannot infer dependency time')
    units, _ = json.JSONDecoder().raw_decode(parts[1].lstrip())
    metadata = json.loads(command(['cargo', 'metadata', '--offline', '--locked', '--format-version', '1'], env, CHECKOUT))
    membership = {}
    for package in metadata['packages']:
        membership.setdefault((package['name'], package['version']), []).append(package['id'] in metadata['workspace_members'])
    dependencies = []
    for unit in units:
        matches = membership.get((unit['name'], unit['version']), [])
        if len(matches) != 1 or not isinstance(unit['duration'], (float, int)) or unit['duration'] < 0:
            raise BenchError('Cargo timing unit is ambiguous or invalid; cannot classify dependency work')
        if not matches[0]:
            dependencies.append(unit)
    return dependencies


def workload(name, env, output, use_cache):
    shutil.rmtree(TARGET, ignore_errors=True)
    if TARGET.exists() or TARGET.is_symlink():
        raise BenchError('cold target removal failed; refusing to measure a warm target')
    stats = None
    steps = []
    try:
        if use_cache:
            command(['sccache', '--start-server'], env)
            command(['sccache', '--zero-stats'], env)
        for label, argv in (('build', ['cargo', 'build', '--locked', '--workspace', '--timings']),
                             ('test-no-run', ['cargo', 'test', '--no-run', '--locked', '--workspace', '--timings'])):
            step_start = time.perf_counter()
            with (output / f'{name}-{label}.log').open('wb') as log:
                result = subprocess.run(argv, cwd=CHECKOUT, env=env, stdout=log, stderr=subprocess.STDOUT)
            steps.append({'command': argv, 'seconds': time.perf_counter() - step_start,
                           'exit_code': result.returncode})
            if result.returncode:
                raise BenchError(f'{name} {label} failed; see workload log')
            if name == 'no-cache':
                units = dependency_timings(env)
                steps[-1]['dependency_units'] = units
                steps[-1]['dependency_unit_seconds'] = sum(u['duration'] for u in units) if units is not None else None
        wall = sum(step['seconds'] for step in steps)
        if use_cache:
            stats = parse_stats(command(['sccache', '--show-stats', '--stats-format', 'json'], env))
    finally:
        if use_cache:
            command(['sccache', '--stop-server'], env)
    return {'name': name, 'wall_seconds': wall, 'steps': steps, 'sccache': stats}


def render_backend(data):
    rows = ['# Remote compile-cache benchmark', '',
            f"Placement: {data.get('placement', '?')}; commit: `{data.get('manifest', {}).get('commit', '?')}`.",
            '', '| Run | Wall s | Requests | Hits | Misses | Errors (cache/read/write) | Download bytes | S3 requests |',
            '|---|---:|---:|---:|---:|---:|---:|---:|']
    for run in data.get('runs', []):
        stats = run.get('sccache') or {}
        transfer = run['transfer']
        rows.append(f"| {run['name']} | {run['wall_seconds']:.3f} | {stats.get('requests', 'n/a')} | "
                    f"{stats.get('hits', 'n/a')} | {stats.get('misses', 'n/a')} | "
                    f"{stats.get('errors', 'n/a')}/{stats.get('read_errors', 'n/a')}/{stats.get('write_errors', 'n/a')} | "
                    f"{transfer['bytes_downloaded']} | {transfer['request_count']} |")
    for tier, latency in data.get('latency', {}).items():
        rows += ['', f"Separate {tier} {latency['samples']}-GET probe of real objects: "
                 f"p50 {latency['p50_get_ms']:.3f} ms; p95 {latency['p95_get_ms']:.3f} ms; "
                  f"{latency['transfer']['bytes_downloaded']} response-body bytes."]
    if data.get('class_audit'):
        rows += ['', 'HEAD class audit: `' + json.dumps(data['class_audit']) + '`']
    rows += ['', 'Raw sccache stats (including read durations), per-command timing and transfer counters are in JSON.',
             'No-cache bypasses sccache (stats n/a). S3 counters exclude listing/latency traffic from workload rows.',
             'Loopback instrumentation adds overhead; byte counts are response bodies, not TLS/wire bytes.',
             f"Complete: {data.get('complete', False)}. Error: {data.get('error', 'none')}.", '']
    return '\n'.join(rows)


def render(data):
    if 'backends' not in data:
        return render_backend(data)
    rows = ['# Compile cache: placement × backend', '',
            '| Placement | Backend | Status | Returned class (requested) | No cache s | Fill s | RO s (3) | Local warm s | RO hits/misses/errors | RO bytes/requests | Full GET direct p50/p95 ms | Full GET gateway p50/p95 ms | TCP direct p50/p95 ms | TCP gateway p50/p95 ms | Tiny GET direct p50/p95 ms | Tiny GET gateway p50/p95 ms |',
            '|---|---|---|---|---:|---:|---|---:|---|---|---|---|---|---|---|---|']
    def pair(value, a, b):
        if not value or value.get(a) is None or value.get(b) is None:
            return 'n/a'
        return f'{value[a]:.2f}/{value[b]:.2f}'
    for entry in data['backends']:
        result = entry.get('result', {})
        runs = {run['name']: run for run in result.get('runs', [])}
        def wall(name):
            return f"{runs[name]['wall_seconds']:.2f}" if name in runs else 'n/a'
        ro = [run for name, run in runs.items() if name.startswith('remote-read-only-')]
        lat, rtt = result.get('latency', {}), result.get('rtt', {})
        audit = result.get('class_audit', {})
        cache = ', '.join(f"{r['sccache']['hits']}/{r['sccache']['misses']}/"
                          f"{r['sccache']['errors']}+{r['sccache']['read_errors']}+{r['sccache']['write_errors']}" for r in ro)
        transfer = ', '.join(f"{r['transfer']['bytes_downloaded']}/{r['transfer']['request_count']}" for r in ro)
        row = [entry.get('placement', data.get('placement', '?')), entry['name'], entry['status'],
               ','.join(audit.get('returned', [])) + ' (' + str(entry.get('storage_class') or 'default') + ')',
               wall('no-cache'), wall('remote-fill'), ', '.join(f"{r['wall_seconds']:.2f}" for r in ro) or 'n/a',
               wall('local-warm'), cache or 'n/a', transfer or 'n/a']
        row += [pair(lat.get(route), 'p50_get_ms', 'p95_get_ms') for route in ('direct', 'gateway')]
        row += [pair(rtt.get(route, {}).get(kind), 'p50_ms', 'p95_ms')
                for kind in ('tcp_connect', 'tiny_get') for route in ('direct', 'gateway')]
        rows.append('| ' + ' | '.join(row) + ' |')
    rows += ['', 'RO: three cold-target repetitions; errors shown cache+read+write. Full GET: 200 real objects per route.',
             'TCP gateway measures loopback, not endpoint RTT. Tiny GET gateway includes fresh upstream TLS.',
             'See JSON for all raw stats/durations, 30-second probes, errors, skip reasons, and sampled HEAD headers.',
             'No live speed conclusion is implied by this report.', '']
    return '\n'.join(rows)


def save(output, data):
    temporary = output / 'results.json.tmp'
    temporary.write_text(json.dumps(data, indent=2, allow_nan=False) + '\n')
    temporary.replace(output / 'results.json')
    (output / 'results.md').write_text(render(data))


def network_probes(s3, prefix, gateway=True):
    audit = verify_class(s3, prefix)
    key = audit['sampled_objects'][0]['key']
    result = {'runs': [], 'warnings': [], 'class_audit': audit,
              'latency': {'direct': latency_probe(s3, prefix)},
              'rtt': {'direct': rtt_probe(s3, key)}, 'complete': True}
    if gateway:
        with Gateway(s3, prefix, False) as proxy:
            client = GatewayClient(proxy)
            try:
                result['latency']['gateway'] = latency_probe(client, prefix)
                result['rtt']['gateway'] = rtt_probe(client, key)
            finally:
                client.close()
    return result


def result_status(result):
    if result.get('class_audit', {}).get('matches_requested') is False:
        return 'class-mismatch'
    if result.get('warnings'):
        return 'cache-warnings'
    if any(p.get('failures', 0) for group in ('latency', 'rtt') for p in result.get(group, {}).values()):
        return 'probe-errors'
    return 'passed'


def benchmark(args, s3):
    profile = getattr(args, 'profile', 'legacy')
    if profile == 'network':
        args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        data = dict(network_probes(s3, args.prefix, gateway=False), placement=args.placement)
        save(args.output, data)
        return data
    if ROOT.is_symlink() or ROOT.resolve() != ROOT or CHECKOUT.resolve() != CHECKOUT or CARGO_HOME.resolve() != CARGO_HOME:
        raise BenchError('fixed checkout/CARGO_HOME paths must not be symlinks')
    manifest = json.loads((ROOT / 'manifest.json').read_text())
    (ROOT / 'tmp').mkdir(exist_ok=True)
    (ROOT / 'home').mkdir(exist_ok=True)
    env = clean_env()
    env.update(RUSTUP_TOOLCHAIN=manifest['toolchain'], CARGO_BUILD_JOBS=str(manifest['jobs']),
               SCCACHE_SERVER_UDS=str(ROOT / 'sccache.sock'), SCCACHE_IDLE_TIMEOUT='0')
    actual = versions(env)
    if actual != manifest['versions'] or sha(CHECKOUT / 'Cargo.lock') != manifest['lock_sha256']:
        raise BenchError('toolchain/architecture/sccache version or Cargo.lock differs from prepared bundle')
    if manifest['checkout'] != str(CHECKOUT) or manifest['cargo_home'] != str(CARGO_HOME):
        raise BenchError('bundle does not use the fixed paths')
    if source_sha() != manifest['source_sha256']:
        raise BenchError('source contents differ from the pinned archive')
    if sha(Path(__file__)) != manifest['driver_sha256']:
        raise BenchError('benchmark driver differs from the script used to prepare A')
    output = args.output.resolve()
    if output == ROOT or ROOT in output.parents:
        raise BenchError('output must be outside /srv/cache-bench so cleanup cannot remove evidence')
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    wrapper = shutil.which('sccache')
    if wrapper is None:
        raise BenchError('sccache executable unavailable')
    data = {'schema': 1, 'placement': args.placement, 'manifest': manifest, 'prefix': args.prefix,
            'host': {'cpu_count': os.cpu_count(), 'uname': list(platform.uname()),
                     'sccache_binary_sha256': sha(Path(wrapper))},
            'runs': [], 'warnings': [], 'complete': False}
    save(output, data)

    def run(name, mode, fill=False):
        if source_sha() != manifest['source_sha256']:
            raise BenchError('source changed during the experiment')
        cached = mode != 'none'
        child_env = dict(env)
        child_env['SCCACHE_SERVER_UDS'] = str(ROOT / ('sccache-' + hashlib.sha256(
            (args.prefix + name).encode()).hexdigest()[:16] + '.sock'))
        if cached:
            child_env['RUSTC_WRAPPER'] = wrapper
        with contextlib.ExitStack() as stack:
            gateway = None
            if mode == 'remote':
                gateway = stack.enter_context(Gateway(s3, args.prefix, fill))
                child_env.update(gateway.env())
            elif cached:
                child_env['SCCACHE_DIR'] = str(ROOT / 'local-cache')
                child_env['SCCACHE_CACHE_SIZE'] = '50G'
            before = len(s3.events)
            result = workload(name, child_env, output, cached)
        result['transfer'] = transfer_summary(s3.events[before:])
        result['gateway_denied'] = gateway.denied if gateway else 0
        data['runs'].append(result)
        save(output, data)
        stats = result['sccache'] or {}
        if cached and (stats.get('errors') or stats.get('read_errors') or stats.get('write_errors')):
            data['warnings'].append(name + ': sccache cache/read/write errors')
        if gateway and (gateway.denied or result['transfer']['errors']):
            data['warnings'].append(name + ': S3 errors or denied requests')
        if (mode == 'remote' and not fill or name == 'local-warm') and not result['sccache']['hits']:
            data['warnings'].append(name + ': no cache hits; check path/toolchain parity')

    try:
        existing = [] if profile == 'baseline' else list(s3.objects(args.prefix))
        filling = profile == 'fill' or profile == 'legacy' and args.placement == 'A'
        if filling and existing:
            raise BenchError('fill prefix is not empty; choose a new prefix or explicitly --purge it')
        if profile != 'baseline' and not filling and not existing:
            raise BenchError('A must fill this prefix before B runs')
        if profile in ('legacy', 'baseline') or getattr(args, 'baseline', False):
            run('no-cache', 'none')
        if profile == 'baseline':
            data['complete'] = True
            return data
        if filling:
            run('remote-fill', 'remote', fill=True)
        data['class_audit'] = verify_class(s3, args.prefix)
        if profile == 'fill':
            data['complete'] = True
            return data
        for number in range(1, 4):
            run(f'remote-read-only-{number}', 'remote')
        probes = network_probes(s3, args.prefix)
        data.update({key: probes[key] for key in ('class_audit', 'latency', 'rtt')})
        save(output, data)
        if profile == 'legacy' and args.placement == 'A':
            shutil.rmtree(ROOT / 'local-cache', ignore_errors=True)
            run('local-fill', 'local')
            run('local-warm', 'local')
        data['complete'] = True
    except (Exception, KeyboardInterrupt) as error:
        # Neither upstream exception bodies nor credentials are ever serialized.
        data['error'] = str(error) if isinstance(error, BenchError) else type(error).__name__
        raise
    finally:
        save(output, data)
    return data


def matrix(args, configs, clients=None):
    if args.output.resolve() == ROOT or ROOT in args.output.resolve().parents:
        raise BenchError('output must be outside /srv/cache-bench')
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {'schema': 1, 'placement': args.placement, 'backends': [], 'complete': False}
    save(args.output, report)
    if getattr(args, 'profile', 'legacy') == 'fill':
        # Check every class before any PUT, so a stale second prefix cannot cause a partial refill.
        for config in configs:
            s3 = clients[config['name']] if clients is not None else S3(*credentials(Path(config['credentials']), config), config)
            if list(s3.objects(args.prefix + config['name'] + '/')):
                raise BenchError('fill prefix is not empty; no fills started')
    for config in configs:
        entry = {key: config.get(key) for key in ('name', 'bucket', 'region', 'addressing', 'storage_class')}
        entry['placement'] = args.placement
        report['backends'].append(entry)
        path = Path(config['credentials'])
        if clients is None and not path.exists():
            entry.update(status='skipped', reason='credential file unavailable')
            save(args.output, report)
            continue
        try:
            s3 = clients[config['name']] if clients is not None else S3(*credentials(path, config), config)
            backend_args = argparse.Namespace(placement=args.placement, output=args.output / config['name'],
                                              prefix=args.prefix + config['name'] + '/',
                                              profile=getattr(args, 'profile', 'legacy'),
                                              baseline=getattr(args, 'baseline', False) and not report['backends'][:-1])
            entry['result'] = benchmark(backend_args, s3)
            result = entry['result']
            entry['status'] = result_status(result)
        except (Exception, KeyboardInterrupt) as error:
            entry.update(status='failed', error=str(error) if isinstance(error, BenchError) else type(error).__name__)
            partial = args.output / config['name'] / 'results.json'
            if partial.exists():
                entry['result'] = json.loads(partial.read_text())
            if isinstance(error, KeyboardInterrupt):
                save(args.output, report)
                raise
        save(args.output, report)
    report['complete'] = any(e['status'] == 'passed' for e in report['backends']) and all(
        e['status'] in ('passed', 'skipped') for e in report['backends'])
    save(args.output, report)
    print(json.dumps(report, indent=2, allow_nan=False))
    return report['complete']


def distribution(values):
    return {'n': len(values), **{label: percentile(values, fraction) if values else None
                               for label, fraction in (('p50', .5), ('p95', .95), ('p99', .99))}}


def measured_records(result):
    for route, probe in result.get('latency', {}).items():
        yield route, 'full-get', probe.get('records', [])
    for route, probes in result.get('rtt', {}).items():
        for kind in ('tcp_connect', 'tiny_get'):
            yield route, kind, probes[kind].get('records', [])


def summarize_data(directory):
    state = json.loads((directory / 'state.json').read_text())
    groups, hourly = {}, []
    for hour in state['iterations']:
        for leg, placement in (('boat', 'B'), ('server', 'A')):
            path = hour.get(leg, {}).get('report')
            if not path:
                continue
            report = json.loads((directory / path).read_text())
            for entry in report['backends']:
                repo = entry.get('repository', 'motor')
                storage = entry.get('storage_class')
                key = (storage, placement, repo)
                group = groups.setdefault(key, {'storage_class': storage, 'placement': placement, 'repository': repo,
                                                'rebuild_seconds': [], 'baseline_seconds': [], 'hits': 0, 'misses': 0,
                                                'cache_errors': 0, 'probes': {}, 'hours': 0, 'failed_hours': 0})
                group['hours'] += 1
                group['failed_hours'] += entry['status'] != 'passed'
                result = entry.get('result', {})
                row = {'hour': hour['hour'], 'due_at': hour['due_at'], 'iteration_status': hour['status'],
                       'storage_class': storage, 'placement': placement, 'repository': repo, 'status': entry['status'],
                       'class_valid': result.get('class_audit', {}).get('matches_requested') is True,
                       'returned_classes': result.get('class_audit', {}).get('returned', []),
                       'probes': {}, 'rebuild_seconds': [], 'hits': 0, 'misses': 0, 'cache_errors': 0}
                for run in result.get('runs', []):
                    if run['name'] == 'no-cache':
                        group['baseline_seconds'].append(run['wall_seconds'])
                    if run['name'].startswith('remote-read-only-'):
                        group['rebuild_seconds'].append(run['wall_seconds'])
                        row['rebuild_seconds'].append(run['wall_seconds'])
                        for field in ('hits', 'misses'):
                            value = (run.get('sccache') or {}).get(field, 0)
                            group[field] += value
                            row[field] += value
                        errors = sum((run.get('sccache') or {}).get(f, 0) for f in ('errors', 'read_errors', 'write_errors'))
                        group['cache_errors'] += errors
                        row['cache_errors'] += errors
                for route, kind, records in measured_records(result):
                    label = route + '/' + kind
                    group['probes'].setdefault(label, []).extend(records)
                    row['probes'][label] = probe_distribution(records)
                row['rebuild_seconds'] = distribution(row['rebuild_seconds'])
                hourly.append(row)
    paired = []
    by_hour = {}
    for row in hourly:
        if not row['class_valid']:
            continue
        by_hour.setdefault((row['hour'], row['placement'], row['repository']), {})[row['storage_class']] = row
    for (hour, placement, repo), classes in sorted(by_hour.items()):
        if not {'STANDARD', 'EXPRESS_ONEZONE'} <= classes.keys():
            continue
        a, b = classes['STANDARD'], classes['EXPRESS_ONEZONE']
        metrics = {}
        for label in a['probes'].keys() & b['probes'].keys():
            for quantile in ('p50', 'p95', 'p99', 'error_rate', 'timeout_rate'):
                left, right = a['probes'][label].get(quantile), b['probes'][label].get(quantile)
                if left is not None and right is not None:
                    metrics[label + '/' + quantile] = right - left
        if a['rebuild_seconds']['n'] and b['rebuild_seconds']['n']:
            metrics['rebuild/p50_seconds'] = b['rebuild_seconds']['p50'] - a['rebuild_seconds']['p50']
        paired.append(dict(hour=hour, placement=placement, repository=repo, difference=metrics))
    comparisons = {}
    for row in paired:
        for metric, delta in row['difference'].items():
            comparisons.setdefault((row['placement'], row['repository'], metric), []).append(delta)
    variation = []
    for (placement, repo, metric), values in sorted(comparisons.items()):
        median = percentile(values, .5)
        iqr = percentile(values, .75) - percentile(values, .25)
        statement = ('insufficient paired hours (need at least 6)' if len(values) < 6 else
                     'median difference exceeds observed paired-hour IQR' if abs(median) > iqr else
                     'median difference does not exceed observed paired-hour IQR')
        variation.append(dict(placement=placement, repository=repo, metric=metric, paired_hours=len(values),
                              median_difference=median, paired_hour_iqr=iqr, statement=statement))
    for group in groups.values():
        group['rebuild_seconds'] = distribution(group['rebuild_seconds'])
        group['baseline_seconds'] = distribution(group['baseline_seconds'])
        group['probes'] = {key: probe_distribution(value) for key, value in group['probes'].items()}
    return {'schema': 1, 'groups': list(groups.values()), 'hourly': hourly, 'paired': paired,
            'variation': variation, 'schedule': state['iterations']}


def probe_distribution(records):
    count = len(records)
    return {**distribution([r['seconds'] * 1000 for r in records if r['ok']]),
            'attempts': count, 'errors': sum(not r['ok'] for r in records),
            'timeouts': sum(r['timeout'] for r in records),
            'error_rate': sum(not r['ok'] for r in records) / count if count else None,
            'timeout_rate': sum(r['timeout'] for r in records) / count if count else None}


def summarize(directory):
    report = summarize_data(directory)
    def fmt(value):
        return 'n/a' if value is None else f'{value:.3f}'
    def dist(value):
        return '/'.join(fmt(value[q]) for q in ('p50', 'p95', 'p99'))
    lines = ['# Hourly cache comparison', '',
             'Nearest-rank p50/p95/p99. Latency milliseconds include body read and any retry; successful requests only.',
             'Error/timeout rates divide logical failed/timed-out probes by all attempted probes; timeouts are a subset of errors.',
             'Unobserved hours are not successes or network errors. Rebuilds are cold-target Cargo seconds.',
             'Classes label requested tiers; raw hourly JSON retains audit validity/returned classes. Invalid/absent audits are excluded from pairing.',
             'Paired difference = EXPRESS_ONEZONE minus STANDARD; negative latency/time favors EXPRESS_ONEZONE.',
             'Variation rule: absolute median paired difference > paired-hour IQR, with at least 6 pairs.',
             'This descriptive rule is not a significance test or a production-readiness claim.', '',
             '| Class | Placement | Repository | Hours/failed | Rebuild p50/p95/p99 s (n) | Baseline p50/p95/p99 s (n) | Hits/misses/cache errors |',
             '|---|---|---|---|---|---|---|']
    for g in report['groups']:
        lines.append(f"| {g['storage_class']} | {g['placement']} | {g['repository']} | {g['hours']}/{g['failed_hours']} | "
                     f"{dist(g['rebuild_seconds'])} ({g['rebuild_seconds']['n']}) | "
                     f"{dist(g['baseline_seconds'])} ({g['baseline_seconds']['n']}) | {g['hits']}/{g['misses']}/{g['cache_errors']} |")
    lines += ['', '| Class | Placement | Repository | Route/probe | p50/p95/p99 ms | Errors/attempts | Timeouts/attempts | Error/timeout rate |', '|---|---|---|---|---|---|---|---|']
    for g in report['groups']:
        for label, p in sorted(g['probes'].items()):
            lines.append(f"| {g['storage_class']} | {g['placement']} | {g['repository']} | {label} | {dist(p)} | "
                         f"{p['errors']}/{p['attempts']} | {p['timeouts']}/{p['attempts']} | {fmt(p['error_rate'])}/{fmt(p['timeout_rate'])} |")
    lines += ['', '## Schedule (including missing/failed hours)', '', '| Hour | Due UTC | Status | Boat | Server |', '|---|---|---|---|---|']
    for h in report['schedule']:
        due = datetime.fromtimestamp(h['due_at'], timezone.utc).isoformat()
        lines.append(f"| {h['hour']} | {due} | {h['status']} | {h.get('boat', {}).get('complete', 'n/a')} | {h.get('server', {}).get('complete', 'n/a')} |")
    lines += ['', '## Hour-by-hour measurements', '', '| Hour | Class | Placement | Repository | Status | Rebuild p50/p95/p99 s | Hits/misses/errors | Route/probe | p50/p95/p99 ms | Errors/attempts (timeouts) |', '|---|---|---|---|---|---|---|---|---|---|']
    for h in report['hourly']:
        for label, p in h['probes'].items():
            lines.append(f"| {h['hour']} | {h['storage_class']} | {h['placement']} | {h['repository']} | {h['status']} | "
                         f"{dist(h['rebuild_seconds'])} | {h['hits']}/{h['misses']}/{h['cache_errors']} | {label} | {dist(p)} | "
                         f"{p['errors']}/{p['attempts']} ({p['timeouts']}) |")
    lines += ['', '## Paired differences each hour', '', '| Hour | Placement | Repository | Metric (ms unless seconds/rate named) | HP minus Standard |', '|---|---|---|---|---|']
    for row in report['paired']:
        for metric, delta in sorted(row['difference'].items()):
            lines.append(f"| {row['hour']} | {row['placement']} | {row['repository']} | {metric} | {fmt(delta)} |")
    lines += ['', '## Difference versus variation', '']
    for row in report['variation']:
        lines.append(f"- {row['placement']}/{row['repository']}/{row['metric']}: median {fmt(row['median_difference'])}, "
                     f"paired-hour IQR {fmt(row['paired_hour_iqr'])}, {row['paired_hours']} pairs: {row['statement']}.")
    (directory / 'summary.json').write_text(json.dumps(report, indent=2, allow_nan=False) + '\n')
    (directory / 'summary.md').write_text('\n'.join(lines) + '\n')
    print(str(directory / 'summary.md'))


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    mode = result.add_mutually_exclusive_group(required=True)
    mode.add_argument('--prepare', action='store_true')
    mode.add_argument('--run', action='store_true')
    mode.add_argument('--purge', action='store_true', help='delete all objects under the explicit benchmark prefix')
    mode.add_argument('--combine', nargs='+', type=Path, help='combine A/B result files offline into one table')
    mode.add_argument('--summarize', type=Path, help='summarize a scheduler output directory offline')
    result.add_argument('--profile', choices=('legacy', 'fill', 'hourly', 'network', 'baseline'), default='legacy')
    result.add_argument('--baseline', action='store_true', help='include one uncached sample in an hourly matrix')
    result.add_argument('--repository', default='motor', choices=('motor', 'prefrontal', 'broca'))
    result.add_argument('--credentials', type=Path)
    result.add_argument('--backends', type=Path, help='JSON backend manifest; unavailable credential files are skipped')
    result.add_argument('--backend-name')
    result.add_argument('--bucket')
    result.add_argument('--region')
    result.add_argument('--addressing', choices=('path', 'virtual-host'))
    result.add_argument('--storage-class')
    result.add_argument('--prefix', type=prefix_arg)
    result.add_argument('--placement', choices=('A', 'B'))
    result.add_argument('--output', type=Path)
    result.add_argument('--source', type=Path)
    result.add_argument('--commit', help='full 40-character commit SHA, not a path or branch')
    result.add_argument('--cargo-home', type=Path)
    result.add_argument('--toolchain', help='exact installed rustup toolchain name, identical on A and B')
    result.add_argument('--jobs', type=int, default=8)
    result.add_argument('--bundle', type=Path)
    return result


def parse_args(argv=None):
    p = parser()
    args = p.parse_args(argv)
    if args.summarize:
        if any((args.credentials, args.backends, args.prefix, args.placement, args.output, args.source,
                args.commit, args.cargo_home, args.toolchain, args.bundle)):
            p.error('--summarize accepts only the scheduler directory')
        return args
    if (args.profile != 'legacy' or args.baseline) and not args.run:
        p.error('--profile/--baseline require --run')
    if args.baseline and args.profile != 'hourly':
        p.error('--baseline requires --profile hourly')
    if args.combine:
        if not args.output or any((args.credentials, args.backends, args.prefix, args.placement, args.bucket,
                                   args.backend_name, args.region, args.addressing, args.storage_class,
                                   args.source, args.commit, args.cargo_home, args.toolchain, args.bundle)):
            p.error('--combine requires --output NEW_DIR and no credential/measurement options')
        return args
    if args.prepare:
        if not (args.source and args.commit and re.fullmatch('[0-9a-f]{40}', args.commit) and
                args.cargo_home and args.toolchain and args.bundle and 1 <= args.jobs <= 256):
            p.error('--prepare requires --source, --commit FULL_SHA, --cargo-home, --toolchain, --bundle and positive --jobs')
        if any((args.credentials, args.backends, args.backend_name, args.bucket, args.region, args.addressing,
                args.storage_class, args.prefix, args.placement, args.output)):
            p.error('--prepare does not accept measurement/purge options')
    else:
        if not args.prefix or bool(args.backends) == bool(args.credentials):
            p.error('--run/--purge require --prefix and exactly one of --backends FILE or --credentials FILE')
        if args.backends and any((args.backend_name, args.bucket, args.region, args.addressing, args.storage_class)):
            p.error('backend properties belong in the manifest when --backends is used')
        if args.credentials and not all((args.backend_name, args.bucket, args.region, args.addressing)):
            p.error('--credentials requires --backend-name, --bucket, --region and --addressing')
        if args.source or args.commit or args.cargo_home or args.toolchain or args.bundle:
            p.error('preparation options require --prepare')
        if args.run and not (args.output and args.placement):
            p.error('--run requires --placement A|B and --output NEW_DIR')
        if args.purge and (args.output or args.placement):
            p.error('--purge does not accept --output or --placement')
    return args


def main(argv=None):
    os.umask(0o077)
    args = parse_args(argv)
    if args.summarize:
        summarize(args.summarize)
        return True
    if args.combine:
        report = {'schema': 1, 'backends': [], 'complete': True}
        for path in args.combine:
            source = json.loads(path.read_text())
            report['backends'].extend(source['backends'])
            report['complete'] &= source['complete']
        args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        save(args.output, report)
        return True
    if platform.system() != 'Linux':
        raise BenchError('measurements and preparation run on Linux only')
    if args.prepare:
        prepare(args)
        return True
    configs = load_backends(args)
    if args.purge:
        # Materialize before deleting: continuation tokens need not survive deletion.
        deleted = []
        for config in configs:
            if not Path(config['credentials']).exists():
                deleted.append({'name': config['name'], 'status': 'skipped', 'reason': 'credential file unavailable'})
                continue
            s3 = S3(*credentials(Path(config['credentials']), config), config)
            prefix = args.prefix + config['name'] + '/'
            keys = list(s3.objects(prefix))
            for key in keys:
                status, _, _ = s3.request('DELETE', object_path(key, s3.bucket))
                if status != 204:
                    raise BenchError(f'DeleteObject returned HTTP {status}')
            if list(s3.objects(prefix)):
                raise BenchError('prefix still has objects after purge')
            deleted.append({'name': config['name'], 'deleted_objects': len(keys), 'prefix': prefix})
        print(json.dumps(deleted))
        return True
    else:
        return matrix(args, configs)


if __name__ == '__main__':
    try:
        raise SystemExit(0 if main() else 1)
    except (Exception, KeyboardInterrupt) as error:
        print(json.dumps({'error': str(error) if isinstance(error, BenchError) else type(error).__name__}))
        raise SystemExit(1)
