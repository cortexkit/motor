"""Portable packet-probe, policy-checker and loader-plan regressions.

Socket mocks check leak grading. Loader tests validate planned transactions,
not kernel packet verdicts; live namespace qualification remains operator-only.
"""
import copy
import contextlib
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
        catchall = 'iifname != "mnet-*" oifname != "mnet-*" iifname != @tenant_taps oifname != @tenant_taps counter drop'
        self.assertIn(catchall, forward, 'only exact registered TAPs may escape unrelated-forwarding drop')
        reply = forward.index('ct state established,related accept')
        inbound = forward.index('oifname "mnet-*" counter drop')
        outbound = forward.index('iifname "mnet-*" oifname "enp6s0"')
        unrelated = forward.index(catchall)
        self.assertLess(reply, inbound, 'only replies may enter a job')
        self.assertLess(inbound, outbound, 'inbound drop follows the reply exception')
        self.assertLess(outbound, unrelated, 'unrelated drop must not shadow job egress')

    def test_installer_check_validates_captured_schema_plus_v2_fields_and_rejects_removed_guards(self):
        installer = load_script('install-job-network.py')
        fixture = pathlib.Path(__file__).parent / 'test-fixtures/nft-motor-net-v2-added-set.json'
        table = json.loads(fixture.read_text())
        installer.check_ruleset(table, uplink="enp6s0")
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
        installer.check_ruleset(changed, uplink="enp6s0")
        for chain in ['input', 'forward']:
            broken = copy.deepcopy(table)
            broken['nftables'] = [entry for entry in broken['nftables'] if entry.get('rule', {}).get('chain') != chain]
            with self.assertRaisesRegex(AssertionError, chain):
                installer.check_ruleset(broken, uplink="enp6s0")
        broken = copy.deepcopy(table)
        chain = next(entry['chain'] for entry in broken['nftables'] if entry.get('chain', {}).get('name') == 'input')
        chain['prio'] = 0
        with self.assertRaisesRegex(AssertionError, 'priorities'):
            installer.check_ruleset(broken, uplink="enp6s0")
        broken = copy.deepcopy(table)
        rules = [entry for entry in broken['nftables'] if entry.get('rule', {}).get('chain') == 'forward']
        first, second = broken['nftables'].index(rules[0]), broken['nftables'].index(rules[1])
        broken['nftables'][first], broken['nftables'][second] = broken['nftables'][second], broken['nftables'][first]
        with self.assertRaisesRegex(AssertionError, 'forward'):
            installer.check_ruleset(broken, uplink="enp6s0")

    def test_legacy_v1_policy_is_not_reported_as_v2_healthy(self):
        installer = load_script('install-job-network.py')
        fixture = pathlib.Path(__file__).parent / 'test-fixtures/nft-motor-net-1.1.6.json'
        with self.assertRaises(AssertionError):
            installer.check_ruleset(json.loads(fixture.read_text()), uplink="enp6s0")

    def test_check_cli_rejects_real_json_with_both_input_rules_removed(self):
        installer = load_script('install-job-network.py')
        fixture = pathlib.Path(__file__).parent / 'test-fixtures/nft-motor-net-1.1.6-no-input.json'
        with patch.object(installer.os, 'geteuid', return_value=0), patch.object(installer.tenant_policy, 'read_settings', return_value={'uplink': 'enp6s0', 'public_ipv4': '8.8.8.8'}), patch.object(installer.tenant_policy, 'network_lock', return_value=contextlib.nullcontext()), patch.object(installer, 'sudo', return_value=SimpleNamespace(stdout=fixture.read_text())) as sudo, patch('sys.argv', ['install-job-network.py', '--check']):
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


