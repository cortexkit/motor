#!/usr/bin/env python3
"""Operator-only motor_net v2 installer. Default is a read-only diff; --apply installs.

Run --apply/--check as root, never motor-job. UFW input rules and foreign rules
are unchanged; only exact comment-owned tenant route grants are reconciled.
"""
import argparse
import difflib
import json
import os
import pathlib
import re
import subprocess
import tempfile
import sys
import stat
import types
import contextlib

HERE = pathlib.Path(__file__).resolve().parent
sys.dont_write_bytecode = True
# Compile the versioned source directly: ignored/stale bytecode must not replace
# the policy that the read-only checker is supposed to inspect.
tenant_policy = types.ModuleType('motor_network_policy')
tenant_policy.__file__ = str(HERE / 'motor_network_policy.py')
exec(compile((HERE / 'motor_network_policy.py').read_bytes(), tenant_policy.__file__, 'exec'), tenant_policy.__dict__)
NFT = pathlib.Path('/etc/nftables.d/motor-net.nft')
SYSCTL = pathlib.Path('/etc/sysctl.d/90-motor-net.conf')
LOADER = pathlib.Path('/usr/local/libexec/motor-net-load')
UNIT = pathlib.Path('/etc/systemd/system/motor-net-rules.service')
POLICY = pathlib.Path('/usr/local/libexec/motor_network_policy.py')


def run(*args, check=True):
    return subprocess.run(args, check=check, text=True, capture_output=True)


def sudo(*args, check=True):
    return run('/usr/bin/sudo', '-n', *args, check=check)


def permissions():
    if os.geteuid() == 0:
        print('SKIP: root cannot establish non-root runner sudo grants; run --dry-run as the trusted runner account')
        return
    for command in json.loads((HERE / 'network-sudo-probes.json').read_text()):
        result = run('/usr/bin/sudo', '-n', '-l', *command, check=False)
        if result.returncode:
            raise SystemExit(f'sudo permission missing: {command}\n{result.stderr}')
    print('PASS: sudo -n accepts all 18 network command forms (permission queries only)')


def prerequisites(allow_fenced=False, locked=False, host_only=False):
    check_live(allow_fenced=allow_fenced, locked=locked, host_only=host_only)
    check_persistence()
    if not locked:
        tenant_policy.readiness.require_ready()
    permissions()
    print('PASS: motor_net base loaded and IPv4 forwarding enabled' + (' (host-only; tenant health independent)' if host_only else ''))


def check_live(settings=None, *, allow_fenced=False, locked=False, host_only=False):
    """Validate live policy without installing files, changing rules or publishing readiness.

    An explicit settings dict passed to check_live is for pre-deployment
    operator fixtures; production prerequisites still require the trusted
    installed settings and generation.
    """
    assert os.geteuid() == 0, 'run full network --check as root'
    settings = tenant_policy.read_settings() if settings is None else tenant_policy.validate_uplink(settings)
    uplink = settings['uplink']
    with contextlib.nullcontext() if locked else tenant_policy.network_lock():
        actual = check_ruleset(json.loads(sudo('/usr/sbin/nft', '-j', 'list', 'table', 'inet', 'motor_net').stdout), uplink=uplink)
        if not host_only:
            source = (HERE / 'motor-net.nft').read_text()
            document = tenant_policy.table('inet', 'motor_tenant')
            try:
                taps, closed = tenant_policy.tenant_snapshot(document, source, uplink=uplink)
            except (AssertionError, KeyError, TypeError, ValueError):
                if not allow_fenced:
                    raise
                assert not actual, 'unsupported tenant policy must have no delegation'
                print('FENCED: base policy installed; inspect tenant policy separately')
            else:
                owned, ready = tenant_policy.inspect_ufw(uplink=uplink)
                assert not closed and actual == taps == owned == ready, 'tenant delegation/UFW state is closed, stale or incomplete'
    route = sudo('/usr/sbin/ufw', 'status').stdout
    assert any('mnet-+' in line and uplink in line and 'ALLOW FWD' in line for line in route.splitlines()), 'narrow ufw route allowance missing'
    assert pathlib.Path('/proc/sys/net/ipv4/ip_forward').read_text().strip() == '1', 'ip_forward is not 1'


