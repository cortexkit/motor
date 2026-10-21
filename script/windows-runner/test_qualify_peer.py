#!/usr/bin/env python3
"""Source-level checks, not Win32 execution tests, for peer qualification."""
from pathlib import Path
import re
import unittest

HERE = Path(__file__).resolve().parent


class PeerQualificationTests(unittest.TestCase):
    def setUp(self):
        self.control = (HERE / "qualify.ps1").read_text()
        self.helper = (HERE / "qualify-peer.ps1").read_text()

    def test_victim_boot_event_and_margin_precede_attacker_start(self):
        self.assertIn("Id=18601", self.helper)
        self.assertIn("Microsoft-Windows-Hyper-V-Worker-Admin", self.helper)
        self.assertIn("$_.InnerText.Trim('{}') -ieq $VmId.ToString('D')", self.helper)
        self.assertIn("$event.TimeCreated.ToUniversalTime() -ge $Since", self.helper)
        self.assertIn("$Timeline.boot_observed_ms + 20000", self.helper)
        self.assertIn("Parse($Timeline.boot_observed_utc).AddSeconds(20)", self.helper)
        boot = self.control.index("$boot = Wait-RunnerPeerBoot $victimVm")
        margin = self.control.index("Start-Sleep -Seconds 20", boot)
        attack = self.control.index("$timeline.attacker_start_ms", margin)
        self.assertLess(boot, margin)
        self.assertLess(margin, attack)
        self.assertLess(attack, self.control.index("'-Job',$attacker)", attack))

    def test_completed_attacker_is_collected_before_only_host_hello(self):
        harvest = self.control.index("-Operation Harvest -Root $Root -Job $attacker")
        proof = self.control.index('ReadAllText("$Root\\evidence\\$attacker-guest\\peer-proof.json")')
        hello = self.control.index("$hello = Receive-RunnerPeerHello")
        self.assertEqual(self.control.count("Receive-RunnerPeerHello"), 1)
        self.assertLess(harvest, proof)
        self.assertLess(proof, hello)
        self.assertLess(hello, self.control.index("-Operation Harvest -Root $Root -Job $victim"))
        self.assertIn("$proof[0].transport_connected -isnot [bool]", self.control)
        self.assertIn("$timeline.attacker_nonce", self.control)

    def test_production_validates_current_peer_evidence_after_settle(self):
        hello = self.control.index("$hello = Receive-RunnerPeerHello")
        settle = self.control.index("Start-Sleep -Seconds 60", hello)
        harvest = self.control.index("-Operation Harvest -Root $Root -Job $victim", settle)
        verify = self.control.index("$timeline.result = Assert-RunnerPeerEvidence $timeline.victim_nonce $victimVm $attackerVm $proof $timeline $nativeLog")
        self.assertLess(hello, settle)
        self.assertLess(settle, harvest)
        self.assertLess(harvest, verify)
        self.assertIn("$Timeline.settled_ms -lt $Timeline.hello_ms + 60000", self.helper)
        self.assertIn("peer-timeline.json", self.control)
        self.assertIn("peer-host-hello.json", self.control)

    def test_single_listener_proof_never_calls_false_a_native_refusal(self):
        self.assertIn("$boots -ne 1 -or $listens -ne 1 -or -not $loopback -or $accepts -ne 1", self.helper)
        self.assertIn("$entries[0].transport_connected -and -not $foreignRefused", self.helper)
        self.assertIn("$Matches[1] -cne 'a42e7cdad03f480c9cc2a4de20abb878'", self.helper)
        self.assertIn("$Timeline.hello_validated -ne $true", self.helper)
        self.assertIn("$Timeline.hello_nonce -cne $Nonce", self.helper)
        self.assertIn("cross-vm-attempt-did-not-consume-single-listener", self.helper)
        self.assertNotIn("PASS native-loopback-refused native-foreign-vm-refused", self.control)

    def test_hello_probe_pins_actual_wire_contract_and_sends_nothing(self):
        protocol = (HERE.parents[1] / "crates/motor-winagent/src/lib.rs").read_text()
        transport = (HERE.parents[1] / "crates/motor-winagent/src/transport.rs").read_text()
        version = re.search(r"PROTOCOL_VERSION: u32 = (\d+)", protocol)
        service = re.search(r'HVSOCK_SERVICE_ID: &str = "([^"]+)"', transport)
        assert version is not None and service is not None
        self.assertIn(f"$hello.version -ne {version.group(1)}", self.helper)
        self.assertIn(service.group(1), self.helper)
        self.assertIn("'kind,nonce,version'", self.helper)
        self.assertIn("$hello.nonce -cne $Nonce", self.helper)
        self.assertIn("[long]$Bytes[0] * 16777216", self.helper)
        self.assertIn("socket(34, 1, 1)", self.helper)
        self.assertIn("setsockopt(handle, 1, 1, ref timeout, 4)", self.helper)
        self.assertNotIn("extern int send(", self.helper)
        self.assertIn("closesocket(handle)", self.helper)
        self.assertIn("Do not retry after an accepted socket", self.helper)

    def test_native_fixtures_exercise_real_peer_validator(self):
        fixtures = (HERE / "test_scripts.ps1").read_text()
        self.assertIn('. "$PSScriptRoot\\qualify-peer.ps1"', fixtures)
        for name in (
            "peer-false-with-final-hello-and-single-parent-accept",
            "peer-host-hello-failure-is-unproven",
            "peer-attacker-before-boot-margin-is-unproven",
            "peer-connected-attacker-needs-native-refusal",
            "peer-requires-native-loopback-refusal",
            "peer-rejects-non-parent-accept",
        ):
            self.assertIn(name, fixtures)
        self.assertIn("Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm", fixtures)
        self.assertIn("Convert-RunnerPeerHello $frame $nonce", fixtures)
        self.assertIn("Initialize-RunnerPeerSocket", fixtures)

    def test_installer_deploys_peer_qualification_helper(self):
        self.assertIn("'qualify-peer.ps1'", (HERE / "install.ps1").read_text())
        self.assertIn('. "$PSScriptRoot\\qualify-peer.ps1"', self.control)


if __name__ == "__main__":
    unittest.main()