class TenantDelegationTests(unittest.TestCase):
    OWNED = ('### tuple ### route:allow any any 0.0.0.0/0 any 10.200.1.2 in_mtap1!out_enp6s0 '
             'comment=6d6f746f722d74656e616e742d6d74617031\n'
             '-A ufw-user-forward -i mtap1 -o enp6s0 -s 10.200.1.2 -j ACCEPT\n')

    def setUp(self):
        self.installer = load_script('install-job-network.py')
        self.policy = self.installer.tenant_policy
        settings = patch.object(self.policy, 'read_settings', return_value={'uplink': 'enp6s0', 'public_ipv4': '8.8.8.8'})
        settings.start()
        self.addCleanup(settings.stop)
        self.root = pathlib.Path(__file__).parent
        self.source = (self.root / 'motor-net.nft').read_text()
        self.net = json.loads((self.root / 'test-fixtures/nft-motor-net-v2-added-set.json').read_text())
        self.tenant = json.loads((self.root / 'test-fixtures/nft-motor-tenant-v2-added-guards.json').read_text())

    def test_alternate_uplink_changes_only_interface_predicates_and_owned_ufw_direction(self):
        net = json.loads(json.dumps(self.net).replace('enp6s0', 'eno1'))
        tenant = json.loads(json.dumps(self.tenant).replace('enp6s0', 'eno1'))
        self.installer.check_ruleset(net, uplink='eno1')
        self.assertEqual(self.policy.tenant_snapshot(tenant, self.source, uplink='eno1'), ({'mtap1'}, set()))
        owned = self.OWNED.replace('enp6s0', 'eno1')
        self.assertEqual(self.policy.owned_grants(owned, uplink='eno1'), {'mtap1'})
        self.assertEqual(self.policy.ufw_arguments('mtap1', uplink='eno1')[7], 'eno1')
        with self.assertRaises(AssertionError):
            self.policy.tenant_snapshot(tenant, self.source, uplink='enp6s0')


    def test_checker_rejects_wildcard_tap_and_extra_inbound_grant(self):
        self.installer.check_ruleset(self.net, set(), uplink="enp6s0")
        self.assertEqual(self.policy.owned_grants(self.OWNED, uplink="enp6s0"), {'mtap1'})
        self.assertEqual(self.policy.live_grants('-A ufw-user-forward -s 10.200.1.2/32 -i mtap1 -o enp6s0 -j ACCEPT', uplink="enp6s0"), {'mtap1'})
        widened = copy.deepcopy(self.net)
        next(e['set'] for e in widened['nftables'] if e.get('set', {}).get('name') == 'tenant_taps')['elem'] = ['mtap*']
        with self.assertRaises(AssertionError):
            self.installer.check_ruleset(widened, uplink="enp6s0")
        for broken in [self.OWNED.replace('-i mtap1', '-i mtap+'),
                       self.OWNED + '-A ufw-user-forward -i enp6s0 -o mtap1 -j ACCEPT\n']:
            with self.subTest(broken=broken), self.assertRaises(AssertionError):
                self.policy.owned_grants(broken, uplink="enp6s0")
            with self.subTest(live=broken), self.assertRaises(AssertionError):
                self.policy.live_grants(broken, uplink="enp6s0")
        with self.assertRaises(AssertionError):
            self.policy.owned_grants(self.OWNED, True, uplink="enp6s0")

    def test_checker_rejects_missing_source_in_comment_owned_grant(self):
        with self.assertRaises(AssertionError):
            self.policy.owned_grants(self.OWNED.replace(' -s 10.200.1.2', ''), uplink="enp6s0")

    def test_full_check_matches_exact_kernel_members_to_guarded_helper_authority(self):
        original_read = pathlib.Path.read_text
        def read(path, *args, **kwargs):
            return '1' if str(path) == '/proc/sys/net/ipv4/ip_forward' else original_read(path, *args, **kwargs)
        for tap, allowed in [('mtap1', True), ('mtap2', False)]:
            document = copy.deepcopy(self.net)
            next(e['set'] for e in document['nftables'] if e.get('set', {}).get('name') == 'tenant_taps')['elem'] = [tap]
            def sudo(*args, **kwargs):
                return SimpleNamespace(stdout=json.dumps(document) if args[0] == '/usr/sbin/nft' else 'mnet-+ enp6s0 ALLOW FWD')
            with self.subTest(tap=tap), patch.object(self.installer.os, 'geteuid', return_value=0), \
                    patch.object(self.policy, 'network_lock', return_value=contextlib.nullcontext()), \
                    patch.object(self.installer, 'sudo', side_effect=sudo), patch.object(self.installer, 'permissions'), patch.object(self.installer, 'check_persistence'), \
                    patch.object(self.policy, 'table', return_value=self.tenant), \
                    patch.object(self.policy, 'inspect_ufw', return_value=({'mtap1'}, {'mtap1'})), \
                    patch.object(pathlib.Path, 'read_text', read):
                if allowed:
                    self.installer.prerequisites()
                else:
                    with self.assertRaisesRegex(AssertionError, 'delegation/UFW'):
                        self.installer.prerequisites()

    def test_persistent_loader_check_reads_installed_bytes_and_rejects_self_comparison(self):
        with tempfile.TemporaryDirectory() as directory:
            target = pathlib.Path(directory) / 'loader'
            target.write_text('correct loader\n')
            target.chmod(0o755)
            expected = {target: ('correct loader\n', 0o755)}
            self.installer.verify_files(expected, owner=self.installer.os.geteuid())
            target.write_text('wrong loader\n')
            with self.assertRaisesRegex(AssertionError, 'differs'):
                self.installer.verify_files(expected, owner=self.installer.os.geteuid())
            nft_copy, policy_copy = pathlib.Path(directory) / 'nft', pathlib.Path(directory) / 'policy'
            nft_copy.write_bytes((self.root / 'motor-net.nft').read_bytes())
            policy_copy.write_bytes((self.root / 'motor_network_policy.py').read_bytes())
            with patch.object(self.installer, 'NFT', nft_copy), patch.object(self.installer, 'POLICY', policy_copy), patch.object(self.installer, 'verify_files') as verify:
                self.installer.check_persistence()
            verify.assert_called_once()
        with patch.object(self.installer, 'NFT', self.root / 'motor-net.nft'), patch.object(self.installer, 'verify_files') as verify:
            with self.assertRaisesRegex(AssertionError, 'versioned source'):
                self.installer.check_persistence()
        verify.assert_not_called()

    def test_helper_policy_checker_requires_explicit_verdicts_unknown_tap_fence_and_exact_source(self):
        self.assertEqual(self.policy.tenant_snapshot(self.tenant, self.source, uplink="enp6s0"), ({'mtap1'}, set()))
        canonical = copy.deepcopy(self.tenant)
        dependency = {'match': {'op': '==', 'left': {'meta': {'key': 'nfproto'}}, 'right': 'ipv4'}}
        for entry in canonical['nftables']:
            if 'rule' in entry:
                entry['rule']['expr'] = [e for e in entry['rule']['expr'] if e != dependency]
        self.assertEqual(self.policy.tenant_snapshot(canonical, self.source, uplink="enp6s0"), ({'mtap1'}, set()))
        for chain, at in [('tenant_guard_forward', 0), ('tenant_mtap1_forward', 8), ('tenant_mtap1_forward', 4)]:
            broken = copy.deepcopy(self.tenant)
            rules = [e for e in broken['nftables'] if e.get('rule', {}).get('chain') == chain]
            broken['nftables'].remove(rules[at])
            with self.subTest(chain=chain, at=at), self.assertRaises(AssertionError):
                self.policy.tenant_snapshot(broken, self.source, uplink="enp6s0")
        wrong_uplink = copy.deepcopy(self.tenant)
        public = next(e['rule'] for e in wrong_uplink['nftables'] if e.get('rule', {}).get('chain') == 'tenant_mtap1_forward' and any(s.get('counter') == 'tenant_mtap1_egress' for s in e['rule']['expr']))
        public['expr'][1]['match']['right'] = 'other0'
        with self.assertRaises(AssertionError):
            self.policy.tenant_snapshot(wrong_uplink, self.source, uplink="enp6s0")

    def test_comment_owned_reconciliation_does_not_touch_mnet_or_foreign_rules(self):
        stale = self.OWNED.replace('mtap1', 'mtap2').replace('10.200.1.2', '10.200.2.2')
        stale = stale.replace('6d6f746f722d74656e616e742d6d74617031', '6d6f746f722d74656e616e742d6d74617032')
        old = self.policy.owned_grants(stale + '### tuple ### route:allow any any 0.0.0.0/0 any 0.0.0.0/0 in_mnet-+!out_enp6s0\n-A ufw-user-forward -i mnet-+ -o enp6s0 -j ACCEPT\n', uplink="enp6s0")
        with patch.object(self.policy, 'command') as command, patch.object(self.policy, 'inspect_ufw', return_value=({'mtap1'}, {'mtap1'})):
            self.policy.reconcile_ufw({'mtap1'}, (old, old), uplink="enp6s0")
        self.assertEqual(command.call_args_list[0].args, ('/usr/sbin/ufw', '--force', 'delete', 'route', 'allow', 'in', 'on', 'mtap2', 'out', 'on', 'enp6s0', 'from', '10.200.2.2', 'to', 'any', 'comment', 'motor-tenant-mtap2'))
        self.assertEqual(command.call_args_list[1].args, ('/usr/sbin/ufw', 'route', 'allow', 'in', 'on', 'mtap1', 'out', 'on', 'enp6s0', 'from', '10.200.1.2', 'to', 'any', 'comment', 'motor-tenant-mtap1'))
        with patch.object(self.policy, 'command') as command, patch.object(self.policy, 'inspect_ufw', return_value=({'mtap1'}, {'mtap1'})):
            self.policy.reconcile_ufw({'mtap1'}, ({'mtap1'}, {'mtap1'}), uplink="enp6s0")
        command.assert_not_called()

    def load_plan(self, tenant, net, prior=({'mtap1'}, {'mtap1'}), failure=False, inspect_failure=False):
        source = SimpleNamespace(read_text=lambda: self.source, lstat=lambda: SimpleNamespace(st_mode=0o100644, st_uid=0))
        writes = []
        with patch.object(self.policy, 'network_lock', return_value=contextlib.nullcontext()), patch.object(self.policy, 'SOURCE', source), \
                patch.object(self.policy, 'table', side_effect=lambda family, name: tenant if name == 'motor_tenant' else net), \
                patch.object(self.policy, 'apply', side_effect=writes.append), patch.object(self.policy, 'inspect_ufw', return_value=prior, side_effect=RuntimeError('UFW failure') if inspect_failure else None) as inspect, \
                patch.object(self.policy, 'reconcile_ufw', side_effect=RuntimeError('UFW failure') if failure else None):
            if failure or inspect_failure:
                with self.assertRaisesRegex(RuntimeError, 'UFW failure'):
                    self.policy.load()
            else:
                self.policy.load()
        return writes, inspect.call_count

    def test_load_plans_are_fail_closed_for_both_install_orders_and_reboot(self):
        first, calls = self.load_plan(None, self.net)
        self.assertEqual(calls, 0, 'no protected policy: do not infer tenants from persisted UFW')
        self.assertNotIn('add element inet motor_net tenant_taps', ''.join(first))
        for net in [None, self.net]:
            writes, _ = self.load_plan(self.tenant, net)
            self.assertIn('add element inet motor_net tenant_taps { "mtap1" }', ''.join(writes))
            self.assertNotIn('delete counter', ''.join(writes))
        reboot, _ = self.load_plan(None, None)
        self.assertNotIn('add element inet motor_net tenant_taps', ''.join(reboot))
        legacy = json.loads((self.root / 'test-fixtures/nft-motor-tenant-v1-capture.json').read_text())
        fenced, calls = self.load_plan(legacy, self.net)
        self.assertEqual(calls, 0, 'legacy object presence cannot authorize delegation')
        self.assertNotIn('add element inet motor_net tenant_taps', ''.join(fenced))

    def test_loader_ufw_failure_closes_tap_and_removes_delegation(self):
        writes, _ = self.load_plan(self.tenant, self.net, failure=True)
        self.assertEqual(writes[-1], 'flush set inet motor_tenant closed_taps\nadd element inet motor_tenant closed_taps { "mtap1" }\nflush set inet motor_net tenant_taps\n')

    def test_loader_cannot_open_tenant_when_ufw_inspection_fails(self):
        writes, _ = self.load_plan(self.tenant, self.net, inspect_failure=True)
        self.assertEqual(len(writes), 1)
        self.assertIn('add element inet motor_tenant closed_taps { "mtap1" }', writes[0])
        self.assertNotIn('add element inet motor_net tenant_taps', writes[0])

    def test_unknown_ufw_status_and_widened_return_state_are_not_assumed_safe(self):
        with patch.object(self.policy, 'read_owned', return_value=''), patch.object(self.policy, 'command', return_value='unavailable'):
            with self.assertRaisesRegex(AssertionError, 'unrecognized UFW status'):
                self.policy.inspect_ufw(uplink='enp6s0')
        with patch.object(self.policy, 'read_owned', return_value=''), patch.object(self.policy, 'command', side_effect=['Status: active', '', '-A ufw-before-forward -m conntrack --ctstate NEW,ESTABLISHED,RELATED -j ACCEPT']):
            with self.assertRaisesRegex(AssertionError, 'established/related'):
                self.policy.inspect_ufw(uplink='enp6s0')


