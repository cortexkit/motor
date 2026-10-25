"""Root-owned tenant delegation/UFW reconciliation used by the network installer.

No client supplies interfaces, commands or paths. Runtime membership comes only
from a structurally validated motor_tenant policy; a failed step leaves its TAP
closed before an exemption can be published.
"""
import contextlib
import fcntl
import ipaddress
import json
import os
import pathlib
import re
import shlex
import signal
import stat
import subprocess
import types

LOCK = pathlib.Path('/run/motor-network/lock')
SOURCE = pathlib.Path('/etc/nftables.d/motor-net.nft')
SETTINGS = pathlib.Path('/etc/motor-network.json')
TEMPLATE = pathlib.Path('/usr/local/libexec/motor-net-template.nft')
readiness = types.ModuleType('motor_network_ready')
exec(compile(pathlib.Path(__file__).with_name('motor_network_ready.py').read_bytes(),
             'motor_network_ready.py', 'exec'), readiness.__dict__)


def checked_settings(value):
    if not isinstance(value, dict) or set(value) != {'uplink', 'public_ipv4'}:
        raise ValueError('network settings require exactly uplink and public_ipv4')
    uplink = value['uplink']
    if not isinstance(uplink, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,14}', uplink) or uplink in {'any', 'lo'}:
        raise ValueError(f'invalid uplink {uplink!r}')
    address = ipaddress.IPv4Address(value['public_ipv4'])
    if not address.is_global or address.is_multicast:
        raise ValueError(f'uplink {uplink} needs a public IPv4 address')
    return value


def validate_uplink(settings, addresses=None):
    checked_settings(settings)
    uplink = settings['uplink']
    if addresses is None:
        try:
            addresses = json.loads(command('/usr/sbin/ip', '-j', '-4', 'address', 'show', 'dev', uplink))
        except (OSError, RuntimeError, ValueError) as error:
            raise ValueError(f'uplink {uplink} is missing or unreadable: {error}') from error
    if (not isinstance(addresses, list) or len(addresses) != 1 or addresses[0].get('ifname') != uplink
            or addresses[0].get('operstate') != 'UP' or 'UP' not in addresses[0].get('flags', [])
            or not any(item.get('family') == 'inet' and item.get('scope') == 'global'
                       and item.get('local') == settings['public_ipv4'] for item in addresses[0].get('addr_info', []))):
        raise ValueError(f"uplink {uplink} must exist, be UP and carry public IPv4 {settings['public_ipv4']}")
    return settings


def read_settings(check_link=True):
    fd = os.open(SETTINGS, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd) as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o022:
            raise ValueError('unsafe /etc/motor-network.json')
        text = source.read(4097)
    if len(text) > 4096:
        raise ValueError('network settings exceed read bound')
    settings = checked_settings(json.loads(text))
    return validate_uplink(settings) if check_link else settings


def render_source(source, uplink):
    # Keep the historical file as the shared host/guest template. Its three
    # interface tokens are replaced only with the explicit, validated host name.
    if not isinstance(uplink, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,14}', uplink) or uplink in {'any', 'lo'}:
        raise ValueError(f'invalid uplink {uplink!r}')
    if source.count('"enp6s0"') != 3:
        raise ValueError('network template must contain exactly three uplink tokens')
    return source.replace('"enp6s0"', f'"{uplink}"')


def install_settings(source):
    settings = checked_settings(json.loads(source.read_text()))
    with network_lock(mutate=True):
        readiness.invalidate()
        temporary = SETTINGS.with_name('.motor-network-settings-' + os.urandom(16).hex())
        try:
            fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
            with os.fdopen(fd, 'w') as output:
                os.fchmod(output.fileno(), 0o644)
                output.write(json.dumps(settings, indent=2) + '\n')
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, SETTINGS)
        finally:
            temporary.unlink(missing_ok=True)


def command(*args, input=None):
    process = subprocess.Popen(args, text=True, stdin=subprocess.PIPE if input is not None else subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
                               env={**os.environ, 'LC_ALL': 'C'})
    try:
        stdout, stderr = process.communicate(input, timeout=20)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise RuntimeError(f'firewall command timed out: {args[0]}')
    if process.returncode:
        raise RuntimeError(f'firewall command failed: {args[0]} ({process.returncode}): {stderr[:512]}')
    if len(stdout) > 4 * 1024 * 1024:
        raise RuntimeError('firewall snapshot exceeds read bound')
    return stdout


