import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
try:
    import runner_prerequisites as prerequisites
finally:
    sys.path.pop(0)


class PrerequisitesTests(unittest.TestCase):
    SCHEMA = """    CreateSandboxRequest:
      type: object
      properties:
        type:
          type: string
        from:
          type: string
        noEnv:
          type: boolean
        environment:
          type: string
    OtherSchema:
      type: object
"""
    def setUp(self):
        self.status = {"exitCode": 0, "stdout": json.dumps({
            "os_release": 'VERSION_ID="24.04"\n',
            "private_pids": {"exit_code": 1, "stderr": "Unknown assignment: PrivatePIDs=yes"},
            "kvm": {"opened": True, "api_version": 12}, "nested": {"kvm_intel": "Y"}})}

    def test_preserves_rejected_private_pids_and_kvm_evidence(self):
        data = prerequisites.parse(self.status)
        self.assertEqual(data["private_pids"]["exit_code"], 1)
        self.assertEqual(data["kvm"]["api_version"], 12)
        self.assertEqual(data["nested"], {"kvm_intel": "Y"})

    def test_refuses_truncation_or_failed_command(self):
        for field, value in (("stdoutTruncated", True), ("stderrTruncated", True), ("exitCode", 1)):
            with self.subTest(field=field), self.assertRaises(ValueError):
                prerequisites.parse(dict(self.status, **{field: value}))

    def test_refuses_missing_or_non_json_evidence(self):
        for value in ("banner", "{}", '{"os_release": 0, "private_pids": {}}'):
            with self.subTest(value=value), self.assertRaises(ValueError):
                prerequisites.parse(dict(self.status, stdout=value))

    def test_guest_is_valid_python_and_never_boots_or_changes_init(self):
        compile(prerequisites.GUEST, "guest", "exec")
        self.assertIn("0xAE00", prerequisites.GUEST)
        for forbidden in ("KVM_CREATE_VM", "daemon-reexec", "apt-get", "write_text"):
            self.assertNotIn(forbidden, prerequisites.GUEST)

    def test_credential_audit_records_only_names_and_paths(self):
        namespace = {}
        exec(prerequisites.SOURCE, namespace)
        with tempfile.TemporaryDirectory() as root:
            token = Path(root, "credentials.json")
            token.write_text("canary-secret-value")
            data = namespace["credential_audit"]([root], {"BOAT_API_KEY": "canary-secret-value"})
            self.assertFalse(data["clean"])
            self.assertEqual(data["environment_names"], ["BOAT_API_KEY"])
            self.assertEqual(data["credential_paths"], [str(token)])
            self.assertNotIn("canary-secret-value", json.dumps(data))
            token.unlink()
            self.assertTrue(namespace["credential_audit"]([root], {})["clean"])

    def test_toolchain_source_files_are_lookalikes_but_stores_still_count(self):
        # Boat's base caches Node's sources under ~/.nvm/.cache; their file names
        # contain "credentials" but they are code, not account credential stores.
        namespace = {}
        exec(prerequisites.SOURCE, namespace)
        with tempfile.TemporaryDirectory() as root:
            sources = [Path(root, name) for name in ("node_credentials.cc", "test-http-proxy-request-invalid-credentials.mjs")]
            store = Path(root, "aws-credentials")
            for path in [*sources, store]:
                path.write_text("x")
            data = namespace["credential_audit"]([root], {})
            self.assertEqual(data["source_lookalikes"], sorted(map(str, sources)))
            self.assertEqual(data["credential_paths"], [str(store)])
            store.unlink()
            self.assertTrue(namespace["credential_audit"]([root], {})["clean"])

    def test_provider_environment_is_scoped_and_npmrc_keys_are_classified(self):
        namespace = {}
        exec(prerequisites.SOURCE, namespace)
        with tempfile.TemporaryDirectory() as root:
            path = Path(root, ".npmrc")
            path.write_text("prefix=/irrelevant\n")
            data = namespace["credential_audit"]([root], {"ASCII_TOKEN": "not-logged", "BOAT_ID": "not-logged"})
            self.assertTrue(data["clean"])
            self.assertEqual(data["provider_environment_names"], ["ASCII_TOKEN", "BOAT_ID"])
            self.assertEqual(data["npmrc_keys"][str(path)], ["prefix"])
            path.write_text("//registry.example/:_authToken=never-log-this\n")
            data = namespace["credential_audit"]([root], {})
            self.assertFalse(data["clean"])
            self.assertNotIn("never-log-this", json.dumps(data))
            path.unlink()
            key = Path(root, ".config/ascii/boat/config.json")
            key.parent.mkdir(parents=True)
            key.write_text("never-log-this")
            self.assertFalse(namespace["credential_audit"]([root], {})["clean"])

    def test_schema_parser_reports_fields_without_inventing_base_choice(self):
        data = prerequisites.create_schema(self.SCHEMA)
        self.assertEqual(data["fields"], ["type", "from", "noEnv", "environment"])
        self.assertEqual(data["documented_base_selectors"], [])
        with self.assertRaises(ValueError):
            prerequisites.create_schema("not a schema")
        added = self.SCHEMA.replace("        from:", "        image:\n          type: string\n        from:")
        self.assertEqual(prerequisites.create_schema(added)["documented_base_selectors"], ["image"])

    def test_run_records_one_sandbox_without_installs(self):
        class Probe:
            size = "large"
            def __init__(self, status):
                self.data, self.status, self.commands = {}, status, []
            def create(self):
                return {"id": "bx_test", "vcpu": 8, "memoryGB": 16}, {"seconds_to_ready": 1}
            def execute(self, sid, command, poll_seconds):
                self.commands.append((sid, command, poll_seconds))
                return copy.deepcopy(self.status)
            def checkpoint(self):
                pass
        probe = Probe(self.status)
        with patch.object(prerequisites.urllib.request, "urlopen") as fetch:
            fetch.return_value.__enter__.return_value.read.return_value = self.SCHEMA.encode()
            prerequisites.run(probe)
        self.assertEqual(len(probe.commands), 1)
        self.assertIn("runner_prerequisites", probe.data)
        self.assertTrue(probe.commands[0][1].startswith("sudo -n -E python3 -c "))


if __name__ == "__main__":
    unittest.main()