class HostUplinkSettingsTests(unittest.TestCase):
    def setUp(self):
        self.policy = load_script('motor_network_policy.py')
        self.settings = {'uplink': 'eno1', 'public_ipv4': '8.8.8.8'}
        self.addresses = [{'ifname': 'eno1', 'operstate': 'UP', 'flags': ['BROADCAST', 'UP', 'LOWER_UP'],
                           'addr_info': [{'family': 'inet', 'scope': 'global', 'local': '8.8.8.8'}]}]

    def test_host_uplink_and_public_ipv4_are_required_and_syntax_checked(self):
        for value in [{}, {'uplink': 'eno1'}, {'public_ipv4': '8.8.8.8'},
                      dict(self.settings, uplink='eno1";accept'), dict(self.settings, uplink='mtap*'),
                      dict(self.settings, public_ipv4='10.0.0.1'), dict(self.settings, public_ipv4='224.0.0.1')]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.policy.checked_settings(value)

    def test_uplink_uses_only_the_explicit_interface_not_a_route_guess(self):
        with patch.object(self.policy, 'command', return_value=json.dumps(self.addresses)) as command:
            self.assertEqual(self.policy.validate_uplink(self.settings), self.settings)
        command.assert_called_once_with('/usr/sbin/ip', '-j', '-4', 'address', 'show', 'dev', 'eno1')

    def test_missing_uplink_refuses_with_the_interface_name(self):
        with patch.object(self.policy, 'command', side_effect=RuntimeError('device absent')):
            with self.assertRaisesRegex(ValueError, 'uplink eno1 is missing'):
                self.policy.validate_uplink(self.settings)
        with self.assertRaisesRegex(ValueError, 'uplink eno1 must exist'):
            self.policy.validate_uplink(self.settings, [])

    def test_down_uplink_refuses_even_with_the_public_address(self):
        self.addresses[0]['operstate'] = 'DOWN'
        with self.assertRaisesRegex(ValueError, 'uplink eno1 must exist, be UP'):
            self.policy.validate_uplink(self.settings, self.addresses)

    def test_uplink_without_the_configured_public_address_refuses(self):
        self.addresses[0]['addr_info'][0]['local'] = '10.0.0.2'
        with self.assertRaisesRegex(ValueError, 'uplink eno1 .*8.8.8.8'):
            self.policy.validate_uplink(self.settings, self.addresses)

    def test_enp6s0_base_rules_are_byte_identical_and_alternate_uplink_is_exact(self):
        source = pathlib.Path(__file__).with_name('motor-net.nft').read_text()
        self.assertEqual(hashlib.sha256(source.encode()).hexdigest(),
                         'b0e1dddc368a7fca09b1c6df87b4c917319eebf75bd14be9ce9dc8193a1eb2bb')
        self.assertEqual(self.policy.render_source(source, 'enp6s0'), source)
        alternate = self.policy.render_source(source, 'eno1')
        self.assertNotIn('enp6s0', alternate)
        self.assertEqual(alternate.count('"eno1"'), 3)
        self.assertEqual(alternate.replace('"eno1"', '"enp6s0"'), source)

    def test_loader_refuses_invalid_uplink_before_any_firewall_change(self):
        with patch.object(self.policy, 'network_lock', return_value=contextlib.nullcontext()), \
                patch.object(self.policy, 'read_settings', side_effect=ValueError('uplink eno1 is missing')), \
                patch.object(self.policy, 'apply') as apply, patch.object(self.policy, 'table') as table:
            with self.assertRaisesRegex(ValueError, 'uplink eno1 is missing'):
                self.policy.load()
        apply.assert_not_called()
        table.assert_not_called()



if __name__ == '__main__':
    unittest.main(verbosity=2)
