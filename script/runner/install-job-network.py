#!/usr/bin/env python3
"""Operator-only motor_net v1 installer. Default is a read-only diff; --apply installs.

Run as the runner's trusted operator account, not motor-job. This never changes
ufw's input rules or any table other than inet motor_net.
"""
import argparse
import difflib
import json
import pathlib
import re
import subprocess
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
NFT = pathlib.Path('/etc/nftables.d/motor-net.nft')
SYSCTL = pathlib.Path('/etc/sysctl.d/90-motor-net.conf')
LOADER = pathlib.Path('/usr/local/libexec/motor-net-load')
UNIT = pathlib.Path('/etc/systemd/system/motor-net-rules.service')


def run(*args, check=True):
    return subprocess.run(args, check=check, text=True, capture_output=True)


def sudo(*args, check=True):
    return run('/usr/bin/sudo', '-n', *args, check=check)


def permissions():
    for command in json.loads((HERE / 'network-sudo-probes.json').read_text()):
        result = run('/usr/bin/sudo', '-n', '-l', *command, check=False)
        if result.returncode:
            raise SystemExit(f'sudo permission missing: {command}\n{result.stderr}')
    print('PASS: sudo -n accepts all 18 network command forms (permission queries only)')


def prerequisites():
    check_ruleset(json.loads(sudo('/usr/sbin/nft', '-j', 'list', 'table', 'inet', 'motor_net').stdout))
    route = sudo('/usr/sbin/ufw', 'status').stdout
    assert any('mnet-+' in line and 'enp6s0' in line and 'ALLOW FWD' in line for line in route.splitlines()), 'narrow ufw route allowance missing'
    assert pathlib.Path('/proc/sys/net/ipv4/ip_forward').read_text().strip() == '1', 'ip_forward is not 1'
    permissions()
    print('PASS: motor_net loaded and IPv4 forwarding enabled')


def check_ruleset(document):
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
            [interface('iifname', 'enp6s0'), interface('oifname'), address('daddr', pool), states('established', 'related'), {'accept': None}],
            [interface('oifname'), {'counter': None}, {'drop': None}],
            [interface('iifname'), interface('oifname', 'enp6s0'), address('saddr', pool), address('daddr', '@non_public_v4', '!='), states('established', 'related', 'new'), {'accept': None}],
            [interface('iifname'), tcp, reset],
            [interface('iifname'), unreachable],
            [interface('iifname', op='!='), interface('oifname', op='!='), {'counter': None}, {'drop': None}],
        ],
        'postrouting': [[address('saddr', pool), interface('oifname', 'enp6s0'), {'masquerade': None}]],
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
    assert len(sets) == 1, 'motor_net needs exactly one non_public_v4 set'
    ranges = sets[0]
    assert ranges['name'] == 'non_public_v4' and ranges['type'] == 'ipv4_addr' and ranges['flags'] == ['interval'], 'invalid private/special IPv4 set'
    # The versioned nft source is the only maintained range inventory. Captured
    # JSON fixtures contain snapshots, not a second policy list to update.
    source = (HERE / 'motor-net.nft').read_text()
    inventory = re.search(r'set non_public_v4\s*\{.*?elements\s*=\s*\{([^}]*)\}', source, re.S)
    assert inventory, 'versioned IPv4 range inventory missing'
    expected_ranges = set(re.findall(r'\b[0-9.]+/[0-9]+\b', inventory[1]))
    actual_ranges = {f"{element['prefix']['addr']}/{element['prefix']['len']}" for element in ranges['elem']}
    assert actual_ranges == expected_ranges, 'loaded private/special IPv4 ranges differ from versioned source'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--apply', action='store_true')
    mode.add_argument('--check', action='store_true')
    mode.add_argument('--dry-run', '--diff', action='store_true')
    args = parser.parse_args()
    print('motor_net host tooling v1')
    if args.check:
        prerequisites()
        return
    permissions()
    desired = {
        NFT: (HERE / 'motor-net.nft').read_text(),
        SYSCTL: 'net.ipv4.ip_forward=1\n',
        LOADER: '''#!/bin/sh
set -eu
# Delete and recreate only our table in one atomic nft transaction.
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
if /usr/sbin/nft list table inet motor_net >/dev/null 2>&1; then
    printf 'delete table inet motor_net\\n' >"$tmp"
fi
printf 'include "/etc/nftables.d/motor-net.nft"\\n' >>"$tmp"
/usr/sbin/nft --check --file "$tmp"
/usr/sbin/nft --file "$tmp"
''',
        UNIT: '''[Unit]
Description=Motor outbound IPv4 firewall v1
After=network-pre.target
Before=motor-runner.service
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/libexec/motor-net-load
[Install]
WantedBy=multi-user.target
''',
    }
    current = sudo('/usr/sbin/nft', '-j', 'list', 'table', 'inet', 'motor_net', check=False)
    if current.returncode:
        print('Live motor_net is absent; the versioned table will be installed.')
    else:
        try:
            check_ruleset(json.loads(current.stdout))
            print('Live motor_net structurally matches the versioned policy (handles/counters ignored).')
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            print(f'Live motor_net differs from the versioned policy: {error}')
    for path, text in desired.items():
        old = path.read_text() if path.exists() else ''
        print(''.join(difflib.unified_diff(old.splitlines(True), text.splitlines(True),
                                          fromfile=str(path), tofile=f'{path} (desired)')))
    print('ufw route allow in on mnet-+ out on enp6s0')
    print('The priority -10 input chain rejects mnet-* before ufw input permits at priority 0.')
    if not args.apply:
        print('DRY RUN: no host state changed; use --apply to install')
        return
    with tempfile.TemporaryDirectory(prefix='motor-net-install-') as temp:
        for index, (path, text) in enumerate(desired.items()):
            source = pathlib.Path(temp) / str(index)
            source.write_text(text)
            sudo('/usr/bin/install', '-d', '-m', '0755', str(path.parent))
            sudo('/usr/bin/install', '-m', '0755' if path == LOADER else '0644', str(source), str(path))
    sudo('/usr/bin/systemctl', 'daemon-reload')
    sudo('/usr/local/libexec/motor-net-load')
    sudo('/usr/sbin/sysctl', '-q', '-w', 'net.ipv4.ip_forward=1')
    sudo('/usr/sbin/ufw', 'route', 'allow', 'in', 'on', 'mnet-+', 'out', 'on', 'enp6s0')
    sudo('/usr/bin/systemctl', 'enable', '--now', 'motor-net-rules.service')
    prerequisites()
    print('PASS: persistent rules and sysctl installed; ufw input rules unchanged')


if __name__ == '__main__':
    main()
