"""Portable service-order tests; no services, VMs or root permissions are used."""
import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import switch_mode as switch


class SwitchModeTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR", "/srv/build/tmp"))
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.host_path = self.root / "host.json"
        self.helper_path = self.root / "helper.json"
        self.disk = self.root / "tenant.ext4"
        self.disk.write_bytes(b"retained tenant disk")
        self.bindings = self.root / "bindings.json"
        self.bindings.write_text(json.dumps({"vm": {"tenant": "tenant", "sub": "account"}}))
        (self.root / "tenant.json").write_text(json.dumps({"account": "account", "disk": str(self.disk), "generation": str(self.root / "generation")}))
        self.host: dict[str, object] = {"host_id": "test-host", "bindings": str(self.bindings), "registry_dir": str(self.root),
                     "admin_socket": str(self.root / "admin.sock"), "helper_socket": str(self.root / "helper.sock")}
        self.host_path.write_text(json.dumps(self.host))
        self.helper_path.write_text(json.dumps({"allowed_uids": [1000]}))
        self.per_job = self.root / "per-job.json"
        self.per_job.write_text(json.dumps({"workspace_disk_cap_bytes": 1024}))
        self.job_vms = self.root / "job-vms.json"
        self.job_vms.write_text(json.dumps({"workspace_disk_max_bytes": 1024}))
        self.status = {"ok": True, "draining": True, **dict.fromkeys(switch.COUNTS, 0)}
        self.events = []
        self.command_run = self.patch(switch, "run", side_effect=lambda *args: self.events.append(tuple(map(str, args))))
        self.admin = self.patch(switch.hosted_host, "admin_exchange", side_effect=lambda *_, **__: dict(self.status))
        self.drain = self.patch(switch.hosted_host, "drain_host", side_effect=lambda *_: self.events.append(("fresh-drain",)))
        self.publish = self.patch(switch, "publish_to_helper", side_effect=lambda *args: self.events.append(("publish", *map(str, args))))
        self.real_clean_stop = switch.clean_stop_tenants
        self.clean_stop = self.patch(switch, "clean_stop_tenants", side_effect=lambda *_: self.events.append(("clean-stop-tenants",)))
        self.real_wait_helper = switch.wait_helper
        self.wait_helper = self.patch(switch, "wait_helper", side_effect=lambda *_: self.events.append(("helper-ready",)))

    def patch(self, target, name, **kwargs):
        patcher = mock.patch.object(target, name, **kwargs)
        value = patcher.start()
        self.addCleanup(patcher.stop)
        return value

    def perform(self, mode):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            switch.switch(mode, per_job=self.per_job, job_vms=self.job_vms, generation="gen-1",
                          host_path=self.host_path, helper_path=self.helper_path, timeout=1)
        return output.getvalue()

    def test_per_job_switch_validates_publishes_selects_and_keeps_placement_drained(self):
        output = self.perform("per_job")
        self.assertEqual(switch.read_json(self.host_path)["mode"], "per_job")
        self.assertEqual(switch.read_json(self.helper_path)["job_vms"], switch.read_json(self.job_vms))
        self.assertEqual([event[:3] for event in self.events[1:]], [
            ("fresh-drain",), ("publish", self.host["helper_socket"], "gen-1"),
            ("systemctl", "stop", "motor-host.service"),
            ("clean-stop-tenants",),
            ("systemctl", "stop", "motor-vm-helper.service"),
            ("systemctl", "start", "motor-vm-helper.service"),
            ("helper-ready",),
            ("runuser", "-u", "motor-host"),
            (str(switch.hosted_host.HOST_BINARY), "check-config", str(self.host_path)),
            ("systemctl", "start", "motor-host.service"),
        ])
        select = next(event for event in self.events if "select-job-images" in event)
        self.assertEqual(select[-2:], (str(self.host_path), "gen-1"))
        self.assertIn("placement_hint.host='test-host'", output)
        self.assertIn("only after success", output)
        self.assertTrue(all(call.args[0] == {"op": "host_status"} for call in self.admin.call_args_list))

    def test_tenant_switch_only_selects_tenant_and_uses_existing_runtime_rollback(self):
        self.host.update(mode="per_job", per_job={"workspace_disk_cap_bytes": 1024})
        self.host_path.write_text(json.dumps(self.host))
        helper = {"allowed_uids": [1000], "job_vms": {"workspace_disk_max_bytes": 1024}}
        self.helper_path.write_text(json.dumps(helper))
        self.perform("tenant")
        self.assertEqual(switch.read_json(self.host_path)["mode"], "tenant")
        self.assertNotIn("per_job", switch.read_json(self.host_path))
        self.assertEqual(switch.read_json(self.helper_path), helper)
        self.assertEqual(self.disk.read_bytes(), b"retained tenant disk")
        self.publish.assert_not_called()
        self.assertFalse(any("select-job-images" in event for event in self.events))
        self.assertEqual([event for event in self.events if event[0] == "systemctl"], [
            ("systemctl", "stop", "motor-host.service"), ("systemctl", "stop", "motor-vm-helper.service"),
            ("systemctl", "start", "motor-vm-helper.service"), ("systemctl", "start", "motor-host.service"),
        ])

    def test_every_background_work_count_blocks_the_switch_with_a_bounded_wait(self):
        self.host["mode"] = "per_job"
        self.host_path.write_text(json.dumps(self.host))
        before = self.host_path.read_bytes()
        for field in switch.COUNTS:
            with self.subTest(field=field):
                self.status.update(dict.fromkeys(switch.COUNTS, 0))
                self.status[field] = 1
                self.events.clear()
                with mock.patch.object(switch.time, "monotonic", side_effect=[0, 0, 2]):
                    with self.assertRaisesRegex(ValueError, f"still running:.*{field}.*host remains drained"):
                        self.perform("tenant")
                self.assertFalse(any(event[0] == "systemctl" for event in self.events))
                self.assertEqual(self.host_path.read_bytes(), before)

    def test_missing_background_work_fields_are_not_treated_as_zero(self):
        for field in switch.COUNTS:
            with self.subTest(field=field):
                status = dict(self.status)
                del status[field]
                with mock.patch.object(switch.hosted_host, "admin_exchange", return_value=status):
                    with self.assertRaisesRegex(ValueError, "lacks mode-switch work counts"):
                        switch.wait_empty(Path("unused.sock"), "per_job", 1)

    def test_failed_fresh_broker_echo_never_stops_services_or_changes_configs(self):
        self.drain.side_effect = ValueError("fresh broker echo timed out")
        before = self.host_path.read_bytes()
        with self.assertRaisesRegex(ValueError, "fresh broker echo"):
            self.perform("per_job")
        self.assertFalse(any(event[0] == "systemctl" for event in self.events))
        self.assertEqual(self.host_path.read_bytes(), before)
        self.publish.assert_not_called()

    def test_missing_rollback_binding_or_account_mismatch_refuses_before_stopping(self):
        for bindings in [{}, {"vm": {"tenant": "tenant", "sub": "other-account"}}]:
            with self.subTest(bindings=bindings):
                self.bindings.write_text(json.dumps(bindings))
                with self.assertRaisesRegex(ValueError, "rollback"):
                    self.perform("per_job")
                self.command_run.assert_not_called()
                self.drain.assert_not_called()

    def test_generation_publication_failure_keeps_existing_config_and_services(self):
        self.publish.side_effect = ValueError("unverified image")
        before = self.host_path.read_bytes()
        with self.assertRaisesRegex(ValueError, "unverified image"):
            self.perform("per_job")
        self.assertEqual(self.host_path.read_bytes(), before)
        self.assertFalse(any(event[0] == "systemctl" for event in self.events))

    def test_refuses_an_undrained_host(self):
        self.status["draining"] = False
        with self.assertRaisesRegex(ValueError, "drain the host"):
            self.perform("per_job")
        self.drain.assert_not_called()
        self.publish.assert_not_called()

    def test_clean_stop_timeout_never_stops_helper_or_rewrites_config(self):
        self.clean_stop.side_effect = TimeoutError("tenant clean stop timed out")
        before = self.host_path.read_bytes()
        with self.assertRaisesRegex(TimeoutError, "clean stop timed out"):
            self.perform("per_job")
        self.assertIn(("systemctl", "stop", "motor-host.service"), self.events)
        self.assertNotIn(("systemctl", "stop", "motor-vm-helper.service"), self.events)
        self.assertEqual(self.host_path.read_bytes(), before)

    def test_clean_stop_requires_stopped_reply_and_no_unclean_markers(self):
        registry = switch.read_json(self.root / "tenant.json")
        with mock.patch.object(switch, "helper_request", return_value={"result": "tenant", "running": False}) as request:
            self.real_clean_stop("helper.sock", {"tenant": registry}, 1)
            self.assertEqual([call.args[1]["op"] for call in request.call_args_list], ["stop_tenant", "adopt"])
            for name in ("running.json", "unclean-stop.json"):
                marker = self.root / name
                marker.write_text("{}")
                with self.assertRaisesRegex(ValueError, "stop was not clean"):
                    self.real_clean_stop("helper.sock", {"tenant": registry}, 1)
                marker.unlink()
        for reply in [{"result": "tenant", "running": True}, {"result": "error"}, {"result": "ok"}]:
            with mock.patch.object(switch, "helper_request", return_value=reply):
                with self.assertRaisesRegex(ValueError, "did not report stopped"):
                    self.real_clean_stop("helper.sock", {"tenant": registry}, 1)


    def test_helper_requests_keep_the_installed_host_account_socket_identity(self):
        request = {"op": "stop_tenant", "tenant": "tenant"}
        with mock.patch.object(switch.subprocess, "run", return_value=mock.Mock(stdout='{"result":"tenant","running":false}')) as run:
            self.assertEqual(switch.helper_request("helper.sock", request, 7), {"result": "tenant", "running": False})
            command = run.call_args.args[0]
            self.assertEqual(command[:4], ["runuser", "-u", "motor-host", "--"])
            self.assertEqual(command[-4:], ["--helper-request", "helper.sock", json.dumps(request), "7"])

    def test_helper_start_wait_retries_unbound_stale_and_not_yet_owned_sockets(self):
        reply = {"result": "tenant", "running": False}
        request = {"op": "adopt", "tenant": "tenant"}
        with mock.patch.object(switch, "helper_exchange", side_effect=[FileNotFoundError(), ConnectionRefusedError(), PermissionError(), reply]) as exchange:
            with mock.patch.object(switch.time, "sleep"):
                self.assertEqual(switch.wait_helper_exchange("helper.sock", request, 1), reply)
            self.assertEqual(exchange.call_count, 4)
        with mock.patch.object(switch, "helper_request", return_value=reply) as request:
            self.real_wait_helper("helper.sock", "tenant", 1)
            self.assertTrue(request.call_args.kwargs["wait"])

    def test_helper_start_wait_is_bounded_and_refuses_a_running_tenant(self):
        with mock.patch.object(switch, "helper_exchange", side_effect=ConnectionRefusedError()):
            with mock.patch.object(switch.time, "monotonic", side_effect=[0, 0, 2]):
                with self.assertRaisesRegex(ValueError, "helper socket did not become ready.*drained"):
                    switch.wait_helper_exchange("helper.sock", {}, 1)
        with mock.patch.object(switch, "helper_request", return_value={"result": "tenant", "running": True}):
            with self.assertRaisesRegex(ValueError, "did not confirm.*stopped"):
                self.real_wait_helper("helper.sock", "tenant", 1)


if __name__ == "__main__":
    unittest.main()
