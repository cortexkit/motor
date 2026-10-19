#!/usr/bin/env python3
"""Portable plumbing tests; these do not assert anything about a real Boat VM."""
import base64
import contextlib
from datetime import datetime, timedelta, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import sys
import unittest
from unittest.mock import MagicMock, patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("boat_probe", HERE / "probe.py")
if spec is None or spec.loader is None:
    raise ImportError("cannot load the Boat probe beside its tests")
probe = importlib.util.module_from_spec(spec)
sys.path.insert(0, str(HERE))
try:
    spec.loader.exec_module(probe)
    import ssh_reach
finally:
    sys.path.pop(0)
reach = sys.modules["reach"]


def evidence(resources=()):
    return {"schema": 1, "run_id": "unit-test", "api": [], "cycles": [], "cleanup": [],
            "started_at": datetime.now(timezone.utc).isoformat(), "pending_creates": [],
            "resources": [dict(r) for r in resources]}


class FakeClient:
    def __init__(self, routes):
        self.routes, self.calls = routes, []

    def request(self, method, path, body=None, **kwargs):
        self.calls.append((method, path, body, kwargs))
        route = self.routes[(method, path)]
        result = route.pop(0) if isinstance(route, list) else route
        if isinstance(result, Exception):
            raise result
        return result


def deleting(status="completed"):
    return {"ok": True, "type": "deletion.operation", "operation": {"id": "bdop_test", "status": status}}


