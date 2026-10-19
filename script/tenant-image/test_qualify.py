#!/usr/bin/env python3
"""Portable qualification plumbing tests, never VM qualification evidence."""
import base64
import importlib.util
import json
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock
import uuid

spec = importlib.util.spec_from_file_location("qualify", Path(__file__).with_name("qualify.py"))
assert spec is not None and spec.loader is not None
q = importlib.util.module_from_spec(spec)
spec.loader.exec_module(q)


def output(stream, data):
    return dict(type="output", stream=stream, bytes=base64.b64encode(data).decode())


def frames():
    return [output("stderr", b"err\n"), output("stdout", b"out\nTARGET /tenant/target 123\n"),
            dict(type="terminal", outcome=dict(type="exit", code=23))]


class QualificationTests(unittest.TestCase):
    def test_program_staging_copies_bytes_not_symlink_or_writable_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "built-program"
            source.write_bytes(b"reviewed program\n")
            source.chmod(0o777)
            link = root / "loader-link"
            link.symlink_to(source)
            destination = root / "trusted/bin/loader"
            own = mock.Mock()
            evidence = q.stage_program(link, destination, own=own)
            self.assertEqual(destination.read_bytes(), b"reviewed program\n")
            self.assertFalse(destination.is_symlink())
            self.assertEqual(destination.stat().st_mode & 0o7777, 0o755)
            self.assertEqual(source.stat().st_mode & 0o7777, 0o777)
            own.assert_any_call(destination.parent, 0, 0)
            own.assert_any_call(destination, 0, 0)
            self.assertEqual(evidence["mode"], "0o755")
            with self.assertRaises(FileExistsError):
                q.stage_program(source, destination, own=own)

    def test_not_loaded_unit_is_idempotent_but_access_denied_is_not(self):
        harness = object.__new__(q.Harness)
        harness.record = mock.Mock()
        gone = subprocess.CompletedProcess([], 5, stdout=b"", stderr=b"Unit gone.service not loaded.\n")
        with mock.patch.object(q.subprocess, "run", return_value=gone):
            self.assertEqual(harness.run("systemctl", "stop", "gone.service", allow_missing_unit=True), b"")
            with self.assertRaises(RuntimeError):
                harness.run("systemctl", "stop", "gone.service")
        denied = subprocess.CompletedProcess([], 1, stdout=b"", stderr=b"Access denied\n")
        with mock.patch.object(q.subprocess, "run", return_value=denied), self.assertRaises(RuntimeError):
            harness.run("systemctl", "stop", "gone.service", allow_missing_unit=True)

    def test_helper_exit_reports_state_and_exact_journal_without_timeout(self):
        harness = object.__new__(q.Harness)
        harness.run = mock.Mock(side_effect=[b"LoadState=not-found\nActiveState=inactive\nExecMainStatus=1\n",
                                             b"helper refusal: path=/trusted/loader uid=0 gid=0 mode=0777 symlink=true\n"])
        with self.assertRaisesRegex(RuntimeError, "helper exited.*(?s:.*path=/trusted/loader uid=0)"):
            harness.wait_for_helper("failed.service")
        self.assertEqual(harness.run.call_count, 2)

    def test_missing_tree_is_already_removed_but_permission_errors_propagate(self):
        with tempfile.TemporaryDirectory() as tmp:
            q.remove_owned_tree(Path(tmp) / "missing")
        def denied(path, onerror):
            error = PermissionError("denied")
            onerror(None, path, (PermissionError, error, None))
        with mock.patch.object(q.shutil, "rmtree", side_effect=denied), self.assertRaises(PermissionError):
            q.remove_owned_tree(Path("/not-used"))

    def test_teardown_twice_tolerates_missing_helper_and_owned_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            registry, bindings, socket = root / "registry", root / "bindings.json", root / "helper.sock"
            registry.mkdir()
            mountinfo, forwarding = root / "mountinfo", root / "forwarding"
            mountinfo.write_text("1 0 8:1 / / rw - xfs /dev/root rw\n")
            forwarding.write_text("1\n")
            harness = object.__new__(q.Harness)
            harness.args = SimpleNamespace(tap_index=254)
            harness.name = "owned-fixture"
            harness.results = root / "results"
            harness.results.mkdir()
            harness.work, harness.home, harness.image = root / "work", root / "tenants/owned-fixture", root / "image"
            harness.registry = registry / "owned-fixture.json"
            harness.registry.write_text("{}\n")
            for path in (harness.work, harness.home, harness.image):
                path.mkdir(parents=True)
            harness.units, harness.processes = ["gone.service"], []
            harness.started_helper = harness.owned = True
            harness.bindings_before = None
            harness.base_jails_before = set()
            harness.forwarding_before = "0\n"
            harness.baseline = dict(failed_units=[])
            harness.record = mock.Mock()
            harness.helper = mock.Mock()
            harness.run = mock.Mock(return_value=b"")
            harness.console = mock.Mock(return_value="retained console\n")
            harness.host_state = mock.Mock(return_value=dict(tenant_vmm=[], tenant_taps=[], tenant_table=False,
                                                            netns=[], failed_units=[], ipv4_forwarding="0"))
            with mock.patch.multiple(q, ROOT=root, REGISTRY=registry, BINDINGS=bindings, HELPER_SOCKET=socket,
                                     MOUNTINFO=mountinfo, FORWARDING=forwarding):
                harness.teardown()
                harness.teardown()
            harness.helper.assert_not_called()
            self.assertFalse(harness.registry.exists())
            self.assertFalse(harness.work.exists())
            self.assertEqual(forwarding.read_text(), "0\n")
            harness.run.assert_any_call("systemctl", "stop", "gone.service", allow_missing_unit=True)

    def test_transient_helper_waits_for_the_real_host_private_storage_unit(self):
        command = q.helper_unit_command("qualification.service", Path("/bin/helper"), Path("/config.json"))
        self.assertIn("--property=Requires=motor-vm-storage-private.service", command)
        self.assertIn("--property=After=motor-vm-storage-private.service", command)
        self.assertIn("--property=DelegateSubgroup=helper", command)
        self.assertEqual(command[-3:], [Path("/bin/helper"), "--config", Path("/config.json")])

    def test_qualification_and_provisioning_use_the_same_trusted_bind_root(self):
        provision = q.module("qualification_layout_provision", q.HERE.parent / "tenant-provision/provision.py")
        self.assertEqual(q.ROOT, Path("/var/lib/motor-vm"))
        self.assertEqual(provision.ROOT, q.ROOT)

    def test_boot_args_preserve_manifest_and_deliver_opt_in_unit(self):
        original = "console=ttyS0 root=/dev/vda ro init=/sbin/motor-init net.ifnames=0"
        args = q.qualification_args(original)
        self.assertTrue(args.startswith(original + " "))
        self.assertIn("systemd.wants=motor-image-check.service", args.split())
        self.assertIn("systemd.wants=motor-qualification.service", args.split())
        credential = next(v.split(":", 1)[1] for v in args.split() if v.startswith("systemd.set_credential_binary="))
        unit = base64.b64decode(credential, validate=True).decode()
        self.assertEqual(unit, (q.HERE / "guest/motor-qualification.service").read_text())
        self.assertIn("StandardOutput=tty", unit)
        self.assertIn("TTYPath=/dev/ttyS0", unit)
        self.assertIn("ConditionKernelCommandLine=systemd.wants=motor-qualification.service", unit)
        self.assertNotIn("[Install]", unit)
        self.assertNotIn("systemd.wants=motor-qualification.service", original)
        self.assertLess(len(args.encode()), 4096)

    def test_normal_image_does_not_install_or_want_qualification_unit(self):
        installer = q.module("qualification_install_policy", q.HERE / "install-guest.py")
        with tempfile.TemporaryDirectory() as tmp:
            source, installed = Path(tmp) / "source", Path(tmp) / "installed"
            source.mkdir()
            production = ("motor-tenant-agent.service", "motor-net-rules.service", "motor-image-check.service")
            for name in (*production, "motor-qualification.service"):
                (source / name).write_text("fixture service\n")
            installer.install_services(source, installed)
            self.assertEqual(sorted(path.name for path in installed.iterdir()), sorted(production))
            self.assertFalse((installed / "motor-qualification.service").exists())
        for path in (q.HERE / "guest").glob("*.service"):
            if path.name != "motor-qualification.service":
                self.assertNotIn("motor-qualification", path.read_text())

    def test_bad_or_already_qualified_boot_args_refused(self):
        for args in ("", "a\nb", None, "x systemd.run=evil", "systemd.wants=motor-qualification.service"):
            with self.subTest(args=args), self.assertRaises(ValueError):
                q.qualification_args(args)

    def test_inventory_lines_match_only_complete_known_success(self):
        for expected in (q.LIVE, q.TENANT):
            self.assertTrue(q.exact_line("boot\r\n" + expected + "\r\n", expected))
            for text in (expected + " extra", "prefix " + expected, expected[:-1], expected.replace("passed", "failed")):
                self.assertFalse(q.exact_line(text, expected), text)
        self.assertFalse(q.exact_line("hosted image live inventory: 10 checks passed", q.LIVE))

    def test_guard_uses_170_gib_and_allocation_bytes(self):
        q.guard(q.FLOOR)
        q.guard(q.FLOOR + 123, 123)
        with self.assertRaisesRegex(RuntimeError, "170 GiB"):
            q.guard(q.FLOOR - 1)
        with self.assertRaises(RuntimeError):
            q.guard(q.FLOOR + 122, 123)

    def test_job_request_has_production_workspace_bundle_and_lease_limits(self):
        value = q.run_request("id", "commit", "tree", b"bundle", "exit 23")
        self.assertEqual(value["workspace_key"], q.WORKSPACE)
        self.assertEqual(value["repository_root"], q.WORKSPACE)
        self.assertEqual(value["weight_hint"], 1)
        self.assertEqual(value["timeout"], 30)
        self.assertEqual(value["bundles"][0]["length"], 6)
        self.assertEqual(value["bundles"][0]["sha256"], "1e6ed65d77d6364eeaed5a745ba5c4985ae2b700dd85d7cf7f027bdf294a33fc")

    def test_wire_framing_roundtrip_and_closed_or_oversized_frame(self):
        a, b = socket.socketpair()
        with a, b:
            q.send_frame(a, {"type": "hello", "protocol_version": 1})
            self.assertEqual(q.frame(b), {"type": "hello", "protocol_version": 1})
            a.sendall(struct.pack(">I", 0))
            with self.assertRaises(ValueError):
                q.frame(b)
            a.sendall(struct.pack(">I", 5) + b"xx")
            a.shutdown(socket.SHUT_WR)
            with self.assertRaises(EOFError):
                q.frame(b)

    def test_uuid_is_lowercase_version_seven_with_unique_random_bits(self):
        ids = [q.job_id() for _ in range(20)]
        self.assertEqual(len(set(ids)), 20)
        for identifier in ids:
            self.assertEqual(str(uuid.UUID(identifier)), identifier)
            self.assertEqual(uuid.UUID(identifier).version, 7)

    def test_result_requires_nonzero_exit_both_streams_and_exact_target(self):
        self.assertEqual(q.assert_job(frames(), 23, b"out\n", b"err\n"), (b"/tenant/target", b"123"))
        for bad in (frames()[:-1], frames() + frames()[-1:],
                    [*frames()[:-1], dict(type="terminal", outcome=dict(type="exit", code=0))],
                    [output("stdout", b"out\nTARGET /tenant/target 123\n"), frames()[-1]],
                    [output("stdout", b"out\nTARGET relative 123\n"), output("stderr", b"err\n"), frames()[-1]]):
            with self.subTest(frames=bad), self.assertRaises(RuntimeError):
                q.assert_job(bad, 23, b"out\n", b"err\n")

    def test_output_chunks_keep_stdout_and_stderr_separate(self):
        value = [output("stdout", b"ou"), output("stderr", b"er"),
                 output("stdout", b"t\nTARGET /x 45\n"), output("stderr", b"r\n"), frames()[-1]]
        self.assertEqual(q.assert_job(value, 23, b"out\n", b"err\n"), (b"/x", b"45"))

    def test_failure_never_skips_teardown_or_promotes_unreached_steps(self):
        with tempfile.TemporaryDirectory() as tmp:
            harness = object.__new__(q.Harness)
            harness.results = Path(tmp)
            harness.verdicts = {}
            harness.name = "portable-fixture"
            harness.log = (Path(tmp) / "raw.jsonl").open("w")
            harness.preflight = mock.Mock(side_effect=RuntimeError("fixture missing test binary"))
            harness.teardown = mock.Mock(return_value="all owned resources gone")
            with mock.patch("builtins.print"):
                self.assertEqual(harness.execute(), 1)
            harness.teardown.assert_called_once()
            summary = json.loads((Path(tmp) / "summary.json").read_text())
            self.assertEqual(summary["verdict"], "FAIL")
            self.assertTrue(summary["steps"]["teardown"])
            self.assertFalse(summary["steps"]["vsock isolation"])
            self.assertFalse(summary["steps"]["post-restore job"])
            self.assertEqual(set(summary["steps"]), set(q.STEPS))

    def test_console_success_cannot_hide_a_guest_payload_failure(self):
        harness = object.__new__(q.Harness)
        harness.console = lambda: q.TENANT + "\nFAIL guest qualification: isolation failed\n"
        with self.assertRaisesRegex(RuntimeError, "isolation failed"):
            harness.expect_console(q.TENANT)

    def test_empty_exception_still_names_the_type_and_preserves_traceback(self):
        with tempfile.TemporaryDirectory() as tmp:
            harness = object.__new__(q.Harness)
            harness.results = Path(tmp)
            harness.verdicts = {}
            harness.name = "portable-fixture"
            harness.log = (Path(tmp) / "raw.jsonl").open("w")
            harness.preflight = mock.Mock(side_effect=EOFError())
            harness.teardown = mock.Mock(return_value="clean")
            with mock.patch("builtins.print"):
                self.assertEqual(harness.execute(), 1)
            summary = json.loads((Path(tmp) / "summary.json").read_text())
            self.assertEqual(summary["error"], "EOFError: EOFError()")
            events = [json.loads(line) for line in (Path(tmp) / "raw.jsonl").read_text().splitlines()]
            failed = next(row for row in events if row.get("step") == "preflight")
            self.assertEqual(failed["exception_type"], "EOFError")
            self.assertIn("EOFError", failed["traceback"])
            self.assertNotEqual(failed["error"], "")

    def test_subprocess_failure_keeps_real_exit_status_stdout_and_stderr(self):
        error = subprocess.CalledProcessError(137, ["jailer", "--id", "base"], output=b"raw stdout\n", stderr=b"raw stderr\n")
        evidence = q.error_evidence(error)
        self.assertEqual(evidence["returncode"], 137)
        self.assertEqual(evidence["stdout"], "raw stdout\n")
        self.assertEqual(evidence["stderr"], "raw stderr\n")
        self.assertEqual(evidence["command"], ["jailer", "--id", "base"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
