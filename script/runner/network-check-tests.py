"""Portable unit tests for network-check.py: mock socket outcomes to prove its
errno and timeout assertions reject leaks instead of accepting them as isolation.
"""
import copy
import errno
import importlib.util
import json
import pathlib
import re
import hashlib
import socket
import unittest
import tempfile
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

SPEC = importlib.util.spec_from_file_location('network_check', pathlib.Path(__file__).with_name('network-check.py'))
assert SPEC is not None and SPEC.loader is not None
PROBE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE)


class NetworkProbeControls(unittest.TestCase):
    def tcp(self, error=None, **kwargs):
        sock = MagicMock()
        sock.__enter__.return_value = sock
        sock.connect.side_effect = error
        with patch.object(PROBE.socket, 'socket', return_value=sock):
            PROBE.blocked_tcp('1.1.1.1', 443, **kwargs)

    def test_tcp_success_is_a_leak(self):
        with self.assertRaisesRegex(AssertionError, 'NETWORK LEAK'):
            self.tcp()

    def test_three_second_tcp_timeout_is_failure(self):
        with self.assertRaisesRegex(AssertionError, 'timeout is a FAILURE'):
            self.tcp(socket.timeout('timed out'))

    def test_tcp_errno_allowlist_is_exact(self):
        for error in [errno.ECONNREFUSED, errno.EHOSTUNREACH, errno.ENETUNREACH, errno.EACCES]:
            self.tcp(OSError(error, 'blocked'))
        with self.assertRaises(AssertionError):
            self.tcp(OSError(errno.ECONNRESET, 'not evidence of isolation'))

    def test_ipv6_and_offline_require_network_unreachable(self):
        self.tcp(OSError(errno.ENETUNREACH, 'blocked'), exact=errno.ENETUNREACH)
        with self.assertRaises(AssertionError):
            self.tcp(OSError(errno.ECONNREFUSED, 'wrong'), exact=errno.ENETUNREACH)

    def udp(self, result):
        sock = MagicMock()
        sock.__enter__.return_value = sock
        if isinstance(result, Exception):
            sock.recv.side_effect = result
        else:
            sock.recv.return_value = result
        with patch.object(PROBE.socket, 'socket', return_value=sock):
            PROBE.blocked_udp('127.0.0.53', 53)
        sock.sendto.assert_called_once()
        sock.recv.assert_called_once()

    def test_udp_send_is_not_sufficient_and_received_data_is_a_leak(self):
        self.udp(OSError(errno.ECONNREFUSED, 'blocked'))
        with self.assertRaisesRegex(AssertionError, 'NETWORK LEAK'):
            self.udp(b'reply')

    def test_udp_only_accepts_connection_refused_never_timeout(self):
        for error in [socket.timeout('timed out'), OSError(errno.ENETUNREACH, 'wrong')]:
            with self.assertRaises(AssertionError):
                self.udp(error)


class HostAddressTests(unittest.TestCase):
    def test_discovery_uses_host_route_source_for_both_families(self):
        routes = ['[{"prefsrc": "8.8.8.8"}]', '[{"prefsrc": "2606:4700:4700::1001"}]']
        with patch.object(PROBE, 'command', side_effect=routes) as command:
            self.assertEqual(PROBE.discover_host_addresses(),
                             {'4': '8.8.8.8', '6': '2606:4700:4700::1001'})
        self.assertEqual(command.call_args_list[0].args,
                         ('ip', '-j', '-4', 'route', 'get', '1.1.1.1'))
        self.assertEqual(command.call_args_list[1].args,
                         ('ip', '-j', '-6', 'route', 'get', '2606:4700:4700::1111'))

    def test_discovery_fails_closed_without_a_public_source(self):
        for route in ('[]', '[{}]', '[{"prefsrc": "10.0.0.2"}]', '[{"prefsrc": "::1"}]'):
            with self.subTest(route=route), patch.object(PROBE, 'command', return_value=route):
                with self.assertRaises((AssertionError, IndexError, KeyError)):
                    PROBE.discover_host_addresses()

    def test_input_and_ipv6_probe_discovered_host_targets(self):
        with patch.object(PROBE.pathlib.Path, 'read_text',
                          return_value='{"4": "8.8.8.8", "6": "2606:4700:4700::1001"}'), \
                patch.object(PROBE, 'command', return_value='[{"gateway": "198.18.0.1"}]'), \
                patch.object(PROBE, 'blocked_tcp') as tcp, patch.object(PROBE, 'blocked_udp') as udp:
            PROBE.input_blocked()
            self.assertEqual([call.args for call in tcp.call_args_list],
                             [('8.8.8.8', 22), ('198.18.0.1', 22), ('198.18.0.1', 53)])
            udp.assert_called_once_with('198.18.0.1', 53)
            tcp.reset_mock()
            PROBE.forbidden()
            self.assertEqual(tcp.call_args.args,
                             ('2606:4700:4700::1001', 22, socket.AF_INET6, errno.ENETUNREACH))

    def test_probe_requires_valid_host_address_fixture(self):
        with patch.object(PROBE.pathlib.Path, 'read_text', side_effect=FileNotFoundError):
            with self.assertRaises(FileNotFoundError):
                PROBE.host_address('4')
        with patch.object(PROBE.pathlib.Path, 'read_text', return_value='{"4": "10.0.0.2"}'):
            with self.assertRaisesRegex(AssertionError, 'not a public host address'):
                PROBE.host_address('4')


