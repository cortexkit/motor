#!/usr/bin/env python3
"""Assertions run as motor-job inside a real opt-in runner namespace.

A timeout is never evidence of isolation. UDP uses a connected socket so the
kernel delivers the ICMP rejection to recv, but still sends with sendto.
The operator harness runs host-addresses outside the job namespace and saves
its result as network-host-addresses.fixture before submitting isolation probes.
"""
import errno
import ipaddress
import json
import os
import pathlib
import socket
import subprocess
import sys
import time

ALLOWED_TCP_ERRORS = {errno.ECONNREFUSED, errno.EHOSTUNREACH, errno.ENETUNREACH, errno.EACCES}


def blocked_tcp(host, port, family=socket.AF_INET, exact=None):
    with socket.socket(family, socket.SOCK_STREAM) as sock:
        sock.settimeout(3)
        started = time.monotonic()
        try:
            sock.connect((host, port))
        except OSError as error:
            assert not isinstance(error, (TimeoutError, socket.timeout)), f'{host}:{port}: 3-second timeout is a FAILURE'
            assert error.errno is not None, f'{host}:{port}: missing kernel errno'
            assert error.errno in ({exact} if exact is not None else ALLOWED_TCP_ERRORS), f'{host}:{port}: {error}'
            assert time.monotonic() - started < 3, f'{host}:{port}: rejection exceeded 3 seconds'
            print(f'blocked TCP {host}:{port}: {errno.errorcode[error.errno]}', flush=True)
            return
        raise AssertionError(f'NETWORK LEAK: TCP connected to {host}:{port}')


def blocked_udp(host, port):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.settimeout(3)
        sock.connect((host, port))
        sock.sendto(b'\x12\x34\x01\x00' + b'\x00' * 8, (host, port))
        started = time.monotonic()
        try:
            data = sock.recv(1024)
        except OSError as error:
            assert not isinstance(error, (TimeoutError, socket.timeout)), f'{host}:{port}: UDP timeout is a FAILURE'
            assert error.errno == errno.ECONNREFUSED, f'{host}:{port}: UDP requires ECONNREFUSED, got {error}'
            assert time.monotonic() - started < 3, f'{host}:{port}: UDP rejection exceeded 3 seconds'
            print(f'blocked UDP {host}:{port}: ECONNREFUSED', flush=True)
            return
        raise AssertionError(f'NETWORK LEAK: UDP received {data!r} from {host}:{port}')


def command(*args, env=None):
    return subprocess.check_output(args, text=True, env=env, timeout=90)


def public():
    assert command('id', '-un').strip() == 'motor-job'
    assert command('getent', 'ahostsv4', 'registry.npmjs.org').strip()
    command('curl', '-4', '--fail', '--silent', '--show-error', '--max-time', '20',
            'https://registry.npmjs.org/lodash')
    resolver = pathlib.Path('/etc/resolv.conf')
    assert resolver.read_text() == 'nameserver 1.1.1.1\nnameserver 9.9.9.9\n'
    try:
        resolver.write_text('nameserver 127.0.0.53\n')
    except OSError as error:
        assert error.errno in {errno.EROFS, errno.EACCES}, error
    else:
        raise AssertionError('resolver is writable')
    print('PASS public HTTPS, DNS and read-only public resolver', flush=True)


def discover_host_addresses():
    addresses = {}
    for family, destination in [('4', '1.1.1.1'), ('6', '2606:4700:4700::1111')]:
        route = json.loads(command('ip', '-j', '-' + family, 'route', 'get', destination))[0]
        address = route['prefsrc']
        parsed = ipaddress.ip_address(address)
        assert parsed.version == int(family) and parsed.is_global, f'not a public host source address: {address}'
        addresses[family] = address
    return addresses


def host_address(family):
    address = json.loads(pathlib.Path('network-host-addresses.fixture').read_text())[family]
    parsed = ipaddress.ip_address(address)
    assert parsed.version == int(family) and parsed.is_global, f'not a public host address: {address}'
    return address


def input_blocked():
    gateway = json.loads(command('ip', '-j', '-4', 'route', 'show', 'default'))[0]['gateway']
    for host, port in [(host_address('4'), 22), (gateway, 22), (gateway, 53)]:
        blocked_tcp(host, port)
    blocked_udp(gateway, 53)


def forbidden():
    for host, port in [('127.0.0.53', 53), ('127.0.0.54', 53), ('127.0.0.1', 323),
                       ('10.0.0.1', 22), ('192.168.0.1', 22), ('169.254.169.254', 80)]:
        blocked_tcp(host, port)
    for host, port in [('127.0.0.53', 53), ('127.0.0.54', 53), ('127.0.0.1', 323)]:
        blocked_udp(host, port)
    blocked_tcp(host_address('6'), 22, socket.AF_INET6, errno.ENETUNREACH)