@contextlib.contextmanager
def network_lock(mutate=False):
    if mutate:
        assert os.geteuid() == 0, 'network loader requires root'
        LOCK.parent.mkdir(mode=0o700, exist_ok=True)
    if not LOCK.parent.exists():
        yield
        return
    directory = LOCK.parent.lstat()
    assert stat.S_ISDIR(directory.st_mode) and directory.st_uid == 0 and stat.S_IMODE(directory.st_mode) == 0o700, 'unsafe network lock directory'
    try:
        fd = os.open(LOCK, (os.O_RDWR | os.O_CREAT if mutate else os.O_RDONLY) | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    except FileNotFoundError:
        assert not mutate
        yield
        return
    try:
        metadata = os.fstat(fd)
        assert stat.S_ISREG(metadata.st_mode) and metadata.st_uid == 0 and metadata.st_nlink == 1 and stat.S_IMODE(metadata.st_mode) == 0o600, 'unsafe network lock file'
        fcntl.flock(fd, fcntl.LOCK_EX if mutate else fcntl.LOCK_SH)
        yield
    finally:
        os.close(fd)


def tap_index(tap):
    assert re.fullmatch(r'mtap[1-9][0-9]{0,2}', tap), 'tenant TAP must be exact, not a wildcard'
    index = int(tap[4:])
    assert 0 < index < 255 and tap == f'mtap{index}', 'invalid tenant TAP'
    return index


def match(left, right, op='=='):
    return {'match': {'op': op, 'left': left, 'right': right}}


def interface(key, value, op='=='):
    return match({'meta': {'key': key}}, value, op)


def address(field, value, op='=='):
    return match({'payload': {'protocol': 'ip', 'field': field}}, value, op)


def states(*values):
    return match({'ct': {'key': 'state'}}, sorted(values), 'in')


def normalized(expressions):
    result = []
    typed_ipv4 = False
    for expression in expressions:
        assert isinstance(expression, dict) and isinstance(expression.get('match', {}), dict), 'invalid rule expression'
        left = expression.get('match', {}).get('left')
        if isinstance(left, dict) and isinstance(left.get('payload'), dict):
            typed_ipv4 |= left['payload'].get('protocol') == 'ip'
    for expression in expressions:
        expression = json.loads(json.dumps(expression))
        # nft may omit a redundant positive family dependency when an `ip`
        # payload already enforces IPv4. Do not discard other predicates.
        if typed_ipv4 and expression == match({'meta': {'key': 'nfproto'}}, 'ipv4'):
            continue
        if 'counter' in expression and not isinstance(expression['counter'], str):
            expression['counter'] = None
        matcher = expression.get('match', {})
        if matcher.get('left') == {'ct': {'key': 'state'}}:
            matcher['right'] = sorted(matcher['right'])
        result.append(expression)
    return result


def exact_taps(value):
    assert value.get('type') == 'ifname' and not value.get('flags'), 'unsafe tenant TAP set schema'
    elements = value.get('elem', [])
    assert isinstance(elements, list) and all(isinstance(tap, str) for tap in elements) and len(elements) == len(set(elements)), 'invalid tenant TAP elements'
    for tap in elements:
        tap_index(tap)
    return set(elements)


def private_ranges(source):
    inventory = re.search(r'set non_public_v4\s*\{.*?elements\s*=\s*\{([^}]*)\}', source, re.S)
    assert inventory, 'versioned private range inventory missing'
    return set(re.findall(r'\b[0-9.]+/[0-9]+\b', inventory[1]))


def check_ranges(value, source):
    assert value['type'] == 'ipv4_addr' and value.get('flags') == ['interval'], 'invalid private range set schema'
    actual = {f"{item['prefix']['addr']}/{item['prefix']['len']}" for item in value['elem']}
    assert set(ipaddress.collapse_addresses(map(ipaddress.ip_network, actual))) == set(ipaddress.collapse_addresses(map(ipaddress.ip_network, private_ranges(source)))), 'private ranges differ'


def live_grants(text, ipv6=False, *, uplink):
    grants = set()
    chain = 'ufw6-user-forward' if ipv6 else 'ufw-user-forward'
    for line in text.splitlines():
        words = shlex.split(line)
        if words[:2] != ['-A', chain] or not any(word.startswith('mtap') for word in words):
            continue
        assert not ipv6, 'extra IPv6 tenant grant'
        assert len(words[2:]) % 2 == 0, 'invalid tenant UFW predicates'
        pairs = list(zip(words[2::2], words[3::2]))
        values = dict(pairs)
        assert len(values) == len(pairs) and set(values) == {'-i', '-o', '-s', '-j'}, 'tenant UFW grant needs exact source and direction'
        tap = values['-i']
        index = tap_index(tap)
        assert values['-o'] == uplink and values['-j'] == 'ACCEPT', 'extra inbound or wrong uplink grant'
        assert values['-s'].removesuffix('/32') == f'10.200.{index}.2', 'tenant source restriction missing'
        assert tap not in grants, 'duplicate tenant UFW grant'
        grants.add(tap)
    return grants


def owned_grants(text, ipv6=False, *, uplink):
    grants, pending = set(), None
    for line in text.splitlines():
        if line.startswith('### tuple ### '):
            assert pending is None, 'owned UFW tuple has no rule'
            pending = None
            words = line.removeprefix('### tuple ### ').split()
            encoded = next((word.removeprefix('comment=') for word in words if word.startswith('comment=')), None)
            comment = bytes.fromhex(encoded).decode() if encoded else ''
            if comment.startswith('motor-tenant-'):
                tap = comment.removeprefix('motor-tenant-')
                index = tap_index(tap)
                assert not ipv6 and len(words) == 8 and words[:5] == ['route:allow', 'any', 'any', '0.0.0.0/0', 'any'], 'malformed owned UFW tuple'
                assert words[5].removesuffix('/32') == f'10.200.{index}.2' and words[6] == f'in_{tap}!out_{uplink}', 'owned UFW source/direction differs'
                pending = tap
        elif line.startswith('-A '):
            actual = live_grants(line, ipv6, uplink=uplink)
            if pending is None:
                assert not actual, 'unowned tenant UFW grant'
            else:
                assert actual == {pending} and pending not in grants, 'owned UFW tuple/rule mismatch'
                grants.add(pending)
            pending = None
    assert pending is None, 'owned UFW tuple has no rule'
    return grants


def read_owned(path, required=False):
    if not path.exists():
        assert not required, 'UFW IPv4 persistent policy missing'
        return ''
    metadata = path.lstat()
    assert stat.S_ISREG(metadata.st_mode) and metadata.st_uid == 0 and not metadata.st_mode & 0o022, 'unsafe UFW policy file'
    with path.open() as file:
        text = file.read(2 * 1024 * 1024 + 1)
    assert len(text) <= 2 * 1024 * 1024, 'UFW policy exceeds read bound'
    return text


def inspect_ufw(*, uplink):
    owned = owned_grants(read_owned(pathlib.Path('/etc/ufw/user.rules'), required=True), uplink=uplink)
    owned_grants(read_owned(pathlib.Path('/etc/ufw/user6.rules')), True, uplink=uplink)
    status = command('/usr/sbin/ufw', 'status').splitlines()
    active = 'Status: active' in status
    assert active or 'Status: inactive' in status, 'unrecognized UFW status'
    if not active:
        return owned, owned.copy()
    live = live_grants(command('/usr/sbin/iptables', '-S', 'ufw-user-forward'), uplink=uplink)
    before = command('/usr/sbin/iptables', '-S', 'ufw-before-forward')
    assert any(shlex.split(line) in [
        ['-A', 'ufw-before-forward', '-m', 'conntrack', '--ctstate', order, '-j', 'ACCEPT']
        for order in ['ESTABLISHED,RELATED', 'RELATED,ESTABLISHED']
    ] for line in before.splitlines()), 'UFW established/related return support missing'
    defaults = pathlib.Path('/etc/default/ufw').read_text()
    if re.search(r'^IPV6=[\"\']?yes[\"\']?\s*$', defaults, re.M):
        read_owned(pathlib.Path('/etc/ufw/user6.rules'), required=True)
        live_grants(command('/usr/sbin/ip6tables', '-S', 'ufw6-user-forward'), True, uplink=uplink)
    assert live <= owned, 'live tenant UFW grant is not comment-owned'
    return owned, live & owned


def ufw_arguments(tap, delete=False, *, uplink):
    index = tap_index(tap)
    return (['--force', 'delete'] if delete else []) + ['route', 'allow', 'in', 'on', tap, 'out', 'on', uplink, 'from', f'10.200.{index}.2', 'to', 'any', 'comment', f'motor-tenant-{tap}']


def reconcile_ufw(desired, prior, *, uplink):
    owned, ready = prior
    for tap in sorted(owned - desired):
        command('/usr/sbin/ufw', *ufw_arguments(tap, True, uplink=uplink))
    for tap in sorted(desired - owned):
        command('/usr/sbin/ufw', *ufw_arguments(tap, uplink=uplink))
    for tap in sorted((desired & owned) - ready):
        command('/usr/sbin/ufw', *ufw_arguments(tap, True, uplink=uplink))
        command('/usr/sbin/ufw', *ufw_arguments(tap, uplink=uplink))
    state = inspect_ufw(uplink=uplink)
    assert state == (desired, desired), 'tenant UFW grants are not live and exact'
    return state


def tenant_snapshot(document, source, *, uplink):
    if document is None:
        return set(), set()
    tables, sets, chains, counters, rules = [], {}, {}, set(), {}
    for entry in document['nftables']:
        if 'metainfo' in entry:
            continue
        kind = next(iter(entry))
        item = entry[kind]
        if kind == 'table':
            tables.append((item['family'], item['name']))
            continue
        assert item['family'] == 'inet' and item['table'] == 'motor_tenant', 'wrong tenant policy scope'
        if kind == 'set':
            assert item['name'] not in sets, 'duplicate tenant set'
            sets[item['name']] = item
        elif kind == 'chain':
            assert item['name'] not in chains, 'duplicate tenant chain'
            chains[item['name']] = tuple(item[key] for key in ['type', 'hook', 'prio', 'policy'])
        elif kind == 'counter':
            assert item['name'] not in counters, 'duplicate tenant counter'
            counters.add(item['name'])
        else:
            assert kind == 'rule', 'unexpected tenant object'
            rules.setdefault(item['chain'], []).append(normalized(item['expr']))
    assert tables == [('inet', 'motor_tenant')], 'wrong tenant table'
    assert set(sets) == {'tenant_taps', 'closed_taps', 'non_public_v4'}, 'unsupported tenant policy sets'
    taps, closed = exact_taps(sets['tenant_taps']), exact_taps(sets['closed_taps'])
    assert closed <= taps, 'closed TAP not registered'
    check_ranges(sets['non_public_v4'], source)
    counter, drop, accept = {'counter': None}, {'drop': None}, {'accept': None}
    expected_chains = {'tenant_guard_input': ('filter', 'input', -20, 'accept'),
                       'tenant_guard_forward': ('filter', 'forward', -20, 'accept')}
    expected = {'tenant_guard_input': [[interface('iifname', 'mtap*'), counter, drop]],
                'tenant_guard_forward': [[interface(key, '@closed_taps'), counter, drop] for key in ['iifname', 'oifname']] +
                [[interface(key, 'mtap*'), interface(key, '@tenant_taps', '!='), counter, drop] for key in ['iifname', 'oifname']]}
    for tap in sorted(taps):
        index = tap_index(tap)
        source_ip = f'10.200.{index}.2'
        prefix = f'tenant_{tap}'
        expected_chains.update({f'{prefix}_input': ('filter', 'input', -10, 'accept'),
                                f'{prefix}_forward': ('filter', 'forward', -10, 'accept'),
                                f'{prefix}_nat': ('nat', 'postrouting', 100, 'accept')})
        incoming, outgoing = interface('iifname', tap), interface('oifname', tap)
        expected[f'{prefix}_input'] = [[incoming, counter, drop]]
        expected[f'{prefix}_forward'] = [
            [incoming, interface('oifname', 'mtap*'), counter, drop],
            [incoming, match({'meta': {'key': 'nfproto'}}, 'ipv6'), counter, drop],
            [incoming, match({'fib': {'result': 'type', 'flags': ['daddr']}}, 'local'), counter, drop],
            [incoming, address('daddr', '@non_public_v4'), counter, drop],
            [incoming, address('saddr', source_ip, '!='), counter, drop],
            [incoming, interface('oifname', uplink), address('saddr', source_ip), states('new', 'established', 'related'), {'counter': f'{prefix}_egress'}, accept],
            [outgoing, interface('iifname', uplink), address('daddr', source_ip), states('established', 'related'), counter, accept],
            [outgoing, counter, drop], [incoming, counter, drop],
        ]
        expected[f'{prefix}_nat'] = [[incoming, interface('oifname', uplink), address('saddr', source_ip), counter, {'masquerade': None}]]
    assert chains == expected_chains, 'tenant chain hooks/priorities differ'
    assert counters == {f'tenant_{tap}_egress' for tap in taps}, 'tenant counters differ'
    assert rules == expected, 'tenant predicates, explicit accepts or final drops differ'
    return taps, closed


def table(family, name):
    tables = json.loads(command('/usr/sbin/nft', '-j', 'list', 'tables'))['nftables']
    if not any(item.get('table', {}).get('family') == family and item.get('table', {}).get('name') == name for item in tables):
        return None
    return json.loads(command('/usr/sbin/nft', '-j', 'list', 'table', family, name))


def members(table_name, set_name, taps):
    values = f'flush set inet {table_name} {set_name}\n'
    if taps:
        values += f'add element inet {table_name} {set_name} {{ ' + ', '.join(json.dumps(tap) for tap in sorted(taps)) + ' }\n'
    return values


def apply(transaction):
    command('/usr/sbin/nft', '--check', '--file', '-', input=transaction)
    command('/usr/sbin/nft', '--file', '-', input=transaction)


def checked_source(settings):
    def read(path):
        metadata = path.lstat()
        assert stat.S_ISREG(metadata.st_mode) and metadata.st_uid == 0 and not metadata.st_mode & 0o022, 'unsafe versioned network source'
        return path.read_text()
    expected = render_source(read(TEMPLATE), settings['uplink'])
    if read(SOURCE) != expected:
        raise ValueError(f"network source does not match uplink {settings['uplink']} and the canonical template")
    return expected


def load():
    """Reload owned policy without restarting the helper or any tenant VM."""
    with network_lock(mutate=True):
        load_locked()


def load_locked(publish=True):
    """Called only while holding the exclusive network mutation lock."""
    readiness.invalidate()
    opened = False
    taps = set()
    try:
        settings = read_settings(check_link=False)
        uplink = settings['uplink']
        source = checked_source(settings)
        document = table('inet', 'motor_tenant')
        policy_error = None
        try:
            taps, closed = tenant_snapshot(document, source, uplink=uplink)
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            policy_error, taps, closed = error, set(), set()
        net_document = table('inet', 'motor_net')
        try:
            validate_uplink(settings)
        except (OSError, RuntimeError, ValueError):
            fence = members('motor_tenant', 'closed_taps', taps) if document is not None and policy_error is None else ''
            if net_document is not None:
                fence += members('motor_net', 'tenant_taps', set())
            if fence:
                apply(fence)
            raise
        ufw_error = None
        try:
            prior = inspect_ufw(uplink=uplink) if document is not None and policy_error is None else (set(), set())
        except (AssertionError, OSError, RuntimeError, ValueError) as error:
            ufw_error, prior = error, (set(), set())
        ready = (taps - closed) & prior[1]
        transaction = ''
        if document is not None and policy_error is None:
            transaction += members('motor_tenant', 'closed_taps', closed | (taps - ready))
        if net_document is not None:
            transaction += 'delete table inet motor_net\n'
        transaction += source + '\n' + members('motor_net', 'tenant_taps', ready)
        apply(transaction)
        if policy_error is not None:
            raise RuntimeError(f'FENCED: unsupported tenant policy; update helper and run --check: {policy_error}')
        if ufw_error is not None:
            raise RuntimeError(f'tenant UFW policy is fenced/unready: {ufw_error}')
        if document is not None:
            try:
                reconcile_ufw(taps, prior, uplink=uplink)
                # Rules match interface names, so a same-name replacement inherits
                # them. Only root can rename interfaces; that case is out of scope.
                validate_uplink(settings)
            except (AssertionError, OSError, RuntimeError, ValueError):
                apply(members('motor_tenant', 'closed_taps', taps) + members('motor_net', 'tenant_taps', set()))
                raise
            apply(members('motor_tenant', 'closed_taps', set()) + members('motor_net', 'tenant_taps', taps))
            opened = True
        if publish:
            validate_uplink(settings)
            readiness.publish()
        print('PASS: motor_net loaded with guarded exact tenant delegation')
        return settings, taps
    except BaseException:
        readiness.invalidate()
        if opened:
            apply(members('motor_tenant', 'closed_taps', taps) + members('motor_net', 'tenant_taps', set()))
        raise
