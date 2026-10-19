#!/usr/bin/env python3
"""Portable plumbing tests; these do not assert anything about a real Boat VM."""
import contextlib
from datetime import datetime, timedelta, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("boat_probe", HERE / "probe.py")
if spec is None or spec.loader is None:
    raise ImportError("cannot load the Boat probe beside its tests")
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


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


if __name__ == "__main__":
    unittest.main(verbosity=2)