def environment(abstract_name):
    assert command('id', '-un').strip() == 'motor-job'
    for family, host in [(socket.AF_INET, '127.0.0.1'), (socket.AF_INET6, '::1')]:
        with socket.socket(family) as sock:
            sock.bind((host, 443))
            sock.listen()
    names = {link['ifname'].split('@')[0] for link in json.loads(command('ip', '-j', 'link'))}
    assert names == {'lo', 'eth0'}, f'host interfaces visible: {names}'
    v6 = json.loads(command('ip', '-j', '-6', 'addr', 'show'))
    assert all(addr['local'] == '::1' for link in v6 for addr in link.get('addr_info', [])), v6
    routes = json.loads(command('ip', '-j', '-4', 'route'))
    assert all(route['dev'] == 'eth0' for route in routes), routes
    with socket.socket(socket.AF_UNIX) as sock:
        try:
            sock.connect('\0' + abstract_name)
        except OSError as error:
            assert error.errno == errno.ECONNREFUSED, error
        else:
            raise AssertionError('host abstract Unix socket is reachable')
    assert os.environ['CARGO_NET_OFFLINE'] == 'true'
    for name in ['registry', 'git']:
        path = pathlib.Path(os.environ['CARGO_HOME']) / name / 'network-write-control'
        assert path.parent.is_dir(), f'missing shared Cargo {name} tree'
        try:
            path.write_text('forbidden')
        except OSError as error:
            assert error.errno in {errno.EROFS, errno.EACCES}, error
        else:
            path.unlink()
            raise AssertionError(f'shared Cargo {name} is writable')
    print('PASS loopback 443, private interfaces/routes/abstract sockets and Cargo defaults', flush=True)


def fetch():
    root = pathlib.Path('/motor-home/fetch-fixture')
    (root / 'src').mkdir(parents=True)
    (root / 'src/lib.rs').write_text('')
    (root / 'Cargo.toml').write_text('''[package]
name = "network-fetch-fixture"
version = "0.0.0"
edition = "2021"
[dependencies]
itoa = { git = "https://github.com/dtolnay/itoa.git" }
''')
    env = dict(os.environ, CARGO_HOME='/motor-home/cargo-net', CARGO_NET_OFFLINE='false')
    command('cargo', 'fetch', '--manifest-path', str(root / 'Cargo.toml'), env=env)
    assert (root / 'Cargo.lock').is_file()
    print('PASS unlocked public git Cargo fetch with private CARGO_HOME', flush=True)


def offline():
    assert not pathlib.Path('/etc/resolv.conf').exists(), 'offline job inherited a resolver'
    blocked_tcp('1.1.1.1', 443, exact=errno.ENETUNREACH)
    names = {link['ifname'] for link in json.loads(command('ip', '-j', 'link'))}
    assert names == {'lo'}, names
    print('PASS offline control: no resolver, only loopback, ENETUNREACH', flush=True)


def measure():
    bun_version = command('bun', '--version').strip()
    print(f'Bun {bun_version}', flush=True)
    root = pathlib.Path('bun-public-fixture')
    root.mkdir()
    (root / 'package.json').write_bytes(pathlib.Path('network-bun-package.fixture').read_bytes())
    (root / 'bun.lock').write_bytes(pathlib.Path('network-bun-lock.fixture').read_bytes())
    initial_lock = (root / 'bun.lock').read_bytes()
    env = dict(os.environ, BUN_INSTALL_CACHE_DIR='/motor-home/bun-net-empty-cache')
    def received():
        return json.loads(command('ip', '-s', '-j', 'link', 'show', 'eth0'))[0]['stats64']['rx']['bytes']
    before = received()
    started = time.monotonic()
    result = command('bun', 'install', '--ignore-scripts', '--frozen-lockfile', '--cwd', str(root), env=env)
    elapsed = time.monotonic() - started
    fetched = received() - before
    lock = root / 'bun.lock'
    assert lock.read_bytes() == initial_lock, 'frozen public fixture lockfile changed'
    # Bun's output is retained alongside the parsed lockfile inventory.
    print(result, flush=True)
    packages = json.loads(command('bun', '-e',
        'const s=await Bun.file(process.argv[1]).text(); const l=JSON.parse(s.replace(/,\\s*([}\\]])/g,"$1")); console.log(JSON.stringify(Object.keys(l.packages).length));', str(lock)))
    assert packages >= 200, 'public install fixture must resolve a few hundred packages'
    print(json.dumps({'lockfile_path': str(lock), 'fixture_source': 'elysiajs/elysia@dafd31b516089e1f6844f98dddaceefd0436da23', 'bun_version': bun_version, 'resolved_packages': packages,
                      'bytes_fetched': fetched, 'wall_seconds': elapsed, 'MB_per_second': fetched / elapsed / 1e6,
                      'byte_definition': 'eth0 RX delta during install, including DNS/TLS/HTTP overhead'}), flush=True)


def inbound_listener():
    with socket.socket() as sock:
        sock.bind(('0.0.0.0', 18080))
        sock.listen()
        sock.settimeout(10)
        pathlib.Path('inbound-ready').write_text('ready\n')
        try:
            connection, address = sock.accept()
        except socket.timeout:
            print('PASS no unsolicited connection reached the job listener', flush=True)
            return
        connection.close()
        raise AssertionError(f'NETWORK LEAK: unsolicited inbound connection from {address}')


def main():
    mode = sys.argv[1]
    if mode == 'host-addresses':
        print(json.dumps(discover_host_addresses()))
        return
    {'public': public, 'input': input_blocked, 'forbidden': forbidden, 'fetch': fetch,
     'offline': offline, 'measure': measure, 'inbound-listener': inbound_listener}.get(mode, lambda: environment(sys.argv[2]))()


if __name__ == '__main__':
    main()