class NetworkForwardPolicyTests(unittest.TestCase):
    def test_table_drops_inbound_and_unrelated_forwarding(self):
        table = pathlib.Path(__file__).with_name('motor-net.nft').read_text()
        forward = table.split('chain forward {', 1)[1].split('chain postrouting', 1)[0]
        self.assertIn('oifname "mnet-*" counter drop', forward, 'NEW inbound traffic must be silently dropped')
        self.assertIn('iifname != "mnet-*" oifname != "mnet-*" counter drop', forward, 'unrelated forwarding must remain disabled')
        reply = forward.index('ct state established,related accept')
        inbound = forward.index('oifname "mnet-*" counter drop')
        outbound = forward.index('iifname "mnet-*" oifname "enp6s0"')
        unrelated = forward.index('iifname != "mnet-*" oifname != "mnet-*" counter drop')
        self.assertLess(reply, inbound, 'only replies may enter a job')
        self.assertLess(inbound, outbound, 'inbound drop follows the reply exception')
        self.assertLess(outbound, unrelated, 'unrelated drop must not shadow job egress')

    def test_installer_check_validates_real_json_and_rejects_removed_guards(self):
        installer = load_script('install-job-network.py')
        fixture = pathlib.Path(__file__).parent / 'test-fixtures/nft-motor-net-1.1.6.json'
        table = json.loads(fixture.read_text())
        installer.check_ruleset(table)
        # Handles, packet counters and ct-state display order are not policy.
        changed = copy.deepcopy(table)
        for entry in changed['nftables']:
            for item in entry.values():
                if isinstance(item, dict):
                    item['handle'] = 999
                    for expression in item.get('expr', []):
                        if 'counter' in expression:
                            expression['counter'] = {'packets': 999, 'bytes': 123456}
                        if expression.get('match', {}).get('left') == {'ct': {'key': 'state'}}:
                            expression['match']['right'].reverse()
        installer.check_ruleset(changed)
        for chain in ['input', 'forward']:
            broken = copy.deepcopy(table)
            broken['nftables'] = [entry for entry in broken['nftables'] if entry.get('rule', {}).get('chain') != chain]
            with self.assertRaisesRegex(AssertionError, chain):
                installer.check_ruleset(broken)
        broken = copy.deepcopy(table)
        chain = next(entry['chain'] for entry in broken['nftables'] if entry.get('chain', {}).get('name') == 'input')
        chain['prio'] = 0
        with self.assertRaisesRegex(AssertionError, 'priorities'):
            installer.check_ruleset(broken)
        broken = copy.deepcopy(table)
        rules = [entry for entry in broken['nftables'] if entry.get('rule', {}).get('chain') == 'forward']
        first, second = broken['nftables'].index(rules[0]), broken['nftables'].index(rules[1])
        broken['nftables'][first], broken['nftables'][second] = broken['nftables'][second], broken['nftables'][first]
        with self.assertRaisesRegex(AssertionError, 'forward'):
            installer.check_ruleset(broken)

    def test_check_cli_rejects_real_json_with_both_input_rules_removed(self):
        installer = load_script('install-job-network.py')
        fixture = pathlib.Path(__file__).parent / 'test-fixtures/nft-motor-net-1.1.6-no-input.json'
        with patch.object(installer, 'sudo', return_value=SimpleNamespace(stdout=fixture.read_text())) as sudo, patch('sys.argv', ['install-job-network.py', '--check']):
            with self.assertRaisesRegex(AssertionError, 'motor_net input'):
                installer.main()
        sudo.assert_called_once_with('/usr/sbin/nft', '-j', 'list', 'table', 'inet', 'motor_net')


