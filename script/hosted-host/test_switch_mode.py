"""Portable service-order tests; no services, VMs or root permissions are used."""
import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import switch_mode as switch


class SwitchModeTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
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
        self.services = {"motor-host.service": True, "motor-vm-helper.service": True}
        self.failure_point = None
        self.failed = False
        self.installed = self.root / "installed"
        self.installed.mkdir()
        self.old_binaries = {}
        for name, constant in [("motor-host", "HOST_BINARY"), ("motor-vm-helper", "HELPER_BINARY")]:
            path = self.installed / name
            path.write_bytes(f"old {name}".encode())
            path.chmod(0o755)
            self.old_binaries[path] = path.read_bytes()
            self.patch(switch.hosted_host, constant, new=path)
        self.original_host = self.host_path.read_bytes()
        self.original_helper = self.helper_path.read_bytes()
        self.command_run = self.patch(switch, "run", side_effect=self.command)
        self.admin = self.patch(switch.hosted_host, "admin_exchange", side_effect=lambda *_, **__: dict(self.status))
        self.drain = self.patch(switch.hosted_host, "drain_host", side_effect=lambda *_: self.events.append(("fresh-drain",)))
        self.publish = self.patch(switch, "publish_to_helper", side_effect=lambda *args: self.events.append(("publish", *map(str, args))))
        self.real_clean_stop = switch.clean_stop_tenants
        self.clean_stop = self.patch(switch, "clean_stop_tenants", side_effect=self.stop_tenants)
        self.real_wait_helper = switch.wait_helper
        self.wait_helper = self.patch(switch, "wait_helper", side_effect=self.helper_ready)
        self.real_available = switch.wait_helper_available
        self.patch(switch, "wait_helper_available", return_value={"result": "tenant", "running": False})
        self.real_wait_started = switch.wait_started
        self.patch(switch, "wait_started", side_effect=self.host_ready)
        self.patch(switch, "service_state", side_effect=lambda unit: "active" if self.services[unit] else "inactive")
        self.real_helper_state = switch.helper_startup_state
        self.helper_state = self.patch(switch, "helper_startup_state", return_value={
            "ActiveState": "active", "SubState": "running", "NRestarts": "0",
        })
        self.real_replace_json = switch.replace_json
        self.patch(switch, "replace_json", side_effect=self.replace_config)
        self.real_credential_check = switch.check_credential_set
        self.credential_check = self.patch(switch, "check_credential_set")

    def fault(self, point):
        if self.failure_point == point and not self.failed:
            self.failed = True
            raise OSError(f"injected {point}")

    def command(self, *args):
        event = tuple(map(str, args))
        self.events.append(event)
        if event[0] == "systemctl":
            action, unit = event[1:]
            self.services[unit] = action == "start"
            point = ("host_" if unit == "motor-host.service" else "helper_") + action
            if point == self.failure_point and not self.failed and action == "start":
                self.services[unit] = False
            self.fault(point)
        elif "select-job-images" in event:
            self.fault("selection")
        elif "check-config" in event and event[-1] == str(self.host_path):
            self.fault("config_check")

    def stop_tenants(self, *_):
        self.events.append(("clean-stop-tenants",))
        self.fault("tenant_stop")

    def helper_ready(self, *_):
        self.events.append(("helper-ready",))
        self.fault("helper_ready")

    def host_ready(self, path, timeout, **kwargs):
        if not kwargs.get("restore_drain"):
            self.fault("drain_check")
        return self.real_wait_started(path, timeout, **kwargs)

    def replace_config(self, path, value):
        self.real_replace_json(path, value)
        self.fault("host_config" if path == self.host_path else "helper_config")

    def patch(self, target, name, **kwargs):
        patcher = mock.patch.object(target, name, **kwargs)
        value = patcher.start()
        self.addCleanup(patcher.stop)
        return value

    def perform(self, mode, **kwargs):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            switch.switch(mode, per_job=self.per_job, job_vms=self.job_vms, generation="gen-1",
                          host_path=self.host_path, helper_path=self.helper_path, timeout=1, **kwargs)
        return output.getvalue()

    def test_per_job_switch_validates_publishes_selects_and_keeps_placement_drained(self):
        output = self.perform("per_job")
        self.assertEqual(switch.read_json(self.host_path)["mode"], "per_job")
        self.assertEqual(switch.read_json(self.helper_path)["job_vms"], switch.read_json(self.job_vms))
        self.assertEqual([event[:3] for event in self.events[2:]], [
            ("fresh-drain",),
            ("systemctl", "stop", "motor-host.service"),
            ("clean-stop-tenants",),
            ("systemctl", "stop", "motor-vm-helper.service"),
            ("systemctl", "start", "motor-vm-helper.service"),
            ("helper-ready",),
            # The replacement helper publishes, not the one it replaced.
            ("publish", self.host["helper_socket"], "gen-1"),
            ("runuser", "-u", "motor-host"),
            (str(switch.hosted_host.HOST_BINARY), "check-config", str(self.host_path)),
            ("systemctl", "start", "motor-host.service"),
        ])
        select = next(event for event in self.events if "select-job-images" in event)
        # A switch always comes from tenant mode, so a generation left current
        # by an earlier per-job period may be replaced while nothing holds it.
        self.assertEqual(select[-3:], (str(self.host_path), "gen-1", "--replace-idle"))
        self.assertIn("placement_hint.host='test-host'", output)
        self.assertIn("only after success", output)
        self.assertTrue(all(call.args[0] == {"op": "host_status"} for call in self.admin.call_args_list))

    def test_target_helper_checks_staged_config_before_any_service_stops(self):
        for install in (False, True):
            with self.subTest(install=install):
                self.events.clear()
                directory = self.artifacts() if install else self.installed
                validator = directory / "motor-vm-helper"
                original_command = self.command

                def check(*args):
                    if Path(args[0]) == validator and args[1] == "check-config":
                        self.assertEqual(Path(args[2]).parent.parent, self.helper_path.parent)
                        self.assertEqual(switch.read_json(args[2]), {
                            "allowed_uids": [1000], "job_vms": switch.read_json(self.job_vms),
                        })
                        raise ValueError("helper refused staged config: overlapping job VMM uid range")
                    original_command(*args)

                self.command_run.side_effect = check
                with self.assertRaisesRegex(ValueError, "helper refused staged config"):
                    self.perform("per_job", **({"install_binaries": directory, "expected_commit": "a" * 40} if install else {}))
                self.assertFalse(any(event[0] == "systemctl" for event in self.events))
                self.drain.assert_not_called()
                self.publish.assert_not_called()
                self.assert_restored()

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

    def test_tenant_rollback_checks_every_bound_account_credential_set_before_stopping(self):
        self.host["mode"] = "per_job"
        self.host_path.write_text(json.dumps(self.host))
        self.bindings.write_text(json.dumps({
            "vm": {"tenant": "tenant", "sub": "account"},
            "vm2": {"tenant": "tenant2", "sub": "other-account"},
        }))
        (self.root / "tenant2.json").write_text(json.dumps({"account": "other-account", "disk": str(self.disk)}))
        self.credential_check.side_effect = [None, ValueError("missing credential set for other-account; set-remote-cache")]
        with self.assertRaisesRegex(ValueError, "other-account; set-remote-cache"):
            self.perform("tenant")
        self.assertEqual(self.credential_check.call_args_list, [mock.call("account"), mock.call("other-account")])
        self.command_run.assert_not_called()
        self.drain.assert_not_called()

    def test_tenant_rollback_names_missing_disk_or_mismatched_registry_before_stopping(self):
        self.host["mode"] = "per_job"
        self.host_path.write_text(json.dumps(self.host))
        for registry, error in [({"account": "other-account", "disk": str(self.disk)}, "registry account.*other-account.*binding sub.*account"),
                                ({"account": "account", "disk": str(self.root / "missing.ext4")}, "rollback disk missing:.*missing.ext4")]:
            with self.subTest(registry=registry):
                (self.root / "tenant.json").write_text(json.dumps(registry))
                with self.assertRaisesRegex(ValueError, error):
                    self.perform("tenant")
                self.command_run.assert_not_called()
                self.drain.assert_not_called()

    def test_credential_set_checks_required_layout_and_permissions_without_reading_keys(self):
        store = self.root / "remote-cache"
        account = store / "account"
        metadata = {
            store: SimpleNamespace(st_uid=0, st_mode=0o40700),
            account: SimpleNamespace(st_uid=0, st_mode=0o40700),
            account / "config.json": SimpleNamespace(st_uid=0, st_mode=0o100600),
            account / "read-key": SimpleNamespace(st_uid=0, st_mode=0o100600),
        }

        def lstat(path):
            if path not in metadata:
                raise FileNotFoundError(2, "No such file or directory", str(path))
            return metadata[path]

        with mock.patch.object(switch, "REMOTE_CACHE_STORE", store), mock.patch.object(Path, "lstat", autospec=True, side_effect=lstat):
            with mock.patch.object(Path, "read_bytes", side_effect=AssertionError("must not read keys")):
                self.real_credential_check("account")
                for path in list(metadata):
                    original = metadata.pop(path)
                    with self.subTest(missing=path), self.assertRaisesRegex(ValueError, "account.*missing or insecure.*set-remote-cache --account account"):
                        self.real_credential_check("account")
                    metadata[path] = original
                for mode, uid in [(0o120777, 0), (0o100644, 0), (0o100600, 1000)]:
                    metadata[account / "read-key"] = SimpleNamespace(st_uid=uid, st_mode=mode)
                    with self.subTest(mode=mode, uid=uid), self.assertRaisesRegex(ValueError, "read-key.*set-remote-cache"):
                        self.real_credential_check("account")
                metadata[account / "read-key"] = SimpleNamespace(st_uid=0, st_mode=0o100600)
                account.mkdir(parents=True)
                (account / "write-key").write_text("not read by preflight")
                metadata[account / "write-key"] = SimpleNamespace(st_uid=0, st_mode=0o100600)
                self.real_credential_check("account")
                metadata[account / "write-key"] = SimpleNamespace(st_uid=0, st_mode=0o120777)
                with self.assertRaisesRegex(ValueError, "write-key.*set-remote-cache"):
                    self.real_credential_check("account")

    def test_credential_account_cannot_escape_the_store(self):
        for account in ("..", "../account", "/account", "."):
            with self.subTest(account=account), self.assertRaisesRegex(ValueError, "invalid rollback credential account"):
                self.real_credential_check(account)

    def test_generation_publication_failure_restores_config_and_services(self):
        self.publish.side_effect = ValueError("unverified image")
        before = self.host_path.read_bytes()
        with self.assertRaisesRegex(ValueError, "unverified image"):
            self.perform("per_job")
        self.assertEqual(self.host_path.read_bytes(), before)
        starts = [event for event in self.events if event[:2] == ("systemctl", "start")]
        self.assertEqual(starts[-2:], [("systemctl", "start", "motor-vm-helper.service"),
                                       ("systemctl", "start", "motor-host.service")])
        self.assertFalse(any("select-job-images" in event for event in self.events))

    def test_refuses_an_undrained_host(self):
        self.status["draining"] = False
        with self.assertRaisesRegex(ValueError, "drain the host"):
            self.perform("per_job")
        self.drain.assert_not_called()
        self.publish.assert_not_called()

    def test_clean_stop_timeout_never_stops_helper_or_rewrites_config(self):
        self.clean_stop.side_effect = TimeoutError("tenant clean stop timed out")
        before = self.host_path.read_bytes()
        with self.assertRaisesRegex(ValueError, "clean stop timed out"):
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
            with mock.patch.object(switch, "wait_helper_available", self.real_available):
                self.real_wait_helper("helper.sock", "tenant", 1)
            self.assertTrue(request.call_args.kwargs["wait"])

    def test_helper_start_wait_is_bounded_and_refuses_a_running_tenant(self):
        with mock.patch.object(switch, "helper_exchange", side_effect=ConnectionRefusedError()):
            with mock.patch.object(switch.time, "monotonic", side_effect=[0, 0, 2]):
                with self.assertRaisesRegex(ValueError, "helper socket did not become ready.*drained"):
                    switch.wait_helper_exchange("helper.sock", {}, 1)
        with mock.patch.object(switch, "helper_request", return_value={"result": "tenant", "running": True}):
            with self.assertRaisesRegex(ValueError, "did not confirm.*stopped"):
                with mock.patch.object(switch, "wait_helper_available", self.real_available):
                    self.real_wait_helper("helper.sock", "tenant", 1)

    def test_helper_start_wait_refuses_failed_or_auto_restarting_service_immediately(self):
        for state in ({"ActiveState": "failed", "SubState": "failed", "NRestarts": "0"},
                      {"ActiveState": "activating", "SubState": "auto-restart", "NRestarts": "1"}):
            with self.subTest(state=state):
                self.helper_state.return_value = state
                with mock.patch.object(switch, "helper_exchange") as exchange, mock.patch.object(switch.time, "sleep") as sleep:
                    with self.assertRaisesRegex(ValueError, "helper failed or is restarting"):
                        switch.wait_helper_exchange("helper.sock", {}, 300)
                    exchange.assert_not_called()
                    sleep.assert_not_called()

    def test_helper_start_wait_detects_restarts_between_polls_and_bounds_socket_timeout(self):
        self.helper_state.side_effect = [
            {"ActiveState": "active", "SubState": "running", "NRestarts": "2"},
            {"ActiveState": "active", "SubState": "running", "NRestarts": "3"},
        ]
        with mock.patch.object(switch, "helper_exchange", side_effect=TimeoutError()) as exchange, mock.patch.object(switch.time, "sleep"):
            with self.assertRaisesRegex(ValueError, "helper failed or is restarting"):
                switch.wait_helper_exchange("helper.sock", {}, 300)
            self.assertEqual(exchange.call_count, 1)
            self.assertLessEqual(exchange.call_args.args[2], 1)

    def test_helper_startup_state_reads_systemd_properties(self):
        with mock.patch.object(switch.subprocess, "run", return_value=mock.Mock(stdout="ActiveState=activating\nSubState=auto-restart\nNRestarts=5\n")) as run:
            self.assertEqual(self.real_helper_state(), {"ActiveState": "activating", "SubState": "auto-restart", "NRestarts": "5"})
            self.assertIn("--property=ActiveState,SubState,NRestarts", run.call_args.args[0])

    def test_failed_helper_probe_prints_last_twenty_journal_lines_as_root(self):
        failure = switch.subprocess.CalledProcessError(1, "runuser", stderr="helper failed or is restarting")
        journal = "\n".join(f"helper log {i}" for i in range(20)) + "\n"
        with mock.patch.object(switch.subprocess, "run", side_effect=[failure, mock.Mock(stdout=journal)]) as run:
            with contextlib.redirect_stderr(io.StringIO()) as output:
                with self.assertRaisesRegex(ValueError, "helper failed or is restarting"):
                    switch.helper_request("helper.sock", {}, 300, wait=True)
            self.assertEqual(output.getvalue(), journal)
            self.assertEqual(run.call_args.args[0], ["journalctl", "-u", "motor-vm-helper.service", "-n", "20", "--no-pager"])

    def artifacts(self, stamp="a" * 40):
        directory = self.root / "release"
        directory.mkdir(exist_ok=True)
        for name in ("motor-host", "motor-vm-helper"):
            path = directory / name
            path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{name} 0.1.0 {stamp}'\n")
            path.chmod(0o755)
        return directory

    def assert_restored(self):
        self.assertTrue(all(self.services.values()), self.services)
        self.assertEqual(self.host_path.read_bytes(), self.original_host)
        self.assertEqual(self.helper_path.read_bytes(), self.original_helper)
        self.assertTrue(self.status["draining"])
        for path, original in self.old_binaries.items():
            self.assertEqual(path.read_bytes(), original)

    def assert_failure_restores(self, point):
        self.failure_point = point
        with self.assertRaisesRegex(ValueError, f"injected {point}") as raised:
            self.perform("per_job", install_binaries=self.artifacts(), expected_commit="a" * 40)
        self.assertTrue(self.failed)
        self.assert_restored()
        self.assertIn("original configs/binaries restored", str(raised.exception))
        starts = [event for event in self.events if event[:2] == ("systemctl", "start")]
        self.assertEqual(starts[-2:], [("systemctl", "start", "motor-vm-helper.service"),
                                      ("systemctl", "start", "motor-host.service")])

    def test_host_stop_failure_restores_previous_running_services(self):
        self.assert_failure_restores("host_stop")

    def test_tenant_stop_failure_restores_previous_running_services(self):
        self.assert_failure_restores("tenant_stop")

    def test_helper_stop_failure_restores_previous_running_services(self):
        self.assert_failure_restores("helper_stop")

    def test_host_config_replace_failure_restores_exact_original_files(self):
        self.assert_failure_restores("host_config")

    def test_helper_config_replace_failure_restores_exact_original_files(self):
        self.assert_failure_restores("helper_config")

    def test_helper_start_failure_restores_previous_running_services(self):
        self.assert_failure_restores("helper_start")

    def test_helper_readiness_failure_restores_previous_running_services(self):
        self.assert_failure_restores("helper_ready")

    def test_image_selection_failure_restores_previous_running_services(self):
        self.assert_failure_restores("selection")

    def test_config_check_failure_restores_previous_running_services(self):
        self.assert_failure_restores("config_check")

    def test_host_start_failure_restores_previous_running_services(self):
        self.assert_failure_restores("host_start")

    def test_drain_check_failure_restores_previous_running_services(self):
        self.assert_failure_restores("drain_check")

    def test_unclean_tenant_stop_resumes_original_mode_without_stopping_helper(self):
        self.clean_stop.side_effect = ValueError("tenant stop was not clean")
        with self.assertRaisesRegex(ValueError, "stop was not clean.*original configs/binaries restored"):
            self.perform("per_job")
        self.assert_restored()
        self.assertNotIn(("systemctl", "stop", "motor-vm-helper.service"), self.events)

    def test_failed_tenant_rollback_switch_restores_original_per_job_mode(self):
        self.host.update(mode="per_job", per_job={"workspace_disk_cap_bytes": 1024})
        self.host_path.write_text(json.dumps(self.host))
        self.original_host = self.host_path.read_bytes()
        self.failure_point = "drain_check"
        with self.assertRaisesRegex(ValueError, "injected drain_check"):
            self.perform("tenant")
        self.assert_restored()

    def test_binary_install_keeps_rollback_copies_and_installs_complete_files_atomically(self):
        directory = self.artifacts()
        old_inodes = {path: path.stat().st_ino for path in self.old_binaries}
        self.perform("per_job", install_binaries=directory, expected_commit="a" * 40)
        for path, original in self.old_binaries.items():
            self.assertEqual(path.read_bytes(), (directory / path.name).read_bytes())
            self.assertNotEqual(path.stat().st_ino, old_inodes[path])
            self.assertEqual(path.stat().st_mode & 0o777, 0o755)
            backups = list(path.parent.glob(path.name + ".bak-*"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_bytes(), original)

    def test_binary_install_rejects_unstamped_mismatched_missing_or_unpinned_artifacts_before_stop(self):
        for stamp, expected in [("unstamped", "a" * 40), ("b" * 40, "a" * 40), ("a" * 40, None)]:
            with self.subTest(stamp=stamp, expected=expected):
                with self.assertRaisesRegex(ValueError, "expected.commit|expected commit"):
                    self.perform("per_job", install_binaries=self.artifacts(stamp), expected_commit=expected)
                self.assert_restored()
                self.assertFalse(any(event[0] == "systemctl" for event in self.events))
        directory = self.artifacts()
        (directory / "motor-vm-helper").unlink()
        with self.assertRaises(FileNotFoundError):
            self.perform("per_job", install_binaries=directory, expected_commit="a" * 40)
        self.assert_restored()

    def test_partial_binary_install_failure_restores_both_binaries_before_any_service_stop(self):
        directory = self.artifacts()
        original_copy = switch.copy_atomic

        def fail_copy(source, target, metadata, **kwargs):
            original_copy(source, target, metadata, **kwargs)
            if Path(source) == directory / "motor-vm-helper":
                raise OSError("binary install failed after rename")

        with mock.patch.object(switch, "copy_atomic", side_effect=fail_copy):
            with self.assertRaisesRegex(ValueError, "binary install failed.*original configs/binaries restored"):
                self.perform("per_job", install_binaries=directory, expected_commit="a" * 40)
        self.assert_restored()
        self.assertFalse(any(event[:2] == ("systemctl", "stop") for event in self.events))

    def test_binary_changed_after_version_validation_is_not_installed(self):
        directory = self.artifacts()
        with contextlib.redirect_stdout(io.StringIO()):
            binaries = switch.BinaryInstall(directory, "a" * 40)
        (directory / "motor-host").write_bytes(b"different artifact")
        with self.assertRaisesRegex(ValueError, "changed since validation"):
            binaries.install()
        self.assert_restored()

    def test_restore_failure_names_the_down_service_and_each_on_disk_config(self):
        original_command = self.command

        def fail_start(*args):
            if args == ("systemctl", "start", "motor-vm-helper.service"):
                self.services["motor-vm-helper.service"] = False
                raise OSError("helper will not start")
            return original_command(*args)

        self.command_run.side_effect = fail_start
        with self.assertRaisesRegex(ValueError, "RESTORATION FAILED") as raised:
            self.perform("per_job")
        message = str(raised.exception)
        self.assertIn("motor-vm-helper.service is inactive", message)
        self.assertIn(f"{self.host_path}: original", message)
        self.assertIn(f"{self.helper_path}: original", message)
        self.assertIn(("systemctl", "start", "motor-host.service"), self.events)

    def test_restore_failure_identifies_a_config_that_could_not_be_put_back(self):
        original_restore = switch.SavedConfig.restore

        def fail_restore(config):
            if config.path == self.host_path:
                raise OSError("host config restore denied")
            return original_restore(config)

        self.failure_point = "selection"
        with mock.patch.object(switch.SavedConfig, "restore", fail_restore):
            with self.assertRaisesRegex(ValueError, "RESTORATION FAILED") as raised:
                self.perform("per_job")
        self.assertIn(f"{self.host_path}: changed (per_job)", str(raised.exception))
        self.assertIn(f"{self.helper_path}: original", str(raised.exception))
        self.assertTrue(all(self.services.values()))

    def test_restore_does_not_stop_helper_when_replacement_host_will_not_stop(self):
        original_command = self.command
        self.failure_point = "drain_check"

        def fail_second_stop(*args):
            if args == ("systemctl", "stop", "motor-host.service") and self.failed:
                self.services["motor-host.service"] = True
                raise OSError("replacement host will not stop")
            return original_command(*args)

        self.command_run.side_effect = fail_second_stop
        with self.assertRaisesRegex(ValueError, "helper restart withheld"):
            self.perform("per_job")
        self.assertEqual(self.clean_stop.call_count, 1)
        self.assertEqual(self.events.count(("systemctl", "stop", "motor-vm-helper.service")), 1)
        self.assert_restored()


if __name__ == "__main__":
    unittest.main()