def check_ruleset(document, tenant_taps=None, *, uplink):
    """Validate nft's expression tree, ignoring handles and live counter values."""
    def match(left, right, op='=='):
        return {'match': {'op': op, 'left': left, 'right': right}}
    def interface(key, value='mnet-*', op='=='):
        return match({'meta': {'key': key}}, value, op)
    def address(field, value, op='=='):
        return match({'payload': {'protocol': 'ip', 'field': field}}, value, op)
    def states(*values):
        return match({'ct': {'key': 'state'}}, sorted(values), 'in')
    def normalized(expressions):
        result = []
        for expression in expressions:
            if 'counter' in expression:
                result.append({'counter': None})
            else:
                expression = json.loads(json.dumps(expression))
                matcher = expression.get('match', {})
                if matcher.get('left') == {'ct': {'key': 'state'}}:
                    matcher['right'] = sorted(matcher['right'])
                result.append(expression)
        return result
    pool = {'prefix': {'addr': '10.88.0.0', 'len': 16}}
    tcp = match({'meta': {'key': 'l4proto'}}, 'tcp')
    reset = {'reject': {'type': 'tcp reset'}}
    unreachable = {'reject': {'type': 'icmpx', 'expr': 'port-unreachable'}}
    expected_rules = {
        'input': [[interface('iifname'), tcp, reset], [interface('iifname'), unreachable]],
        'forward': [
            [interface('iifname', uplink), interface('oifname'), address('daddr', pool), states('established', 'related'), {'accept': None}],
            [interface('oifname'), {'counter': None}, {'drop': None}],
            [interface('iifname'), interface('oifname', uplink), address('saddr', pool), address('daddr', '@non_public_v4', '!='), states('established', 'related', 'new'), {'accept': None}],
            [interface('iifname'), tcp, reset],
            [interface('iifname'), unreachable],
            [interface('iifname', op='!='), interface('oifname', op='!='), interface('iifname', '@tenant_taps', '!='), interface('oifname', '@tenant_taps', '!='), {'counter': None}, {'drop': None}],
        ],
        'postrouting': [[address('saddr', pool), interface('oifname', uplink), {'masquerade': None}]],
    }
    expected_chains = {
        'input': ('filter', 'input', -10, 'accept'),
        'forward': ('filter', 'forward', -10, 'accept'),
        'postrouting': ('nat', 'postrouting', 100, 'accept'),
    }
    tables, chains, sets = [], {}, []
    rules = {name: [] for name in expected_rules}
    for entry in document['nftables']:
        if 'metainfo' in entry:
            continue
        if 'table' in entry:
            tables.append((entry['table']['family'], entry['table']['name']))
            continue
        kind = next(iter(entry))
        assert kind in {'chain', 'set', 'rule'}, f'unexpected nft object: {kind}'
        item = entry[kind]
        assert item['family'] == 'inet' and item['table'] == 'motor_net', 'wrong table scope'
        if kind == 'chain':
            assert item['name'] not in chains, 'duplicate chain'
            chains[item['name']] = tuple(item[key] for key in ['type', 'hook', 'prio', 'policy'])
        elif kind == 'set':
            sets.append(item)
        else:
            assert item['chain'] in rules, 'unexpected rule chain'
            rules[item['chain']].append(normalized(item['expr']))
    assert tables == [('inet', 'motor_net')], 'wrong motor_net table'
    assert chains == expected_chains, 'motor_net chain hooks, priorities or policies differ'
    for name, expected in expected_rules.items():
        assert rules[name] == expected, f'motor_net {name} rule predicates, order or verdicts differ'
    assert len(sets) == 2 and {s['name'] for s in sets} == {'non_public_v4', 'tenant_taps'}, 'motor_net needs exact private and tenant TAP sets'
    by_name = {s['name']: s for s in sets}
    taps = tenant_policy.exact_taps(by_name['tenant_taps'])
    if tenant_taps is not None:
        assert taps == tenant_taps, 'tenant TAP delegation differs from guarded policy'
    ranges = by_name['non_public_v4']
    assert ranges['name'] == 'non_public_v4' and ranges['type'] == 'ipv4_addr' and ranges['flags'] == ['interval'], 'invalid private/special IPv4 set'
    # The versioned nft source is the only maintained range inventory. Captured
    # JSON fixtures contain snapshots, not a second policy list to update.
    source = (HERE / 'motor-net.nft').read_text()
    inventory = re.search(r'set non_public_v4\s*\{.*?elements\s*=\s*\{([^}]*)\}', source, re.S)
    assert inventory, 'versioned IPv4 range inventory missing'
    expected_ranges = set(re.findall(r'\b[0-9.]+/[0-9]+\b', inventory[1]))
    actual_ranges = {f"{element['prefix']['addr']}/{element['prefix']['len']}" for element in ranges['elem']}
    assert actual_ranges == expected_ranges, 'loaded private/special IPv4 ranges differ from versioned source'
    return taps


def desired_files(settings):
    tenant_policy.checked_settings(settings)
    return {
        tenant_policy.SETTINGS: json.dumps(settings, indent=2) + '\n',
        tenant_policy.TEMPLATE: (HERE / 'motor-net.nft').read_text(),
        NFT: tenant_policy.render_source((HERE / 'motor-net.nft').read_text(), settings['uplink']),
        SYSCTL: 'net.ipv4.ip_forward=1\n',
        POLICY: (HERE / 'motor_network_policy.py').read_text(),
        POLICY.with_name('motor_network_ready.py'): (HERE / 'motor_network_ready.py').read_text(),
        LOADER: '''#!/usr/bin/python3 -I
import os, runpy
try:
    os.unlink('/run/motor-net-ready')
except FileNotFoundError:
    pass
runpy.run_path('/usr/local/libexec/motor_network_policy.py')['load']()
''',
        UNIT: '''[Unit]
Description=Motor outbound IPv4 firewall v2 with exact tenant delegation
Wants=network-online.target
After=network-online.target
Before=motor-runner.service
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStartPre=/usr/bin/rm -f /run/motor-net-ready
ExecStart=/usr/local/libexec/motor-net-load
[Install]
WantedBy=multi-user.target
''',
    }