def load_script(name):
    spec = importlib.util.spec_from_file_location(name, pathlib.Path(__file__).with_name(name))
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ProofGradingTests(unittest.TestCase):
    def setUp(self):
        self.proof = load_script('prove-job-network.py')
        self.expected = 'isolation::tests::isolated_net_input_rejects_host_and_gateway'

    def test_duplicate_fail_rows_still_grade_one_failure_with_public_pass(self):
        output = f'FAIL [0.10s] (1/2) motor-runner {self.expected}\nPASS [0.20s] (2/2) motor-runner {self.proof.PUBLIC_CONTROL}\nSummary [0.30s] 2 tests run: 1 passed, 1 failed\nFAIL [0.10s] motor-runner {self.expected}\n'
        outcome, captured = self.proof.grade(output, self.expected, 100)
        self.assertEqual(outcome, 'reddened')
        self.assertEqual(captured.count(self.expected), 1)
        self.assertIn(self.proof.PUBLIC_CONTROL, captured)

    def test_fail_fast_missing_public_control_is_not_reached_not_reddened(self):
        output = f'FAIL [0.10s] motor-runner {self.expected}\nSummary [0.10s] 1/2 tests run: 0 passed, 1 failed\nFAIL [0.10s] motor-runner {self.expected}\n'
        self.assertEqual(self.proof.grade(output, self.expected, 100)[0], 'not_reached')

    def test_other_failure_or_two_passes_cannot_be_graded_reddened(self):
        both_failed = f'FAIL [0.10s] motor-runner {self.expected}\nFAIL [0.20s] motor-runner {self.proof.PUBLIC_CONTROL}\n'
        self.assertEqual(self.proof.grade(both_failed, self.expected, 100)[0], 'not_reached')
        both_passed = both_failed.replace('FAIL', 'PASS')
        self.assertEqual(self.proof.grade(both_passed, self.expected, 0)[0], 'undefended')

    def test_second_control_runs_and_both_records_survive_first_failure(self):
        first = {'outcome': 'not_reached'}
        second = {'outcome': 'reddened'}
        with tempfile.TemporaryDirectory() as directory, patch.object(self.proof, 'control', side_effect=[(first, True), (second, True)]) as control:
            result = pathlib.Path(directory)
            self.assertFalse(self.proof.run_controls(result))
            self.assertEqual(control.call_count, 2)
            self.assertEqual(json.loads((result / 'mutation-evidence.json').read_text()), [first, second])


class NetworkFixtureTests(unittest.TestCase):
    def test_large_public_fixture_is_pinned_and_registry_only(self):
        root = pathlib.Path(__file__).parent / 'network-fixture'
        for name, expected in {
            'package.json': 'c32153f5e268c2680b31fa42f3627faf3462b3011bede209dd4b143df98ec816',
            'bun.lock': 'ea56ec3dd2c95aadc05ce0210b824e0683b8c86f2b90fc80c923cc581f579de3',
        }.items():
            self.assertEqual(hashlib.sha256((root / name).read_bytes()).hexdigest(), expected, name)
        manifest = json.loads((root / 'package.json').read_text())
        lock = json.loads(re.sub(r',\s*([}\]])', r'\1', (root / 'bun.lock').read_text()))
        self.assertGreaterEqual(len(lock['packages']), 200)
        self.assertEqual(set(lock['workspaces']), {''})
        self.assertNotIn('workspaces', manifest)
        for group in ['dependencies', 'devDependencies', 'peerDependencies', 'optionalDependencies']:
            for spec in manifest.get(group, {}).values():
                self.assertNotRegex(spec, r'^(file:|link:|workspace:|git|https?:|\.\./)')


if __name__ == '__main__':
    unittest.main(verbosity=2)
