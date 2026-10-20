"""Run with python3 -m unittest -v script/runner/test_inbound_policy.py."""
import importlib.util
from pathlib import Path
import re
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("runner_inbound_policy", HERE / "inbound_policy.py")
if spec is None or spec.loader is None:
    raise ImportError("cannot load inbound_policy.py")
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)

HEADER = """Status: active
Logging: on (low)
Default: deny (incoming), allow (outgoing), deny (routed)
New profiles: skip

To                         Action      From
--                         ------      ----
"""
SSH = ("22/tcp                     ALLOW IN    Anywhere\n"
       "22/tcp (v6)                ALLOW IN    Anywhere (v6)\n")
# What `ufw allow in proto tcp from any to 0.0.0.0/0 port 8443 comment motor-host-agent` lists.
AGENT = "8443/tcp                   ALLOW IN    Anywhere                   # motor-host-agent\n"
LISTENERS = """tcp   LISTEN 0      4096   127.0.0.53%lo:53        0.0.0.0:*    users:(("systemd-resolve",pid=600,fd=15))
tcp   LISTEN 0      4096         0.0.0.0:22        0.0.0.0:*    users:(("sshd",pid=900,fd=3))
tcp   LISTEN 0      4096            [::]:22           [::]:*    users:(("sshd",pid=900,fd=4))
udp   UNCONN 0      0      10.1.2.3%eno1:68        0.0.0.0:*    users:(("systemd-network",pid=500,fd=20))
"""
AGENT_LISTENER = 'tcp   LISTEN 0      1024         0.0.0.0:8443      0.0.0.0:*    users:(("motor-host",pid=1200,fd=9))\n'


class FirewallPolicyTests(unittest.TestCase):
    def test_ssh_only_passes_without_a_named_port(self):
        self.assertEqual(policy.firewall_violations(HEADER + SSH, None), [])
        self.assertEqual(policy.listener_violations(LISTENERS, None), [])

    def test_host_agent_rule_passes_only_when_its_port_is_named(self):
        self.assertEqual(policy.firewall_violations(HEADER + SSH + AGENT, 8443), [])
        self.assertEqual(policy.listener_violations(LISTENERS + AGENT_LISTENER, 8443), [])
        # Without the named port the same rule and listener are refused.
        self.assertEqual(len(policy.firewall_violations(HEADER + SSH + AGENT, None)), 1)
        self.assertEqual(len(policy.listener_violations(LISTENERS + AGENT_LISTENER, None)), 1)
        # Naming a different port does not admit this one.
        self.assertEqual(len(policy.firewall_violations(HEADER + SSH + AGENT, 9443)), 1)

    def test_host_agent_exception_is_ipv4_tcp_with_its_name_only(self):
        refused = {
            "ipv6 twin": AGENT + "8443/tcp (v6)              ALLOW IN    Anywhere (v6)              # motor-host-agent\n",
            "no comment": "8443/tcp                   ALLOW IN    Anywhere\n",
            "other comment": "8443/tcp                   ALLOW IN    Anywhere                   # web\n",
            "udp": "8443/udp                   ALLOW IN    Anywhere                   # motor-host-agent\n",
            "any protocol": "8443                       ALLOW IN    Anywhere                   # motor-host-agent\n",
            "source restricted": "8443/tcp                   ALLOW IN    10.0.0.0/8                 # motor-host-agent\n",
            "second port": AGENT + "443/tcp                    ALLOW IN    Anywhere                   # motor-host-agent\n",
            "everything from a host": AGENT + "Anywhere                   ALLOW IN    203.0.113.9\n",
        }
        for name, rules in refused.items():
            with self.subTest(name):
                self.assertNotEqual(policy.firewall_violations(HEADER + SSH + rules, 8443), [])
        for name, listener in {
            "ipv6": 'tcp   LISTEN 0      1024            [::]:8443         [::]:*\n',
            "dual stack": 'tcp   LISTEN 0      1024               *:8443            *:*\n',
            "udp": 'udp   UNCONN 0      0            0.0.0.0:8443      0.0.0.0:*\n',
            "other port": 'tcp   LISTEN 0      1024         0.0.0.0:9443      0.0.0.0:*\n',
        }.items():
            with self.subTest(name):
                self.assertEqual(len(policy.listener_violations(LISTENERS + listener, 8443)), 1)

    def test_other_inbound_allows_inactive_or_permissive_firewall_are_refused(self):
        self.assertNotEqual(policy.firewall_violations(HEADER + SSH + "80/tcp                     ALLOW IN    Anywhere\n", None), [])
        self.assertNotEqual(policy.firewall_violations("Status: inactive\n", None), [])
        self.assertNotEqual(policy.firewall_violations(HEADER.replace("deny (incoming)", "allow (incoming)") + SSH, None), [])
        # Outbound and deny rules are not inbound exceptions.
        self.assertEqual(policy.firewall_violations(
            HEADER + SSH + "25/tcp                     DENY OUT    Anywhere\n"
            "8443/tcp                   DENY IN     Anywhere\n", None), [])

    def test_non_verbose_status_cannot_pass_for_an_extra_rule(self):
        # Plain `ufw status` omits the IN direction; a check reading it would see
        # no inbound allows at all. The checker therefore requires verbose output.
        plain = HEADER.replace("Logging: on (low)\nDefault: deny (incoming), allow (outgoing), deny (routed)\nNew profiles: skip\n", "")
        rules = "22/tcp                     ALLOW       Anywhere\n80/tcp                     ALLOW       Anywhere\n"
        self.assertEqual(policy.firewall_violations(plain + rules, None), ["ufw default incoming policy is not deny"])

    def test_port_argument_refuses_ssh_and_out_of_range(self):
        self.assertEqual(policy.port("8443"), 8443)
        for text in ("0", "22", "65536", "08443", "-1"):
            with self.subTest(text), self.assertRaises(Exception):
                policy.port(text)

    def test_setup_runs_this_checker_and_has_no_other_inbound_check(self):
        setup = (HERE / "setup.sh").read_text()
        self.assertIn('python3 - $policy_args < "$script_dir/inbound_policy.py"', setup)
        self.assertIn('policy_args="--host-agent-port $RUNNER_HOST_AGENT_PORT"', setup)
        self.assertIsNone(re.search(r"ALLOW IN|ss -H -lntup", setup))


if __name__ == "__main__":
    unittest.main()