def verify_files(expected, owner=0):
    for path, (text, mode) in expected.items():
        metadata = path.lstat()
        assert stat.S_ISREG(metadata.st_mode) and metadata.st_uid == owner and metadata.st_nlink == 1, f'unsafe installed network file: {path}'
        assert stat.S_IMODE(metadata.st_mode) == mode, f'wrong installed network mode: {path}'
        assert path.read_bytes() == text.encode(), f'installed network file differs: {path}'


def check_persistence():
    for source, target in [(HERE / 'motor-net.nft', NFT), (HERE / 'motor_network_policy.py', POLICY)]:
        assert not target.exists() or not os.path.samefile(source, target), 'run checker from versioned source, not the installed file itself'
    verify_files({path: (text, 0o755 if path == LOADER else 0o644) for path, text in desired_files(tenant_policy.read_settings()).items()})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--apply', action='store_true')
    mode.add_argument('--check', action='store_true')
    mode.add_argument('--dry-run', '--diff', action='store_true')
    parser.add_argument('--uplink', help='explicit host uplink; required except with --check')
    parser.add_argument('--public-ip', help='public IPv4 carried by the uplink')
    parser.add_argument('--host-only', action='store_true', help='with --check, verify host job networking independently of tenant health')
    args = parser.parse_args()
    print('motor_net host tooling v2')
    if args.check:
        if args.uplink is not None or args.public_ip is not None:
            parser.error('--check reads the installed network settings')
        prerequisites(host_only=args.host_only)
        return
    if args.host_only:
        parser.error('--host-only is a --check option')
    if args.apply:
        assert os.geteuid() == 0, 'run network --apply as root'
    if args.uplink is None or args.public_ip is None:
        parser.error('--uplink and --public-ip are required')
    settings = tenant_policy.validate_uplink({'uplink': args.uplink, 'public_ipv4': args.public_ip})
    uplink = settings['uplink']
    helper = pathlib.Path('/etc/motor-vm-helper.json')
    if helper.exists():
        config = json.loads(helper.read_text())
        assert all(config.get(key) == value for key, value in settings.items()), 'network settings differ from helper config'
    permissions()
    desired = desired_files(settings)
    current = sudo('/usr/sbin/nft', '-j', 'list', 'table', 'inet', 'motor_net', check=False)
    if current.returncode:
        print('Live motor_net is absent; the versioned table will be installed.')
    else:
        try:
            check_ruleset(json.loads(current.stdout), uplink=uplink)
            print('Live motor_net structurally matches the versioned policy (handles/counters ignored).')
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            print(f'Live motor_net differs from the versioned policy: {error}')
    for path, text in desired.items():
        old = path.read_text() if path.exists() else ''
        print(''.join(difflib.unified_diff(old.splitlines(True), text.splitlines(True),
                                          fromfile=str(path), tofile=f'{path} (desired)')))
    print(f'ufw route allow in on mnet-+ out on {uplink}')
    print('The priority -10 input chain rejects mnet-* before ufw input permits at priority 0.')
    if not args.apply:
        print('DRY RUN: no host state changed; use --apply to install')
        return
    install_policy(desired, settings)
    sudo('/usr/bin/systemctl', 'enable', '--now', 'motor-net-rules.service')
    prerequisites(host_only=True)
    print('Persistent rules/sysctl installed; run --check after helper reconciliation. UFW input rules unchanged.')


def install_policy(desired, settings):
    with tenant_policy.network_lock(mutate=True):
        tenant_policy.readiness.invalidate()
        loaded = None
        try:
            with tempfile.TemporaryDirectory(prefix='motor-net-install-') as temp:
                for index, (path, text) in enumerate(desired.items()):
                    source = pathlib.Path(temp) / str(index)
                    source.write_text(text)
                    sudo('/usr/bin/install', '-d', '-m', '0755', str(path.parent))
                    sudo('/usr/bin/install', '-m', '0755' if path == LOADER else '0644', str(source), str(path))
            tenant_policy.checked_source(settings)
            sudo('/usr/bin/systemctl', 'daemon-reload')
            sudo('/usr/sbin/sysctl', '-q', '-w', 'net.ipv4.ip_forward=1')
            sudo('/usr/sbin/ufw', 'route', 'allow', 'in', 'on', 'mnet-+', 'out', 'on', settings['uplink'])
            loaded = tenant_policy.load_locked(publish=False)
            prerequisites(locked=True, host_only=True)
            tenant_policy.validate_uplink(settings)
            tenant_policy.readiness.publish()
        except BaseException:
            tenant_policy.readiness.invalidate()
            if loaded is not None and loaded[1]:
                tenant_policy.apply(tenant_policy.members('motor_tenant', 'closed_taps', loaded[1])
                                    + tenant_policy.members('motor_net', 'tenant_taps', set()))
            raise


if __name__ == '__main__':
    main()