class ParsingTests(unittest.TestCase):
    def test_envelopes_and_sandbox_id(self):
        body = probe.parse_response('{"ok":true,"type":"sandbox.info","sandbox":{"id":"bx_23456789"}}')
        self.assertEqual(probe.sandbox(body)["id"], "bx_23456789")
        for raw in ("[]", "null", "not json", '{"ok":false}', '{"ok":1}'):
            with self.subTest(raw=raw), self.assertRaises(probe.ProbeError):
                probe.parse_response(raw)
        with self.assertRaises(probe.ProbeError):
            probe.sandbox({"id": "bx_23456789"})

    def test_command_output_requires_success_and_complete_json(self):
        good = {"exitCode": 0, "stdout": '{"uid":1000}\n'}
        self.assertEqual(probe.command_output(good), {"uid": 1000})
        for change in ({"exitCode": 1}, {"timedOut": True}, {"stdoutTruncated": True}, {"stderrTruncated": True},
                       {"stdout": "[]"}, {"stdout": "log before JSON\n{}"}):
            with self.subTest(change=change), self.assertRaises(probe.ProbeError):
                probe.command_output(dict(good, **change))

    def test_retry_only_documented_startup_refusals(self):
        for status, body, expected in (
            (409, {"code": "sandbox_starting"}, True),
            (409, {"code": "machine_not_running"}, False),
            (502, {"message": "sandbox_restoring", "error": {"details": {"retryable": True}}}, True),
            (502, {"message": "something_else", "error": {"details": {"retryable": True}}}, False),
            (403, {"code": "sandbox_starting"}, False),
        ):
            with self.subTest(status=status, body=body):
                self.assertEqual(probe.retryable_start(probe.ApiError(status, json.dumps(body))), expected)

    def test_hydration_wait_polls_past_ready_until_hydrated(self):
        client = FakeClient({("GET", "/sandboxes/bx_23456789"):
                             [{"sandbox": {"id": "bx_23456789", "state": "ready", "hydrated": False}},
                              {"sandbox": {"id": "bx_23456789", "state": "ready", "hydrated": True}}]})
        with patch.object(probe.time, "sleep"):
            item = probe.Probe(client, evidence(), lambda: None).wait_hydrated("bx_23456789")
        self.assertTrue(item["hydrated"])
        self.assertEqual(len(client.calls), 2)

    def test_async_poll_uses_numeric_process_id_not_command_id(self):
        client = FakeClient({("POST", "/sandboxes/bx_23456789/commands"):
                             {"processId": 42, "commandId": "cmd-not-poll-key"},
                             ("GET", "/sandboxes/bx_23456789/commands/42?tailBytes=8388608"):
                             {"status": "exited", "exitCode": 0, "stdout": "{}"}})
        runner = probe.Probe(client, evidence(), lambda: None)
        self.assertEqual(runner.execute("bx_23456789", "true")["status"], "exited")
        self.assertEqual(client.calls[0][2]["detached"], True)

    def test_guest_listener_command_accepts_an_alternate_loopback_port(self):
        runner = object.__new__(probe.Probe)
        runner.source, runner.base, runner.port = b"guest script", ".boat-probe-unit", 9443
        runner.execute = MagicMock(return_value={"exitCode": 0, "stdout": "{}"})
        with patch.object(probe, "command_output", return_value={"ready": True}):
            self.assertEqual(probe.Probe.guest(runner, "bx_23456789", "listener-loopback", port=9444),
                             {"ready": True})
        self.assertIn("--port 9444", runner.execute.call_args.args[1])

    def test_lost_command_is_not_success(self):
        client = FakeClient({("POST", "/sandboxes/bx_23456789/commands"): {"processId": 2},
                             ("GET", "/sandboxes/bx_23456789/commands/2?tailBytes=8388608"): {"status": "lost"}})
        with self.assertRaisesRegex(probe.ProbeError, "lost"):
            probe.Probe(client, evidence(), lambda: None).execute("bx_23456789", "true")

    def test_create_uses_large_no_account_environment_and_journals_before_polling(self):
        item = {"id": "bx_23456789", "state": "ready"}
        client = FakeClient({("POST", "/sandboxes"): {"sandbox": item},
                             ("GET", "/sandboxes/bx_23456789"): {"sandbox": item}})
        data = evidence()
        checkpoints = []
        probe.Probe(client, data, lambda: checkpoints.append(json.loads(json.dumps(data)))).create()
        self.assertEqual(client.calls[0][2], {"type": "large", "ttlSeconds": 1800, "noEnv": True})
        self.assertTrue(checkpoints[0]["pending_creates"])
        self.assertEqual(checkpoints[1]["resources"][0]["id"], "bx_23456789")
        self.assertFalse(data["pending_creates"])

    def test_secret_is_redacted_from_nested_evidence_and_files(self):
        key = "boat_TEST_SECRET_NOT_REAL"
        data = {"api": [{"response": {"stdout": "echo " + key, "desktopUrl": "https://secret"}}], key: key}
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "results.json"
            probe.save(path, data, key)
            text = path.read_text()
            self.assertNotIn(key, text)
            self.assertNotIn("https://secret", text)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_key_read_never_uses_arguments_and_removes_env(self):
        with patch.dict(os.environ, {"BOAT_API_KEY": "env-secret"}), patch("sys.stdin", io.StringIO("stdin-secret\n")):
            args = type("Args", (), {"key_stdin": True})()
            self.assertEqual(probe.read_key(args), "stdin-secret")
            self.assertNotIn("BOAT_API_KEY", os.environ)

    def test_no_redirect_can_forward_the_bearer(self):
        with self.assertRaisesRegex(probe.ProbeError, "redirect"):
            probe.NoRedirect().redirect_request(None, None, 302, "", {}, "https://elsewhere.invalid")

    def test_render_is_offline_and_missing_values_stay_unmeasured(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "findings.md"
            probe.render(evidence(), path)
            text = path.read_text()
            self.assertIn("unmeasured", text)
            self.assertIn("**Unassessed.**", text)
            self.assertNotIn("{{", text)
            self.assertIn("Error: none recorded", text)


    def test_render_explains_pending_purge_from_an_older_checkpoint(self):
        data = evidence([{"kind": "sandbox", "id": "bx_23456789", "deletion_operation": "bdop_test"}])
        data["cleanup"] = [{"id": "bx_23456789", "operation":
                            {"status": "blocked", "stage": "waiting_for_uploads", "expectedBy": "2026-10-19T19:00:00Z"}}]
        data["cleanup_summary"] = "retry --cleanup with delete scope"
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "findings.md"
            probe.render(data, path)
            text = path.read_text()
        self.assertIn("Deletion accepted; final data purge awaits upload URL expiry", text)
        self.assertIn("2026-10-19T19:00:00Z", text)
        self.assertNotIn("with delete scope", text)


    def test_standalone_reach_verifies_exact_echo_over_tls(self):
        context, raw, stream = MagicMock(), MagicMock(), MagicMock()
        raw.__enter__.return_value = raw
        context.wrap_socket.return_value.__enter__.return_value = stream
        stream.recv.side_effect = [b"boat-probe:", b"challenge\n"]
        stream.version.return_value = "TLSv1.3"
        with patch.object(reach.ssl, "create_default_context", return_value=context), \
             patch.object(reach.socket, "create_connection", return_value=raw) as connect, \
             patch.object(reach.uuid, "uuid4", return_value=type("Uuid", (), {"hex": "challenge"})()):
            result = reach.tcp_probe("2001:db8::2", 9443, "PUBLIC CERT")
        self.assertTrue(result["verified_echo"])
        self.assertEqual(result["tls_version"], "TLSv1.3")
        connect.assert_called_once_with(("2001:db8::2", 9443), timeout=4)
        stream.sendall.assert_called_once_with(b"challenge\n")

    def test_standalone_reach_rejects_wrong_echo(self):
        context, raw, stream = MagicMock(), MagicMock(), MagicMock()
        raw.__enter__.return_value = raw
        context.wrap_socket.return_value.__enter__.return_value = stream
        stream.recv.side_effect = [b"wrong\n", b""]
        with patch.object(reach.ssl, "create_default_context", return_value=context), \
             patch.object(reach.socket, "create_connection", return_value=raw):
            result = reach.tcp_probe("2001:db8::2", 9443, "PUBLIC CERT")
        self.assertFalse(result["verified_echo"])



class CleanupTests(unittest.TestCase):
    def test_all_resources_deleted_in_reverse_order_and_operation_polled(self):
        data = evidence([{"kind": "sandbox", "id": "bx_23456789"},
                         {"kind": "named-snapshot", "id": "motor-probe-test"}])
        client = FakeClient({("DELETE", "/named-snapshots/motor-probe-test"): {"status": "deleted"},
                             ("GET", "/sandboxes/bx_23456789/usage"): {"seconds": 60, "dollars": .0012},
                             ("DELETE", "/sandboxes/bx_23456789"): deleting("pending"),
                             ("GET", "/deletion-operations/bdop_test"): [deleting("processing"), deleting()]})
        probe.cleanup(client, data, lambda: None, pause=lambda _: None)
        self.assertTrue(data["cleanup_complete"])
        self.assertTrue(all(r["deleted"] for r in data["resources"]))
        self.assertEqual(client.calls[0][:2], ("DELETE", "/named-snapshots/motor-probe-test"))
        delete = next(c for c in client.calls if c[:2] == ("DELETE", "/sandboxes/bx_23456789"))
        self.assertEqual(delete[3], {"confirm": "bx_23456789"})
        self.assertEqual(data["resources"][0]["usage"]["seconds"], 60)

    def test_forbidden_delete_stops_but_does_not_claim_cleanup_and_continues(self):
        data = evidence([{"kind": "named-snapshot", "id": "image"}, {"kind": "sandbox", "id": "bx_23456789"}])
        client = FakeClient({("GET", "/sandboxes/bx_23456789/usage"): probe.ApiError(403, "denied"),
                             ("DELETE", "/sandboxes/bx_23456789"): probe.ApiError(403, "delete scope missing"),
                             ("POST", "/sandboxes/bx_23456789/stop"): {"status": "archiving"},
                             ("DELETE", "/named-snapshots/image"): {"status": "deleted"}})
        probe.cleanup(client, data, lambda: None, attempts=2, pause=lambda _: None)
        self.assertFalse(data["cleanup_complete"])
        self.assertFalse(data["resources"][1].get("deleted", False))
        self.assertTrue(data["resources"][0]["deleted"])
        self.assertIn("fallback_stop", data["resources"][1])

    def test_accepted_deletion_is_not_completed_deletion(self):
        data = evidence([{"kind": "sandbox", "id": "bx_23456789"}])
        client = FakeClient({("GET", "/sandboxes/bx_23456789/usage"): {},
                             ("DELETE", "/sandboxes/bx_23456789"): deleting("pending"),
                             ("GET", "/deletion-operations/bdop_test"): deleting("blocked"),
                             ("POST", "/sandboxes/bx_23456789/stop"): {}})
        probe.cleanup(client, data, lambda: None, attempts=1, pause=lambda _: None)
        self.assertFalse(data["cleanup_complete"])
        self.assertEqual(data["resources"][0]["deletion_operation"], "bdop_test")

    def test_recovery_polls_existing_operation_instead_of_trusting_hidden_404(self):
        data = evidence([{"kind": "sandbox", "id": "bx_23456789", "deletion_operation": "bdop_test", "usage": {}}])
        client = FakeClient({("GET", "/deletion-operations/bdop_test"): probe.ApiError(404, "not found"),
                             ("POST", "/sandboxes/bx_23456789/stop"): probe.ApiError(404, "not found")})
        probe.cleanup(client, data, lambda: None, attempts=1, pause=lambda _: None)
        self.assertFalse(data["cleanup_complete"])
        self.assertFalse(any(call[0] == "DELETE" for call in client.calls))

    def test_waiting_for_uploads_is_pending_purge_not_missing_delete_scope(self):
        data = evidence([{"kind": "sandbox", "id": "bx_23456789", "usage": {}}])
        operation = deleting("processing")
        operation["operation"].update(stage="waiting_for_uploads", expectedBy="2026-10-19T19:00:00Z")
        client = FakeClient({("DELETE", "/sandboxes/bx_23456789"): operation,
                             ("GET", "/deletion-operations/bdop_test"): operation})
        probe.cleanup(client, data, lambda: None, pause=lambda _: None)
        self.assertFalse(data["cleanup_complete"])
        self.assertEqual(data["cleanup_state"], "purge_pending")
        self.assertEqual(len(client.calls), 2)
        self.assertEqual(data["resources"][0]["deletion_state"]["stage"], "waiting_for_uploads")
        self.assertNotIn("fallback_stop", data["resources"][0])
        self.assertIn("upload URL expiry", data["cleanup_summary"])
        self.assertNotIn("with delete scope", data["cleanup_summary"])

    def test_lost_create_response_recovers_same_idempotency_key_and_body(self):
        data = evidence()
        pending = {"body": {"type": "large", "noEnv": True}, "idempotency": "stable-uuid"}
        data["pending_creates"] = [pending]
        client = FakeClient({("POST", "/sandboxes"): {"sandbox": {"id": "bx_23456789"}},
                             ("GET", "/sandboxes/bx_23456789/usage"): {},
                             ("DELETE", "/sandboxes/bx_23456789"): deleting(),
                             ("GET", "/deletion-operations/bdop_test"): deleting()})
        checkpoints = []
        probe.cleanup(client, data, lambda: checkpoints.append(json.dumps(data)), pause=lambda _: None)
        self.assertEqual(client.calls[0], ("POST", "/sandboxes", pending["body"], {"idempotency": "stable-uuid"}))
        self.assertEqual(data["pending_creates"], [])
        self.assertTrue(data["cleanup_complete"])
        self.assertTrue(any('"bx_23456789"' in item for item in checkpoints))

    def test_expired_create_idempotency_is_not_replayed(self):
        data = evidence()
        data["started_at"] = (datetime.now(timezone.utc) - timedelta(days=2)).isoformat()
        data["pending_creates"] = [{"body": {}, "idempotency": "old"}]
        client = FakeClient({})
        probe.cleanup(client, data, lambda: None, attempts=1, pause=lambda _: None)
        self.assertEqual(client.calls, [])
        self.assertFalse(data["cleanup_complete"])

    def test_cleanup_is_idempotent_and_missing_resources_are_absent(self):
        data = evidence([{"kind": "sandbox", "id": "bx_23456789"}])
        client = FakeClient({("GET", "/sandboxes/bx_23456789/usage"): probe.ApiError(404, "missing"),
                             ("DELETE", "/sandboxes/bx_23456789"): probe.ApiError(404, "missing")})
        probe.cleanup(client, data, lambda: None, pause=lambda _: None)
        calls = list(client.calls)
        probe.cleanup(client, data, lambda: None, pause=lambda _: None)
        self.assertEqual(client.calls, calls)
        self.assertTrue(data["cleanup_complete"])

    def test_snapshot_saving_conflict_is_waited_out_before_retry(self):
        data = evidence([{"kind": "named-snapshot", "id": "image"}])
        client = FakeClient({("DELETE", "/named-snapshots/image"):
                             [probe.ApiError(409, "save_in_progress"), {"status": "deleted"}],
                             ("GET", "/named-snapshots/image"): {"snapshot": {"status": "ready"}}})
        probe.cleanup(client, data, lambda: None, attempts=2, pause=lambda _: None)
        self.assertTrue(data["cleanup_complete"])
        self.assertEqual([c[0] for c in client.calls], ["DELETE", "GET", "DELETE"])

    def test_main_failure_still_cleans_up_and_prints_only_redacted_json(self):
        with tempfile.TemporaryDirectory() as tmp:
            destination = Path(tmp) / "run"
            def fail(runner):
                runner.data["resources"].append({"kind": "named-snapshot", "id": "image"})
                raise RuntimeError("remote text: boat_NOT_REAL")
            client = FakeClient({("DELETE", "/named-snapshots/image"): {"status": "deleted"}})
            output = io.StringIO()
            with patch("sys.argv", ["probe.py", "--output", str(destination)]), \
                 patch.dict(os.environ, {"BOAT_API_KEY": "boat_NOT_REAL"}), \
                 patch.object(probe, "Client", return_value=client), \
                 patch.object(probe.Probe, "run", fail), patch.object(probe.signal, "signal"), \
                 contextlib.redirect_stdout(output):
                self.assertEqual(probe.main(), 1)
            self.assertNotIn("boat_NOT_REAL", output.getvalue())
            self.assertTrue(json.loads(output.getvalue())["cleanup_complete"])
            self.assertNotIn("boat_NOT_REAL", (destination / "results.md").read_text())


class SSHTests(unittest.TestCase):
    def test_ipv4_ssh_endpoint_wins_over_ipv6_machine_ip(self):
        route = ssh_reach.endpoint({"sshEndpoint": "192.0.2.3:22001", "machineIp": "2001:db8::2"})
        self.assertEqual((route["host"], route["port"], route["family"]), ("192.0.2.3", 22001, 4))
        self.assertEqual(ssh_reach.endpoint({"machineIp": "192.0.2.4"})["port"], 22)

    def test_api_host_key_is_only_trust_anchor_and_ssh_never_uses_tofu(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "known_hosts"
            ssh_reach.pin_host(path, "bx_23456789", {"hostKey": "ssh-ed25519 dGVzdC1rZXk="})
            self.assertEqual(path.read_text(), "bx_23456789 ssh-ed25519 dGVzdC1rZXk=\n")
            argv = ssh_reach.ssh_argv({"host": "192.0.2.3", "port": 22001}, Path(tmp) / "key", path, "bx_23456789")
        for option in ("StrictHostKeyChecking=yes", "GlobalKnownHostsFile=/dev/null", "UpdateHostKeys=no",
                       "IdentityAgent=none", "IdentitiesOnly=yes", "ControlPath=none"):
            self.assertIn(option, argv)
        self.assertIn("UserKnownHostsFile=" + str(path), argv)
        self.assertIn("HostKeyAlias=bx_23456789", argv)
        self.assertIn("-4", argv)
        self.assertNotIn("accept-new", " ".join(argv))

    def test_missing_or_multiline_host_key_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            for key in ("", "ssh-ed25519 dGVzdA==\nother injected", "unknown dGVzdA=="):
                with self.subTest(key=key), self.assertRaises(ValueError):
                    ssh_reach.pin_host(Path(tmp) / "known_hosts", "bx_23456789", {"hostKey": key})

    def test_refusal_requires_authenticated_connection_and_no_command_marker(self):
        auth = 'Authenticated to host using "publickey".\n'
        self.assertTrue(ssh_reach.refused({"exit_code": 1, "stderr": auth, "stdout": ""}, "marker"))
        self.assertTrue(ssh_reach.refused({"exit_code": 255, "stderr": auth + "PTY allocation request failed on channel 0\n",
                                           "stdout": ""}, "marker"))
        self.assertFalse(ssh_reach.refused({"exit_code": 255, "stderr": "Permission denied (publickey)."}, "marker"))
        self.assertFalse(ssh_reach.refused({"exit_code": 255, "stderr": auth + "Connection reset"}, "marker"))
        self.assertFalse(ssh_reach.refused({"exit_code": 0, "stderr": auth, "stdout": "marker"}, "marker"))

    def test_remote_forward_row_requires_authenticated_open_failure(self):
        auth = 'Authenticated to host using "publickey".\n'
        refusal = "remote port forwarding failed for listen port 0"
        self.assertTrue(ssh_reach.remote_forward_refused({"exit_code": 255, "stderr": auth + refusal}))
        self.assertFalse(ssh_reach.remote_forward_refused({"exit_code": 255, "stderr": refusal}))
        self.assertFalse(ssh_reach.remote_forward_refused({"exit_code": 1, "stderr": auth + "Connection reset"}))
        self.assertFalse(ssh_reach.remote_forward_refused({"exit_code": None, "stderr": auth + refusal}))

    def test_allowed_command_is_reported_fail_for_forward_only_requirement(self):
        runner = type("Runner", (), {"data": {"run_id": "unit"}, "checkpoint": lambda self: None})()
        measurement = ssh_reach.SSHReach(runner)
        result = {"exit_code": 0, "stdout": "boat-ssh-unit\n", "stderr": 'Authenticated to host using "publickey".'}
        with patch.object(ssh_reach, "process", return_value=result):
            measurement.connection(["ssh", "user@192.0.2.3"], "restricted.exec_refused", refuse=True)
        self.assertEqual(measurement.data["checks"][0]["status"], "FAIL")

    def test_shell_refusal_checks_plain_and_forced_pty_without_marker_echo(self):
        runner = type("Runner", (), {"data": {"run_id": "unit"}, "checkpoint": lambda self: None})()
        measurement = ssh_reach.SSHReach(runner)
        auth = 'Authenticated to host using "publickey".\n'
        with patch.object(ssh_reach, "process", return_value={"exit_code": 1, "stdout": "", "stderr": auth}) as run:
            measurement.connection(["ssh", "-T", "user@192.0.2.3"], "restricted.shell_without_pty_refused",
                                   refuse=True, shell=True)
            measurement.connection(["ssh", "-T", "user@192.0.2.3"], "restricted.shell_with_pty_refused",
                                   refuse=True, shell=True, pty=True)
        plain, terminal = run.call_args_list
        self.assertNotIn("-tt", plain.args[0])
        self.assertIn("-tt", terminal.args[0])
        for call in (plain, terminal):
            self.assertNotIn(measurement.marker, call.kwargs["input_text"])
        self.assertEqual([row["status"] for row in measurement.data["checks"]], ["PASS", "PASS"])

    def test_restricted_phase_emits_independent_json_rows_for_all_capabilities(self):
        auth = 'Authenticated to host using "publickey".\n'
        runner = MagicMock()
        runner.data = {"run_id": "unit"}
        runner.guest.side_effect = [
            {"ready": True, "listening_addresses": ["127.0.0.1:9443"], "certificate_pem": "CERT"},
            {"ready": True, "listening_addresses": ["127.0.0.1:9444"], "certificate_pem": "CERT"},
        ]
        measurement = ssh_reach.SSHReach(runner)
        with tempfile.TemporaryDirectory() as tmp, \
             patch.object(ssh_reach, "process", return_value={"exit_code": 1, "stdout": "", "stderr": auth}), \
             patch.object(ssh_reach, "forward", return_value={"verified_echo": True, "tunnel_process_reaped": True}), \
             patch.object(ssh_reach, "blocked_forward", return_value={"server_refused": True,
                                                                        "verified_echo": False,
                                                                        "tunnel_process_reaped": True}), \
             patch.object(ssh_reach, "remote_forward", return_value={"refused": True,
                                                                       "tunnel_process_reaped": True}):
            measurement.phase("bx_23456789", "restricted", {
                "hostKey": "ssh-ed25519 dGVzdA==", "sshEndpoint": "192.0.2.3:22001"},
                Path(tmp) / "key", Path(tmp), restricted=True)
        checks = json.loads(json.dumps(measurement.data["checks"]))
        rows = {row["name"]: row["status"] for row in checks}
        expected = {
            "restricted.exec_refused",
            "restricted.shell_without_pty_refused",
            "restricted.shell_with_pty_refused",
            "restricted.tls_over_permitted_forward",
            "restricted.other_destination_forward_refused",
            "restricted.remote_forward_refused",
        }
        self.assertTrue(expected.issubset(rows))
        self.assertTrue(all(rows[name] == "PASS" for name in expected))
        runner.guest.assert_called_with("bx_23456789", "listener-loopback", port=ssh_reach.OTHER_PORT)

    def test_restrict_writes_forced_command_byte_for_byte_and_preserves_other_keys(self):
        public = "ssh-ed25519 dGVzdC1rZXk= tested"
        unrelated = "ssh-ed25519 b3RoZXIta2V5 unrelated"
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / ".ssh/authorized_keys"
            path.parent.mkdir()
            path.write_text(public + "\n" + unrelated + "\n" + public + "\n")

            class LocalRunner:
                data = {"run_id": "unit"}

                def execute(self, sid, text):
                    completed = ssh_reach.subprocess.run(["/bin/sh", "-c", text], capture_output=True, text=True,
                                                         env=dict(os.environ, HOME=tmp))
                    return {"exitCode": completed.returncode, "stdout": completed.stdout,
                            "stderr": completed.stderr}

                def checkpoint(self):
                    pass

            observed = ssh_reach.SSHReach(LocalRunner()).key_lines("bx_23456789", public, "restrict")
            forced = 'restrict,port-forwarding,permitopen="127.0.0.1:9443",command="/bin/false"'
            expected = forced + " " + public
            self.assertEqual(observed["after"], [expected])
            self.assertEqual(observed["raw_after"], [base64.b64encode((expected + "\n").encode()).decode("ascii")])
            self.assertEqual(path.read_text().splitlines(), [unrelated, expected])

    def test_tunnel_uses_no_session_channel_and_reaps_local_ssh(self):
        child = MagicMock()
        child.poll.return_value = None
        with tempfile.TemporaryDirectory() as tmp, \
             patch.object(ssh_reach.subprocess, "Popen", return_value=child) as launch, \
             patch.object(ssh_reach, "tcp_probe", return_value={"verified_echo": True}):
            result = ssh_reach.forward(["ssh", "-4", "user@192.0.2.3"], "PUBLIC CERT", Path(tmp))
        argv = launch.call_args.args[0]
        self.assertIn("-N", argv)
        self.assertIn("-L", argv)
        self.assertTrue(any(part.endswith(":127.0.0.1:9443") for part in argv))
        child.terminate.assert_called_once()
        child.wait.assert_called_once()
        self.assertTrue(result["verified_echo"])
        self.assertTrue(result["tunnel_process_reaped"])

    def test_private_api_credential_not_in_local_ssh_environment(self):
        completed = type("Completed", (), {"returncode": 0, "stdout": "ok", "stderr": ""})()
        with patch.dict(os.environ, {"BOAT_API_KEY": "not-for-ssh"}), \
             patch.object(ssh_reach.subprocess, "run", return_value=completed) as run:
            ssh_reach.process(["ssh", "-V"])
        self.assertNotIn("BOAT_API_KEY", run.call_args.kwargs["env"])

    def test_resume_refresh_does_not_reauthorize_tested_key_before_persistence_checks(self):
        runner = MagicMock()
        runner.data = {"run_id": "unit"}
        runner.create.return_value = ({"id": "bx_23456789", "vcpu": 8, "memoryGB": 16}, {})
        measurement = ssh_reach.SSHReach(runner)
        public = "ssh-ed25519 dGVzdA== tested"
        witness_public = "ssh-ed25519 d2l0bmVzcw== witness"
        restricted = 'restrict,port-forwarding,permitopen="127.0.0.1:9443",command="/bin/false" ' + public
        raw_restricted = base64.b64encode((restricted + "\n").encode()).decode("ascii")
        def generated(argv, **kwargs):
            if argv[0] == "ssh-keygen":
                path = Path(argv[-1])
                path.write_text("not-a-real-private-key")
                path.with_suffix(".pub").write_text(public if path.name.startswith("probe") else witness_public)
            return {"exit_code": 0, "stdout": "", "stderr": ""}
        response = {"hostKey": "ssh-ed25519 dGVzdA==", "sshEndpoint": "192.0.2.3:22001", "machineIp": "2001:db8::2"}
        with patch.object(ssh_reach, "process", side_effect=generated), \
             patch.object(measurement, "authorize", return_value=response) as authorize, \
             patch.object(measurement, "phase") as phase, patch.object(measurement, "resume"), \
             patch.object(measurement, "key_lines", side_effect=[{"matching_keys": 1, "after": [public]},
                                                                {"after": [restricted], "raw_after": [raw_restricted]},
                                                                {"after": [restricted], "raw_after": [raw_restricted]}]):
            measurement.run()
        self.assertEqual([call.args[1] for call in authorize.call_args_list],
                         [public, witness_public, public, witness_public])
        self.assertEqual([call.args[1] for call in phase.call_args_list],
                         ["initial", "old_key_before_reauthorize", "reauthorized", "restricted", "restricted_after_resume"])
        self.assertTrue(measurement.data["local_private_keys_removed"])


class JsonStdoutTests(unittest.TestCase):
    def test_live_failure_keeps_stdout_json_and_never_echoes_exception_values(self):
        output, errors = io.StringIO(), io.StringIO()
        def fail(report_stream=None):
            print('incidental progress')
            raise ValueError('canary-credential-never-echo')
        with patch('sys.argv', ['probe.py','--runner-host']), patch.object(probe, 'main', side_effect=fail), \
             contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            self.assertEqual(probe.entrypoint(), 1)
        self.assertEqual(json.loads(output.getvalue())['error_type'], 'ValueError')
        self.assertIn('incidental progress', errors.getvalue())
        self.assertNotIn('canary-credential-never-echo', output.getvalue() + errors.getvalue())

    def test_argument_failure_also_has_one_json_report(self):
        output, errors = io.StringIO(), io.StringIO()
        with patch('sys.argv', ['probe.py','--not-a-probe-option']), contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            self.assertEqual(probe.entrypoint(), 1)
        self.assertEqual(json.loads(output.getvalue())['error'], 'invalid probe arguments')


class PreparedImageCleanupTests(unittest.TestCase):
    def test_sandbox_is_never_retained_even_if_checkpoint_has_the_flag(self):
        data = evidence([{'kind':'sandbox','id':'bx_12345678','operator_retained':True}])
        client = FakeClient({('GET','/sandboxes/bx_12345678/usage'):{},
                             ('DELETE','/sandboxes/bx_12345678'):{'operation':{'id':'operation'}},
                             ('GET','/deletion-operations/operation'):{'operation':{'status':'completed'}}})
        probe.cleanup(client, data, lambda: None, pause=lambda _: None)
        self.assertTrue(data['resources'][0]['deleted'])
        self.assertEqual(data['operator_retained_resources'], [])

    def test_only_explicit_retained_image_survives_and_measurement_snapshot_is_deleted(self):
        image = {'kind':'named-snapshot','id':'prepared','operator_retained':True}
        measurement = {'kind':'named-snapshot','id':'measurement'}
        data = evidence([image, measurement])
        client = FakeClient({('DELETE','/named-snapshots/measurement'):{'status':'deleted'}})
        probe.cleanup(client, data, lambda: None, pause=lambda _: None)
        self.assertTrue(data['cleanup_complete'])
        self.assertEqual([r['id'] for r in data['operator_retained_resources']], ['prepared'])
        self.assertFalse(data['resources'][0].get('deleted', False))
        self.assertTrue(data['resources'][1]['deleted'])
        self.assertIn('not deleted', data['cleanup_summary'])

    def test_explicit_delete_retained_images_cleanup_removes_the_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'results.json'
            path.write_text(json.dumps(evidence([{'kind':'named-snapshot','id':'prepared','operator_retained':True}])))
            client = FakeClient({('DELETE','/named-snapshots/prepared'):{'status':'deleted'}})
            output = io.StringIO()
            with patch('sys.argv', ['probe.py','--cleanup',str(path),'--delete-retained-images']), \
                 patch.object(probe,'read_key',return_value='fake-key'), patch.object(probe,'Client',return_value=client), \
                 patch.object(probe.signal,'signal'), contextlib.redirect_stdout(output):
                self.assertEqual(probe.main(), 0)
            result = json.loads(output.getvalue())
            self.assertEqual(result['operator_retained_resources'], [])
            self.assertTrue(result['resources'][0]['deleted'])


def waiting_for_uploads(sid="bx_23456789"):
    return {"kind": "sandbox", "id": sid, "deletion_operation": "bdop_test",
            "deletion_state": {"status": "blocked", "stage": "waiting_for_uploads", "expectedBy": "2026-10-20T04:00:00Z"}}


class ExitStatusTests(unittest.TestCase):
    def test_cleanup_state_separates_accepted_purge_from_possible_leaks(self):
        retained = {"kind": "named-snapshot", "id": "prepared", "operator_retained": True}
        deleted = {"kind": "named-snapshot", "id": "measurement", "deleted": True}
        self.assertEqual(probe.cleanup_state(evidence([retained, deleted])), "complete")
        self.assertEqual(probe.cleanup_state(evidence([retained, deleted, waiting_for_uploads()])), "purge_pending")
        undeleted_snapshot = dict(waiting_for_uploads(), kind="named-snapshot")
        other_stage = waiting_for_uploads()
        other_stage["deletion_state"] = {"status": "processing", "stage": "removing"}
        never_accepted = {"kind": "sandbox", "id": "bx_23456789"}
        for leak in (undeleted_snapshot, other_stage, never_accepted):
            with self.subTest(leak=leak):
                self.assertEqual(probe.cleanup_state(evidence([waiting_for_uploads(), leak])), "incomplete")
        unrecorded = evidence([waiting_for_uploads()])
        unrecorded["pending_creates"] = [{"idempotency": "key", "body": {}}]
        self.assertEqual(probe.cleanup_state(unrecorded), "incomplete")

    def test_exit_status_follows_the_verdict_and_reports_leaks_with_three(self):
        passed = dict(evidence([waiting_for_uploads()]), measurements_complete=True,
                      runner_host={"all_passed": True})
        self.assertEqual(probe.exit_status(passed), 0)
        for change in ({"runner_host": {"all_passed": False}}, {"ssh_reach": {}}, {"error": "boom"},
                       {"measurements_complete": False}):
            with self.subTest(change=change):
                self.assertEqual(probe.exit_status(dict(passed, **change)), 1)
        leaked = dict(passed, resources=[{"kind": "sandbox", "id": "bx_23456789"}])
        self.assertEqual(probe.exit_status(leaked), 3)
        self.assertEqual(probe.exit_status(dict(leaked, error="boom")), 3)
        self.assertEqual(probe.exit_status(evidence([waiting_for_uploads()]), cleanup_only=True), 0)

    def test_passed_runner_host_with_pending_purge_exits_zero_and_reports_cleanup_separately(self):
        def passing(host):
            host.probe.data["resources"].append({"kind": "sandbox", "id": "bx_23456789"})
            host.data.update(checks=[{"name": "initial.exit", "status": "PASS", "evidence": {}}], all_passed=True)
        operation = deleting("blocked")
        operation["operation"].update(stage="waiting_for_uploads", expectedBy="2026-10-20T04:00:00Z")
        client = FakeClient({("GET", "/sandboxes/bx_23456789/usage"): {"seconds": 1, "dollars": 0.00001},
                             ("DELETE", "/sandboxes/bx_23456789"): operation,
                             ("GET", "/deletion-operations/bdop_test"): operation})
        runner_host = sys.modules.get("runner_host")
        if runner_host is None:
            sys.path.insert(0, str(HERE))
            try:
                import runner_host
            finally:
                sys.path.pop(0)
        with tempfile.TemporaryDirectory() as tmp:
            output = io.StringIO()
            with patch("sys.argv", ["probe.py", "--runner-host", "--output", str(Path(tmp) / "run")]), \
                 patch.object(probe, "read_key", return_value="fake-key"), patch.object(probe, "Client", return_value=client), \
                 patch.object(runner_host.RunnerHost, "run", passing), patch.object(probe.signal, "signal"), \
                 contextlib.redirect_stdout(output):
                self.assertEqual(probe.main(), 0)
            result = json.loads(output.getvalue())
            findings = (Path(tmp) / "run" / "results.md").read_text()
        self.assertFalse(result["cleanup_complete"])
        self.assertEqual(result["cleanup_state"], "purge_pending")
        self.assertEqual(result["verdict"], {"passed": True, "reasons": []})
        self.assertEqual(result["exit_status"], 0)
        self.assertIn("Runner-host verdict: **PASSED**", findings)
        self.assertIn('Cleanup state (reported separately from the verdict): "purge_pending"', findings)


def runner_phase(name, memory_max, code=23):
    job = {"user": "motor-job", "interfaces": ["lo"], "nproc": "8", "namespaces": {"pid": "pid:[1]"},
           "limits": {"memory.max": str(memory_max), "cpu.max": "800000 100000", "pids.max": "4096"}}
    return {"name": name,
            "host": {"namespaces": {"pid": "pid:[1]"},
                     "memory_budget": {"mem_available_bytes": memory_max + (600 << 20), "runner_idle_bytes": 88 << 20,
                                       "margin_bytes": 512 << 20, "memory_max_bytes": memory_max}},
            "exit": {"seconds": 1.234, "result": {"code": code}}, "rust": {"seconds": 1.5, "result": {"code": 0}},
            "isolation": {"seconds": 1.0, "result": {"code": 0, "stdout": json.dumps(job)}}}


class RunnerHostRenderTests(unittest.TestCase):
    def runner_data(self, status="PASS"):
        data = evidence([waiting_for_uploads(), {"kind": "named-snapshot", "id": "motor-boat-prepared-abc",
                                                  "operator_retained": True}])
        data.update(measurements_complete=True, runner_host={
            "all_passed": status == "PASS", "source_revision": "a" * 40,
            "checks": [{"name": "initial.exit", "status": "PASS"}, {"name": "resumed.memory_limit", "status": status},
                       {"name": "sshd.dropin_survives_resume", "status": "PASS"}],
            "phases": [runner_phase("initial", 13452 << 20), runner_phase("resumed", 13391 << 20)],
            "create": {"seconds_to_ready": 2.72}, "install_seconds": 63.62, "snapshot_create_to_first_job_seconds": 68.105,
            "create_from_snapshot": {"seconds_to_ready": 5.41},
            "snapshot": {"name": "motor-runner-x", "id": "snap-1", "size_bytes": 7, "seconds": 19.05},
            "prepared_image": {"name": "motor-boat-prepared-abc", "id": "snap-2", "size_bytes": 8, "seconds": 12.03,
                               "operator_retained": True, "real_jobs_qualified": True, "delete_command": "probe --cleanup x"}})
        return data

    def rendered(self, data):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "results.md"
            probe.render(data, path)
            return path.read_text()

    def test_runner_host_report_shows_verdict_phases_timings_and_memory_not_the_environment_template(self):
        text = self.rendered(self.runner_data())
        self.assertIn("Runner-host verdict: **PASSED**. Checks: 3 of 3 passed.", text)
        self.assertIn("Exit status under the current rules: 0 (the run did not record one)", text)
        self.assertIn('| initial | 1 of 1 | 23 | 0 | [1.23, 1.5, 1.0] | "motor-job" | ["lo"] | "14105444352" | '
                      '"800000 100000" | "4096" | "8" | true |', text)
        self.assertIn("| Create from measurement snapshot to first finished job | 68.1 |", text)
        self.assertIn("| Source upload and recipe install | 63.6 |", text)
        self.assertIn("| resumed | ", text)
        self.assertIn("| 13391 | ", text)
        self.assertIn("Smallest job memory budget across phases: 13391 MiB.", text)
        self.assertIn("Checks outside a phase: sshd.dropin_survives_resume PASS.", text)
        self.assertIn("| named-snapshot motor-boat-prepared-abc | retained by operator |", text)
        self.assertIn("`probe --cleanup x`", text)
        self.assertIn("final data purge awaits upload URL expiry", text)
        for stale in ("Unassessed", "{{", "environment probe"):
            self.assertNotIn(stale, text)

    def test_failed_check_is_named_and_fails_the_rendered_verdict(self):
        data = self.runner_data(status="FAIL")
        data["exit_status"] = 1
        text = self.rendered(data)
        self.assertIn("Runner-host verdict: **FAILED**. Checks: 2 of 3 passed.", text)
        self.assertIn('Failing checks: ["resumed.memory_limit"].', text)
        self.assertIn('Reasons: ["runner_host checks did not all pass"].', text)
        self.assertIn("Exit status: 1", text)

    def test_missing_runner_evidence_stays_unmeasured(self):
        data = evidence()
        data["runner_host"] = {"checks": [], "phases": [{"name": "initial"}], "all_passed": False}
        text = self.rendered(data)
        self.assertIn("**FAILED**", text)
        self.assertIn("| Create from measurement snapshot to first finished job | \"unmeasured\" |", text)
        self.assertIn("Smallest job memory budget across phases: unmeasured MiB.", text)
        self.assertNotIn("{{", text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
